//! scz-worker-ort: the ONNX Runtime backend (worker §7.1) and its memory profiles (worker §5).
//!
//! The model is the file the supervisor passed, mapped read-only; an ORT-format model is used
//! in place. Each request column is bound to the model input of the same name as a tensor over
//! slot memory, shape `[rows, …]`, and each response column likewise to an output, so the
//! runtime reads the request and writes the answer where they already are. After every run the
//! backend checks that each output is still the slot memory it bound; the worker counts a run
//! where it was not (`zero_copy_misses`).
//!
//! Options, from the environment the supervisor passes through:
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `SCZ_ORT_PROFILE` | `A` | `A`: the runtime's arena off, memory returned to glibc and purged on `TRIM`. `B`: an arena capped at `SCZ_ORT_ARENA_MAX_MB`, shrunk on the run after `TRIM` |
//! | `SCZ_ORT_ARENA_MAX_MB` | 128 | Profile B's cap |
//! | `SCZ_ORT_PREPACK` | 1 | 0 turns weight prepacking off, so the weights are read from the mapped file (worker §7.1) |

use scz_shm::abi::{elem_type as et, status as st};
use scz_shm::registry::{Column, Schema};
use scz_worker_core::{Backend, BatchSink, BatchView, Signature};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering::*};
use std::sync::OnceLock;

mod ffi {
    #[repr(C)]
    pub struct Opts {
        pub profile: i32,
        pub arena_max: u64,
        pub prepack: i32,
        pub ort_format: i32,
    }
    #[repr(C)]
    pub struct Io {
        pub name: [u8; 128],
        pub elem: i32,
        pub rank: i32,
        pub dims: [i64; 8],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Tensor {
        pub data: *mut u8,
        pub bytes: u64,
        pub elem: i32,
        pub rank: i32,
        pub dims: [i64; 8],
    }
    #[repr(C)]
    pub struct Handle {
        _private: [u8; 0],
    }
    extern "C" {
        pub fn scz_ort_open(bytes: *const u8, len: u64, o: *const Opts, out: *mut *mut Handle, err: *mut u8, errlen: u64) -> i32;
        pub fn scz_ort_count(s: *const Handle, outputs: i32) -> u64;
        pub fn scz_ort_info(s: *const Handle, outputs: i32, i: u64, info: *mut Io) -> i32;
        pub fn scz_ort_run(
            s: *mut Handle,
            inp: *const Tensor,
            nin: u64,
            out: *const Tensor,
            nout: u64,
            shrink: i32,
            in_place: *mut i32,
            err: *mut u8,
            errlen: u64,
        ) -> i32;
        pub fn scz_ort_close(s: *mut Handle);
        pub fn scz_ort_trim();
    }
}

pub use ffi::Tensor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    A,
    B,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub profile: Profile,
    pub arena_max: u64,
    pub prepack: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { profile: Profile::A, arena_max: 128 << 20, prepack: true }
    }
}

impl Options {
    pub fn from_env() -> Options {
        let var = |k: &str| std::env::var(k).ok();
        Options {
            profile: if var("SCZ_ORT_PROFILE").as_deref() == Some("B") { Profile::B } else { Profile::A },
            arena_max: var("SCZ_ORT_ARENA_MAX_MB").and_then(|v| v.parse::<u64>().ok()).unwrap_or(128) << 20,
            prepack: var("SCZ_ORT_PREPACK").as_deref() != Some("0"),
        }
    }
}

/// A model input or output as the runtime describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Io {
    pub name: String,
    /// `ONNXTensorElementDataType`; -1 for anything that is not a tensor.
    pub elem: i32,
    /// -1 for a symbolic dimension.
    pub dims: Vec<i64>,
}

/// Why a run failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The runtime refused or failed: `MODEL_ERROR`.
    Model(String),
    /// It could not allocate (profile B's cap, or `RLIMIT_DATA`): `RESOURCE_EXHAUSTED`.
    Exhausted(String),
}

/// One session, shared by every consume thread: `Run` is safe to call concurrently.
pub struct Session {
    h: *mut ffi::Handle,
    pub inputs: Vec<Io>,
    pub outputs: Vec<Io>,
}

// SAFETY: ONNX Runtime documents `Run` on one session as safe from several threads; the
// handle is otherwise read-only after `open`.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

fn c_string(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

impl Session {
    /// A session over `bytes`, which must stay mapped for as long as the session lives.
    pub fn open(bytes: &'static [u8], o: &Options) -> Result<Session, String> {
        // An ORT-format model carries the flatbuffer file identifier "ORTM".
        let ort_format = bytes.get(4..8) == Some(b"ORTM");
        let opts = ffi::Opts { profile: (o.profile == Profile::B) as i32, arena_max: o.arena_max, prepack: o.prepack as i32, ort_format: ort_format as i32 };
        let mut h = core::ptr::null_mut();
        let mut err = [0u8; 512];
        // SAFETY: valid pointers for the duration of the call; the model bytes are 'static.
        if unsafe { ffi::scz_ort_open(bytes.as_ptr(), bytes.len() as u64, &opts, &mut h, err.as_mut_ptr(), err.len() as u64) } != 0 {
            return Err(c_string(&err));
        }
        let io = |outputs: i32| -> Vec<Io> {
            // SAFETY: `h` is a live handle; `info` is written by the call.
            let n = unsafe { ffi::scz_ort_count(h, outputs) };
            (0..n)
                .map(|i| {
                    let mut info = ffi::Io { name: [0; 128], elem: 0, rank: 0, dims: [0; 8] };
                    unsafe { ffi::scz_ort_info(h, outputs, i, &mut info) };
                    Io { name: c_string(&info.name), elem: info.elem, dims: info.dims[..info.rank.clamp(0, 8) as usize].to_vec() }
                })
                .collect()
        };
        Ok(Session { inputs: io(0), outputs: io(1), h })
    }

    /// Run with every input and output bound to the memory given. Ok(true) when every output
    /// is still that memory afterwards.
    pub fn run(&self, inputs: &[Tensor], outputs: &[Tensor], shrink: bool) -> Result<bool, RunError> {
        let mut in_place = 0;
        let mut err = [0u8; 512];
        // SAFETY: the tensors point at memory the caller owns for the call; the runtime only
        // reads inputs and writes outputs within their stated byte lengths.
        let r = unsafe {
            ffi::scz_ort_run(
                self.h,
                inputs.as_ptr(),
                inputs.len() as u64,
                outputs.as_ptr(),
                outputs.len() as u64,
                shrink as i32,
                &mut in_place,
                err.as_mut_ptr(),
                err.len() as u64,
            )
        };
        match r {
            0 => Ok(in_place != 0),
            2 => Err(RunError::Exhausted(c_string(&err))),
            _ => Err(RunError::Model(c_string(&err))),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the handle came from scz_ort_open and is closed once.
        unsafe { ffi::scz_ort_close(self.h) }
    }
}

/// Make glibc's allocator give memory back when `TRIM` asks (worker §5) by fixing its mmap
/// threshold. glibc otherwise raises the threshold each time a mapped block is freed, after
/// which buffers the size of a large batch's activations are carved from a heap instead; a
/// consume thread's heap is a thread arena, whose top `malloc_trim` leaves in place, so that
/// memory stayed resident (measured: 25 MB above the baseline after trimming, every other
/// cycle). With the threshold fixed, such buffers are mapped and unmapped each time.
pub fn tune_allocator() {
    // SAFETY: mallopt has no memory-safety preconditions; it runs before the session exists.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
    }
}

/// Return freed memory to the system: glibc's `malloc_trim(0)`.
pub fn trim() {
    // SAFETY: no preconditions.
    unsafe { ffi::scz_ort_trim() }
}

/// How one column becomes one tensor: its element type and the dimensions after the batch.
#[derive(Clone, Debug)]
pub struct Shape {
    pub elem: i32,
    pub inner: Vec<i64>,
    pub row_bytes: usize,
}

impl Shape {
    pub fn tensor(&self, data: *mut u8, rows: u32) -> Tensor {
        let mut dims = [0i64; 8];
        dims[0] = rows as i64;
        dims[1..1 + self.inner.len()].copy_from_slice(&self.inner);
        Tensor { data, bytes: (rows as usize * self.row_bytes) as u64, elem: self.elem, rank: 1 + self.inner.len() as i32, dims }
    }
}

/// The ONNX element type codes and the ABI's.
fn elem_of(onnx: i32) -> Option<u32> {
    Some(match onnx {
        1 => et::F32,
        2 => et::U8,
        3 => et::I8,
        4 => et::U16,
        5 => et::I16,
        6 => et::I32,
        7 => et::I64,
        10 => et::F16,
        11 => et::F64,
        12 => et::U32,
        13 => et::U64,
        _ => return None,
    })
}

/// The model's signature (worker §7.1): one column per input and output, named as the model
/// names it. The first dimension is the batch, symbolic or fixed and the same everywhere; the
/// others must be fixed, and a column carries their product per row (`[rows, 256]` is
/// `FixedSizeList<float32>[256]`; `[rows]` and `[rows, 1]` are a scalar column).
pub fn signature(s: &Session) -> Result<(Signature, Vec<Shape>, Vec<Shape>), String> {
    let mut fixed: Option<Option<u32>> = None;
    let mut map = |ios: &[Io]| -> Result<(Vec<Column>, Vec<Shape>), String> {
        let (mut cols, mut shapes) = (vec![], vec![]);
        for io in ios {
            let elem = elem_of(io.elem).ok_or(format!("{}: element type {} has no fixed-width column", io.name, io.elem))?;
            let (&batch, inner) = io.dims.split_first().ok_or(format!("{}: a scalar, with no batch dimension", io.name))?;
            let b = if batch > 0 { Some(batch as u32) } else { None };
            match fixed {
                None => fixed = Some(b),
                Some(f) if f != b => return Err(format!("{}: batch dimension {batch} differs from another tensor's", io.name)),
                _ => {}
            }
            if inner.iter().any(|&d| d <= 0) {
                return Err(format!("{}: dimensions {:?} after the batch must be fixed", io.name, io.dims));
            }
            let n: i64 = inner.iter().product();
            let c = Column::new(&io.name, elem, n as u32);
            let row_bytes = c.row_bytes().ok_or("a column too wide")? as usize;
            cols.push(c);
            shapes.push(Shape { elem: io.elem, inner: inner.to_vec(), row_bytes });
        }
        Ok((cols, shapes))
    };
    let (request, ins) = map(&s.inputs)?;
    let (response, outs) = map(&s.outputs)?;
    Ok((Signature { schema: Schema { request, response }, fixed_batch: fixed.flatten() }, ins, outs))
}

struct Loaded {
    session: Session,
    sig: Signature,
    ins: Vec<Shape>,
    outs: Vec<Shape>,
}

thread_local! {
    static IN_PLACE: Cell<bool> = const { Cell::new(true) };
    static COPIED: Cell<bool> = const { Cell::new(false) };
    /// Private buffers: warm-up, and padding to a fixed batch.
    static SCRATCH: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

pub struct OrtBackend {
    opts: Options,
    loaded: OnceLock<Loaded>,
    /// Profile B: the next run shrinks the arena (worker §5, `TRIM`).
    shrink: AtomicBool,
}

impl OrtBackend {
    pub fn new(opts: Options) -> OrtBackend {
        OrtBackend { opts, loaded: OnceLock::new(), shrink: AtomicBool::new(false) }
    }
    pub fn from_env() -> OrtBackend {
        Self::new(Options::from_env())
    }
    fn get(&self) -> &Loaded {
        self.loaded.get().expect("load runs before anything else")
    }
    fn shrink(&self) -> bool {
        self.opts.profile == Profile::B && self.shrink.swap(false, Relaxed)
    }
    /// Run over private buffers of `rows` rows: warm-up, and the fixed-batch path.
    fn run_private(&self, rows: u32, fill: impl Fn(usize, &mut [u8]), mut take: impl FnMut(usize, &[u8])) -> Result<bool, RunError> {
        let l = self.get();
        SCRATCH.with(|s| {
            let mut s = s.borrow_mut();
            let n = l.ins.len() + l.outs.len();
            s.resize_with(n, Vec::new);
            for (k, sh) in l.ins.iter().chain(&l.outs).enumerate() {
                s[k].clear();
                s[k].resize(rows as usize * sh.row_bytes, 0);
            }
            for i in 0..l.ins.len() {
                fill(i, &mut s[i]);
            }
            let ins: Vec<Tensor> = l.ins.iter().enumerate().map(|(i, sh)| sh.tensor(s[i].as_mut_ptr(), rows)).collect();
            let outs: Vec<Tensor> = l.outs.iter().enumerate().map(|(o, sh)| sh.tensor(s[l.ins.len() + o].as_mut_ptr(), rows)).collect();
            let r = l.session.run(&ins, &outs, self.shrink());
            for o in 0..l.outs.len() {
                take(o, &s[l.ins.len() + o]);
            }
            r
        })
    }
}

fn status_of(r: &Result<bool, RunError>) -> u32 {
    use std::sync::atomic::AtomicU32;
    static LOGGED: AtomicU32 = AtomicU32::new(0);
    match r {
        Ok(_) => st::OK,
        Err(e) => {
            // A failing model fails every request; log the first few.
            if LOGGED.fetch_add(1, Relaxed) < 5 {
                eprintln!("sidecarzero-worker[{}]: run failed: {e:?}", std::process::id());
            }
            if matches!(e, RunError::Exhausted(_)) {
                st::RESOURCE_EXHAUSTED
            } else {
                st::MODEL_ERROR
            }
        }
    }
}

impl Backend for OrtBackend {
    fn load(&self, _gen: u32, model: &'static [u8]) {
        let fail = |what: String| -> ! {
            eprintln!("sidecarzero-worker[{}]: {what}", std::process::id());
            std::process::exit(4)
        };
        if model.is_empty() {
            fail("no model: the model file is empty".into());
        }
        tune_allocator();
        let session = Session::open(model, &self.opts).unwrap_or_else(|e| fail(format!("loading the model: {e}")));
        let (sig, ins, outs) = signature(&session).unwrap_or_else(|e| fail(format!("the model's signature: {e}")));
        let _ = self.loaded.set(Loaded { session, sig, ins, outs });
    }

    fn signature(&self) -> Option<Signature> {
        Some(self.get().sig.clone())
    }

    fn warm(&self, rows: u32) {
        let rows = self.get().sig.fixed_batch.unwrap_or(rows);
        let _ = self.run_private(rows, |_, _| {}, |_, _| {});
    }

    fn infer_batch(&self, input: &BatchView<'_>, output: &mut BatchSink<'_>) -> u32 {
        let l = self.get();
        let rows = input.rows();
        if let Some(b) = l.sig.fixed_batch.filter(|&b| b != rows) {
            // Fewer rows than the model's fixed batch: pad in a scratch tensor and copy the
            // answers back (worker §4: this path copies).
            COPIED.set(true);
            IN_PLACE.set(true);
            let r = self.run_private(b, |i, buf| input.copy_column(i, buf), |o, buf| output.write_column(o, buf));
            return status_of(&r);
        }
        COPIED.set(false);
        let ins: Vec<Tensor> = l.ins.iter().enumerate().map(|(i, sh)| sh.tensor(input.raw(i).0 as *mut u8, rows)).collect();
        let outs: Vec<Tensor> = l.outs.iter().enumerate().map(|(o, sh)| sh.tensor(output.raw(o).0, rows)).collect();
        let r = l.session.run(&ins, &outs, self.shrink());
        let in_arena = ins.iter().all(|t| input.in_arena(t.data)) && outs.iter().all(|t| output.in_arena(t.data));
        IN_PLACE.set(matches!(r, Ok(true)) && in_arena || r.is_err());
        status_of(&r)
    }

    fn in_place(&self) -> bool {
        IN_PLACE.get()
    }

    fn copied(&self) -> bool {
        COPIED.get()
    }

    fn trim(&self) {
        if self.opts.profile == Profile::B {
            self.shrink.store(true, Relaxed);
        }
        trim();
    }
}
