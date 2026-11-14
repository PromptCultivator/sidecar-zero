//! `sidecarzero._core`: the Python SDK's native core (sdk-python §2), PyO3 bindings over the
//! Rust client (`sdks/rust`, crate `sidecarzero`).
//!
//! The Rust client does all of the protocol: the handshake, the background thread, binding,
//! the call algorithm of sdk-contract §4, retirement, fork handling. This crate adds what
//! Python needs around it: inputs read through the buffer protocol, the GIL released for the
//! Rust part of a call, and the caller's Python fallback called once the GIL is back. On
//! anything but Linux the Rust client does not build, and the client here never connects
//! (sdk-contract §9).
//!
//! It also carries the worker runner (`python -m sidecarzero.worker`, sdk-python §5): the
//! worker core (scz-worker-core) around a backend that calls the user's module.

mod client;
mod codecs;
mod fallbacks;
mod fingerprint;
mod input;
/// The Rust SDK's metrics module (sdk-contract §8), compiled here too: off Linux the Rust
/// client does not build, and the client here times calls itself (its fallback runs outside
/// the Rust client).
#[path = "../../rust/src/metrics.rs"]
#[allow(dead_code)]
mod metrics;
mod outcome;
mod worker;

use pyo3::prelude::*;

/// Test hook (the conformance agent's UNWIND): the next enqueue in this process panics
/// between claiming a ring cell and publishing it. The ring publishes the cell void, the
/// client cancels the slot, and that `infer` raises `Unwound`.
#[pyfunction]
fn _arm_unwind() -> bool {
    #[cfg(target_os = "linux")]
    {
        scz_shm::ring::UNWIND_ONCE.store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Nanoseconds of CLOCK_MONOTONIC, the clock deadlines are in (the same clock as Python's
/// `time.monotonic_ns()` on Linux).
#[pyfunction]
fn monotonic_ns() -> u64 {
    #[cfg(target_os = "linux")]
    {
        scz_shm::wait::monotonic_ns()
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::time::Instant;
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

/// Keep the test hook's panic, which is expected and caught, out of stderr.
fn quiet_test_hook_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let p = info.payload();
        let hook = p.downcast_ref::<&str>().is_some_and(|s| s.contains("test hook")) || p.downcast_ref::<String>().is_some_and(|s| s.contains("test hook"));
        if !hook {
            default(info);
        }
    }));
}

#[pymodule(name = "_core")]
fn core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    quiet_test_hook_panics();
    m.add_class::<client::Client>()?;
    m.add_class::<outcome::Outcome>()?;
    m.add_class::<codecs::Bytes>()?;
    m.add_class::<codecs::DenseF32ToScore>()?;
    m.add_class::<fallbacks::NativeFallback>()?;
    m.add_function(wrap_pyfunction!(fallbacks::constant, m)?)?;
    m.add_function(wrap_pyfunction!(fallbacks::threshold, m)?)?;
    m.add_function(wrap_pyfunction!(fallbacks::linear, m)?)?;
    m.add_function(wrap_pyfunction!(fallbacks::lookup, m)?)?;
    m.add_function(wrap_pyfunction!(fingerprint::fingerprint, m)?)?;
    m.add_function(wrap_pyfunction!(_arm_unwind, m)?)?;
    m.add_function(wrap_pyfunction!(monotonic_ns, m)?)?;
    worker::register(m)?;
    m.add("Unwound", py.get_type::<client::Unwound>())?;
    m.add("REASONS", outcome::reason_names())?;
    m.add("LINUX", cfg!(target_os = "linux"))?;
    m.add("HAS_CRASHPOINTS", cfg!(feature = "crashpoints"))?;
    m.add("VERSION", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
