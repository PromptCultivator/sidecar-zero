//! `Client`: the Rust client underneath, with the GIL released for the whole of the Rust call
//! and the caller's Python fallback called after it is held again.
//!
//! The Rust client calls its fallback itself, from inside `infer`. The one it is built with
//! here answers nothing (a unit value); the reason comes back in the outcome's source, and the
//! caller's fallback runs once the GIL is back: a Python callable under the GIL, or a native
//! description evaluated in Rust. The Rust client has already counted the fallback by then, so
//! a fallback that raises is counted under its reason.

use crate::codecs::{Bytes, DenseF32ToScore, OUT_BYTES, OUT_F32};
use crate::fallbacks::NativeFallback;
use crate::input::{self, DenseIn, Export, Seen};
use crate::metrics::{self, HistShard, Histogram, MetricsSink};
use crate::outcome::{self, Outcome};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyDict, PyFloat, PyList, PyString, PyType};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

thread_local! {
    /// Calls since this thread's last timed one (sdk-contract §8, "Sampling").
    static SAMPLE: Cell<u32> = const { Cell::new(0) };
}

fn ns(d: Duration) -> u64 {
    d.as_nanos().min(u64::MAX as u128) as u64
}

pyo3::create_exception!(
    sidecarzero,
    Unwound,
    pyo3::exceptions::PyRuntimeError,
    "A call unwound inside the core between claiming a ring cell and publishing it (the test hook). The cell was published void and the slot released."
);

enum Fb {
    Py(Py<PyAny>),
    Native(Py<NativeFallback>),
}

#[derive(Clone, Copy)]
enum CodecKind {
    Raw,
    Dense { n: usize, strict: bool },
}

/// How a call ended inside the core.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Res {
    Model { gen: u32 },
    Fallback(u8),
    Unwound,
}

/// Counters, summed over every caller.
#[derive(Clone, Default)]
pub struct Snap {
    pub model: u64,
    pub fallbacks: Vec<(u8, u64)>,
    pub sessions: u64,
    pub retired: u64,
    pub late_wakes: u64,
    pub clamped: u64,
    pub breaker_open: u64,
    pub model_gen: u64,
    /// sdk-contract §8's number for the connection state.
    pub state: u32,
}

fn budget_ns(ms: f64) -> u64 {
    if ms.is_nan() || ms <= 0.0 {
        0
    } else if ms >= 1e12 {
        u64::MAX / 2
    } else {
        (ms * 1e6).round() as u64
    }
}

/// `numpy.empty` and `numpy.float32`, if NumPy can be imported; else `array.array`.
enum ArrayMaker {
    NumPy(Py<PyAny>, Py<PyAny>),
    Array(Py<PyAny>),
}

static ARRAYS: PyOnceLock<ArrayMaker> = PyOnceLock::new();

fn arrays(py: Python<'_>) -> PyResult<&'static ArrayMaker> {
    ARRAYS.get_or_try_init(py, || {
        if let Ok(np) = py.import("numpy") {
            return Ok(ArrayMaker::NumPy(np.getattr("empty")?.unbind(), np.getattr("float32")?.unbind()));
        }
        Ok::<_, PyErr>(ArrayMaker::Array(py.import("array")?.getattr("array")?.unbind()))
    })
}

/// A float32 array of `v`: NumPy's when it is installed, else `array.array('f')`.
pub fn f32_array(py: Python<'_>, v: &[f32]) -> PyResult<Py<PyAny>> {
    match arrays(py)? {
        ArrayMaker::NumPy(empty, dtype) => {
            let a = empty.bind(py).call1((v.len(), dtype.bind(py)))?;
            let e = Export::writable(&a).ok_or_else(|| PyTypeError::new_err("numpy.empty returned an array without a writable buffer"))?;
            if e.len() != v.len() * 4 {
                return Err(PyTypeError::new_err("numpy.empty returned an array of the wrong size"));
            }
            // SAFETY: a writable, C-contiguous export of exactly v.len() float32 values.
            unsafe { std::ptr::copy_nonoverlapping(v.as_ptr() as *const u8, e.ptr_mut(), v.len() * 4) };
            drop(e);
            Ok(a.unbind())
        }
        ArrayMaker::Array(ctor) => {
            // SAFETY: f32 has no padding; reading its bytes is always valid.
            let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) };
            Ok(ctor.bind(py).call1(("f", PyBytes::new(py, bytes)))?.unbind())
        }
    }
}

/// A client for one sidecar. `fallback` is required: it answers whenever the model does not,
/// in time (sdk-contract §1, §3).
#[pyclass(frozen, module = "sidecarzero", name = "Client")]
pub struct Client {
    engine: engine::Engine,
    fallback: Fb,
    codec: CodecKind,
    endpoint: String,
    warned_input: AtomicBool,
    warned_sequence: AtomicBool,
    on_fallback: Option<Py<PyAny>>,
    sample_every: u32,
    /// The sampled histograms, kept here rather than in the Rust client: the fallback that
    /// counts is the Python one, called after the Rust client has returned.
    hists: HistShard,
}

/// A Python object with `counter`, `gauge`, and `histogram` methods, as a sink.
struct PySink<'py> {
    obj: Bound<'py, PyAny>,
    err: Option<PyErr>,
}

impl PySink<'_> {
    fn labels(&self, l: &[(&str, &str)]) -> PyResult<Bound<'_, PyDict>> {
        let d = PyDict::new(self.obj.py());
        for (k, v) in l {
            d.set_item(k, v)?;
        }
        Ok(d)
    }
    fn keep(&mut self, r: PyResult<()>) {
        if let (Err(e), None) = (r, &self.err) {
            self.err = Some(e);
        }
    }
}

impl MetricsSink for PySink<'_> {
    fn counter(&mut self, name: &str, labels: &[(&str, &str)], value: u64) {
        let r = self.labels(labels).and_then(|l| self.obj.call_method1("counter", (name, l, value)).map(drop));
        self.keep(r);
    }
    fn gauge(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        let r = self.labels(labels).and_then(|l| self.obj.call_method1("gauge", (name, l, value)).map(drop));
        self.keep(r);
    }
    fn histogram(&mut self, name: &str, labels: &[(&str, &str)], bounds: &[f64], h: &Histogram) {
        let r = self.labels(labels).and_then(|l| {
            let py = self.obj.py();
            let b = PyList::new(py, bounds)?;
            let c = PyList::new(py, h.cumulative)?;
            self.obj.call_method1("histogram", (name, l, b, c, h.sum_seconds, h.count)).map(drop)
        });
        self.keep(r);
    }
}

impl Client {
    fn warn_once(&self, py: Python<'_>, flag: &AtomicBool, category: &str, text: &str) {
        if flag.swap(true, Relaxed) {
            return;
        }
        let w = py.import("warnings").and_then(|w| {
            let cat = py.import("builtins")?.getattr(category)?;
            w.getattr("warn")?.call1((text, cat, 1))
        });
        if let Err(e) = w {
            // A warning filter set to "error" turns the warning into an exception; infer
            // raises nothing of its own, so it is reported and dropped.
            e.write_unraisable(py, None);
        }
    }

    fn model_value(&self, py: Python<'_>, scalar: bool) -> PyResult<Py<PyAny>> {
        match self.codec {
            CodecKind::Raw => Ok(OUT_BYTES.with(|b| PyBytes::new(py, &b.borrow()).into_any().unbind())),
            CodecKind::Dense { .. } => {
                if scalar {
                    let v = OUT_F32.with(|o| o.borrow().first().copied().unwrap_or(f32::NAN));
                    Ok(PyFloat::new(py, v as f64).into_any().unbind())
                } else {
                    OUT_F32.with(|o| f32_array(py, &o.borrow()))
                }
            }
        }
    }

    fn fallback_value(&self, py: Python<'_>, input: &Bound<'_, PyAny>, reason: u8, dense: Option<&DenseIn>) -> PyResult<Py<PyAny>> {
        match &self.fallback {
            Fb::Py(f) => Ok(f.bind(py).call1((input, outcome::reason_obj(py, reason)?))?.unbind()),
            Fb::Native(nf) => {
                let nf = nf.get();
                if let Some(v) = nf.constant() {
                    return Ok(v.clone_ref(py));
                }
                let d = dense.copied().unwrap_or(DenseIn::NONE);
                if d.scalar || !d.ok {
                    return Ok(PyFloat::new(py, nf.score(&d, 0) as f64).into_any().unbind());
                }
                OUT_F32.with(|o| {
                    let mut o = o.borrow_mut();
                    o.clear();
                    for r in 0..d.rows as usize {
                        o.push(nf.score(&d, r));
                    }
                    f32_array(py, &o)
                })
            }
        }
    }

    fn finish(&self, py: Python<'_>, input: &Bound<'_, PyAny>, res: Res, dense: Option<&DenseIn>, t0: Option<Instant>) -> PyResult<Py<Outcome>> {
        let o = match res {
            Res::Model { gen } => {
                let scalar = dense.is_some_and(|d| d.scalar);
                let o = Outcome { value: self.model_value(py, scalar)?, model: true, reason: 0, model_gen: gen };
                if let Some(t) = t0 {
                    self.hists.model.record(ns(t.elapsed()));
                }
                o
            }
            Res::Fallback(r) => {
                if let Some(h) = &self.on_fallback {
                    h.bind(py).call1((outcome::reason_obj(py, r)?,))?;
                }
                let t1 = t0.map(|_| Instant::now());
                let value = self.fallback_value(py, input, r, dense)?;
                if let (Some(t), Some(t1)) = (t0, t1) {
                    self.hists.fallback_fn.record(ns(t1.elapsed()));
                    self.hists.fallback.record(ns(t.elapsed()));
                }
                Outcome { value, model: false, reason: r, model_gen: 0 }
            }
            Res::Unwound => return Err(Unwound::new_err("the call unwound between claiming a ring cell and publishing it")),
        };
        Py::new(py, o)
    }

    /// Whether this thread times this call: those whose number on this thread (counted over
    /// every client) is a multiple of `sample_every`, the thread's first call among them.
    fn sampled(&self) -> bool {
        let every = self.sample_every;
        every != 0
            && SAMPLE.with(|c| {
                let n = c.get();
                c.set(n.wrapping_add(1));
                if every.is_power_of_two() {
                    n & (every - 1) == 0
                } else {
                    n % every == 0
                }
            })
    }

    /// Every metric of sdk-contract §8, in one pass, the same as the Rust client's export.
    fn export_to(&self, sink: &mut dyn MetricsSink) {
        let s = self.engine.stats();
        sink.counter("scz_client_calls_total", &[("source", "model")], s.model);
        for (i, name) in outcome::reason_names().iter().enumerate() {
            let n = s.fallbacks.iter().find(|x| x.0 as usize == i).map_or(0, |x| x.1);
            sink.counter("scz_client_calls_total", &[("source", "fallback"), ("reason", name)], n);
        }
        let b = &metrics::BUCKETS_SECONDS;
        let one = |h| Histogram::sum_of(std::iter::once(h));
        sink.histogram("scz_client_latency_seconds", &[("source", "model")], b, &one(&self.hists.model));
        sink.histogram("scz_client_latency_seconds", &[("source", "fallback")], b, &one(&self.hists.fallback));
        sink.histogram("scz_client_fallback_seconds", &[], b, &one(&self.hists.fallback_fn));
        sink.counter("scz_client_budget_clamped_total", &[], s.clamped);
        sink.gauge("scz_client_breaker_open", &[], s.breaker_open as f64);
        sink.counter("scz_client_late_wakes_total", &[], s.late_wakes);
        sink.counter("scz_client_reconnects_total", &[], s.sessions.saturating_sub(1));
        sink.gauge("scz_client_state", &[], s.state as f64);
        sink.gauge("scz_client_model_gen", &[], s.model_gen as f64);
    }
}

fn hist_dict<'py>(py: Python<'py>, h: &Histogram) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("le", PyList::new(py, metrics::BUCKETS_SECONDS)?)?;
    d.set_item("cumulative", PyList::new(py, h.cumulative)?)?;
    d.set_item("sum", h.sum_seconds)?;
    d.set_item("count", h.count)?;
    Ok(d)
}

#[pymethods]
impl Client {
    /// Returns at once; a background thread connects. Calls fall back with NOT_CONNECTED
    /// until it has (sdk-contract §6, "Startup").
    #[new]
    #[pyo3(signature = (endpoint, *, codec, fallback, fallback_reserve_ms = 0.0, connect_timeout_ms = 1000.0, callers = 256, spin_ceiling_ms = None, on_fallback = None, sample_every = 64))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        endpoint: &Bound<'_, PyAny>,
        codec: &Bound<'_, PyAny>,
        fallback: &Bound<'_, PyAny>,
        fallback_reserve_ms: f64,
        connect_timeout_ms: f64,
        callers: usize,
        spin_ceiling_ms: Option<f64>,
        on_fallback: Option<&Bound<'_, PyAny>>,
        sample_every: u32,
    ) -> PyResult<Self> {
        let endpoint: String =
            if endpoint.is_instance_of::<PyString>() { endpoint.extract()? } else { py.import("os")?.getattr("fspath")?.call1((endpoint,))?.extract()? };
        let kind = if let Ok(c) = codec.cast::<Bytes>() {
            let _ = c;
            CodecKind::Raw
        } else if let Ok(c) = codec.cast::<DenseF32ToScore>() {
            let c = c.get();
            CodecKind::Dense { n: c.n, strict: c.strict }
        } else {
            return Err(PyTypeError::new_err("codec must be a sidecarzero.codecs codec (Bytes or DenseF32ToScore)"));
        };
        if fallback.is_none() {
            return Err(PyValueError::new_err("fallback must not be None: every call needs an answer when the model has none"));
        }
        let fb = if let Ok(n) = fallback.cast::<NativeFallback>() {
            n.get().check(match kind {
                CodecKind::Dense { n, .. } => Some(n),
                CodecKind::Raw => None,
            })?;
            Fb::Native(n.clone().unbind())
        } else if fallback.is_callable() {
            Fb::Py(fallback.clone().unbind())
        } else {
            return Err(PyTypeError::new_err("fallback must be callable as fallback(input, reason), or a sidecarzero.fallbacks description"));
        };
        if callers == 0 {
            return Err(PyValueError::new_err("callers must be at least 1"));
        }
        let on_fallback = match on_fallback {
            Some(h) if h.is_none() => None,
            Some(h) if h.is_callable() => Some(h.clone().unbind()),
            Some(_) => return Err(PyTypeError::new_err("on_fallback must be callable as on_fallback(reason), or None")),
            None => None,
        };
        let opts = engine::Opts {
            reserve: Duration::from_nanos(budget_ns(fallback_reserve_ms)),
            connect_timeout: Duration::from_nanos(budget_ns(connect_timeout_ms).max(1_000_000)),
            callers,
            spin_ceiling: spin_ceiling_ms.map(|ms| Duration::from_nanos(budget_ns(ms))),
        };
        outcome::enums(py)?; // fail here, not on a call, if the package is broken
        if let CodecKind::Dense { .. } = kind {
            arrays(py)?; // importing NumPy takes far longer than any call's budget
        }
        let engine = engine::Engine::new(&endpoint, codec, opts)?;
        Ok(Client {
            engine,
            fallback: fb,
            codec: kind,
            endpoint,
            warned_input: AtomicBool::new(false),
            warned_sequence: AtomicBool::new(false),
            on_fallback,
            sample_every,
            hists: HistShard::default(),
        })
    }

    /// Always returns an `Outcome`: the model's answer if it comes within `budget_ms`, else
    /// the fallback's. Raises only what the fallback raises.
    #[pyo3(signature = (input, budget_ms))]
    fn infer(&self, py: Python<'_>, input: &Bound<'_, PyAny>, budget_ms: f64) -> PyResult<Py<Outcome>> {
        let t0 = self.sampled().then(Instant::now);
        let budget = Duration::from_nanos(budget_ns(budget_ms));
        match self.codec {
            CodecKind::Raw => {
                let (export, raw) = input::raw(input);
                if !raw.ok {
                    self.warn_once(
                        py,
                        &self.warned_input,
                        "RuntimeWarning",
                        "sidecarzero: the input has no contiguous buffer; calls with it fall back with CODEC_ERROR",
                    );
                }
                let res = self.engine.call_raw(py, &raw, budget);
                let out = self.finish(py, input, res, None, t0);
                drop(export);
                out
            }
            CodecKind::Dense { n, strict } => {
                let (export, d, seen) = input::dense(input, n, strict);
                match seen {
                    Seen::Buffer => {}
                    Seen::Sequence => self.warn_once(
                        py,
                        &self.warned_sequence,
                        "RuntimeWarning",
                        "sidecarzero: a list input is converted element by element; pass an array (NumPy, array.array) for speed",
                    ),
                    Seen::Refused => self.warn_once(
                        py,
                        &self.warned_input,
                        "RuntimeWarning",
                        "sidecarzero: the input is not a (rows, n) array of numbers the codec takes; calls with it fall back with CODEC_ERROR",
                    ),
                }
                let res = self.engine.call_dense(py, &d, budget);
                let out = self.finish(py, input, res, Some(&d), t0);
                drop(export);
                out
            }
        }
    }

    /// Wait until an arena is mapped and its gate is open (sdk-contract §6, "Startup").
    #[pyo3(signature = (timeout = 5.0))]
    fn await_bound(&self, py: Python<'_>, timeout: f64) -> bool {
        self.engine.await_bound(py, Duration::from_nanos(budget_ns(timeout * 1000.0)))
    }

    /// Cap the spin before a caller parks (sdk-contract §7, "Spin ceiling"): 0 parks at once,
    /// None restores the sidecar's value.
    #[pyo3(signature = (ms))]
    fn set_spin_ceiling(&self, ms: Option<f64>) {
        self.engine.set_spin_ceiling(ms.map(|ms| Duration::from_nanos(budget_ns(ms))));
    }

    /// DISCONNECTED, CONNECTING, BOUND, REJECTED, SCHEMA_REJECTED, or CLOSED.
    #[getter]
    fn state(&self) -> &'static str {
        self.engine.state()
    }

    #[getter]
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[getter]
    fn closed(&self) -> bool {
        self.engine.is_closed()
    }

    /// Every metric of sdk-contract §8, in one pass, to `sink`: an object with
    /// `counter(name, labels, value)`, `gauge(name, labels, value)`, and
    /// `histogram(name, labels, bounds, cumulative, sum, count)`. An adapter for a metrics
    /// library is such an object. The first exception a method raises is raised here.
    fn export(&self, sink: &Bound<'_, PyAny>) -> PyResult<()> {
        let mut s = PySink { obj: sink.clone(), err: None };
        self.export_to(&mut s);
        s.err.map_or(Ok(()), Err)
    }

    /// `export` in the Prometheus text exposition format, for a service that serves
    /// `/metrics` itself.
    fn prometheus_text(&self) -> String {
        let mut t = metrics::PrometheusText::new();
        self.export_to(&mut t);
        t.finish()
    }

    /// Counters summed over every caller: `model`, `fallbacks` (by reason), `sessions`,
    /// `retired`, `late_wakes`, `clamped`, `breaker_open`, `model_gen`; and the sampled
    /// histograms `latency_model`, `latency_fallback`, `fallback_time` (sdk-contract §8),
    /// each a dict of `le` (the bounds), `cumulative` (one more entry, for +Inf), `sum`
    /// (seconds), and `count`.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let s = self.engine.stats();
        let d = PyDict::new(py);
        d.set_item("model", s.model)?;
        let fb = PyDict::new(py);
        for (r, n) in &s.fallbacks {
            fb.set_item(outcome::reason_obj(py, *r)?, *n)?;
        }
        d.set_item("fallbacks", fb)?;
        d.set_item("sessions", s.sessions)?;
        d.set_item("retired", s.retired)?;
        d.set_item("late_wakes", s.late_wakes)?;
        d.set_item("clamped", s.clamped)?;
        d.set_item("breaker_open", s.breaker_open)?;
        d.set_item("model_gen", s.model_gen)?;
        let one = |h| Histogram::sum_of(std::iter::once(h));
        d.set_item("latency_model", hist_dict(py, &one(&self.hists.model))?)?;
        d.set_item("latency_fallback", hist_dict(py, &one(&self.hists.fallback))?)?;
        d.set_item("fallback_time", hist_dict(py, &one(&self.hists.fallback_fn))?)?;
        Ok(d)
    }

    /// sdk-contract §6 `close()`: calls in flight finish, later calls fall back with
    /// NOT_CONNECTED, the arena is unmapped, the socket closed last. False if a call was
    /// still inside the arena after the wait, in which case the mapping is left in place.
    fn close(&self, py: Python<'_>) -> bool {
        self.engine.close(py)
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __exit__(&self, py: Python<'_>, _t: &Bound<'_, PyAny>, _v: &Bound<'_, PyAny>, _tb: &Bound<'_, PyAny>) -> bool {
        self.engine.close(py);
        false
    }

    /// Slots in each state (FREE, CLAIMED, READY, PROCESSING, DONE, FAILED, ABANDONED) in the
    /// current arena, for tests; None when none is mapped.
    fn _census(&self) -> Option<Vec<u32>> {
        self.engine.census()
    }

    fn __repr__(&self) -> String {
        format!("Client({:?}, state={})", self.endpoint, self.engine.state())
    }

    #[classmethod]
    fn __class_getitem__(cls: &Bound<'_, PyType>, _item: &Bound<'_, PyAny>) -> Py<PyType> {
        cls.clone().unbind()
    }
}

// ── the engine: the Rust client on Linux, a client that never connects elsewhere ──────────

#[cfg(target_os = "linux")]
mod engine {
    use super::{Res, Snap};
    use crate::codecs::{Bytes, DenseCodec, DenseF32ToScore, RawCodec};
    use crate::input::{DenseIn, RawIn};
    use crate::outcome::{not_connected, reason_index};
    use pyo3::prelude::*;
    use sidecarzero::{ClientState, Endpoint, FallbackReason as R, Options, Source};
    use std::cell::UnsafeCell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    pub struct Opts {
        pub reserve: Duration,
        pub connect_timeout: Duration,
        pub callers: usize,
        pub spin_ceiling: Option<Duration>,
    }

    /// The version the handshake announces: 0.1.0.
    const SDK_VERSION: u32 = 0x00_01_00;

    fn no_answer_raw(_: &RawIn, _: R) {}
    fn no_answer_dense(_: &DenseIn, _: R) {}

    type RawClient = sidecarzero::Client<RawCodec, fn(&RawIn, R)>;
    type DenseClient = sidecarzero::Client<DenseCodec, fn(&DenseIn, R)>;

    enum Kind {
        Raw(RawClient),
        Dense(DenseClient),
    }

    impl Kind {
        fn stats(&self) -> Snap {
            let (s, state) = match self {
                Kind::Raw(c) => (c.stats(), c.state()),
                Kind::Dense(c) => (c.stats(), c.state()),
            };
            Snap {
                model: s.model,
                fallbacks: s.fallback.iter().map(|(r, n)| (reason_index(*r), *n)).collect(),
                sessions: s.sessions,
                retired: s.retired,
                late_wakes: s.late_wakes,
                clamped: s.clamped,
                breaker_open: s.breaker_open,
                model_gen: s.model_gen,
                state: match state {
                    ClientState::Disconnected => 0,
                    ClientState::Connecting => 1,
                    ClientState::Bound => 2,
                    ClientState::Rejected => 3,
                    ClientState::SchemaRejected => 4,
                },
            }
        }
    }

    /// The Rust client, and what lets `close()` drop it while other threads may be calling:
    /// a count of calls inside it. A call raises the count, then checks `closed`; `close`
    /// sets `closed`, then waits for the count to reach zero. With both sequentially
    /// consistent, either the call sees `closed` or `close` sees the call.
    pub struct Engine {
        cell: UnsafeCell<Option<Kind>>,
        inflight: AtomicUsize,
        closed: AtomicBool,
        closing: Mutex<()>,
        /// NOT_CONNECTED fallbacks after close, which the dropped client cannot count.
        after_close: AtomicU64,
        last: Mutex<Snap>,
    }

    // SAFETY: `cell` is written only by `close`, once no call is inside (see the type's doc),
    // and by Drop, which has exclusive access; everything else only reads it.
    unsafe impl Sync for Engine {}
    unsafe impl Send for Engine {}

    struct Inside<'a>(&'a Engine);

    impl Drop for Inside<'_> {
        fn drop(&mut self) {
            self.0.inflight.fetch_sub(1, SeqCst);
        }
    }

    impl Inside<'_> {
        fn kind(&self) -> &Kind {
            // SAFETY: entered while not closed, so close() has not taken the client and will
            // not until this Inside is dropped.
            unsafe { (*self.0.cell.get()).as_ref() }.expect("a client is present until close")
        }
    }

    impl Engine {
        pub fn new(endpoint: &str, codec: &Bound<'_, PyAny>, o: Opts) -> PyResult<Engine> {
            let options = Options {
                fallback_reserve: o.reserve,
                connect_timeout: o.connect_timeout,
                callers: o.callers,
                spin_ceiling: o.spin_ceiling,
                // Timed here instead: see `Client::hists`.
                sample_every: 0,
                sdk_lang: scz_shm::abi::sdk_lang::PYTHON,
                sdk_version: SDK_VERSION,
            };
            let ep = Endpoint::parse(endpoint);
            let kind = if let Ok(c) = codec.cast::<Bytes>() {
                let c = c.get();
                let codec = RawCodec { response_len: c.response_len, fault: c.fault.clone() };
                Kind::Raw(sidecarzero::Client::builder(ep, codec).fallback(no_answer_raw as fn(&RawIn, R)).options(options).build())
            } else {
                let c = codec.cast::<DenseF32ToScore>()?.get();
                let codec = DenseCodec { n: c.n, input: c.input.clone(), output: c.output.clone(), fault: c.fault.clone() };
                Kind::Dense(sidecarzero::Client::builder(ep, codec).fallback(no_answer_dense as fn(&DenseIn, R)).options(options).build())
            };
            Ok(Engine {
                cell: UnsafeCell::new(Some(kind)),
                inflight: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                closing: Mutex::new(()),
                after_close: AtomicU64::new(0),
                last: Mutex::new(Snap::default()),
            })
        }

        fn enter(&self) -> Option<Inside<'_>> {
            self.inflight.fetch_add(1, SeqCst);
            let g = Inside(self);
            if self.closed.load(SeqCst) {
                return None;
            }
            Some(g)
        }

        fn closed_call(&self) -> Res {
            self.after_close.fetch_add(1, Relaxed);
            Res::Fallback(not_connected())
        }

        pub fn call_raw(&self, py: Python<'_>, input: &RawIn, budget: Duration) -> Res {
            let Some(g) = self.enter() else { return self.closed_call() };
            let Kind::Raw(c) = g.kind() else { unreachable!("a raw input for a raw client") };
            let r = py.detach(|| catch_unwind(AssertUnwindSafe(|| c.infer(input, budget).source)));
            drop(g);
            resolve(r)
        }

        pub fn call_dense(&self, py: Python<'_>, input: &DenseIn, budget: Duration) -> Res {
            let Some(g) = self.enter() else { return self.closed_call() };
            let Kind::Dense(c) = g.kind() else { unreachable!("a dense input for a dense client") };
            let r = py.detach(|| catch_unwind(AssertUnwindSafe(|| c.infer(input, budget).source)));
            drop(g);
            resolve(r)
        }

        pub fn await_bound(&self, py: Python<'_>, timeout: Duration) -> bool {
            let Some(g) = self.enter() else { return false };
            py.detach(|| match g.kind() {
                Kind::Raw(c) => c.await_bound(timeout),
                Kind::Dense(c) => c.await_bound(timeout),
            })
        }

        pub fn state(&self) -> &'static str {
            let Some(g) = self.enter() else { return "CLOSED" };
            let s = match g.kind() {
                Kind::Raw(c) => c.state(),
                Kind::Dense(c) => c.state(),
            };
            match s {
                ClientState::Disconnected => "DISCONNECTED",
                ClientState::Connecting => "CONNECTING",
                ClientState::Bound => "BOUND",
                ClientState::Rejected => "REJECTED",
                ClientState::SchemaRejected => "SCHEMA_REJECTED",
            }
        }

        pub fn is_closed(&self) -> bool {
            self.closed.load(SeqCst)
        }

        pub fn set_spin_ceiling(&self, d: Option<Duration>) {
            if let Some(g) = self.enter() {
                match g.kind() {
                    Kind::Raw(c) => c.set_spin_ceiling(d),
                    Kind::Dense(c) => c.set_spin_ceiling(d),
                }
            }
        }

        pub fn stats(&self) -> Snap {
            let mut s = match self.enter() {
                Some(g) => g.kind().stats(),
                None => self.last.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            };
            let late = self.after_close.load(Relaxed);
            if late > 0 {
                let nc = not_connected();
                match s.fallbacks.iter_mut().find(|x| x.0 == nc) {
                    Some(x) => x.1 += late,
                    None => s.fallbacks.push((nc, late)),
                }
            }
            s
        }

        pub fn census(&self) -> Option<Vec<u32>> {
            let g = self.enter()?;
            let c = match g.kind() {
                Kind::Raw(c) => c.slot_census(),
                Kind::Dense(c) => c.slot_census(),
            };
            c.map(|a| a.to_vec())
        }

        pub fn close(&self, py: Python<'_>) -> bool {
            self.closed.store(true, SeqCst);
            py.detach(|| {
                let _one = self.closing.lock().unwrap_or_else(|e| e.into_inner());
                // Calls in flight finish within their budget (at most the sidecar's
                // max_deadline); allow far more before giving up.
                let t = Instant::now();
                while self.inflight.load(SeqCst) != 0 {
                    if t.elapsed() > Duration::from_secs(10) {
                        eprintln!("sidecarzero: a call is still inside the client after 10 s; leaving the arena mapped");
                        return false;
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                // SAFETY: closed is set and no call is inside, so nothing else reads the cell
                // (see Engine's doc), and `closing` serializes this with other closers.
                if let Some(k) = unsafe { (*self.cell.get()).take() } {
                    *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Snap { state: 0, ..k.stats() };
                    drop(k); // stops the background thread: retire, unmap, then close the socket
                }
                true
            })
        }
    }

    fn resolve(r: std::thread::Result<Source>) -> Res {
        match r {
            Ok(Source::Model { model_gen }) => Res::Model { gen: model_gen },
            Ok(Source::Fallback { reason }) => Res::Fallback(reason_index(reason)),
            Err(_) => Res::Unwound,
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod engine {
    //! Off Linux the client constructs normally, never connects, and every call falls back
    //! with NOT_CONNECTED (sdk-contract §9).
    use super::{Res, Snap};
    use crate::input::{DenseIn, RawIn};
    use crate::outcome::not_connected;
    use pyo3::prelude::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
    use std::time::Duration;

    #[allow(dead_code)]
    pub struct Opts {
        pub reserve: Duration,
        pub connect_timeout: Duration,
        pub callers: usize,
        pub spin_ceiling: Option<Duration>,
    }

    pub struct Engine {
        closed: AtomicBool,
        fallbacks: AtomicU64,
    }

    impl Engine {
        pub fn new(_endpoint: &str, _codec: &Bound<'_, PyAny>, _o: Opts) -> PyResult<Engine> {
            Ok(Engine { closed: AtomicBool::new(false), fallbacks: AtomicU64::new(0) })
        }
        fn call(&self) -> Res {
            self.fallbacks.fetch_add(1, Relaxed);
            Res::Fallback(not_connected())
        }
        pub fn call_raw(&self, _py: Python<'_>, _input: &RawIn, _budget: Duration) -> Res {
            self.call()
        }
        pub fn call_dense(&self, _py: Python<'_>, _input: &DenseIn, _budget: Duration) -> Res {
            self.call()
        }
        pub fn await_bound(&self, py: Python<'_>, timeout: Duration) -> bool {
            py.detach(|| std::thread::sleep(timeout));
            false
        }
        pub fn state(&self) -> &'static str {
            if self.closed.load(SeqCst) {
                "CLOSED"
            } else {
                "DISCONNECTED"
            }
        }
        pub fn is_closed(&self) -> bool {
            self.closed.load(SeqCst)
        }
        pub fn set_spin_ceiling(&self, _d: Option<Duration>) {}
        pub fn stats(&self) -> Snap {
            let n = self.fallbacks.load(Relaxed);
            Snap { fallbacks: if n > 0 { vec![(not_connected(), n)] } else { vec![] }, ..Snap::default() }
        }
        pub fn census(&self) -> Option<Vec<u32>> {
            None
        }
        pub fn close(&self, _py: Python<'_>) -> bool {
            self.closed.store(true, SeqCst);
            true
        }
    }
}
