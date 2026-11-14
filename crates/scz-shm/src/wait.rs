//! Futex waits and wakes on shared words (signaling §5), and the clock they are timed by.
//!
//! The futexes are shared ones (no `FUTEX_PRIVATE_FLAG`): waiter and waker are in different
//! processes. Deadlines are absolute `CLOCK_MONOTONIC`, so a wait interrupted and resumed
//! does not drift (`FUTEX_WAIT_BITSET`).

use crate::sync::AtomicU32;
#[cfg(not(loom))]
use crate::sync::AtomicU64;

/// Nanoseconds of `CLOCK_MONOTONIC`, the clock every deadline and heartbeat is in.
#[cfg(not(loom))]
#[inline]
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-pointer; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(loom))]
fn futex_word(word: &AtomicU32) -> *const u32 {
    word as *const AtomicU32 as *const u32
}

/// Sleep while `*word == expected`, until `deadline_ns` (absolute, monotonic) if given.
/// Returns false only on a timeout. Spurious returns are possible: callers re-check.
#[cfg(not(loom))]
pub fn futex_wait(word: &AtomicU32, expected: u32, deadline_ns: Option<u64>) -> bool {
    futex_wait_raw(futex_word(word), expected, deadline_ns)
}

/// The same, on the low half of a 64-bit word (the slot word; little-endian only).
#[cfg(not(loom))]
pub fn futex_wait_low32(word: &AtomicU64, expected: u32, deadline_ns: Option<u64>) -> bool {
    futex_wait_raw(word as *const AtomicU64 as *const u32, expected, deadline_ns)
}

#[cfg(not(loom))]
fn futex_wait_raw(addr: *const u32, expected: u32, deadline_ns: Option<u64>) -> bool {
    let ts;
    let tsp = match deadline_ns {
        Some(d) => {
            ts = libc::timespec { tv_sec: (d / 1_000_000_000) as libc::time_t, tv_nsec: (d % 1_000_000_000) as libc::c_long };
            &ts as *const libc::timespec
        }
        None => core::ptr::null(),
    };
    // SAFETY: `addr` points into a live mapping for the duration of the call; the kernel only
    // reads the word. FUTEX_WAIT_BITSET with an absolute CLOCK_MONOTONIC timeout.
    let r = unsafe { libc::syscall(libc::SYS_futex, addr, libc::FUTEX_WAIT_BITSET, expected, tsp, core::ptr::null::<u32>(), libc::FUTEX_BITSET_MATCH_ANY) };
    !(r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ETIMEDOUT))
}

/// Wake up to `n` waiters on `word`.
#[cfg(not(loom))]
pub fn futex_wake(word: &AtomicU32, n: u32) {
    futex_wake_raw(futex_word(word), n)
}

#[cfg(not(loom))]
pub fn futex_wake_low32(word: &AtomicU64, n: u32) {
    futex_wake_raw(word as *const AtomicU64 as *const u32, n)
}

#[cfg(not(loom))]
fn futex_wake_raw(addr: *const u32, n: u32) {
    // SAFETY: as above; FUTEX_WAKE does not dereference the word.
    unsafe { libc::syscall(libc::SYS_futex, addr, libc::FUTEX_WAKE, n, core::ptr::null::<libc::timespec>(), core::ptr::null::<u32>(), 0) };
}

/// A futex for loom models. It promises what the kernel promises and no more: `wait`
/// compares and sleeps as one step with respect to `wake`, and may return spuriously
/// (testing §2.2). Built from loom's own mutex and condition variable.
#[cfg(loom)]
pub struct MockFutex {
    m: loom::sync::Mutex<()>,
    cv: loom::sync::Condvar,
}

#[cfg(loom)]
impl MockFutex {
    pub fn new() -> Self {
        MockFutex { m: loom::sync::Mutex::new(()), cv: loom::sync::Condvar::new() }
    }
    /// The kernel compares against the word's current value, so the mock reads it with a
    /// read-modify-write, which always sees the latest value. A plain load could return an
    /// older one under loom, and report a lost wake-up the kernel cannot produce.
    pub fn wait32(&self, word: &AtomicU32, expected: u32) {
        let g = self.m.lock().unwrap();
        if word.fetch_or(0, crate::sync::Ordering::SeqCst) != expected {
            return;
        }
        let _g = self.cv.wait(g).unwrap();
    }
    pub fn wait_low32(&self, word: &crate::sync::AtomicU64, expected: u32) {
        let g = self.m.lock().unwrap();
        if word.fetch_or(0, crate::sync::Ordering::SeqCst) as u32 != expected {
            return;
        }
        let _g = self.cv.wait(g).unwrap();
    }
    pub fn wake(&self) {
        let _g = self.m.lock().unwrap();
        self.cv.notify_all();
    }
}

#[cfg(loom)]
impl Default for MockFutex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn a_wait_on_a_changed_word_returns_at_once_and_a_deadline_times_out() {
        let w = AtomicU32::new(5);
        assert!(futex_wait(&w, 4, None), "the word is not 4, so this must not sleep");
        let t0 = monotonic_ns();
        assert!(!futex_wait(&w, 5, Some(t0 + 2_000_000)), "nobody wakes it: a timeout");
        assert!(monotonic_ns() - t0 >= 2_000_000);
    }

    #[test]
    fn a_wake_reaches_a_sleeper() {
        use std::sync::Arc;
        let w = Arc::new(AtomicU32::new(0));
        let w2 = w.clone();
        let t = std::thread::spawn(move || {
            while w2.load(crate::sync::Ordering::Acquire) == 0 {
                futex_wait(&w2, 0, Some(monotonic_ns() + 5_000_000_000));
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        w.store(1, crate::sync::Ordering::Release);
        futex_wake(&w, 1);
        t.join().unwrap();
    }
}
