//! Crash points (testing §2.4). Every store the recovery path must cope with is tagged; with
//! the `crashpoints` feature and SCZ_CRASH_AT=<tag>, the process kills itself at that store.
//! Without the feature the macro expands to nothing.

/// Every tag in the workspace: the stores of `scz-shm` and the worker core's protocol steps
/// (testing §2.4). The crash-point harness (`conformance/runner/tests/crashpoints.rs`) fails
/// if the source uses a tag missing here, or if a tag here has no scenario.
pub const TAGS: &[&str] = &[
    "ring.enqueue.claimed",
    "ring.enqueue.written",
    "ring.dequeue.after_a",
    "slot.worker.claimed",
    "slot.worker.response_written",
    "slot.worker.completed",
    "worker.activated",
    "worker.drained",
    "worker.handover.stopped",
    "worker.handover.released",
];

#[macro_export]
macro_rules! crash_point {
    ($tag:literal) => {
        #[cfg(feature = "crashpoints")]
        $crate::crash::hit($tag);
    };
}

/// The generation this process runs as. A crash point fires only in the generation named by
/// SCZ_CRASH_GEN (if set), so the replacement worker does not crash at the same place.
#[cfg(feature = "crashpoints")]
pub static GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(feature = "crashpoints")]
pub fn hit(tag: &'static str) {
    use std::sync::OnceLock;
    static AT: OnceLock<Option<(String, Option<u32>)>> = OnceLock::new();
    let at = AT.get_or_init(|| {
        let tag = std::env::var("SCZ_CRASH_AT").ok()?;
        Some((tag, std::env::var("SCZ_CRASH_GEN").ok().and_then(|g| g.parse().ok())))
    });
    let Some((t, gen)) = at else { return };
    let gen_ok = gen.is_none_or(|g| g == GEN.load(std::sync::atomic::Ordering::Relaxed));
    if t == tag && gen_ok {
        // SAFETY: raising a signal has no memory-safety preconditions.
        unsafe { libc::raise(libc::SIGKILL) };
    }
}
