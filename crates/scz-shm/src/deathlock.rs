//! Death locks (supervisor §5.2, "Early death signal"; shm-abi §2, the worker segment).
//!
//! Each consume thread holds a robust, process-shared mutex in its worker's segment from just
//! before the worker reports WARM until it exits. When the thread dies, for any reason, the
//! kernel sets the owner-died bit in the mutex's futex word and wakes its waiters, before the
//! dying process's memory is freed. A supervisor watcher sleeps on that word. Spike S6b
//! measured the report at about 0.05 ms after a kill, at any worker size.
//!
//! The worker side goes through glibc (the mutex must be registered on the thread's robust
//! list, which only the C library does); the supervisor side touches only the futex word,
//! which glibc keeps at offset 0 of `pthread_mutex_t`.

use crate::abi;
use crate::seg::Seg;
use crate::wait::futex_wait;
use std::io;
use std::sync::atomic::Ordering::SeqCst;

pub const FUTEX_WAITERS: u32 = 0x8000_0000;
pub const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
pub const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

const CELL: usize = abi::worker_segment::death_lock::STRIDE;
const _: () = assert!(core::mem::size_of::<libc::pthread_mutex_t>() <= CELL, "a mutex must fit in a 64-byte cell");

/// How many locks a worker segment holds: one per ring at most.
pub const MAX: usize = abi::worker_segment::death_lock::COUNT;

fn mutex(seg: &Seg<'_>, i: usize) -> *mut libc::pthread_mutex_t {
    assert!(i < MAX && (i + 1) * CELL <= seg.len());
    // SAFETY: in bounds (checked above), 64-byte aligned within a page-aligned mapping.
    unsafe { seg.as_ptr().add(abi::worker_segment::death_lock::OFF + i * CELL) as *mut libc::pthread_mutex_t }
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::new(io::Error::from_raw_os_error(r).kind(), format!("{what}: {}", io::Error::from_raw_os_error(r))))
    }
}

/// Worker: make lock `i` a robust, process-shared mutex. Once, before any thread takes it.
pub fn init(seg: &Seg<'_>, i: usize) -> io::Result<()> {
    let m = mutex(seg, i);
    // SAFETY: a zeroed attribute object initialised by pthread_mutexattr_init, then used to
    // initialise a mutex in memory that is mapped writable and outlives every use of it.
    unsafe {
        let mut a: libc::pthread_mutexattr_t = core::mem::zeroed();
        check(libc::pthread_mutexattr_init(&mut a), "mutexattr_init")?;
        check(libc::pthread_mutexattr_setpshared(&mut a, libc::PTHREAD_PROCESS_SHARED), "setpshared")?;
        check(libc::pthread_mutexattr_setrobust(&mut a, libc::PTHREAD_MUTEX_ROBUST), "setrobust")?;
        let r = libc::pthread_mutex_init(m, &a);
        libc::pthread_mutexattr_destroy(&mut a);
        check(r, "mutex_init")
    }
}

/// Worker: take lock `i` on the calling thread and keep it until the thread exits.
pub fn take(seg: &Seg<'_>, i: usize) -> io::Result<()> {
    // SAFETY: initialised by `init`; the mapping outlives the thread.
    check(unsafe { libc::pthread_mutex_lock(mutex(seg, i)) }, "mutex_lock")
}

/// What a watcher saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watch {
    /// The owner died: the kernel set the owner-died bit.
    Died,
    /// The lock is free, so nobody holds it: never taken, or released (a worker never does).
    Free,
}

/// Supervisor: wait until lock `i`'s owner dies. Sets the waiters bit so that the kernel wakes
/// this thread when it marks the owner dead (the protocol of spike S6b).
pub fn watch(seg: &Seg<'_>, i: usize) -> Watch {
    let w = seg.u32_at(abi::worker_segment::death_lock::OFF + i * CELL + abi::death_lock::FUTEX_WORD);
    loop {
        let v = w.load(SeqCst);
        if v & FUTEX_OWNER_DIED != 0 {
            return Watch::Died;
        }
        if v & FUTEX_TID_MASK == 0 {
            return Watch::Free;
        }
        let mut v = v;
        if v & FUTEX_WAITERS == 0 {
            if w.compare_exchange(v, v | FUTEX_WAITERS, SeqCst, SeqCst).is_err() {
                continue;
            }
            v |= FUTEX_WAITERS;
        }
        futex_wait(w, v, None);
    }
}

/// Supervisor: has lock `i` been taken by a live owner? (Watchers start once it has.)
pub fn held(seg: &Seg<'_>, i: usize) -> bool {
    let v = seg.u32_at(abi::worker_segment::death_lock::OFF + i * CELL).load(SeqCst);
    v & FUTEX_TID_MASK != 0 && v & FUTEX_OWNER_DIED == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LocalSegment;
    use std::sync::Arc;

    struct Shared(LocalSegment);
    // SAFETY: the segment is reached only through atomics and the pthread calls above.
    unsafe impl Sync for Shared {}
    unsafe impl Send for Shared {}

    #[test]
    fn a_thread_that_exits_holding_its_lock_is_reported_dead() {
        let s = Arc::new(Shared(LocalSegment::new(4096)));
        init(&s.0.seg(), 0).unwrap();
        let s2 = s.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            take(&s2.0.seg(), 0).unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
            // the thread ends holding the lock
        });
        rx.recv().unwrap();
        assert!(held(&s.0.seg(), 0));
        let t0 = std::time::Instant::now();
        assert_eq!(watch(&s.0.seg(), 0), Watch::Died);
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        holder.join().unwrap();
    }

    #[test]
    fn a_lock_never_taken_reads_free() {
        let s = LocalSegment::new(4096);
        init(&s.seg(), 1).unwrap();
        assert_eq!(watch(&s.seg(), 1), Watch::Free);
    }
}
