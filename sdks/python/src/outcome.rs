//! `Outcome`, and the Python enum members it hands out (`Source`, `FallbackReason`).
//!
//! The enums are plain Python `enum.Enum`s (`sidecarzero._enums`), so `out.source is
//! Source.MODEL` holds: the core keeps references to their members and returns those.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyType;

/// The reasons in the Rust core's order (sdk-contract §3.3), which is also the index the core
/// counts them by.
#[cfg(target_os = "linux")]
pub fn reason_names() -> Vec<&'static str> {
    sidecarzero::FallbackReason::ALL.iter().map(|r| r.as_str()).collect()
}

#[cfg(not(target_os = "linux"))]
pub fn reason_names() -> Vec<&'static str> {
    vec![
        "NOT_CONNECTED",
        "ABI_MISMATCH",
        "SCHEMA_MISMATCH",
        "REBINDING",
        "GATE_INIT",
        "GATE_SHEDDING",
        "GATE_RELOADING",
        "GATE_DEAD",
        "GATE_DRAINING",
        "STALE_HEARTBEAT",
        "CLOCK_FAULT",
        "OVERLOADED",
        "TOO_LARGE",
        "NO_SLOT",
        "CODEC_ERROR",
        "RING_FULL",
        "DEADLINE",
        "WORKER_FAILED",
        "MODEL_ERROR",
        "BAD_REQUEST",
        "RESOURCE_EXHAUSTED",
    ]
}

#[cfg(target_os = "linux")]
pub fn reason_index(r: sidecarzero::FallbackReason) -> u8 {
    sidecarzero::FallbackReason::ALL.iter().position(|x| *x == r).unwrap_or(0) as u8
}

pub fn not_connected() -> u8 {
    reason_names().iter().position(|n| *n == "NOT_CONNECTED").unwrap_or(0) as u8
}

pub struct Enums {
    pub model: Py<PyAny>,
    pub fallback: Py<PyAny>,
    pub reasons: Vec<Py<PyAny>>,
}

static ENUMS: PyOnceLock<Enums> = PyOnceLock::new();

pub fn enums(py: Python<'_>) -> PyResult<&'static Enums> {
    ENUMS.get_or_try_init(py, || {
        let m = py.import("sidecarzero._enums")?;
        let src = m.getattr("Source")?;
        let fr = m.getattr("FallbackReason")?;
        let reasons = reason_names().iter().map(|n| fr.getattr(*n).map(Bound::unbind)).collect::<PyResult<Vec<_>>>()?;
        Ok(Enums { model: src.getattr("MODEL")?.unbind(), fallback: src.getattr("FALLBACK")?.unbind(), reasons })
    })
}

pub fn reason_obj(py: Python<'_>, i: u8) -> PyResult<Py<PyAny>> {
    Ok(enums(py)?.reasons[i as usize].clone_ref(py))
}

/// What `infer` returns: always a value, from the model or from the fallback.
#[pyclass(frozen, module = "sidecarzero", name = "Outcome")]
pub struct Outcome {
    pub value: Py<PyAny>,
    pub model: bool,
    pub reason: u8,
    pub model_gen: u32,
}

#[pymethods]
impl Outcome {
    /// For test doubles: `Outcome(value, Source.MODEL, model_gen=g)` or
    /// `Outcome(value, Source.FALLBACK, reason)`.
    #[new]
    #[pyo3(signature = (value, source, reason = None, model_gen = 0))]
    fn new(py: Python<'_>, value: Py<PyAny>, source: &Bound<'_, PyAny>, reason: Option<&Bound<'_, PyAny>>, model_gen: u32) -> PyResult<Self> {
        let e = enums(py)?;
        let model = source.is(e.model.bind(py));
        if !model && !source.is(e.fallback.bind(py)) {
            return Err(pyo3::exceptions::PyTypeError::new_err("source must be a Source"));
        }
        let reason = match (model, reason) {
            (true, _) => 0,
            (false, Some(r)) => {
                e.reasons.iter().position(|m| r.is(m.bind(py))).ok_or_else(|| pyo3::exceptions::PyTypeError::new_err("reason must be a FallbackReason"))? as u8
            }
            (false, None) => return Err(pyo3::exceptions::PyTypeError::new_err("a fallback outcome needs its reason")),
        };
        Ok(Outcome { value, model, reason, model_gen: if model { model_gen } else { 0 } })
    }
    #[getter]
    fn value(&self, py: Python<'_>) -> Py<PyAny> {
        self.value.clone_ref(py)
    }
    /// `Source.MODEL` or `Source.FALLBACK`.
    #[getter]
    fn source(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let e = enums(py)?;
        Ok(if self.model { e.model.clone_ref(py) } else { e.fallback.clone_ref(py) })
    }
    /// Why the fallback answered; None for a model answer.
    #[getter]
    fn reason(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if self.model {
            Ok(None)
        } else {
            reason_obj(py, self.reason).map(Some)
        }
    }
    /// The model generation that answered; 0 for a fallback.
    #[getter]
    fn model_gen(&self) -> u32 {
        self.model_gen
    }
    #[getter]
    fn is_model(&self) -> bool {
        self.model
    }
    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let v = self.value.bind(py).repr()?;
        Ok(if self.model {
            format!("Outcome(value={v}, source=MODEL, model_gen={})", self.model_gen)
        } else {
            format!("Outcome(value={v}, source=FALLBACK, reason={})", reason_names()[self.reason as usize])
        })
    }
    /// `Outcome[float]` in annotations evaluated at run time.
    #[classmethod]
    fn __class_getitem__(cls: &Bound<'_, PyType>, _item: &Bound<'_, PyAny>) -> Py<PyType> {
        cls.clone().unbind()
    }
}
