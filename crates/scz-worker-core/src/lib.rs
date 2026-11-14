//! scz-worker-core: a worker's startup, heartbeat, control requests, and consume loop around
//! a backend (worker §2–§4).
//!
//! Startup (worker §3): map and check the arena, tidy the inherited descriptors, write the
//! beat slot and start the heartbeat, map the model and load it, render and stage the
//! registry, warm up on every consume thread, take one death lock per consume thread, report
//! WARM, wait to be activated, read the published registry epoch, report ACTIVE, and consume
//! one ring per thread (worker §4), obeying DRAIN and HANDOVER at the top of each loop.
//!
//! A backend either declares a [`Signature`], and is then served Arrow frames laid out by the
//! registry's templates (the real path), or declares none and is served opaque bytes (the echo
//! and chaos workers, which stage an empty registry).

mod batch;
pub mod render;

pub use batch::{BatchSink, BatchView};
pub use render::Signature;

use scz_shm::abi::{self, acc, beat_state, ctl_bit, direction, status as st, status_byte};
use scz_shm::arena::{MappedArena, Mapping};
use scz_shm::layout::Directory;
use scz_shm::registry::{self, ColumnLoc, Registry, Template};
use scz_shm::ring::{self, Descriptor, RingMem};
use scz_shm::seg::{Seg, ShmSlot};
use scz_shm::slot::{self, Claim};
use scz_shm::standby::{ShmStandby, StandbyMem};
use scz_shm::view::{SlotReader, SlotWriter};
use scz_shm::wait::{futex_wait, monotonic_ns};
use scz_shm::{deathlock, sys, word};
use std::cell::RefCell;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering::*};
use std::sync::{Once, OnceLock};

macro_rules! log {
    ($($t:tt)*) => { eprintln!("scz-worker[{}]: {}", std::process::id(), format_args!($($t)*)) };
}

/// What a worker computes (worker §2).
pub trait Backend: Sync {
    /// Load the model and get ready to warm up (worker §3 step 5). `model` is the mapped model
    /// file the supervisor opened and hashed; it stays mapped for the life of the process.
    fn load(&self, _gen: u32, _model: &'static [u8]) {}
    /// The model's columns (step 6). None: this backend is served opaque bytes through
    /// [`Backend::infer`], and the worker stages an empty registry.
    fn signature(&self) -> Option<Signature> {
        None
    }
    /// Run the model at `rows` rows on private buffers (step 7). Called on every consume
    /// thread, at one row and at the most rows any template allows, before WARM.
    fn warm(&self, _rows: u32) {}
    /// Opaque bytes in, opaque bytes out, for backends without a signature.
    fn infer(&self, _req: &SlotReader<'_>, _resp: &mut SlotWriter<'_>) -> u32 {
        st::INTERNAL
    }
    /// One frame in, one frame out (worker §4 step 5): read `input`'s columns, write `output`'s,
    /// in place, and return the status to complete with.
    fn infer_batch(&self, _input: &BatchView<'_>, _output: &mut BatchSink<'_>) -> u32 {
        st::INTERNAL
    }
    /// Whether the last `infer_batch` on this thread read its input and wrote its output in
    /// slot memory, as the zero-copy path must (arrow-data-plane §11). Counted when it did not.
    fn in_place(&self) -> bool {
        true
    }
    /// Whether the last `infer_batch` on this thread copied the request or the response, as
    /// padding to a fixed batch does (worker §4). Counted.
    fn copied(&self) -> bool {
        false
    }
    /// Release what memory can be released (`TRIM`, worker §5).
    fn trim(&self) {}
    /// Called with the status and response length about to be published, before the slot
    /// record is written and the slot completed. Only the chaos worker uses it.
    fn finish(&self, _status: &mut u32, _resp_len: &mut usize) {}
    /// Called after the slot is completed and before its parked client is woken. Only the
    /// chaos worker uses it, to stall the wake.
    fn before_wake(&self) {}
    /// Called once the worker is active, with the data segment and its geometry. Only the chaos
    /// worker uses it: its scribbler writes over the segment as a hostile worker would.
    fn attach(&'static self, _data: Seg<'static>, _dir: &'static Directory) {}
}

static MODEL_FD: AtomicI32 = AtomicI32::new(-1);

/// A path that opens the model the supervisor hashed, for a framework that loads only from a
/// path (llama.cpp, a Python runner's `load(model_path)`): `/proc/self/fd/N` of the inherited
/// model descriptor (worker §7.3). None before [`run`] has adopted it. Landlock checks the
/// reopen against the model file's own path, not `/proc`: the worker's view must hold the model
/// file, or its directory if reloads put new files there (`--worker-read=PATH`). Measured on
/// kernel 6.12; `--worker-read=/proc` does not admit it.
pub fn model_path() -> Option<String> {
    let fd = MODEL_FD.load(Relaxed);
    (fd >= 0).then(|| format!("/proc/self/fd/{fd}"))
}

/// The command line of supervisor §5.1.
#[derive(Debug)]
pub struct Args {
    pub ctl_fd: i32,
    pub data_fd: i32,
    pub model_fd: i32,
    pub status_fd: i32,
    pub wrk_fd: i32,
    pub gen: u32,
    pub beat_slot: u32,
    /// Maximum total rows to coalesce opportunistically across already-queued descriptors (worker §4).
    /// 0 disables coalescing.
    pub max_batch_rows: u32,
}

pub fn parse_args(it: impl Iterator<Item = String>) -> Result<Args, String> {
    let (args, rest) = parse_known(it)?;
    match rest.first() {
        None => Ok(args),
        Some(a) => Err(format!("unknown argument {a}")),
    }
}

/// The supervisor's arguments, and the others in order: a worker's own, which the supervisor
/// puts first (`--worker-arg`, supervisor §5.1).
pub fn parse_known(it: impl Iterator<Item = String>) -> Result<(Args, Vec<String>), String> {
    let mut v = [None; 7];
    let mut max_batch_rows = std::env::var("SCZ_MAX_BATCH_ROWS").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let mut rest = Vec::new();
    const KEYS: [&str; 7] = ["--ctl-fd", "--data-fd", "--model-fd", "--status-fd", "--wrk-fd", "--gen", "--beat-slot"];
    for a in it {
        if let Some(val) = a.strip_prefix("--max-batch-rows=") {
            max_batch_rows = val.parse::<u32>().map_err(|_| "--max-batch-rows: not a number".to_string())?;
            continue;
        }
        let Some(i) = a.split_once('=').and_then(|(k, _)| KEYS.iter().position(|&x| x == k)) else {
            rest.push(a);
            continue;
        };
        let val = &a[KEYS[i].len() + 1..];
        v[i] = Some(val.parse::<i64>().map_err(|_| format!("{}: not a number", KEYS[i]))?);
    }
    let get = |i: usize| v[i].ok_or_else(|| format!("{} is required", KEYS[i]));
    let beat_slot = get(6)? as u32;
    if beat_slot >= abi::BEAT_SLOTS as u32 {
        return Err("--beat-slot must be 0 or 1".into());
    }
    let args = Args {
        ctl_fd: get(0)? as i32,
        data_fd: get(1)? as i32,
        model_fd: get(2)? as i32,
        status_fd: get(3)? as i32,
        wrk_fd: get(4)? as i32,
        gen: get(5)? as u32,
        beat_slot,
        max_batch_rows,
    };
    Ok((args, rest))
}

/// Everything the threads share. Leaked: it lives as long as the process.
struct Worker<B> {
    arena: MappedArena,
    wrk: Mapping,
    status: OwnedFd,
    _model: OwnedFd,
    model: &'static [u8],
    /// What this worker rendered and staged; None in raw mode.
    registry: OnceLock<Option<Registry>>,
    /// The registry epoch published with this worker's model, read once at activation.
    epoch: AtomicU32,
    activated: Once,
    gen: u32,
    beat_slot: u32,
    rings: usize,
    max_batch_rows: u32,
    variable_batch: AtomicBool,
    backend: B,
    /// Consume threads paused by DRAIN, and stopped for good by HANDOVER.
    paused: AtomicUsize,
    stopped: AtomicUsize,
}

impl<B> Worker<B> {
    fn ctl(&self) -> Seg<'_> {
        self.arena.ctl.seg()
    }
    fn data(&self) -> Seg<'_> {
        self.arena.data.seg()
    }
    fn dir(&self) -> &Directory {
        &self.arena.dir
    }
    fn beat(&self) -> usize {
        self.dir().beat(self.beat_slot as usize)
    }
    fn stats(&self) -> usize {
        self.dir().status_off as usize + abi::status_page::worker_stats::OFF + self.beat_slot as usize * abi::status_page::worker_stats::STRIDE
    }
    fn registry(&self) -> Option<&Registry> {
        self.registry.get().and_then(Option::as_ref)
    }
    /// The registry epoch this worker serves, read from the model block once its generation is
    /// the gate's (worker §4 step 4 compares every request's stamp with it). The supervisor
    /// publishes the block before it activates a generation, and publishes exactly the bytes
    /// this worker staged: anything else means this worker must not serve.
    fn activate(&self) {
        self.activated.call_once(|| {
            let Some(reg) = self.registry() else { return };
            let Some((block, bytes)) = scz_shm::model::read(&self.ctl(), 10_000) else { die("model block", "unreadable") };
            if bytes != reg.to_bytes() {
                die("activation", "the published registry is not the one this worker staged");
            }
            self.epoch.store(block.registry_epoch, Relaxed);
        });
    }
    fn ctl_req(&self) -> u32 {
        acc::control_header::ctl_req(&self.ctl(), 0)
    }
    fn ctl_ack(&self) -> u32 {
        acc::beat::ctl_ack(&self.data(), self.beat())
    }
    fn current(&self) -> bool {
        word::gate_gen(acc::control_header::gate(&self.ctl(), 0)) == self.gen
    }
    fn report(&self, state: u32, byte: u32) {
        acc::beat::set_state(&self.data(), self.beat(), state);
        if let Err(e) = sys::write_byte(self.status.as_fd(), byte as u8) {
            log!("status pipe: {e}; the supervisor has gone");
            std::process::exit(1);
        }
    }
    /// Sleep on standby_wait until `done` holds (signaling §3.3): the event counter is read
    /// before the condition, so a change between the two is never missed.
    fn wait_standby(&self, done: impl Fn() -> bool) {
        let ctl = self.ctl();
        let m = ShmStandby(ctl);
        loop {
            let v = m.standby_wait().load(Acquire);
            if done() {
                return;
            }
            futex_wait(m.standby_wait(), v, None);
        }
    }
}

/// Map the model file read-only and shared (worker §7.1: the mapping outlives any session the
/// backend makes from it). An empty file maps to an empty slice.
fn map_model(fd: &OwnedFd) -> std::io::Result<&'static [u8]> {
    use std::os::fd::AsRawFd;
    // SAFETY: fstat into a zeroed struct.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = st.st_size as usize;
    if len == 0 || st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Ok(&[]);
    }
    // SAFETY: a new read-only mapping of a regular file, never unmapped.
    let p = unsafe { libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if p == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the mapping is `len` bytes and lives as long as the process. A served model
    // file is never modified (supervisor §5.1), so the bytes do not change under the slice.
    Ok(unsafe { core::slice::from_raw_parts(p as *const u8, len) })
}

fn die(what: &str, e: impl std::fmt::Display) -> ! {
    log!("{what}: {e}");
    std::process::exit(3);
}

/// Run a worker around `backend`, with the command line of supervisor §5.1. Never returns.
pub fn run<B: Backend + Send + 'static>(backend: B) -> ! {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        log!("{e}");
        std::process::exit(2)
    });
    run_with(args, backend)
}

/// Run a worker around `backend` with arguments already parsed: for a worker whose command
/// line carries its own arguments before the supervisor's (`--worker-arg`), such as a runner
/// embedded in an interpreter. Never returns.
pub fn run_with<B: Backend + Send + 'static>(args: Args, backend: B) -> ! {
    #[cfg(feature = "crashpoints")]
    scz_shm::crash::GEN.store(args.gen, Relaxed);
    // Step 3, first half: own every inherited descriptor and make it close-on-exec.
    let ctl_fd = sys::adopt_fd(args.ctl_fd).unwrap_or_else(|e| die("control fd", e));
    let data_fd = sys::adopt_fd(args.data_fd).unwrap_or_else(|e| die("data fd", e));
    let model = sys::adopt_fd(args.model_fd).unwrap_or_else(|e| die("model fd", e));
    let status = sys::adopt_fd(args.status_fd).unwrap_or_else(|e| die("status fd", e));
    let wrk_fd = sys::adopt_fd(args.wrk_fd).unwrap_or_else(|e| die("worker segment fd", e));
    // Step 2, and the populate of step 3.
    let arena = MappedArena::open(ctl_fd.as_fd(), data_fd.as_fd(), None).unwrap_or_else(|e| die("arena", e));
    let wrk = Mapping::map(wrk_fd.as_fd(), abi::WORKER_SEGMENT_LEN as usize, true).unwrap_or_else(|e| die("worker segment", e));
    drop((ctl_fd, data_fd, wrk_fd)); // the mappings stay; the memfd descriptors go
    let rings = arena.dir.layout.rings.len().min(deathlock::MAX);
    let model_bytes = map_model(&model).unwrap_or_else(|e| die("model", e));
    MODEL_FD.store(std::os::fd::AsRawFd::as_raw_fd(&model), Relaxed);

    let w: &'static Worker<B> = Box::leak(Box::new(Worker {
        arena,
        wrk,
        status,
        _model: model,
        model: model_bytes,
        registry: OnceLock::new(),
        epoch: AtomicU32::new(0),
        activated: Once::new(),
        gen: args.gen,
        beat_slot: args.beat_slot,
        rings,
        max_batch_rows: args.max_batch_rows,
        variable_batch: AtomicBool::new(false),
        backend,
        paused: AtomicUsize::new(0),
        stopped: AtomicUsize::new(0),
    }));
    // Step 4.
    let (data, beat) = (w.data(), w.beat());
    acc::beat::set_pid(&data, beat, std::process::id());
    acc::beat::set_gen(&data, beat, w.gen);
    acc::beat::set_ctl_ack(&data, beat, 0);
    acc::beat::set_hb_mono_ns(&data, beat, monotonic_ns());
    acc::beat::set_state(&data, beat, beat_state::LOADING);
    scz_shm::next::fill_worker(&data, w.arena.dir.status_off as usize, w.beat_slot as usize);
    std::thread::Builder::new().name("scz-heartbeat".into()).spawn(move || heartbeat(w)).unwrap_or_else(|e| die("heartbeat thread", e));
    // Steps 5–7: load and warm up; then each consume thread takes its death lock.
    // Step 5.
    w.backend.load(w.gen, w.model);
    // Step 6: render, verify, and stage the registry; empty in raw mode.
    let mut variable_batch = false;
    let reg = w.backend.signature().map(|sig| {
        variable_batch = sig.fixed_batch.is_none();
        let caps: Vec<(u32, u32)> = w.dir().layout.slabs.iter().map(|s| (s.2.req_cap, s.2.resp_cap)).collect();
        render::render(&sig, &caps).unwrap_or_else(|e| die("templates", e))
    });
    w.variable_batch.store(variable_batch, Relaxed);
    let staged = reg.as_ref().map(Registry::to_bytes).unwrap_or_default();
    if staged.len() as u64 > w.dir().layout.staging_len {
        die("templates", format!("a registry of {} bytes, more than the {} byte staging area", staged.len(), w.dir().layout.staging_len));
    }
    data.copy_in(w.dir().layout.staging_off as usize + w.beat_slot as usize * w.dir().layout.staging_len as usize, &staged);
    acc::beat::set_staged_len(&data, beat, staged.len() as u32);
    let warm_rows = reg.as_ref().map(|r| r.templates.iter().map(|t| t.max_rows).max().unwrap_or(1));
    let _ = w.registry.set(reg);
    for i in 0..rings {
        deathlock::init(&w.wrk.seg(), i).unwrap_or_else(|e| die("death lock", e));
    }
    let locked = std::sync::Arc::new(std::sync::Barrier::new(rings + 1));
    let threads: Vec<_> = (0..rings)
        .map(|i| {
            let locked = locked.clone();
            std::thread::Builder::new()
                .name(format!("scz-consume-{i}"))
                .spawn(move || {
                    // Step 7: warm up where real requests will run, then take the death lock.
                    if let Some(max) = warm_rows {
                        w.backend.warm(1);
                        w.backend.warm(max);
                    }
                    deathlock::take(&w.wrk.seg(), i).unwrap_or_else(|e| die("death lock", e));
                    locked.wait();
                    // Step 8, each thread: wait until the gate carries this generation.
                    w.wait_standby(|| w.current());
                    w.activate();
                    if i == 0 {
                        w.report(beat_state::ACTIVE, status_byte::ACTIVE); // step 9
                        log!("gen {} active on {} ring(s)", w.gen, w.rings);
                        w.backend.attach(w.data(), w.dir());
                    }
                    scz_shm::crash_point!("worker.activated");
                    consume(w, i);
                })
                .unwrap_or_else(|e| die("consume thread", e))
        })
        .collect();
    locked.wait();
    w.report(beat_state::WARM, status_byte::WARM); // step 8
    for t in threads {
        let _ = t.join();
    }
    // Every consume thread returns only when the gate names another generation.
    log!("gen {} superseded; exiting", w.gen);
    std::process::exit(0);
}

/// The heartbeat thread: beat, and apply the waker rule to every ring that has work or a
/// pending control request (the backstop of signaling §3.1). It never acts on a request.
fn heartbeat<B>(w: &Worker<B>) {
    let interval = std::time::Duration::from_nanos(w.dir().hb_interval_ns);
    loop {
        let data = w.data();
        acc::beat::set_hb_mono_ns(&data, w.beat(), monotonic_ns());
        if w.current() {
            let pending = w.ctl_req() != w.ctl_ack();
            for i in 0..w.rings {
                let r = w.dir().ring(&data, i);
                if pending || ring::head_published(&r, r.head().load(Acquire)) {
                    ring::wake_if_parked(&r);
                }
            }
        }
        std::thread::sleep(interval);
    }
}

/// One consume thread (worker §4). Control requests are handled at the top of the loop, where
/// the thread holds no slot.
fn consume<B: Backend>(w: &Worker<B>, i: usize) {
    let (ctl, data) = (w.ctl(), w.data());
    let r = w.dir().ring(&data, i);
    let mut head = r.head().load(Acquire);
    let owner = w.beat_slot + 1;
    let spin_ns = w.dir().spin_ns;
    let beat = w.beat();
    let pending = || acc::control_header::ctl_req(&ctl, 0) != acc::beat::ctl_ack(&data, beat);
    let mut pace = Pace::default();
    loop {
        if !w.current() {
            return; // defensive: the supervisor changes it only after this worker has gone
        }
        let req = w.ctl_req();
        if req & ctl_bit::HANDOVER != 0 {
            handover(w, i); // never returns
        }
        if req & ctl_bit::DRAIN != 0 {
            drain(w, i);
            head = r.head().load(Acquire); // resume where the header says
            continue;
        }
        if i == 0 {
            control(w, req);
        }
        if let Some(d) = ring::dequeue(&r, &mut head) {
            if w.max_batch_rows > 1 && w.variable_batch.load(Relaxed) && ring::head_published(&r, head) {
                serve_opportunistic_batch(w, &data, &r, &mut head, d, owner, &mut pace);
            } else {
                serve(w, &data, &d, owner, &mut pace);
            }
            continue;
        }
        // Spin, then park (signaling §3.1).
        let until = monotonic_ns() + spin_ns;
        let mut arrived = false;
        while monotonic_ns() < until {
            if ring::head_published(&r, head) || pending() {
                arrived = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !arrived {
            ring::park(&r, head, pending);
        }
    }
}

/// The control thread's part (ring 0): TRIM, and acknowledging bits the supervisor cleared.
fn control<B: Backend>(w: &Worker<B>, req: u32) {
    let (data, beat) = (w.data(), w.beat());
    let ack = w.ctl_ack();
    if req & ctl_bit::TRIM != 0 && ack & ctl_bit::TRIM == 0 {
        w.backend.trim();
        acc::beat::set_ctl_ack(&data, beat, ack | ctl_bit::TRIM);
    } else if ack & !req != 0 {
        acc::beat::set_ctl_ack(&data, beat, ack & req);
    }
}

/// DRAIN: every consume thread stops dequeuing; once all have, the control thread
/// acknowledges and reports DRAINED; all park on standby_wait until the bit clears.
fn drain<B: Backend>(w: &Worker<B>, i: usize) {
    w.paused.fetch_add(1, AcqRel);
    if i == 0 {
        while w.paused.load(Acquire) < w.rings {
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        let (data, beat) = (w.data(), w.beat());
        acc::beat::set_ctl_ack(&data, beat, w.ctl_ack() | ctl_bit::DRAIN);
        w.report(beat_state::DRAINING, status_byte::DRAINED);
        scz_shm::crash_point!("worker.drained");
    }
    w.wait_standby(|| w.ctl_req() & ctl_bit::DRAIN == 0 || !w.current());
    if i == 0 {
        let (data, beat) = (w.data(), w.beat());
        acc::beat::set_ctl_ack(&data, beat, w.ctl_ack() & !ctl_bit::DRAIN);
        acc::beat::set_state(&data, beat, beat_state::ACTIVE);
    }
    w.paused.fetch_sub(1, AcqRel);
}

/// HANDOVER: every consume thread stops dequeuing for good; once all have, the control thread
/// reports RELEASED and the process exits. Nothing touches a ring or a slot after that.
fn handover<B: Backend>(w: &Worker<B>, i: usize) -> ! {
    w.stopped.fetch_add(1, AcqRel);
    if i == 0 {
        while w.stopped.load(Acquire) < w.rings {
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        scz_shm::crash_point!("worker.handover.stopped");
        w.report(beat_state::RELEASED, status_byte::RELEASED);
        scz_shm::crash_point!("worker.handover.released");
        log!("gen {} released its rings; exiting", w.gen);
        std::process::exit(0);
    }
    loop {
        std::thread::park();
    }
}

/// A consume thread's moving average of inference time per row: the `expected_run` of worker
/// §4 step 3.
///
/// It halves for every `PACE_HALF_LIFE_NS` without an inference. A request that is dropped is
/// never timed, so without the decay one slow inference (a node busy at start-up, a page fault
/// storm) above every caller's budget would make the thread drop every later request, and the
/// estimate would never come down again: the worker would answer nothing until replaced. The
/// Kind tests met exactly that on a freshly made cluster. Under real overload the decay costs
/// one probing inference per half-life.
#[derive(Default)]
struct Pace {
    ns_per_row: u64,
    last_run_ns: u64,
}

const PACE_HALF_LIFE_NS: u64 = 100_000_000;

impl Pace {
    /// The estimate per row now, decayed for the time since the last inference.
    fn per_row(&self, now: u64) -> u64 {
        let halvings = now.saturating_sub(self.last_run_ns) / PACE_HALF_LIFE_NS;
        self.ns_per_row >> halvings.min(63)
    }
    fn expected(&self, rows: u32, now: u64) -> u64 {
        self.per_row(now).saturating_mul(rows as u64)
    }
    /// A new sample folds into the decayed estimate, not the stale one.
    fn record(&mut self, ns: u64, rows: u32, now: u64) {
        let x = ns / rows.max(1) as u64;
        let prior = self.per_row(now);
        self.ns_per_row = if prior == 0 { x } else { (prior * 7 + x) / 8 };
        self.last_run_ns = now;
    }
}

fn complete_slot<B: Backend>(w: &Worker<B>, data: &Seg<'_>, s: &ShmSlot<'_>, d: &Descriptor, status: u32, resp_len: usize) {
    use acc::slot_record as rec;
    let (mut status, mut resp_len) = (status, resp_len);
    w.backend.finish(&mut status, &mut resp_len);
    // 6. Publish.
    rec::set_resp_len(data, s.rec, resp_len as u32);
    rec::set_status(data, s.rec, status);
    rec::set_model_gen(data, s.rec, w.gen);
    rec::set_t_done_ns(data, s.rec, monotonic_ns());
    slot::complete_then(s, d.slot_epoch, || w.backend.before_wake());
    // Shared by every consume thread of this worker: an atomic add.
    data.u64_at(w.beat() + abi::beat::COMPLETED).fetch_add(1, Relaxed);
}

fn serve<B: Backend>(w: &Worker<B>, data: &Seg<'_>, d: &Descriptor, owner: u32, pace: &mut Pace) {
    use acc::slot_record as rec;
    // 0. A void cell: a producer that could not finish.
    if d.is_void() {
        return;
    }
    // 1. Locate, checking bounds before touching slot memory.
    let Some(s) = w.dir().locate(data, d.slab_class, d.slot_index) else { return };
    // 2. Claim.
    if slot::worker_claim(&s, d.slot_epoch, owner) != Claim::Claimed {
        return;
    }
    let t = monotonic_ns();
    rec::set_t_dequeue_ns(data, s.rec, t);
    match w.registry() {
        Some(reg) => serve_frame(w, data, d, &s, reg, t, pace),
        None => {
            let (status, resp_len) = serve_raw(w, data, d, &s, t);
            complete_slot(w, data, &s, d, status, resp_len);
        }
    }
}

/// Opaque bytes: phase 1's path, kept for the echo and chaos workers.
fn serve_raw<B: Backend>(w: &Worker<B>, data: &Seg<'_>, d: &Descriptor, s: &ShmSlot<'_>, t: u64) -> (u32, usize) {
    if t >= d.deadline_ns {
        return (st::DEADLINE_EXCEEDED, 0);
    }
    match SlotReader::request(s, acc::slot_record::req_len(data, s.rec) as usize) {
        None => (st::BAD_REQUEST, 0),
        Some(req) => {
            let mut resp = SlotWriter::response(s);
            let status = w.backend.infer(&req, &mut resp);
            (status, if status == st::OK { resp.len() } else { 0 })
        }
    }
}

struct ClaimedFrame<'a> {
    d: Descriptor,
    slot: ShmSlot<'a>,
    rq: &'a Template,
    rs: &'a Template,
    rows: u32,
    t_deq: u64,
}

fn validate_claimed_frame<'a, B: Backend>(
    w: &'a Worker<B>,
    data: &Seg<'a>,
    d: &Descriptor,
    s: ShmSlot<'a>,
    reg: &'a Registry,
    epoch: u32,
    t: u64,
) -> Option<ClaimedFrame<'a>> {
    use acc::{slot_record as rec, worker_stats as ws};
    let stats = w.stats();
    let class = d.slab_class as usize;
    let (Some(rq), Some(rs)) = (reg.template(class, direction::REQUEST), reg.template(class, direction::RESPONSE)) else {
        ws::add_bad_requests(data, stats, 1);
        complete_slot(w, data, &s, d, st::BAD_REQUEST, 0);
        return None;
    };
    // 4. Validate: the stamp names this registry and this class's template, the length is the
    //    template's frame, and the row count is one the template holds.
    let stamp = rec::req_stamp(data, s.rec);
    if stamp >> 16 != epoch & 0xFFFF {
        ws::add_stale_requests(data, stats, 1);
        complete_slot(w, data, &s, d, st::STALE_REGISTRY, 0);
        return None;
    }
    let rows = registry::rows(data, s.req_off(), rq);
    if stamp & 0xFFFF != rq.id as u32 || rec::req_len(data, s.rec) != rq.frame_cap || rows < 1 || rows > rq.max_rows as u64 {
        ws::add_bad_requests(data, stats, 1);
        complete_slot(w, data, &s, d, st::BAD_REQUEST, 0);
        return None;
    }
    Some(ClaimedFrame { d: *d, slot: s, rq, rs, rows: rows as u32, t_deq: t })
}

fn run_single_claimed<B: Backend>(w: &Worker<B>, data: &Seg<'_>, cf: &ClaimedFrame<'_>, epoch: u32, pace: &mut Pace) {
    use acc::worker_stats as ws;
    let stats = w.stats();
    // 5. Infer over slot memory, into a response region stamped for this template.
    registry::prepare(data, cf.slot.resp_off(), cf.rs, data.u32_at(cf.slot.rec + abi::slot_record::RESP_STAMP), cf.rs.stamp(epoch), cf.rows);
    let input = BatchView { seg: *data, region: cf.slot.req_off(), tmpl: cf.rq, rows: cf.rows, deadline_ns: cf.d.deadline_ns };
    let mut output = BatchSink { seg: *data, region: cf.slot.resp_off(), tmpl: cf.rs, rows: cf.rows };
    let t0 = monotonic_ns();
    let status = w.backend.infer_batch(&input, &mut output);
    let t1 = monotonic_ns();
    let dt = t1 - t0;
    pace.record(dt, cf.rows, t1);
    ws::add_infer_ns(data, stats, dt);
    if !w.backend.in_place() {
        ws::add_zero_copy_misses(data, stats, 1);
    }
    if w.backend.copied() {
        ws::add_copies(data, stats, 1);
    }
    let (code, len) = match status {
        st::OK => {
            ws::add_served(data, stats, 1);
            (st::OK, cf.rs.frame_cap as usize)
        }
        st::MODEL_ERROR | st::RESOURCE_EXHAUSTED => {
            ws::add_model_errors(data, stats, 1);
            (status, 0)
        }
        other => (other, 0),
    };
    complete_slot(w, data, &cf.slot, &cf.d, code, len);
}

/// An Arrow frame laid out by the registry (worker §4 steps 3–6). Everything read from the slot
/// is read once into a local and checked before it is used.
fn serve_frame<B: Backend>(w: &Worker<B>, data: &Seg<'_>, d: &Descriptor, s: &ShmSlot<'_>, reg: &Registry, t: u64, pace: &mut Pace) {
    let epoch = w.epoch.load(Relaxed);
    let Some(cf) = validate_claimed_frame(w, data, d, *s, reg, epoch, t) else { return };
    // 3. Can it still finish before its caller stops waiting?
    if t.saturating_add(pace.expected(cf.rows, t)) >= d.deadline_ns {
        acc::worker_stats::add_deadline_drops(data, w.stats(), 1);
        complete_slot(w, data, &cf.slot, d, st::DEADLINE_EXCEEDED, 0);
        return;
    }
    run_single_claimed(w, data, &cf, epoch, pace);
}

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct ScratchPage([u8; 4096]);

thread_local! {
    static BATCH_SCRATCH: RefCell<Vec<ScratchPage>> = const { RefCell::new(Vec::new()) };
}

fn up64(v: usize) -> usize {
    v.div_ceil(64) * 64
}

fn run_claimed_batch<B: Backend>(w: &Worker<B>, data: &Seg<'_>, batch: &[ClaimedFrame<'_>], total_rows: u32, epoch: u32, pace: &mut Pace) {
    if batch.is_empty() {
        return;
    }
    if batch.len() == 1 {
        run_single_claimed(w, data, &batch[0], epoch, pace);
        return;
    }
    use acc::worker_stats as ws;
    let stats = w.stats();
    let rq = batch[0].rq;
    let rs = batch[0].rs;

    let mut off = 0usize;
    let mut in_cols = Vec::with_capacity(rq.columns.len());
    for c in &rq.columns {
        in_cols.push(ColumnLoc { column: c.column.clone(), values_off: off as u32, row_bytes: c.row_bytes });
        off += up64(total_rows as usize * c.row_bytes as usize);
    }
    let mut out_off = up64(off);
    let mut out_cols = Vec::with_capacity(rs.columns.len());
    for c in &rs.columns {
        out_cols.push(ColumnLoc { column: c.column.clone(), values_off: out_off as u32, row_bytes: c.row_bytes });
        out_off += up64(total_rows as usize * c.row_bytes as usize);
    }
    let coal_rq =
        Template { columns: in_cols, max_rows: total_rows, prefix: vec![], body_off: 0, frame_cap: 0, rows_field_off: 0, patches: vec![], ..rq.clone() };
    let coal_rs =
        Template { columns: out_cols, max_rows: total_rows, prefix: vec![], body_off: 0, frame_cap: 0, rows_field_off: 0, patches: vec![], ..rs.clone() };
    let need_pages = out_off.div_ceil(4096).max(1);
    let min_deadline = batch.iter().map(|cf| cf.d.deadline_ns).min().unwrap_or(0);

    let (status, dt, t1) = BATCH_SCRATCH.with(|cell| {
        let mut pages = cell.borrow_mut();
        if pages.len() < need_pages {
            pages.resize_with(need_pages, || ScratchPage([0u8; 4096]));
        }
        let seg_len = pages.len() * 4096;
        // SAFETY: ScratchPage is 4096-byte aligned and stays pinned for the scope of this borrow.
        let scratch = unsafe { Seg::new(pages.as_mut_ptr() as *mut u8, seg_len) };

        let mut row_off = 0usize;
        for cf in batch {
            let rows_u = cf.rows as usize;
            for (src_c, dst_c) in rq.columns.iter().zip(&coal_rq.columns) {
                let rb = src_c.row_bytes as usize;
                let n = rows_u * rb;
                // SAFETY: thread-local scratch exclusively borrowed here; bounds are within `need_pages * 4096`.
                let dst = unsafe { scratch.bytes_mut(dst_c.values_off as usize + row_off * rb, n) };
                data.copy_out(cf.slot.req_off() + src_c.values_off as usize, dst);
            }
            row_off += rows_u;
        }

        let input = BatchView { seg: scratch, region: 0, tmpl: &coal_rq, rows: total_rows, deadline_ns: min_deadline };
        let mut output = BatchSink { seg: scratch, region: 0, tmpl: &coal_rs, rows: total_rows };
        let t0 = monotonic_ns();
        let status = w.backend.infer_batch(&input, &mut output);
        let t1 = monotonic_ns();
        let dt = t1 - t0;

        if status == st::OK {
            let mut row_off = 0usize;
            for cf in batch {
                let rows_u = cf.rows as usize;
                registry::prepare(data, cf.slot.resp_off(), rs, data.u32_at(cf.slot.rec + abi::slot_record::RESP_STAMP), rs.stamp(epoch), cf.rows);
                for (src_c, dst_c) in coal_rs.columns.iter().zip(&rs.columns) {
                    let rb = dst_c.row_bytes as usize;
                    let n = rows_u * rb;
                    // SAFETY: backend finished writing `output`; no overlapping slices remain.
                    let src = unsafe { scratch.bytes_mut(src_c.values_off as usize + row_off * rb, n) };
                    data.copy_in(cf.slot.resp_off() + dst_c.values_off as usize, src);
                }
                row_off += rows_u;
            }
        }
        (status, dt, t1)
    });

    pace.record(dt, total_rows, t1);
    ws::add_infer_ns(data, stats, dt);
    ws::add_copies(data, stats, batch.len() as u64);
    if !w.backend.in_place() {
        ws::add_zero_copy_misses(data, stats, 1);
    }
    match status {
        st::OK => {
            ws::add_served(data, stats, batch.len() as u64);
            for cf in batch {
                complete_slot(w, data, &cf.slot, &cf.d, st::OK, rs.frame_cap as usize);
            }
        }
        st::MODEL_ERROR | st::RESOURCE_EXHAUSTED => {
            ws::add_model_errors(data, stats, batch.len() as u64);
            for cf in batch {
                complete_slot(w, data, &cf.slot, &cf.d, status, 0);
            }
        }
        other => {
            for cf in batch {
                complete_slot(w, data, &cf.slot, &cf.d, other, 0);
            }
        }
    }
}

const MAX_BATCH_REQUESTS: usize = 64;

/// Opportunistically coalesce additional same-class descriptors already published in the ring
/// without ever blocking or adding wait latency (worker §4).
fn serve_opportunistic_batch<B: Backend>(w: &Worker<B>, data: &Seg<'_>, r: &impl RingMem, head: &mut u64, first: Descriptor, owner: u32, pace: &mut Pace) {
    let Some(reg) = w.registry() else {
        serve(w, data, &first, owner, pace);
        return;
    };
    let epoch = w.epoch.load(Relaxed);
    let mut seed = Some(first);
    while let Some(d0) = seed.take() {
        if d0.is_void() {
            continue;
        }
        let Some(s0) = w.dir().locate(data, d0.slab_class, d0.slot_index) else { continue };
        if slot::worker_claim(&s0, d0.slot_epoch, owner) != Claim::Claimed {
            continue;
        }
        let t0 = monotonic_ns();
        acc::slot_record::set_t_dequeue_ns(data, s0.rec, t0);
        let Some(cf0) = validate_claimed_frame(w, data, &d0, s0, reg, epoch, t0) else { continue };
        if cf0.t_deq.saturating_add(pace.expected(cf0.rows, cf0.t_deq)) >= cf0.d.deadline_ns {
            acc::worker_stats::add_deadline_drops(data, w.stats(), 1);
            complete_slot(w, data, &cf0.slot, &cf0.d, st::DEADLINE_EXCEEDED, 0);
            continue;
        }
        let class = cf0.d.slab_class;
        let mut total_rows = cf0.rows;
        let mut min_deadline = cf0.d.deadline_ns;
        let mut batch = vec![cf0];

        while total_rows < w.max_batch_rows && batch.len() < MAX_BATCH_REQUESTS {
            let Some(dk) = ring::try_dequeue_if(r, head, |cand| cand.slab_class == class) else { break };
            if dk.is_void() {
                continue;
            }
            let Some(sk) = w.dir().locate(data, dk.slab_class, dk.slot_index) else { continue };
            if slot::worker_claim(&sk, dk.slot_epoch, owner) != Claim::Claimed {
                continue;
            }
            let tk = monotonic_ns();
            acc::slot_record::set_t_dequeue_ns(data, sk.rec, tk);
            let Some(cfk) = validate_claimed_frame(w, data, &dk, sk, reg, epoch, tk) else { continue };
            if cfk.t_deq.saturating_add(pace.expected(cfk.rows, cfk.t_deq)) >= cfk.d.deadline_ns {
                acc::worker_stats::add_deadline_drops(data, w.stats(), 1);
                complete_slot(w, data, &cfk.slot, &cfk.d, st::DEADLINE_EXCEEDED, 0);
                continue;
            }
            let next_rows = total_rows.saturating_add(cfk.rows);
            let next_deadline = min_deadline.min(cfk.d.deadline_ns);
            if next_rows > w.max_batch_rows || cfk.t_deq.saturating_add(pace.expected(next_rows, cfk.t_deq)) >= next_deadline {
                run_claimed_batch(w, data, &batch, total_rows, epoch, pace);
                total_rows = cfk.rows;
                min_deadline = cfk.d.deadline_ns;
                batch.clear();
                batch.push(cfk);
                continue;
            }
            total_rows = next_rows;
            min_deadline = next_deadline;
            batch.push(cfk);
        }
        run_claimed_batch(w, data, &batch, total_rows, epoch, pace);
    }
}

#[cfg(test)]
mod pace_tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn one_slow_inference_does_not_latch_the_drops() {
        let mut p = Pace::default();
        let t = 1_000 * MS;
        p.record(5 * MS, 1, t); // the first inference ran slowly
        assert_eq!(p.expected(1, t), 5 * MS, "a 2 ms request would be dropped now");
        // Every later request is dropped, so none is timed; the estimate decays instead.
        assert_eq!(p.expected(1, t + 150 * MS), 5 * MS / 2);
        assert!(p.expected(1, t + 250 * MS) < 2 * MS, "within 250 ms a 2 ms request runs again");
        p.record(10_000, 1, t + 250 * MS); // and is timed at its real speed
        assert!(p.expected(1, t + 250 * MS) < 2 * MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scz_shm::slot::SlotMem;

    #[test]
    fn arguments_parse() {
        let a =
            parse_args(["--ctl-fd=3", "--data-fd=4", "--model-fd=5", "--status-fd=6", "--wrk-fd=7", "--gen=9", "--beat-slot=1"].map(String::from).into_iter())
                .unwrap();
        assert_eq!((a.ctl_fd, a.wrk_fd, a.gen, a.beat_slot, a.max_batch_rows), (3, 7, 9, 1, 0));
        assert!(parse_args(["--gen=1"].map(String::from).into_iter()).is_err());
        assert!(parse_args(
            ["--ctl-fd=3", "--data-fd=4", "--model-fd=5", "--status-fd=6", "--wrk-fd=7", "--gen=9", "--beat-slot=2"].map(String::from).into_iter()
        )
        .is_err());
        let full =
            ["--max-tokens=64", "--max-batch-rows=16", "--ctl-fd=3", "--data-fd=4", "--model-fd=5", "--status-fd=6", "--wrk-fd=7", "--gen=9", "--beat-slot=0"];
        assert!(parse_args(full.map(String::from).into_iter()).is_err());
        let (a, rest) = parse_known(["my_model"].into_iter().chain(full).map(String::from)).unwrap();
        assert_eq!((a.model_fd, a.max_batch_rows, rest), (5, 16, vec!["my_model".to_string(), "--max-tokens=64".to_string()]));
    }

    struct BatchScaleBackend {
        batches: std::sync::Mutex<Vec<u32>>,
    }

    impl Backend for BatchScaleBackend {
        fn signature(&self) -> Option<Signature> {
            use scz_shm::abi::elem_type as et;
            use scz_shm::registry::{Column, Schema};
            Some(Signature { schema: Schema { request: vec![Column::new("x", et::F32, 2)], response: vec![Column::new("y", et::F32, 1)] }, fixed_batch: None })
        }
        fn infer_batch(&self, input: &BatchView<'_>, output: &mut BatchSink<'_>) -> u32 {
            let rows = input.rows();
            self.batches.lock().unwrap().push(rows);
            let (in_ptr, in_len) = input.raw(0);
            let (out_ptr, out_len) = output.raw(0);
            assert_eq!(in_len, rows as usize * 8);
            assert_eq!(out_len, rows as usize * 4);
            // SAFETY: input and output pointers cover rows of f32 elements within the segment.
            let xs = unsafe { core::slice::from_raw_parts(in_ptr as *const f32, rows as usize * 2) };
            // SAFETY: output pointer is uniquely owned by this consume thread.
            let ys = unsafe { core::slice::from_raw_parts_mut(out_ptr as *mut f32, rows as usize) };
            for r in 0..rows as usize {
                ys[r] = xs[r * 2] + xs[r * 2 + 1];
            }
            st::OK
        }
    }

    #[test]
    fn opportunistic_batch_coalesces_queued_slots() {
        use scz_shm::arena::{memfd, OwnedArena};
        use scz_shm::layout::{Config as ArenaConfig, Geometry, SlabClass};
        let g =
            Geometry { rings: vec![16], slab_classes: vec![SlabClass { slot_size: 4096, slot_count: 8, req_cap: 2048, resp_cap: 2048 }], staging_len: 0x4000 };
        let owned = OwnedArena::create(&g, &ArenaConfig::with_defaults(42, 1)).unwrap();
        let arena = MappedArena::open(owned.ctl_fd.as_fd(), owned.data_fd.as_fd(), None).unwrap();
        let wrk_fd = memfd(c"wrk", abi::WORKER_SEGMENT_LEN).unwrap();
        let wrk = Mapping::map(wrk_fd.as_fd(), abi::WORKER_SEGMENT_LEN as usize, true).unwrap();
        let status = memfd(c"status", 0).unwrap();
        let model_fd = memfd(c"model", 0).unwrap();

        let backend = BatchScaleBackend { batches: std::sync::Mutex::new(Vec::new()) };
        let sig = backend.signature().unwrap();
        let caps: Vec<(u32, u32)> = arena.dir.layout.slabs.iter().map(|s| (s.2.req_cap, s.2.resp_cap)).collect();
        let reg = render::render(&sig, &caps).unwrap();
        let epoch = 1u32;
        let w = Worker {
            arena,
            wrk,
            status,
            _model: model_fd,
            model: &[],
            registry: OnceLock::from(Some(reg)),
            epoch: AtomicU32::new(epoch),
            activated: Once::new(),
            gen: 1,
            beat_slot: 0,
            rings: 1,
            max_batch_rows: 4,
            variable_batch: AtomicBool::new(true),
            backend,
            paused: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
        };
        let data = w.data();
        let rq = w.registry().unwrap().template(0, direction::REQUEST).unwrap();
        let rs = w.registry().unwrap().template(0, direction::RESPONSE).unwrap();
        let ring = w.dir().ring(&data, 0);
        let deadline = monotonic_ns() + 10_000_000_000;

        // Queue three 2-row requests: the cap is 4 rows, so the worker must run two batches (4, then 2).
        let mut epochs = vec![];
        for k in 0..3u32 {
            let s = w.dir().locate(&data, 0, k).unwrap();
            let ep = slot::try_claim(&s).unwrap();
            registry::prepare(&data, s.req_off(), rq, data.u32_at(s.rec + abi::slot_record::REQ_STAMP), rq.stamp(epoch), 2);
            acc::slot_record::set_req_len(&data, s.rec, rq.frame_cap);
            let vals: [f32; 4] = [1.0 + k as f32 * 10.0, 2.0, 3.0 + k as f32 * 10.0, 4.0];
            let mut bytes = [0u8; 16];
            for (i, v) in vals.iter().enumerate() {
                bytes[i * 4..(i + 1) * 4].copy_from_slice(&v.to_le_bytes());
            }
            data.copy_in(s.req_off() + rq.columns[0].values_off as usize, &bytes);
            slot::set_ready(&s, ep);
            ring::enqueue(
                &ring,
                &Descriptor { request_id: k as u64 + 1, slot_index: k, deadline_ns: deadline, slot_epoch: ep, slab_class: 0, ..Descriptor::default() },
            )
            .unwrap();
            epochs.push(ep);
        }

        let mut head = 0u64;
        let mut pace = Pace::default();
        while let Some(d) = ring::dequeue(&ring, &mut head) {
            if w.max_batch_rows > 1 && w.variable_batch.load(Relaxed) && ring::head_published(&ring, head) {
                serve_opportunistic_batch(&w, &data, &ring, &mut head, d, 1, &mut pace);
            } else {
                serve(&w, &data, &d, 1, &mut pace);
            }
        }

        assert_eq!(head, 3);
        assert_eq!(*w.backend.batches.lock().unwrap(), vec![4, 2]);
        for k in 0..3u32 {
            let s = w.dir().locate(&data, 0, k).unwrap();
            let word = s.word().load(Acquire);
            assert_eq!(scz_shm::word::state(word), abi::slot_state::DONE);
            assert_eq!(acc::slot_record::status(&data, s.rec), st::OK);
            assert_eq!(registry::rows(&data, s.resp_off(), rs), 2);
            let mut out = [0u8; 8];
            data.copy_out(s.resp_off() + rs.columns[0].values_off as usize, &mut out);
            let y0 = f32::from_le_bytes(out[0..4].try_into().unwrap());
            let y1 = f32::from_le_bytes(out[4..8].try_into().unwrap());
            assert_eq!((y0, y1), (3.0 + k as f32 * 10.0, 7.0 + k as f32 * 10.0));
        }
        assert_eq!(acc::worker_stats::served(&data, w.stats()), 3);
        assert_eq!(acc::worker_stats::copies(&data, w.stats()), 2, "coalesced batch counts copies, singleton is zero-copy");
    }
}
