//! The prebuilt codecs (sdk-contract §2, sdk-python §3): opaque bytes, and a dense float32
//! matrix in with one float32 score per row out. Each is a Python class describing the codec
//! and, on Linux, a Rust `sidecarzero::Codec` the client runs with the GIL released.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

/// A codec fault for tests (the conformance agent's `CODEC_FAULT`): the codec fails on every
/// encode, or every decode, while it is set.
pub const FAULT_ENCODE: u8 = 1;
pub const FAULT_DECODE: u8 = 2;

fn fault_code(mode: &str) -> PyResult<u8> {
    match mode {
        "off" | "" => Ok(0),
        "encode" => Ok(FAULT_ENCODE),
        "decode" => Ok(FAULT_DECODE),
        _ => Err(PyValueError::new_err(format!("fault mode must be off, encode, or decode, not {mode:?}"))),
    }
}

/// Opaque bytes: any buffer-protocol object in, the response's bytes out (`bytes`).
///
/// `response_len`, if given, is the exact response length the codec accepts; any other length
/// fails to decode (CODEC_ERROR). An opaque codec binds only to a sidecar whose registry is
/// empty, such as the echo and chaos workers (arrow-data-plane §9).
#[pyclass(frozen, module = "sidecarzero.codecs", name = "Bytes")]
pub struct Bytes {
    pub response_len: Option<usize>,
    pub fault: Arc<AtomicU8>,
}

#[pymethods]
impl Bytes {
    #[new]
    #[pyo3(signature = (*, response_len = None))]
    fn new(response_len: Option<usize>) -> Self {
        Bytes { response_len, fault: Arc::new(AtomicU8::new(0)) }
    }
    /// None: opaque bytes declare no columns.
    fn schema_fingerprint(&self) -> Option<u64> {
        None
    }
    #[getter]
    fn response_len(&self) -> Option<usize> {
        self.response_len
    }
    /// Test hook: fail every encode or decode ("encode", "decode", "off").
    fn _set_fault(&self, mode: &str) -> PyResult<()> {
        self.fault.store(fault_code(mode)?, Relaxed);
        Ok(())
    }
    /// The request bytes the client would write for `input`, or None if it would refuse it
    /// (for the in-process test double).
    fn _prepare<'py>(&self, py: Python<'py>, input: &Bound<'py, PyAny>) -> Option<Bound<'py, PyBytes>> {
        if self.fault.load(Relaxed) == FAULT_ENCODE {
            return None;
        }
        let (e, raw) = crate::input::raw(input);
        let b = raw.ok.then(|| PyBytes::new(py, raw.bytes()));
        drop(e);
        b
    }
    /// The answer for response bytes, or None if the codec would refuse them.
    fn _decode<'py>(&self, py: Python<'py>, response: &Bound<'py, PyAny>) -> Option<Bound<'py, PyBytes>> {
        if self.fault.load(Relaxed) == FAULT_DECODE {
            return None;
        }
        let (e, raw) = crate::input::raw(response);
        let b = (raw.ok && self.response_len.is_none_or(|n| n == raw.len)).then(|| PyBytes::new(py, raw.bytes()));
        drop(e);
        b
    }
    fn __repr__(&self) -> String {
        match self.response_len {
            Some(n) => format!("Bytes(response_len={n})"),
            None => "Bytes()".into(),
        }
    }
}

/// `features: float32[rows, n]` in, `score: float32[rows, 1]` out: the launch workload.
///
/// Input: a one-dimensional buffer of `n` numbers (the answer is a `float`), a two-dimensional
/// `(rows, n)` buffer (the answer is a float32 array, NumPy's when NumPy is installed), raw
/// little-endian float32 `bytes`, or a list. Other numeric types are converted during the copy
/// unless `strict`.
#[pyclass(frozen, module = "sidecarzero.codecs", name = "DenseF32ToScore")]
pub struct DenseF32ToScore {
    pub n: usize,
    pub input: String,
    pub output: String,
    pub strict: bool,
    pub fault: Arc<AtomicU8>,
}

#[pymethods]
impl DenseF32ToScore {
    #[new]
    #[pyo3(signature = (n, *, input = "features".to_string(), output = "score".to_string(), strict = false))]
    fn new(n: usize, input: String, output: String, strict: bool) -> PyResult<Self> {
        if n == 0 || n > u32::MAX as usize {
            return Err(PyValueError::new_err("n must be at least 1"));
        }
        if input.is_empty() || output.is_empty() || input.len() > u16::MAX as usize || output.len() > u16::MAX as usize {
            return Err(PyValueError::new_err("column names must be non-empty"));
        }
        Ok(DenseF32ToScore { n, input, output, strict, fault: Arc::new(AtomicU8::new(0)) })
    }
    #[getter]
    fn n(&self) -> usize {
        self.n
    }
    #[getter]
    fn strict(&self) -> bool {
        self.strict
    }
    /// The canonical hash of the declared columns (arrow-data-plane §9), which the client
    /// binds by.
    fn schema_fingerprint(&self) -> u64 {
        crate::fingerprint::of(
            &[(self.input.as_str(), crate::fingerprint::F32, self.n as u32, false)],
            &[(self.output.as_str(), crate::fingerprint::F32, 1, false)],
        )
    }
    fn _set_fault(&self, mode: &str) -> PyResult<()> {
        self.fault.store(fault_code(mode)?, Relaxed);
        Ok(())
    }
    /// `(float32 bytes, rows, scalar)` as the client would write `input`, or None if it would
    /// refuse it (for the in-process test double).
    fn _prepare<'py>(&self, py: Python<'py>, input: &Bound<'py, PyAny>) -> Option<(Bound<'py, PyBytes>, u32, bool)> {
        if self.fault.load(Relaxed) == FAULT_ENCODE {
            return None;
        }
        let (e, d, _) = crate::input::dense(input, self.n, self.strict);
        if !d.ok {
            return None;
        }
        let mut v = vec![0f32; d.rows as usize * self.n];
        d.convert(&mut v);
        drop(e);
        let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        Some((PyBytes::new(py, &bytes), d.rows, d.scalar))
    }
    /// The answer for `rows` scores, as the client builds it: a float for a one-dimensional
    /// input, else a float32 array. None if the codec would refuse them.
    fn _decode(&self, py: Python<'_>, scores: Vec<f32>, rows: u32, scalar: bool) -> PyResult<Option<Py<PyAny>>> {
        if self.fault.load(Relaxed) == FAULT_DECODE || scores.len() != rows as usize {
            return Ok(None);
        }
        if scalar {
            return Ok(Some(pyo3::types::PyFloat::new(py, scores[0] as f64).into_any().unbind()));
        }
        crate::client::f32_array(py, &scores).map(Some)
    }
    fn __repr__(&self) -> String {
        format!("DenseF32ToScore({}, input={:?}, output={:?}, strict={})", self.n, self.input, self.output, if self.strict { "True" } else { "False" })
    }
}

thread_local! {
    /// The last response this thread decoded: bytes, or one score per row. Read back once the
    /// GIL is held again. Each grows to the largest response seen and is then reused.
    pub static OUT_BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    pub static OUT_F32: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    /// A converted request (float64, integers, strided): rows × n floats.
    static CONV: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::input::{DenseIn, RawIn};
    use scz_shm::abi::elem_type as et;
    use sidecarzero::{Codec, CodecError, Column, FrameReader, FrameWriter, Schema, SlotReader, SlotWriter};

    pub struct RawCodec {
        pub response_len: Option<usize>,
        pub fault: Arc<AtomicU8>,
    }

    impl Codec for RawCodec {
        type In = RawIn;
        type Out = ();
        fn request_len(&self, input: &RawIn) -> usize {
            input.len
        }
        fn encode(&self, input: &RawIn, w: &mut SlotWriter<'_>) -> Result<(), CodecError> {
            if !input.ok || self.fault.load(Relaxed) == FAULT_ENCODE {
                return Err(CodecError::Shape);
            }
            Ok(w.write(input.bytes())?)
        }
        fn decode(&self, r: &SlotReader<'_>) -> Result<(), CodecError> {
            if self.fault.load(Relaxed) == FAULT_DECODE || self.response_len.is_some_and(|n| n != r.len()) {
                return Err(CodecError::Shape);
            }
            OUT_BYTES.with(|b| {
                let mut b = b.borrow_mut();
                b.clear();
                b.resize(r.len(), 0);
                r.copy_to(0, &mut b)
            })?;
            Ok(())
        }
    }

    pub struct DenseCodec {
        pub n: usize,
        pub input: String,
        pub output: String,
        pub fault: Arc<AtomicU8>,
    }

    impl Codec for DenseCodec {
        type In = DenseIn;
        type Out = ();
        fn schema(&self) -> Option<Schema> {
            Some(Schema { request: vec![Column::new(&self.input, et::F32, self.n as u32)], response: vec![Column::new(&self.output, et::F32, 1)] })
        }
        fn rows(&self, input: &DenseIn) -> u32 {
            if input.ok {
                input.rows
            } else {
                1
            }
        }
        fn encode_frame(&self, input: &DenseIn, w: &mut FrameWriter<'_>) -> Result<(), CodecError> {
            if !input.ok || input.n != self.n || input.rows != w.rows() || self.fault.load(Relaxed) == FAULT_ENCODE {
                return Err(CodecError::Shape);
            }
            if let Some(b) = input.as_f32_bytes() {
                return w.write_bytes(0, b);
            }
            CONV.with(|c| {
                let mut c = c.borrow_mut();
                c.clear();
                c.resize(input.rows as usize * self.n, 0.0);
                input.convert(&mut c);
                w.write_f32(0, &c)
            })
        }
        fn decode_frame(&self, r: &FrameReader<'_>) -> Result<(), CodecError> {
            if self.fault.load(Relaxed) == FAULT_DECODE {
                return Err(CodecError::Shape);
            }
            OUT_F32.with(|o| {
                let mut o = o.borrow_mut();
                o.clear();
                o.resize(r.rows() as usize, 0.0);
                r.read_f32(0, &mut o)
            })
        }
    }
}
