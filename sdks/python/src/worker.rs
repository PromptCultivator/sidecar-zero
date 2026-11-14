//! The worker runner (sdk-python §5, worker §7.3): `python -m sidecarzero.worker MODULE`.
//!
//! scz-worker-core does everything a worker does (startup, heartbeat, the consume loop, the
//! slot protocol, deadlines, stats, DRAIN/HANDOVER/TRIM); the backend here calls the user's
//! module for load, signature, warm-up, and each request. The interpreter's main thread
//! releases the GIL before it enters the worker loop, which never returns; consume threads
//! attach to the interpreter for each request. The Python half (`sidecarzero.worker._runner`)
//! imports the module, checks the signature, and builds and checks each request's arrays.

use pyo3::prelude::*;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod other {
    use pyo3::exceptions::PyRuntimeError;
    use pyo3::prelude::*;

    #[pyfunction]
    pub fn _worker_run(_argv: Vec<String>) -> PyResult<()> {
        Err(PyRuntimeError::new_err("the SidecarZero worker runner runs on Linux only"))
    }
    #[pyfunction]
    pub fn _worker_model_path() -> Option<String> {
        None
    }
    #[pyfunction]
    pub fn _worker_model_bytes() -> Option<Py<PyAny>> {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub use other::*;

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(_worker_run, m)?)?;
    m.add_function(wrap_pyfunction!(_worker_model_path, m)?)?;
    m.add_function(wrap_pyfunction!(_worker_model_bytes, m)?)?;
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::input::Export;
    use pyo3::exceptions::{PyMemoryError, PyValueError};
    use pyo3::ffi;
    use pyo3::prelude::*;
    use pyo3::types::{PyByteArray, PyList, PyTuple};
    use scz_shm::abi::status as st;
    use scz_shm::registry::{Column, Schema};
    use scz_worker_core::{Backend, BatchSink, BatchView, Signature};
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    macro_rules! log {
        ($($t:tt)*) => { eprintln!("sidecarzero.worker[{}]: {}", std::process::id(), format_args!($($t)*)) };
    }

    /// The model file as mapped by the worker core: (address, length).
    static MODEL: OnceLock<(usize, usize)> = OnceLock::new();

    /// MemoryErrors in a row after which the runner exits, so that the supervisor replaces it:
    /// a heap that keeps failing is a leak, and recycling is the dependable bound (worker §5).
    const MEMORY_ERRORS_TO_EXIT: u32 = 3;

    /// Tracebacks printed before the runner falls quiet (it counts the rest).
    const TRACEBACKS: u64 = 10;

    struct State {
        runner: Py<PyAny>,
        /// (predict, model, inputs, outputs), passed to `_runner.run` as is.
        plan: Py<PyAny>,
        /// Per request column: values are copied to private memory (integers, index-like).
        copy: Vec<bool>,
        signature: Signature,
    }

    struct PyBackend {
        spec: String,
        state: OnceLock<State>,
        memory_errors: AtomicU32,
        failures: AtomicU64,
        exit: (Mutex<bool>, Condvar),
    }

    fn fatal(py: Python<'_>, what: &str, e: PyErr) -> ! {
        log!("{what} failed; exiting");
        e.print(py);
        std::process::exit(3);
    }

    impl PyBackend {
        fn state(&self) -> &State {
            self.state.get().expect("load runs before any request")
        }

        fn report(&self, py: Python<'_>, e: PyErr) {
            let n = self.failures.fetch_add(1, Relaxed) + 1;
            if n <= TRACEBACKS {
                log!("predict failed (MODEL_ERROR){}", if n == TRACEBACKS { "; further failures are counted, not printed" } else { "" });
                e.print(py);
            }
        }

        /// Ask the exit thread to end the process once this request has completed.
        fn exit_soon(&self) {
            let (m, c) = &self.exit;
            *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
            c.notify_one();
        }

        fn load_module(&'static self, py: Python<'_>) -> PyResult<State> {
            let runner = py.import("sidecarzero.worker._runner")?;
            let path = scz_worker_core::model_path();
            let r = runner.getattr("load")?.call1((self.spec.as_str(), path))?;
            let (plan, fixed): (Bound<'_, PyTuple>, Option<u32>) = r.extract()?;
            let cols = |i: usize| -> PyResult<Vec<(String, u8, u32, bool)>> {
                let list = plan.get_item(i)?;
                let mut v = vec![];
                for c in list.try_iter()? {
                    let c = c?;
                    v.push((c.get_item(0)?.extract()?, c.get_item(1)?.extract()?, c.get_item(2)?.extract()?, c.get_item(5)?.extract()?));
                }
                Ok(v)
            };
            let (inputs, outputs) = (cols(2)?, cols(3)?);
            let col = |&(ref name, elem, list_size, _): &(String, u8, u32, bool)| Column { name: name.clone(), elem, list_size, nullable: false };
            let signature =
                Signature { schema: Schema { request: inputs.iter().map(col).collect(), response: outputs.iter().map(col).collect() }, fixed_batch: fixed };
            Ok(State { runner: runner.unbind().into_any(), plan: plan.unbind().into_any(), copy: inputs.iter().map(|c| c.3).collect(), signature })
        }

        /// One request through `_runner.run`: the arrays it returns, checked against the template.
        fn run(&self, py: Python<'_>, input: &BatchView<'_>, output: &mut BatchSink<'_>) -> PyResult<()> {
            let s = self.state();
            let mut bufs = Vec::with_capacity(input.columns().len());
            for (i, &copy) in s.copy.iter().enumerate() {
                let (p, len) = input.raw(i);
                let b: Bound<'_, PyAny> = if copy {
                    // Index-like values must not change after the model has checked them, and
                    // the host can write the request region at any time (worker §7.1).
                    PyByteArray::new_with(py, len, |dst| {
                        input.copy_column(i, dst);
                        Ok(())
                    })?
                    .into_any()
                } else {
                    // SAFETY: p..p+len lies in the data segment, mapped for the life of the
                    // process; the view is read-only. Its contents are the request's only
                    // until predict returns (documented): the slot is reused afterwards.
                    let mv = unsafe { ffi::PyMemoryView_FromMemory(p as *mut std::os::raw::c_char, len as ffi::Py_ssize_t, ffi::PyBUF_READ) };
                    // SAFETY: a new reference, or null with an exception set.
                    unsafe { Bound::from_owned_ptr_or_err(py, mv)? }
                };
                bufs.push(b);
            }
            let out = s.runner.bind(py).getattr("run")?.call1((s.plan.bind(py), PyTuple::new(py, bufs)?, input.rows()))?;
            let out = out.cast_into::<PyList>()?;
            if out.len() != output.columns().len() {
                return Err(PyValueError::new_err("the runner returned the wrong number of columns"));
            }
            for (i, a) in out.iter().enumerate() {
                let (_, want) = output.raw(i);
                let e = Export::get(&a, ffi::PyBUF_C_CONTIGUOUS).ok_or_else(|| PyValueError::new_err("an output is not C-contiguous"))?;
                if e.len() != want {
                    return Err(PyValueError::new_err(format!("output column {i}: {} bytes, the template holds {want}", e.len())));
                }
                // SAFETY: a C-contiguous export of e.len() bytes of this process's memory.
                output.write_column(i, unsafe { std::slice::from_raw_parts(e.ptr(), e.len()) });
            }
            Ok(())
        }
    }

    impl Backend for PyBackend {
        fn load(&self, gen: u32, model: &'static [u8]) {
            let _ = MODEL.set((model.as_ptr() as usize, model.len()));
            // SAFETY: the backend is leaked by the worker core for the life of the process; the
            // state's Python objects live as long.
            let me: &'static PyBackend = unsafe { &*(self as *const PyBackend) };
            Python::attach(|py| match me.load_module(py) {
                Ok(s) => {
                    let _ = me.state.set(s);
                }
                Err(e) => fatal(py, &format!("loading {} (gen {gen})", me.spec), e),
            });
            std::thread::Builder::new()
                .name("scz-py-exit".into())
                .spawn(move || {
                    let (m, c) = &me.exit;
                    let mut go = m.lock().unwrap_or_else(|e| e.into_inner());
                    while !*go {
                        go = c.wait(go).unwrap_or_else(|e| e.into_inner());
                    }
                    std::thread::sleep(Duration::from_millis(5)); // the failed request completes and wakes its caller
                    log!("{MEMORY_ERRORS_TO_EXIT} MemoryErrors in a row; exiting so that the supervisor replaces this worker");
                    std::process::exit(3);
                })
                .unwrap_or_else(|e| {
                    log!("cannot start the exit thread: {e}");
                    std::process::exit(3)
                });
            log!("gen {gen}: {} loaded", self.spec);
        }

        fn signature(&self) -> Option<Signature> {
            Some(self.state().signature.clone())
        }

        fn warm(&self, rows: u32) {
            Python::attach(|py| {
                let s = self.state();
                if let Err(e) = s.runner.bind(py).getattr("warm").and_then(|w| w.call1((s.plan.bind(py), rows))) {
                    log!("warm-up at {rows} row(s) failed; serving anyway");
                    e.print(py);
                }
            });
        }

        fn infer_batch(&self, input: &BatchView<'_>, output: &mut BatchSink<'_>) -> u32 {
            Python::attach(|py| match self.run(py, input, output) {
                Ok(()) => {
                    self.memory_errors.store(0, Relaxed);
                    st::OK
                }
                Err(e) if e.is_instance_of::<PyMemoryError>(py) => {
                    let n = self.memory_errors.fetch_add(1, Relaxed) + 1;
                    log!("predict ran out of memory (RESOURCE_EXHAUSTED, {n} in a row)");
                    drop(e);
                    if n >= MEMORY_ERRORS_TO_EXIT {
                        self.exit_soon();
                    }
                    st::RESOURCE_EXHAUSTED
                }
                Err(e) => {
                    self.memory_errors.store(0, Relaxed);
                    self.report(py, e);
                    st::MODEL_ERROR
                }
            })
        }

        /// The input is read in place, but predict's answer is copied into the response region:
        /// Python cannot be told where to allocate it (worker §7.3).
        fn in_place(&self) -> bool {
            false
        }
        fn copied(&self) -> bool {
            true
        }

        fn trim(&self) {
            Python::attach(|py| {
                if let Some(s) = self.state.get() {
                    if let Err(e) = s.runner.bind(py).getattr("trim").and_then(|t| t.call0()) {
                        e.print(py);
                    }
                }
                // SAFETY: malloc_trim has no preconditions; done with the GIL released.
                py.detach(|| unsafe { libc::malloc_trim(0) });
            });
        }
    }

    /// `python -m sidecarzero.worker MODULE` with the supervisor's arguments after it. Never
    /// returns: the worker core exits the process.
    #[pyfunction]
    pub fn _worker_run(py: Python<'_>, argv: Vec<String>) -> PyResult<()> {
        let (args, rest) =
            scz_worker_core::parse_known(argv.into_iter()).map_err(|e| PyValueError::new_err(format!("{e} (this is started by the supervisor)")))?;
        let spec = match rest.as_slice() {
            [m] => m.clone(),
            [] => return Err(PyValueError::new_err("usage: python -m sidecarzero.worker MODULE (a module name, or a path to a .py file)")),
            [_, x, ..] => return Err(PyValueError::new_err(format!("unknown argument {x}"))),
        };
        let backend = PyBackend {
            spec,
            state: OnceLock::new(),
            memory_errors: AtomicU32::new(0),
            failures: AtomicU64::new(0),
            exit: (Mutex::new(false), Condvar::new()),
        };
        // The main thread gives up the GIL for good; consume threads attach per request.
        py.detach(move || -> () { scz_worker_core::run_with(args, backend) });
        #[allow(unreachable_code)]
        Ok(())
    }

    #[pyfunction]
    pub fn _worker_model_path() -> Option<String> {
        scz_worker_core::model_path()
    }

    /// The mapped model file as a read-only memoryview; None outside a runner.
    #[pyfunction]
    pub fn _worker_model_bytes(py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let Some(&(p, len)) = MODEL.get() else { return Ok(None) };
        // SAFETY: the mapping lives as long as the process and is never written.
        let mv = unsafe { ffi::PyMemoryView_FromMemory(p as *mut std::os::raw::c_char, len as ffi::Py_ssize_t, ffi::PyBUF_READ) };
        // SAFETY: a new reference, or null with an exception set.
        Ok(Some(unsafe { Bound::from_owned_ptr_or_err(py, mv)? }.unbind()))
    }
}
