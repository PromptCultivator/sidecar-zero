//! The atomics every protocol function is written against. Under `--cfg loom` they are
//! loom's, so the same code can be model-checked (testing §2.2).

#[cfg(not(loom))]
pub use core::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
#[cfg(loom)]
pub use loom::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
#[cfg(loom)]
pub use loom::sync::Mutex;
#[cfg(not(loom))]
pub use std::sync::Mutex;

/// The store-load barrier the wake protocols need. Written as an explicit fence, not as
/// sequentially consistent accesses, because that is the form loom can check (shm-abi §4.3).
#[inline]
pub fn full_fence() {
    fence(Ordering::SeqCst);
}

/// One turn of a spin loop.
#[inline]
pub fn spin_hint() {
    #[cfg(loom)]
    loom::thread::yield_now();
    #[cfg(not(loom))]
    core::hint::spin_loop();
}
