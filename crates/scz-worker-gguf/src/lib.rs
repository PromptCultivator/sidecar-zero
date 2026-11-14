//! scz-worker-gguf: the GGUF backend (worker §7.2). Embeddings from a GGUF model, through
//! llama.cpp, on the CPU. Not text generation.
//!
//! ## Columns
//!
//! | Direction | Column | Arrow type |
//! |---|---|---|
//! | Request | `tokens` | `FixedSizeList<int32>[L]`: one sequence of token ids per row |
//! | Response | `embedding` | `FixedSizeList<float32>[D]`: that sequence's embedding; `D` is the model's |
//!
//! **The input convention.** A row holds a sequence's ids from its first element. The
//! sequence ends at the first negative id, or at the end of the row if it fills all `L`;
//! clients pad with -1, and whatever follows the first negative id is ignored. The ids are the
//! model's own, special tokens included: for a BERT model, `[CLS] … [SEP]` as llama.cpp's
//! `llama_tokenize` makes them with `add_special`. The worker adds nothing. A row whose first
//! id is negative (an empty sequence), or that holds an id at or above the vocabulary's size,
//! fails the request with `BAD_REQUEST`; the worker carries on.
//!
//! The ids are copied into the worker's private memory before they are checked and used: they
//! index the model's embedding table, and request memory stays writable by the host (worker
//! §7.1, index-like inputs).
//!
//! **The output** is the model's pooled embedding, pooled as the model says (CLS for bge),
//! which llama.cpp returns unnormalized; the worker L2-normalizes it unless
//! `--normalize=none`.
//!
//! **Rows** of one request are packed into one llama.cpp call, each its own sequence, while
//! their tokens fit `L` and they number at most 16; a request with more is several calls.
//! Packs never hold more than `L` tokens, so one costs at most what one full row does.
//!
//! ## Options
//!
//! The worker's own arguments, which the supervisor passes first (`--worker-arg=ARG`):
//!
//! | Argument | Default | Meaning |
//! |---|---|---|
//! | `--max-tokens=L` | the model's training context, at most 128 | Tokens per row: the request column's width. At most the training context |
//! | `--threads=N` | 2 | llama.cpp threads per consume thread. More than one is a ggml thread pool that sleeps between calls |
//! | `--normalize=l2\|none` | `l2` | Whether embeddings are L2-normalized |
//! | `--max-infer-ms=N` | the arena's maximum deadline | When an inference is abandoned, measured from its start (below) |
//! | `--repack=on\|off` | `on` | llama.cpp's CPU weight repacking (`use_extra_bufts`): on, weight types it has a faster layout for are copied into private memory; off, every weight is read from the shared mapping |
//! | `--log=warn\|info\|error\|none` | `warn` | llama.cpp's messages on stderr at and above this level |
//!
//! ## Loading
//!
//! llama.cpp loads from a path, so the backend opens [`scz_worker_core::model_path`], the
//! inherited descriptor as `/proc/self/fd/N`, with the file memory-mapped. Under Landlock that
//! open is checked against the model file's own location, not `/proc`: the model's directory
//! must be in the worker's view (`--worker-read=DIR`).
//!
//! ## Cancellation
//!
//! Every llama.cpp call carries a deadline, which its abort callback checks between the
//! nodes of the graph; a call past it is abandoned and the request completes with
//! `DEADLINE_EXCEEDED`. The deadline is the request's own (`BatchView::deadline_ns`, when its
//! caller stops waiting), or the inference's start plus `--max-infer-ms` if that is sooner, by
//! default the arena's maximum deadline: no caller waits longer than that, and the supervisor
//! condemns a worker holding a slot for twice as long. The cap stays because the request's
//! deadline is written by the client.

use scz_shm::abi::{elem_type as et, status as st};
use scz_shm::layout::Directory;
use scz_shm::registry::{Column, Schema};
use scz_shm::seg::Seg;
use scz_shm::wait::monotonic_ns;
use scz_worker_core::{Backend, BatchSink, BatchView, Signature};
use std::cell::RefCell;
use std::ffi::CString;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Once, OnceLock};

mod ffi {
    use std::ffi::c_char;

    #[repr(C)]
    pub struct Info {
        pub n_vocab: i32,
        pub n_embd: i32,
        pub n_ctx_train: i32,
        pub pooling: i32,
        pub memoryless: i32,
        pub desc: [u8; 128],
    }
    #[repr(C)]
    pub struct LlamaModel {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct Ctx {
        _private: [u8; 0],
    }
    extern "C" {
        pub fn scz_gguf_init(log_min: i32);
        pub fn scz_gguf_load(path: *const c_char, repack: i32, err: *mut u8, errlen: usize) -> *mut LlamaModel;
        pub fn scz_gguf_model_free(m: *mut LlamaModel);
        pub fn scz_gguf_info(m: *const LlamaModel, info: *mut Info, err: *mut u8, errlen: usize) -> i32;
        pub fn scz_gguf_tokenize(m: *const LlamaModel, text: *const u8, len: i32, out: *mut i32, max: i32) -> i32;
        pub fn scz_gguf_ctx_new(m: *mut LlamaModel, n_tokens: i32, n_seqs: i32, n_threads: i32, err: *mut u8, errlen: usize) -> *mut Ctx;
        pub fn scz_gguf_ctx_free(c: *mut Ctx);
        pub fn scz_gguf_embed(c: *mut Ctx, tokens: *const i32, lens: *const i32, n_seqs: i32, deadline_ns: u64, out: *mut f32) -> i32;
        pub fn scz_gguf_quantize(input: *const c_char, output: *const c_char, ftype: i32, nthread: i32) -> i32;
        pub fn scz_gguf_reference(m: *mut LlamaModel, tokens: *const i32, n: i32, out: *mut f32, err: *mut u8, errlen: usize) -> i32;
    }
}

fn c_string(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// ggml's log levels, as `--log` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    None = 0,
    Info = 2,
    Warn = 3,
    Error = 4,
}

static INIT: Once = Once::new();

/// Route llama.cpp's log to stderr at `level` and initialize its backends. Once per process;
/// later calls change nothing.
pub fn init(level: LogLevel) {
    // SAFETY: no preconditions; called once.
    INIT.call_once(|| unsafe { ffi::scz_gguf_init(level as i32) });
}

/// How a model pools its tokens into one embedding (`enum llama_pooling_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pooling {
    None,
    Mean,
    Cls,
    Last,
    Rank,
    Other(i32),
}

impl Pooling {
    fn from_raw(v: i32) -> Pooling {
        match v {
            0 => Pooling::None,
            1 => Pooling::Mean,
            2 => Pooling::Cls,
            3 => Pooling::Last,
            4 => Pooling::Rank,
            v => Pooling::Other(v),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub n_vocab: u32,
    /// The size of one pooled embedding.
    pub n_embd: u32,
    /// The longest sequence the model was trained on: positions beyond it do not exist.
    pub n_ctx_train: u32,
    pub pooling: Pooling,
    /// The model keeps no memory between calls (an encoder such as BERT), so several
    /// sequences can share one call.
    pub memoryless: bool,
    pub desc: String,
}

/// A loaded model, shared by every consume thread's context.
pub struct Model {
    p: *mut ffi::LlamaModel,
    pub info: ModelInfo,
}

// SAFETY: llama.cpp's model is read-only once loaded, and is shared by contexts that run on
// different threads; that is how llama.cpp is meant to serve several sequences at once.
unsafe impl Send for Model {}
unsafe impl Sync for Model {}

impl Model {
    /// Load the GGUF file at `path`, memory-mapped.
    pub fn load(path: &str, repack: bool) -> Result<Model, String> {
        init(LogLevel::Warn);
        let c = CString::new(path).map_err(|_| "a path with a NUL byte".to_string())?;
        let mut err = [0u8; 256];
        // SAFETY: valid C string and buffer for the duration of the call.
        let p = unsafe { ffi::scz_gguf_load(c.as_ptr(), repack as i32, err.as_mut_ptr(), err.len()) };
        if p.is_null() {
            return Err(c_string(&err));
        }
        let mut i = ffi::Info { n_vocab: 0, n_embd: 0, n_ctx_train: 0, pooling: -1, memoryless: 0, desc: [0; 128] };
        // SAFETY: a live model; `i` is written by the call.
        if unsafe { ffi::scz_gguf_info(p, &mut i, err.as_mut_ptr(), err.len()) } != 0 {
            // SAFETY: freed once, never used again.
            unsafe { ffi::scz_gguf_model_free(p) };
            return Err(c_string(&err));
        }
        let info = ModelInfo {
            n_vocab: i.n_vocab.max(0) as u32,
            n_embd: i.n_embd.max(0) as u32,
            n_ctx_train: i.n_ctx_train.max(0) as u32,
            pooling: Pooling::from_raw(i.pooling),
            memoryless: i.memoryless != 0,
            desc: c_string(&i.desc),
        };
        Ok(Model { p, info })
    }

    /// `text` in the model's tokens, with its special tokens added (`[CLS] … [SEP]` for BERT).
    pub fn tokenize(&self, text: &str) -> Vec<i32> {
        let mut out = vec![0i32; text.len() + 8];
        loop {
            // SAFETY: the text and the output buffer are valid for the lengths given.
            let n = unsafe { ffi::scz_gguf_tokenize(self.p, text.as_ptr(), text.len() as i32, out.as_mut_ptr(), out.len() as i32) };
            if n >= 0 {
                out.truncate(n as usize);
                return out;
            }
            out.resize(n.unsigned_abs() as usize, 0);
        }
    }

    /// The pooled embedding of one sequence, unnormalized, by the plainest use of llama.cpp: a
    /// fresh context with its default parameters and one `llama_decode`. For tests.
    pub fn reference(&self, tokens: &[i32]) -> Result<Vec<f32>, String> {
        let mut out = vec![0f32; self.info.n_embd as usize];
        let mut err = [0u8; 256];
        // SAFETY: the tokens and the output are valid for the lengths given; the ids are the
        // caller's, as for any direct use of llama.cpp.
        let r = unsafe { ffi::scz_gguf_reference(self.p, tokens.as_ptr(), tokens.len() as i32, out.as_mut_ptr(), err.as_mut_ptr(), err.len()) };
        if r != 0 {
            return Err(c_string(&err));
        }
        Ok(out)
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: freed once; every context made from it has been dropped first by its owner.
        unsafe { ffi::scz_gguf_model_free(self.p) }
    }
}

/// A quantization, as `enum llama_ftype` numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quant {
    Q4_0 = 2,
    Q8_0 = 7,
}

/// Write the GGUF model at `input` quantized to `q` at `output`, with llama.cpp's quantizer. For
/// tests and tooling; the worker never quantizes.
pub fn quantize(input: &str, output: &str, q: Quant) -> Result<(), String> {
    init(LogLevel::Warn);
    let (i, o) = (CString::new(input).map_err(|e| e.to_string())?, CString::new(output).map_err(|e| e.to_string())?);
    // SAFETY: valid C strings for the duration of the call.
    match unsafe { ffi::scz_gguf_quantize(i.as_ptr(), o.as_ptr(), q as i32, 4) } {
        0 => Ok(()),
        r => Err(format!("llama_model_quantize returned {r}")),
    }
}

/// Why an embedding call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedError {
    /// llama.cpp stopped at the deadline: `DEADLINE_EXCEEDED`.
    Aborted,
    /// It could not allocate: `RESOURCE_EXHAUSTED`.
    Exhausted,
    /// The batch was not one this context takes (a bug on this side): `MODEL_ERROR`.
    Invalid,
    /// llama.cpp failed: `MODEL_ERROR`.
    Failed,
}

/// One llama.cpp context, with its batch, thread pool, and abort deadline. Not shared:
/// each consume thread has its own.
pub struct Context {
    p: *mut ffi::Ctx,
    pub n_embd: usize,
    pub max_tokens: usize,
    pub max_seqs: usize,
}

impl Context {
    /// A context for calls of up to `max_tokens` tokens in up to `max_seqs` sequences, on
    /// `threads` threads.
    pub fn new(model: &Model, max_tokens: u32, max_seqs: u32, threads: u32) -> Result<Context, String> {
        let mut err = [0u8; 256];
        // SAFETY: a live model, which outlives the context (the backend never frees it).
        let p = unsafe { ffi::scz_gguf_ctx_new(model.p, max_tokens as i32, max_seqs as i32, threads as i32, err.as_mut_ptr(), err.len()) };
        if p.is_null() {
            return Err(c_string(&err));
        }
        Ok(Context { p, n_embd: model.info.n_embd as usize, max_tokens: max_tokens as usize, max_seqs: max_seqs as usize })
    }

    /// Embed sequences in one llama.cpp call: `tokens` holds their ids back to back, `lens`
    /// their lengths. Writes each pooled embedding, unnormalized, to `out`. llama.cpp is told
    /// to stop at `deadline_ns` (`CLOCK_MONOTONIC`).
    pub fn embed(&mut self, tokens: &[i32], lens: &[i32], deadline_ns: u64, out: &mut [f32]) -> Result<(), EmbedError> {
        let total: i64 = lens.iter().map(|&n| n as i64).sum();
        if lens.is_empty()
            || lens.len() > self.max_seqs
            || total != tokens.len() as i64
            || tokens.len() > self.max_tokens
            || out.len() < lens.len() * self.n_embd
        {
            return Err(EmbedError::Invalid);
        }
        // SAFETY: every array is valid for the lengths checked above; the shim checks each
        // id against the vocabulary and each length against the batch again before use.
        let r = unsafe { ffi::scz_gguf_embed(self.p, tokens.as_ptr(), lens.as_ptr(), lens.len() as i32, deadline_ns, out.as_mut_ptr()) };
        match r {
            0 => Ok(()),
            1 => Err(EmbedError::Aborted),
            2 => Err(EmbedError::Exhausted),
            3 => Err(EmbedError::Invalid),
            _ => Err(EmbedError::Failed),
        }
    }

    /// Embed every sequence, packing consecutive ones into one call while their tokens fit
    /// the context and their count its sequences.
    pub fn embed_all(&mut self, tokens: &[i32], lens: &[i32], deadline_ns: u64, out: &mut [f32]) -> Result<(), EmbedError> {
        let d = self.n_embd;
        let (mut s, mut t) = (0usize, 0usize);
        while s < lens.len() {
            let (mut e, mut n) = (s, 0usize);
            while e < lens.len() && e - s < self.max_seqs && n + lens[e].max(0) as usize <= self.max_tokens {
                n += lens[e] as usize;
                e += 1;
            }
            if e == s || t + n > tokens.len() || e * d > out.len() {
                return Err(EmbedError::Invalid);
            }
            self.embed(&tokens[t..t + n], &lens[s..e], deadline_ns, &mut out[s * d..e * d])?;
            (s, t) = (e, t + n);
        }
        Ok(())
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: freed once.
        unsafe { ffi::scz_gguf_ctx_free(self.p) }
    }
}

/// Scale `v` to unit length, accumulating in f64 as llama.cpp's own tools do. A zero vector
/// stays zero.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|&x| x as f64 * x as f64).sum::<f64>().sqrt();
    if norm > 0.0 {
        let k = (1.0 / norm) as f32;
        v.iter_mut().for_each(|x| *x *= k);
    }
}

/// Why a request row was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowError {
    /// Its first id is negative: no tokens.
    Empty,
    /// It holds this id, which is not in the vocabulary.
    OutOfRange(i32),
}

/// The sequence a request row holds, by the input convention: its ids before the first
/// negative one. Refused when that is empty or holds an id outside the vocabulary.
pub fn sequence(row: &[i32], n_vocab: u32) -> Result<&[i32], RowError> {
    let n = row.iter().position(|&t| t < 0).unwrap_or(row.len());
    if n == 0 {
        return Err(RowError::Empty);
    }
    match row[..n].iter().find(|&&t| t as u32 >= n_vocab) {
        Some(&bad) => Err(RowError::OutOfRange(bad)),
        None => Ok(&row[..n]),
    }
}

/// The worker's own arguments (the table in the crate documentation).
#[derive(Clone, Debug)]
pub struct Options {
    pub max_tokens: Option<u32>,
    pub threads: u32,
    pub normalize: bool,
    pub max_infer_ns: Option<u64>,
    pub repack: bool,
    pub log: LogLevel,
}

impl Default for Options {
    fn default() -> Self {
        Options { max_tokens: None, threads: 2, normalize: true, max_infer_ns: None, repack: true, log: LogLevel::Warn }
    }
}

/// Rows of one request in one llama.cpp call, at most.
pub const PACK_SEQS: u32 = 16;
/// `--max-tokens` when it is not given, if the model was trained on longer sequences.
pub const DEFAULT_MAX_TOKENS: u32 = 128;

impl Options {
    pub fn parse(args: &[String]) -> Result<Options, String> {
        let mut o = Options::default();
        for a in args {
            let (k, v) = a.split_once('=').unwrap_or((a.as_str(), ""));
            let num = |lo: u64, hi: u64| v.parse::<u64>().ok().filter(|n| (lo..=hi).contains(n)).ok_or(format!("{k}: {v:?} is not a number from {lo} to {hi}"));
            match k {
                "--max-tokens" => o.max_tokens = Some(num(1, 1 << 20)? as u32),
                "--threads" => o.threads = num(1, 64)? as u32,
                "--max-infer-ms" => o.max_infer_ns = Some(num(1, 3_600_000)? * 1_000_000),
                "--normalize" => {
                    o.normalize = match v {
                        "l2" => true,
                        "none" => false,
                        _ => return Err(format!("{k}: {v:?} is not l2 or none")),
                    }
                }
                "--repack" => {
                    o.repack = match v {
                        "on" => true,
                        "off" => false,
                        _ => return Err(format!("{k}: {v:?} is not on or off")),
                    }
                }
                "--log" => {
                    o.log = match v {
                        "info" => LogLevel::Info,
                        "warn" => LogLevel::Warn,
                        "error" => LogLevel::Error,
                        "none" => LogLevel::None,
                        _ => return Err(format!("{k}: {v:?} is not info, warn, error, or none")),
                    }
                }
                _ => return Err(format!("unknown argument {a}")),
            }
        }
        Ok(o)
    }
}

struct Loaded {
    model: Model,
    /// Tokens per row, and floats per embedding.
    l: usize,
    d: usize,
    seqs: u32,
    sig: Signature,
}

/// Per consume thread: the private copy of the request's ids, the sequences found in it, and
/// the embeddings before they are written out.
#[derive(Default)]
struct Scratch {
    raw: Vec<u8>,
    ids: Vec<i32>,
    tokens: Vec<i32>,
    lens: Vec<i32>,
    out: Vec<f32>,
    bytes: Vec<u8>,
}

thread_local! {
    static CTX: RefCell<Option<Context>> = const { RefCell::new(None) };
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

pub struct GgufBackend {
    opts: Options,
    loaded: OnceLock<Loaded>,
    /// How long one inference may run before it is abandoned.
    cap_ns: AtomicU64,
}

fn log(m: std::fmt::Arguments<'_>) {
    eprintln!("sidecarzero-worker-gguf[{}]: {m}", std::process::id());
}

fn fail(m: String) -> ! {
    log(format_args!("{m}"));
    std::process::exit(4)
}

/// Requests refused or failed so far, of which the first few are logged.
static LOGGED: AtomicU32 = AtomicU32::new(0);

fn log_some(m: std::fmt::Arguments<'_>) {
    if LOGGED.fetch_add(1, Relaxed) < 5 {
        log(m);
    }
}

impl GgufBackend {
    pub fn new(opts: Options) -> GgufBackend {
        let cap = opts.max_infer_ns.unwrap_or(scz_shm::abi::defaults::MAX_DEADLINE_NS);
        GgufBackend { opts, loaded: OnceLock::new(), cap_ns: AtomicU64::new(cap) }
    }
    fn get(&self) -> &Loaded {
        self.loaded.get().expect("load runs before anything else")
    }
    /// Run `f` with this thread's context, made on first use.
    fn with_context<R>(&self, f: impl FnOnce(&mut Context) -> R) -> Result<R, String> {
        let l = self.get();
        CTX.with(|c| {
            let mut c = c.borrow_mut();
            if c.is_none() {
                *c = Some(Context::new(&l.model, l.l as u32, l.seqs, self.opts.threads)?);
            }
            Ok(f(c.as_mut().expect("made above")))
        })
    }
}

/// glibc's mmap threshold, fixed before the model loads (worker §5): buffers of a few hundred
/// kilobytes and more are then mapped and unmapped, never left in a thread arena that
/// `malloc_trim` cannot shrink.
fn tune_allocator() {
    // SAFETY: mallopt has no memory-safety preconditions.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
    }
}

impl Backend for GgufBackend {
    fn load(&self, _gen: u32, model: &'static [u8]) {
        if model.get(..4) != Some(b"GGUF".as_slice()) {
            fail("the model file is not GGUF (it does not start with the GGUF magic)".into());
        }
        let path = scz_worker_core::model_path().unwrap_or_else(|| fail("no model descriptor".into()));
        // llama.cpp would say only that it failed to open the file; say why.
        if let Err(e) = std::fs::File::open(&path) {
            fail(format!(
                "opening the model at {path}: {e}. Under Landlock the worker may open it only when the model file's directory is in its view (--worker-read=DIR)"
            ));
        }
        init(self.opts.log);
        tune_allocator();
        let m = Model::load(&path, self.opts.repack).unwrap_or_else(|e| fail(format!("loading the model: {e}")));
        let i = m.info.clone();
        if !matches!(i.pooling, Pooling::Mean | Pooling::Cls | Pooling::Last) {
            fail(format!("the model's pooling is {:?}: this backend serves one pooled embedding per sequence (mean, CLS, or last)", i.pooling));
        }
        if i.n_embd == 0 || i.n_vocab == 0 || i.n_ctx_train == 0 {
            fail(format!("the model reports a vocabulary of {}, embeddings of {}, a context of {}", i.n_vocab, i.n_embd, i.n_ctx_train));
        }
        let l = self.opts.max_tokens.unwrap_or(i.n_ctx_train.min(DEFAULT_MAX_TOKENS));
        if l > i.n_ctx_train {
            fail(format!("--max-tokens={l} is more than the {} tokens the model was trained on", i.n_ctx_train));
        }
        let seqs = if i.memoryless { PACK_SEQS.min(l) } else { 1 };
        let sig = Signature {
            schema: Schema { request: vec![Column::new("tokens", et::I32, l)], response: vec![Column::new("embedding", et::F32, i.n_embd)] },
            fixed_batch: None,
        };
        log(format_args!(
            "{}: vocabulary {}, embedding {} ({:?} pooling{}), {l} tokens per row, {} thread(s) per consume thread",
            i.desc,
            i.n_vocab,
            i.n_embd,
            i.pooling,
            if self.opts.normalize { ", L2-normalized" } else { "" },
            self.opts.threads
        ));
        let _ = self.loaded.set(Loaded { model: m, l: l as usize, d: i.n_embd as usize, seqs, sig });
    }

    fn signature(&self) -> Option<Signature> {
        Some(self.get().sig.clone())
    }

    /// One full-length row, and if `rows` allows, one call packed with as many short rows as a
    /// call takes. A call never holds more than `L` tokens and llama.cpp reserves its compute
    /// memory for a full call when the context is made, so more rows would only repeat these.
    fn warm(&self, rows: u32) {
        let l = self.get();
        let n_vocab = l.model.info.n_vocab as i32;
        let full: Vec<i32> = (0..l.l as i32).map(|i| i % n_vocab).collect();
        let short = (rows.min(l.seqs) as usize).max(1);
        let lens = vec![(l.l / short) as i32; short];
        let mut out = vec![0f32; short * l.d];
        let r = self.with_context(|c| {
            c.embed_all(&full, &[l.l as i32], u64::MAX, &mut out)?;
            if rows > 1 {
                c.embed_all(&full[..lens.iter().sum::<i32>() as usize], &lens, u64::MAX, &mut out)?;
            }
            Ok::<(), EmbedError>(())
        });
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => fail(format!("warming up: {e:?}")),
            Err(e) => fail(format!("making a context: {e}")),
        }
    }

    fn infer_batch(&self, input: &BatchView<'_>, output: &mut BatchSink<'_>) -> u32 {
        let start = monotonic_ns();
        let l = self.get();
        let rows = input.rows() as usize;
        // The abort deadline: see "Cancellation" in the crate documentation.
        let deadline = start.saturating_add(self.cap_ns.load(Relaxed)).min(input.deadline_ns());
        SCRATCH.with(|s| {
            let s = &mut *s.borrow_mut();
            // Copy the ids out of slot memory before reading any of them (worker §7.1).
            s.raw.resize(rows * l.l * 4, 0);
            input.copy_column(0, &mut s.raw);
            s.ids.clear();
            s.ids.extend(s.raw.as_chunks::<4>().0.iter().map(|b| i32::from_le_bytes(*b)));
            s.tokens.clear();
            s.lens.clear();
            for (r, row) in s.ids.chunks_exact(l.l).enumerate() {
                match sequence(row, l.model.info.n_vocab) {
                    Ok(seq) => {
                        s.tokens.extend_from_slice(seq);
                        s.lens.push(seq.len() as i32);
                    }
                    Err(e) => {
                        log_some(format_args!("a request refused: row {r}: {e:?} (a vocabulary of {})", l.model.info.n_vocab));
                        return st::BAD_REQUEST;
                    }
                }
            }
            s.out.resize(rows * l.d, 0.0);
            let r = self.with_context(|c| c.embed_all(&s.tokens, &s.lens, deadline, &mut s.out));
            match r {
                Ok(Ok(())) => {}
                Ok(Err(EmbedError::Aborted)) => return st::DEADLINE_EXCEEDED,
                Ok(Err(EmbedError::Exhausted)) => return st::RESOURCE_EXHAUSTED,
                Ok(Err(e)) => {
                    log_some(format_args!("llama.cpp failed a request: {e:?}"));
                    return st::MODEL_ERROR;
                }
                Err(e) => {
                    log_some(format_args!("making a context: {e}"));
                    return st::RESOURCE_EXHAUSTED;
                }
            }
            if self.opts.normalize {
                s.out.chunks_exact_mut(l.d).for_each(l2_normalize);
            }
            s.bytes.clear();
            s.bytes.extend(s.out.iter().flat_map(|v| v.to_le_bytes()));
            output.write_column(0, &s.bytes);
            st::OK
        })
    }

    /// The ids are copied in and the embeddings out, by design (the crate documentation); that
    /// path is counted as `copies`, as the ONNX Runtime backend's padding path is.
    fn copied(&self) -> bool {
        true
    }

    fn trim(&self) {
        // SAFETY: no preconditions.
        unsafe {
            libc::malloc_trim(0);
        }
    }

    /// The default abort deadline is the arena's maximum deadline, known once the worker is
    /// active.
    fn attach(&'static self, _data: Seg<'static>, dir: &'static Directory) {
        if self.opts.max_infer_ns.is_none() && dir.max_deadline_ns > 0 {
            self.cap_ns.store(dir.max_deadline_ns, Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_ends_at_its_first_negative_id_and_holds_only_vocabulary_ids() {
        assert_eq!(sequence(&[101, 7, 102, -1, -1], 30522), Ok(&[101, 7, 102][..]));
        assert_eq!(sequence(&[101, 7, 102], 30522), Ok(&[101, 7, 102][..]), "a full row");
        assert_eq!(sequence(&[101, -1, 5000000], 30522), Ok(&[101][..]), "after the end, anything");
        assert_eq!(sequence(&[-1, 101], 30522), Err(RowError::Empty));
        assert_eq!(sequence(&[101, 30522, 102, -1], 30522), Err(RowError::OutOfRange(30522)));
        assert_eq!(sequence(&[i32::MAX], 30522), Err(RowError::OutOfRange(i32::MAX)));
    }

    #[test]
    fn options_parse() {
        let o = Options::parse(&["--max-tokens=64", "--threads=1", "--normalize=none", "--max-infer-ms=50", "--repack=off"].map(String::from)).unwrap();
        assert_eq!((o.max_tokens, o.threads, o.normalize, o.max_infer_ns, o.repack), (Some(64), 1, false, Some(50_000_000), false));
        assert!(Options::parse(&["--threads=0".to_string()]).is_err());
        assert!(Options::parse(&["--bogus".to_string()]).is_err());
        let d = Options::parse(&[]).unwrap();
        assert_eq!((d.max_tokens, d.threads, d.normalize, d.repack), (None, 2, true, true));
    }

    #[test]
    fn normalizing_makes_unit_vectors() {
        let mut v = [3.0f32, 4.0];
        l2_normalize(&mut v);
        assert_eq!(v, [0.6, 0.8]);
        let mut z = [0f32; 3];
        l2_normalize(&mut z);
        assert_eq!(z, [0.0; 3]);
    }
}
