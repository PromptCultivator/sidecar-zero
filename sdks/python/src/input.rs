//! Inputs, read through the buffer protocol (sdk-python §3 "Inputs").
//!
//! An input is prepared with the GIL held: its buffer is exported (which pins its memory and
//! forbids a resize) and described in a plain struct of raw pointers. The call itself then
//! runs with the GIL released, reading that memory directly; the export is released only
//! after the GIL is back. Nothing here allocates on the buffer path.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes, PyList, PyTuple};
use std::cell::RefCell;
use std::os::raw::c_int;

/// An exported buffer, released when dropped. Created and dropped with the GIL held.
pub struct Export(ffi::Py_buffer);

impl Export {
    /// Export `obj`'s buffer with `flags`; None (and no pending exception) if it has none.
    pub fn get(obj: &Bound<'_, PyAny>, flags: c_int) -> Option<Export> {
        let mut view = ffi::Py_buffer::new();
        // SAFETY: obj is a live object and the GIL is held; view is a valid out-pointer.
        let r = unsafe { ffi::PyObject_GetBuffer(obj.as_ptr(), &mut view, flags) };
        if r != 0 {
            // SAFETY: the GIL is held. A refused export is an input the codec cannot take.
            unsafe { ffi::PyErr_Clear() };
            return None;
        }
        Some(Export(view))
    }
    pub fn ptr(&self) -> *const u8 {
        self.0.buf as *const u8
    }
    pub fn len(&self) -> usize {
        self.0.len.max(0) as usize
    }
    pub fn ndim(&self) -> usize {
        self.0.ndim.max(0) as usize
    }
    pub fn itemsize(&self) -> usize {
        self.0.itemsize.max(0) as usize
    }
    pub fn shape(&self, i: usize) -> isize {
        // SAFETY: shape has ndim entries when requested with PyBUF_ND or more.
        if self.0.shape.is_null() || i >= self.ndim() {
            0
        } else {
            unsafe { *self.0.shape.add(i) }
        }
    }
    pub fn stride(&self, i: usize) -> isize {
        if self.0.strides.is_null() || i >= self.ndim() {
            // C-contiguous without strides: the item size times the trailing extents.
            let mut s = self.itemsize() as isize;
            for k in i + 1..self.ndim() {
                s *= self.shape(k);
            }
            s
        } else {
            // SAFETY: strides has ndim entries when requested with PyBUF_STRIDES.
            unsafe { *self.0.strides.add(i) }
        }
    }
    pub fn format(&self) -> &[u8] {
        if self.0.format.is_null() {
            b"B"
        } else {
            // SAFETY: format is a NUL-terminated string owned by the export.
            unsafe { std::ffi::CStr::from_ptr(self.0.format) }.to_bytes()
        }
    }
    pub fn writable(obj: &Bound<'_, PyAny>) -> Option<Export> {
        Export::get(obj, ffi::PyBUF_WRITABLE | ffi::PyBUF_C_CONTIGUOUS)
    }
    pub fn ptr_mut(&self) -> *mut u8 {
        self.0.buf as *mut u8
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        // SAFETY: the view was filled by a successful PyObject_GetBuffer, and the GIL is held
        // wherever an Export is dropped (it never enters a detached closure).
        unsafe { ffi::PyBuffer_Release(&mut self.0) };
    }
}

// ── opaque bytes ────────────────────────────────────────────────────────────────────────

/// The bytes of a contiguous buffer, or nothing (the codec then fails: CODEC_ERROR).
#[derive(Clone, Copy)]
pub struct RawIn {
    pub ptr: *const u8,
    pub len: usize,
    pub ok: bool,
}

// SAFETY: the pointer refers to an exported buffer that stays pinned for the whole call; the
// call only reads it.
unsafe impl Send for RawIn {}
unsafe impl Sync for RawIn {}

impl RawIn {
    pub const NONE: RawIn = RawIn { ptr: std::ptr::null(), len: 0, ok: false };
    pub fn bytes(&self) -> &[u8] {
        if self.ptr.is_null() {
            &[]
        } else {
            // SAFETY: see the Send impl: ptr..ptr+len is the pinned export.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

pub fn raw(obj: &Bound<'_, PyAny>) -> (Option<Export>, RawIn) {
    match Export::get(obj, ffi::PyBUF_SIMPLE) {
        Some(e) => {
            let r = RawIn { ptr: e.ptr(), len: e.len(), ok: true };
            (Some(e), r)
        }
        None => (None, RawIn::NONE),
    }
}

// ── dense numeric ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Elem {
    F16,
    F32,
    F64,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Bool,
}

impl Elem {
    pub fn width(self) -> usize {
        match self {
            Elem::I8 | Elem::U8 | Elem::Bool => 1,
            Elem::F16 | Elem::I16 | Elem::U16 => 2,
            Elem::F32 | Elem::I32 | Elem::U32 => 4,
            Elem::F64 | Elem::I64 | Elem::U64 => 8,
        }
    }
}

/// A struct-module format string with the export's item size: the element type and whether
/// it is big-endian. None for anything that is not one number.
fn parse_format(fmt: &[u8], itemsize: usize) -> Option<(Elem, bool)> {
    let (be, code) = match fmt {
        [c] => (false, *c),
        [b'@' | b'=' | b'<', c] => (false, *c),
        [b'>' | b'!', c] => (true, *c),
        _ => return None,
    };
    let e = match (code, itemsize) {
        (b'e', 2) => Elem::F16,
        (b'f', 4) => Elem::F32,
        (b'd', 8) => Elem::F64,
        (b'?', 1) => Elem::Bool,
        (b'b' | b'h' | b'i' | b'l' | b'q' | b'n', w) => match w {
            1 => Elem::I8,
            2 => Elem::I16,
            4 => Elem::I32,
            8 => Elem::I64,
            _ => return None,
        },
        (b'B' | b'H' | b'I' | b'L' | b'Q' | b'N', w) => match w {
            1 => Elem::U8,
            2 => Elem::U16,
            4 => Elem::U32,
            8 => Elem::U64,
            _ => return None,
        },
        _ => return None,
    };
    Some((e, be && e.width() > 1))
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let man = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if man == 0 => sign,
        0 => {
            // subnormal: normalize
            let mut e = 127 - 15 + 1;
            let mut m = man;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((m & 0x3ff) << 13)
        }
        0x1f => sign | 0x7f80_0000 | (man << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (man << 13),
    };
    f32::from_bits(bits)
}

/// A `rows × n` matrix of numbers, as the dense codec reads it: element (r, c) is at
/// `ptr + r·s0 + c·s1`. `ok` is false for an input of the wrong shape or type, which the
/// codec then refuses (CODEC_ERROR).
#[derive(Clone, Copy)]
pub struct DenseIn {
    pub ptr: *const u8,
    pub elem: Elem,
    pub swap: bool,
    pub rows: u32,
    pub n: usize,
    pub s0: isize,
    pub s1: isize,
    pub ok: bool,
    /// A one-dimensional input: the answer is one number, not an array.
    pub scalar: bool,
}

// SAFETY: as for RawIn; the memory is a pinned export, or this thread's list scratch, and is
// only read.
unsafe impl Send for DenseIn {}
unsafe impl Sync for DenseIn {}

impl DenseIn {
    pub const NONE: DenseIn = DenseIn { ptr: std::ptr::null(), elem: Elem::F32, swap: false, rows: 1, n: 0, s0: 0, s1: 0, ok: false, scalar: true };

    /// Element (r, c) as f64. Only for r < rows, c < n of an `ok` input.
    #[inline]
    pub fn get(&self, r: usize, c: usize) -> f64 {
        // SAFETY: r and c are within the shape the export described, so the address is inside
        // the exported memory; read_unaligned allows any alignment.
        unsafe {
            let p = self.ptr.offset(r as isize * self.s0 + c as isize * self.s1);
            macro_rules! rd {
                ($t:ty) => {{
                    let v = std::ptr::read_unaligned(p as *const $t);
                    if self.swap {
                        v.swap_bytes()
                    } else {
                        v
                    }
                }};
            }
            match self.elem {
                Elem::F32 => f32::from_bits(rd!(u32)) as f64,
                Elem::F64 => f64::from_bits(rd!(u64)),
                Elem::F16 => f16_to_f32(rd!(u16)) as f64,
                Elem::I8 => *(p as *const i8) as f64,
                Elem::U8 => *p as f64,
                Elem::Bool => (*p != 0) as u8 as f64,
                Elem::I16 => rd!(i16) as f64,
                Elem::U16 => rd!(u16) as f64,
                Elem::I32 => rd!(i32) as f64,
                Elem::U32 => rd!(u32) as f64,
                Elem::I64 => rd!(i64) as f64,
                Elem::U64 => rd!(u64) as f64,
            }
        }
    }

    /// The whole input as little-endian float32 bytes, when it already is exactly that
    /// (the fast path: one copy into the slot).
    pub fn as_f32_bytes(&self) -> Option<&[u8]> {
        let row = self.n as isize * 4;
        let contiguous = self.s1 == 4 && (self.rows == 1 || self.s0 == row);
        (self.ok && self.elem == Elem::F32 && !self.swap && contiguous && cfg!(target_endian = "little"))
            // SAFETY: a contiguous rows × n float32 block inside the pinned export.
            .then(|| unsafe { std::slice::from_raw_parts(self.ptr, self.rows as usize * self.n * 4) })
    }

    /// Every value as f32, row-major, into `out` (rows × n long).
    pub fn convert(&self, out: &mut [f32]) {
        for r in 0..self.rows as usize {
            for c in 0..self.n {
                out[r * self.n + c] = self.get(r, c) as f32;
            }
        }
    }
}

thread_local! {
    /// A list or tuple input, converted to float32 with the GIL held. Grows once per thread.
    static LIST: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// What kind of input a dense codec saw, for the client's one-time warnings.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    Buffer,
    /// A list or tuple: converted element by element, slowly.
    Sequence,
    /// Wrong shape or not numbers.
    Refused,
}

/// Describe `obj` as a `rows × n` matrix. `strict`: float32 only, no conversion.
pub fn dense(obj: &Bound<'_, PyAny>, n: usize, strict: bool) -> (Option<Export>, DenseIn, Seen) {
    if n == 0 {
        return (None, DenseIn::NONE, Seen::Refused);
    }
    // Raw little-endian float32 bytes.
    if obj.is_instance_of::<PyBytes>() || obj.is_instance_of::<PyByteArray>() {
        let Some(e) = Export::get(obj, ffi::PyBUF_SIMPLE) else { return (None, DenseIn::NONE, Seen::Refused) };
        let len = e.len();
        let row = n * 4;
        if len == 0 || len % row != 0 || len / row > u32::MAX as usize {
            return (Some(e), DenseIn::NONE, Seen::Refused);
        }
        let rows = (len / row) as u32;
        let d = DenseIn { ptr: e.ptr(), elem: Elem::F32, swap: false, rows, n, s0: row as isize, s1: 4, ok: true, scalar: rows == 1 };
        return (Some(e), d, Seen::Buffer);
    }
    if let Some(e) = Export::get(obj, ffi::PyBUF_RECORDS_RO) {
        let Some((elem, swap)) = parse_format(e.format(), e.itemsize()) else { return (Some(e), DenseIn::NONE, Seen::Refused) };
        if strict && (elem != Elem::F32 || swap) {
            return (Some(e), DenseIn::NONE, Seen::Refused);
        }
        let d = match e.ndim() {
            1 if e.shape(0) as usize == n => DenseIn { ptr: e.ptr(), elem, swap, rows: 1, n, s0: 0, s1: e.stride(0), ok: true, scalar: true },
            2 if e.shape(1) as usize == n && e.shape(0) >= 1 && e.shape(0) as u64 <= u32::MAX as u64 => {
                DenseIn { ptr: e.ptr(), elem, swap, rows: e.shape(0) as u32, n, s0: e.stride(0), s1: e.stride(1), ok: true, scalar: false }
            }
            _ => return (Some(e), DenseIn::NONE, Seen::Refused),
        };
        return (Some(e), d, Seen::Buffer);
    }
    if strict || !(obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>()) {
        return (None, DenseIn::NONE, Seen::Refused);
    }
    // A sequence of numbers, or of rows of numbers: converted into this thread's scratch.
    match sequence(obj, n) {
        Some((ptr, rows, scalar)) => {
            (None, DenseIn { ptr, elem: Elem::F32, swap: false, rows, n, s0: (n * 4) as isize, s1: 4, ok: true, scalar }, Seen::Sequence)
        }
        None => (None, DenseIn::NONE, Seen::Refused),
    }
}

fn number(v: &Bound<'_, PyAny>) -> Option<f32> {
    v.extract::<f64>().ok().map(|x| x as f32)
}

fn sequence(obj: &Bound<'_, PyAny>, n: usize) -> Option<(*const u8, u32, bool)> {
    let len = obj.len().ok()?;
    if len == 0 {
        return None;
    }
    let first = obj.get_item(0).ok()?;
    let nested = first.is_instance_of::<PyList>() || first.is_instance_of::<PyTuple>();
    LIST.with(|cell| {
        let mut v = cell.borrow_mut();
        v.clear();
        if nested {
            for r in 0..len {
                let row = obj.get_item(r).ok()?;
                if row.len().ok()? != n {
                    return None;
                }
                for c in 0..n {
                    v.push(number(&row.get_item(c).ok()?)?);
                }
            }
        } else {
            if len != n {
                return None;
            }
            for c in 0..n {
                v.push(number(&obj.get_item(c).ok()?)?);
            }
        }
        let rows = if nested { len } else { 1 };
        Some((v.as_ptr() as *const u8, u32::try_from(rows).ok()?, !nested))
    })
}
