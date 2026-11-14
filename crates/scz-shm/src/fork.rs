//! Fork without exec (sdk-contract §7, "Fork"). A child forked from a connected process must
//! not use its parent's session: in the child, every registered client stops publishing its
//! snapshot, so its calls fall back with `NOT_CONNECTED`, and every registered session socket
//! is closed, so the supervisor still sees the session end when the parent's does.
//!
//! Clients register the word that names their current snapshot and their session socket. The
//! child handler, run by `pthread_atfork` in the child, clears and closes them with plain
//! atomic stores and `close`, which is all a child of a multithreaded process may safely do.
//! v1 allows one client process per sidecar, so the child serves fallbacks.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Once;

const MAX: usize = 64;

static WORDS: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static FDS: [AtomicI32; MAX] = [const { AtomicI32::new(-1) }; MAX];
static INSTALL: Once = Once::new();
static FORKED: AtomicBool = AtomicBool::new(false);

extern "C" fn in_child() {
    FORKED.store(true, SeqCst);
    for w in &WORDS {
        let a = w.load(SeqCst);
        if a != 0 {
            // SAFETY: a registered word lives until its Registration is dropped, which removes
            // it from this table first (in the parent, before the memory is freed).
            unsafe { &*(a as *const AtomicU64) }.store(0, SeqCst);
        }
    }
    for f in &FDS {
        let fd = f.swap(-1, SeqCst);
        if fd >= 0 {
            // SAFETY: close on a descriptor number this process owned at the fork.
            unsafe { libc::close(fd) };
        }
    }
}

fn install() {
    INSTALL.call_once(|| {
        // SAFETY: registers a handler that only performs atomic stores and close.
        unsafe { libc::pthread_atfork(None, None, Some(in_child)) };
    });
}

/// Whether this process is a child forked after a client registered.
pub fn forked() -> bool {
    FORKED.load(SeqCst)
}

/// One registration; dropping it removes the entry.
#[derive(Debug)]
pub struct Registration {
    word: Option<usize>,
    fd: Option<usize>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(i) = self.word {
            WORDS[i].store(0, SeqCst);
        }
        if let Some(i) = self.fd {
            FDS[i].store(-1, SeqCst);
        }
    }
}

/// Clear `w` (a snapshot id; 0 means none) in a forked child. `w` must outlive the
/// registration. None if the table is full, in which case a child would not fall back.
pub fn clear_in_child(w: &AtomicU64) -> Option<Registration> {
    install();
    let a = w as *const AtomicU64 as usize;
    let i = WORDS.iter().position(|s| s.compare_exchange(0, a, SeqCst, SeqCst).is_ok())?;
    Some(Registration { word: Some(i), fd: None })
}

/// Close `fd` in a forked child. Drop the registration before closing `fd` in this process.
pub fn close_in_child(fd: RawFd) -> Option<Registration> {
    install();
    let i = FDS.iter().position(|s| s.compare_exchange(-1, fd, SeqCst, SeqCst).is_ok())?;
    Some(Registration { word: None, fd: Some(i) })
}
