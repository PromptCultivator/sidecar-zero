//! The client: its builder, and the call algorithm of sdk-contract §4.

use crate::codec::{Codec, FrameReader, FrameWriter, SlotReader, SlotWriter};
use crate::conn::{self, ArenaSnap, Mode};
use crate::fallback::{Fallback, FallbackReason as R, Outcome, Source};
use crate::metrics::{self, HistShard, Histogram, MetricsSink, HIST_SHARDS};
use scz_shm::abi::{self, acc, gate_state as gs, status as st};
use scz_shm::handshake::Endpoint;
use scz_shm::ring::{self, Descriptor, EnqueueError};
use scz_shm::seg::{Seg, ShmSlot};
use scz_shm::slot::{self, Abandon, Cancel, Wait, WaitSpec};
use scz_shm::snapshot::{CallerTable, EnterError, Published};
use scz_shm::wait::monotonic_ns;
use scz_shm::{layout::Directory, sys, word};
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Options {
    /// Stop waiting this long before the deadline, to leave the fallback its time (sdk-contract §1).
    pub fallback_reserve: Duration,
    /// Applied to the connect and to each handshake message.
    pub connect_timeout: Duration,
    /// Caller-state entries: at most this many calls can be in flight at once.
    pub callers: usize,
    /// A ceiling on the spin before a caller parks, below the sidecar's `spin_ns`; zero parks
    /// at once (sdk-contract §7, "Spin ceiling"). None: the sidecar's value.
    pub spin_ceiling: Option<Duration>,
    /// Time one call in this many on each thread for the latency histograms (sdk-contract
    /// §8); 0 times none.
    pub sample_every: u32,
    /// What the handshake announces: the Python SDK, built on this crate, says PYTHON.
    #[doc(hidden)]
    pub sdk_lang: u32,
    #[doc(hidden)]
    pub sdk_version: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fallback_reserve: Duration::ZERO,
            connect_timeout: Duration::from_secs(1),
            callers: 256,
            spin_ceiling: None,
            sample_every: metrics::DEFAULT_SAMPLE_EVERY,
            sdk_lang: abi::sdk_lang::RUST,
            sdk_version: crate::conn::SDK_VERSION,
        }
    }
}

/// The client states of sdk-contract §6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientState {
    Disconnected,
    Connecting,
    Bound,
    /// The sidecar does not speak this SDK's ABI.
    Rejected,
    /// The model's schema is not the codec's: calls fall back with `SchemaMismatch`.
    SchemaRejected,
}

impl ClientState {
    pub(crate) fn from_u32(v: u32) -> ClientState {
        match v {
            1 => ClientState::Connecting,
            2 => ClientState::Bound,
            3 => ClientState::Rejected,
            4 => ClientState::SchemaRejected,
            _ => ClientState::Disconnected,
        }
    }
}

/// Per-caller state. Only the thread holding the entry writes it, so counters are plain
/// load-and-store; another thread reads them only to sum them.
#[derive(Default)]
pub(crate) struct CallerData {
    /// The slot to try first in each class, plus one; 0 until the entry has claimed one, when
    /// the probe starts at a hash of the entry instead, so that many callers making their first
    /// calls at once do not all probe the same sixteen slots.
    cached_slot: [AtomicU32; abi::MAX_SLAB_CLASSES as usize],
    ring_cursor: AtomicU32,
    next_request: AtomicU64,
    model: AtomicU64,
    fallbacks: [AtomicU64; R::ALL.len()],
    /// Waits that timed out and found the slot completed more than 1 ms earlier (signaling §7).
    late_wakes: AtomicU64,
    /// Calls whose budget was above the sidecar's max_deadline_ns, and the last model
    /// generation that answered this caller (sdk-contract §8).
    clamped: AtomicU64,
    model_gen: AtomicU64,
    /// The breaker (sdk-contract §4, "Shedding under overload"): consecutive misses while
    /// closed; when open, the time it stops shedding (0: closed), and its current interval.
    brk_misses: AtomicU32,
    brk_until: AtomicU64,
    brk_interval: AtomicU64,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Consecutive `DEADLINE` or `RING_FULL` outcomes that open the breaker, and its longest interval.
const BREAKER_AFTER: u32 = 8;
const BREAKER_MAX_NS: u64 = 100_000_000;

impl CallerData {
    /// Shedding: open, and its interval not yet over. When it is over, the next call probes.
    #[inline]
    fn shedding(&self, t0: u64) -> bool {
        let until = self.brk_until.load(Relaxed);
        until != 0 && t0 < until
    }
    /// The breaker's view of a finished call. Nothing is written on the fast path.
    #[inline]
    fn record(&self, outcome: Option<R>, budget_ns: u64) {
        match outcome {
            Some(R::Overloaded) => {}
            Some(R::Deadline | R::RingFull) => {
                if self.brk_until.load(Relaxed) != 0 {
                    // The probe missed: shed again, for twice as long.
                    self.open((self.brk_interval.load(Relaxed) * 2).min(BREAKER_MAX_NS));
                } else {
                    let m = self.brk_misses.load(Relaxed) + 1;
                    if m >= BREAKER_AFTER {
                        self.brk_misses.store(0, Relaxed);
                        self.open(budget_ns.saturating_mul(2).clamp(1, BREAKER_MAX_NS));
                    } else {
                        self.brk_misses.store(m, Relaxed);
                    }
                }
            }
            _ => {
                if self.brk_misses.load(Relaxed) != 0 {
                    self.brk_misses.store(0, Relaxed);
                }
                if self.brk_until.load(Relaxed) != 0 {
                    self.brk_until.store(0, Relaxed);
                }
            }
        }
    }
    fn open(&self, interval: u64) {
        let now = monotonic_ns();
        // Jittered: between half and one and a half intervals, so callers do not probe together.
        let j = mix(now ^ self as *const _ as u64) % interval.max(1);
        self.brk_interval.store(interval, Relaxed);
        self.brk_until.store(now + interval / 2 + j, Relaxed);
    }
}

fn bump(c: &AtomicU64) {
    c.store(c.load(Relaxed) + 1, Relaxed);
}

/// What the client and its background thread share.
pub(crate) struct Shared {
    /// Clears `published` in a forked child (sdk-contract §7). Declared first so that it is
    /// dropped before `published`.
    pub fork_reg: std::sync::OnceLock<Option<scz_shm::fork::Registration>>,
    pub published: Published<ArenaSnap>,
    pub callers: CallerTable<CallerData>,
    pub state: AtomicU32,
    pub stop: AtomicBool,
    pub endpoint: Endpoint,
    pub connect_timeout: Duration,
    pub sessions: AtomicU64,
    /// Arenas unmapped after their session ended.
    pub retired: AtomicU64,
    /// Fallbacks taken with no snapshot, so with no entry to count them in: every call during
    /// an outage. Sharded by the thread's index hint, so threads rarely share a line.
    pub unattached: Box<[Shard]>,
    /// The codec's schema fingerprint; None for opaque bytes.
    pub want: Option<u64>,
    /// A request thread found the registry changed: the background thread rebinds.
    pub rebind: AtomicBool,
    /// The application's spin ceiling in nanoseconds; u64::MAX for none.
    pub spin_ceiling_ns: AtomicU64,
    pub sdk_lang: u32,
    pub sdk_version: u32,
    /// The sampled histograms (sdk-contract §8), sharded by the thread's index hint.
    pub hists: Box<[HistShard]>,
}

const SHARDS: usize = 64;

#[repr(align(64))]
#[derive(Default)]
pub(crate) struct Shard([AtomicU64; R::ALL.len()]);

impl Shared {
    fn unattached_reason(&self) -> R {
        match ClientState::from_u32(self.state.load(Relaxed)) {
            ClientState::Rejected => R::AbiMismatch,
            _ => R::NotConnected,
        }
    }
}

thread_local! {
    static TID: Cell<u32> = const { Cell::new(0) };
    static HINT: Cell<u32> = const { Cell::new(0) };
    /// Calls since this thread's last timed one (sdk-contract §8, "Sampling").
    static SAMPLE: Cell<u32> = const { Cell::new(0) };
}

/// What `on_fallback` holds.
type Hook = Arc<dyn Fn(R) + Send + Sync>;

fn tid() -> u32 {
    TID.with(|t| {
        if t.get() == 0 {
            let id = sys::tid();
            t.set(id);
            HINT.with(|h| h.set(id.wrapping_mul(0x9E37_79B9) >> 8));
        }
        t.get()
    })
}

// ── builder (sdk-rust §2): build() exists only once a fallback is given ────────────────

pub struct NoFallback;
pub struct WithFallback<F>(F);

pub struct ClientBuilder<C, S> {
    endpoint: Endpoint,
    codec: C,
    fallback: S,
    options: Options,
    on_fallback: Option<Hook>,
}

impl<C: Codec> ClientBuilder<C, NoFallback> {
    pub fn fallback<F: Fallback<C::In, C::Out>>(self, f: F) -> ClientBuilder<C, WithFallback<F>> {
        ClientBuilder { endpoint: self.endpoint, codec: self.codec, fallback: WithFallback(f), options: self.options, on_fallback: self.on_fallback }
    }
}

impl<C: Codec, S> ClientBuilder<C, S> {
    /// Called with the reason once per fallback, on the calling thread, just before the
    /// fallback function (sdk-contract §8): for sampled logging. Off by default, and not on
    /// the zero-allocation path: what it does costs every fallback. A panic in it propagates
    /// as the fallback's would.
    pub fn on_fallback(mut self, hook: impl Fn(R) + Send + Sync + 'static) -> Self {
        self.on_fallback = Some(Arc::new(hook));
        self
    }
}

impl<C: Codec, F: Fallback<C::In, C::Out>> ClientBuilder<C, WithFallback<F>> {
    pub fn options(mut self, o: Options) -> Self {
        self.options = o;
        self
    }
    /// Returns at once; connecting happens on a background thread. Calls fall back with
    /// `NotConnected` until it has (sdk-contract §6, "Startup").
    pub fn build(self) -> Client<C, F> {
        let shared = Arc::new(Shared {
            fork_reg: std::sync::OnceLock::new(),
            published: Published::new(),
            callers: CallerTable::new(self.options.callers),
            state: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            endpoint: self.endpoint,
            connect_timeout: self.options.connect_timeout,
            sessions: AtomicU64::new(0),
            retired: AtomicU64::new(0),
            unattached: (0..SHARDS).map(|_| Shard::default()).collect(),
            want: self.codec.schema().map(|s| scz_shm::registry::fingerprint(&s)),
            rebind: AtomicBool::new(false),
            spin_ceiling_ns: AtomicU64::new(self.options.spin_ceiling.map_or(u64::MAX, |d| d.as_nanos().min(u64::MAX as u128) as u64)),
            sdk_lang: self.options.sdk_lang,
            sdk_version: self.options.sdk_version,
            hists: (0..HIST_SHARDS).map(|_| HistShard::default()).collect(),
        });
        let _ = shared.fork_reg.set(shared.published.clear_in_forked_child());
        let s2 = shared.clone();
        let bg =
            std::thread::Builder::new().name("sidecarzero".into()).spawn(move || conn::background(s2)).expect("cannot start the SidecarZero background thread");
        Client {
            shared,
            codec: self.codec,
            fallback: self.fallback.0,
            reserve_ns: self.options.fallback_reserve.as_nanos() as u64,
            sample_every: self.options.sample_every,
            on_fallback: self.on_fallback,
            bg: Some(bg),
            pid: std::process::id(),
        }
    }
}

// ── the client ──────────────────────────────────────────────────────────────────────────

pub struct Client<C, F> {
    shared: Arc<Shared>,
    codec: C,
    fallback: F,
    reserve_ns: u64,
    sample_every: u32,
    on_fallback: Option<Hook>,
    bg: Option<std::thread::JoinHandle<()>>,
    /// The process that built the client. In a forked child the background thread does not
    /// exist, so closing there must not wait for it.
    pid: u32,
}

impl Client<(), ()> {
    pub fn builder<C: Codec>(endpoint: Endpoint, codec: C) -> ClientBuilder<C, NoFallback> {
        ClientBuilder { endpoint, codec, fallback: NoFallback, options: Options::default(), on_fallback: None }
    }
}

/// Counters summed over every caller entry, and the sampled histograms (sdk-contract §8).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub model: u64,
    pub fallback: Vec<(R, u64)>,
    /// Sessions bound so far, and arenas unmapped after theirs ended.
    pub sessions: u64,
    pub retired: u64,
    /// Waits that timed out and then found the slot done more than 1 ms earlier.
    pub late_wakes: u64,
    /// Calls whose budget exceeded the sidecar's max_deadline_ns.
    pub clamped: u64,
    /// Callers whose breaker is open now (shedding).
    pub breaker_open: u64,
    /// The highest model generation that has answered any caller.
    pub model_gen: u64,
    /// End-to-end `infer` time of sampled calls the model answered, and of those that fell
    /// back; and the time spent inside the fallback function.
    pub latency_model: Histogram,
    pub latency_fallback: Histogram,
    pub fallback_time: Histogram,
}

impl Stats {
    pub fn fallbacks(&self, r: R) -> u64 {
        self.fallback.iter().find(|x| x.0 == r).map_or(0, |x| x.1)
    }
}

impl<C, F> Client<C, F> {
    pub fn state(&self) -> ClientState {
        ClientState::from_u32(self.shared.state.load(Acquire))
    }
    /// Wait until an arena is mapped and its gate is open, for callers that prefer to start
    /// warm. Mapped alone is not enough: the handshake can finish before the worker is
    /// active, and calls then fall back with `GateInit`.
    pub fn await_bound(&self, timeout: Duration) -> bool {
        let end = std::time::Instant::now() + timeout;
        loop {
            if self.gate_open() {
                return true;
            }
            if std::time::Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// The gate is open and the binding is current and usable.
    fn gate_open(&self) -> bool {
        let Ok(g) = HINT.with(|h| self.shared.callers.enter(&self.shared.published, h, tid())) else { return false };
        let snap = g.snap();
        let gate = acc::control_header::gate(&snap.arena.ctl.seg(), 0);
        word::gate_state(gate) == gs::HEALTHY
            && confirm(&self.shared, snap, word::gate_gen(gate)).is_ok()
            && matches!(snap.binding.mode, Mode::Raw | Mode::Frames(_))
    }
    pub fn stats(&self) -> Stats {
        let mut s = Stats { sessions: self.shared.sessions.load(Relaxed), retired: self.shared.retired.load(Relaxed), ..Default::default() };
        let mut fb = [0u64; R::ALL.len()];
        let now = monotonic_ns();
        for d in self.shared.callers.data() {
            s.model += d.model.load(Relaxed);
            s.late_wakes += d.late_wakes.load(Relaxed);
            s.clamped += d.clamped.load(Relaxed);
            s.model_gen = s.model_gen.max(d.model_gen.load(Relaxed));
            s.breaker_open += u64::from(d.shedding(now));
            for (i, c) in d.fallbacks.iter().enumerate() {
                fb[i] += c.load(Relaxed);
            }
        }
        for shard in self.shared.unattached.iter() {
            for (i, c) in shard.0.iter().enumerate() {
                fb[i] += c.load(Relaxed);
            }
        }
        s.fallback = R::ALL.iter().zip(fb).filter(|x| x.1 > 0).map(|(r, n)| (*r, n)).collect();
        let h = &self.shared.hists;
        s.latency_model = Histogram::sum_of(h.iter().map(|x| &x.model));
        s.latency_fallback = Histogram::sum_of(h.iter().map(|x| &x.fallback));
        s.fallback_time = Histogram::sum_of(h.iter().map(|x| &x.fallback_fn));
        s
    }
    /// Every metric of sdk-contract §8, in one pass, to `sink`.
    pub fn export(&self, sink: &mut dyn MetricsSink) {
        let s = self.stats();
        sink.counter("scz_client_calls_total", &[("source", "model")], s.model);
        for r in R::ALL {
            sink.counter("scz_client_calls_total", &[("source", "fallback"), ("reason", r.as_str())], s.fallbacks(r));
        }
        let b = &metrics::BUCKETS_SECONDS;
        sink.histogram("scz_client_latency_seconds", &[("source", "model")], b, &s.latency_model);
        sink.histogram("scz_client_latency_seconds", &[("source", "fallback")], b, &s.latency_fallback);
        sink.histogram("scz_client_fallback_seconds", &[], b, &s.fallback_time);
        sink.counter("scz_client_budget_clamped_total", &[], s.clamped);
        sink.gauge("scz_client_breaker_open", &[], s.breaker_open as f64);
        sink.counter("scz_client_late_wakes_total", &[], s.late_wakes);
        sink.counter("scz_client_reconnects_total", &[], s.sessions.saturating_sub(1));
        sink.gauge("scz_client_state", &[], self.shared.state.load(Acquire) as f64);
        sink.gauge("scz_client_model_gen", &[], s.model_gen as f64);
    }
    /// [`export`](Self::export) in the Prometheus text exposition format.
    pub fn prometheus_text(&self) -> String {
        let mut t = metrics::PrometheusText::new();
        self.export(&mut t);
        t.finish()
    }
    /// How many slots are in each state (indexed by `slot_state`), read through the current
    /// arena. For tests and diagnosis; None when no arena is mapped.
    #[doc(hidden)]
    pub fn slot_census(&self) -> Option<[u32; 7]> {
        let g = HINT.with(|h| self.shared.callers.enter(&self.shared.published, h, tid())).ok()?;
        let (data, dir) = (g.snap().arena.data.seg(), &g.snap().arena.dir);
        let mut n = [0u32; 7];
        for (class, &(_, _, c)) in dir.layout.slabs.iter().enumerate() {
            for i in 0..c.slot_count {
                let w = slot::SlotMem::word(&dir.slot(&data, class, i)).load(Acquire);
                n[(word::state(w) as usize).min(6)] += 1;
            }
        }
        Some(n)
    }
    /// Fix the spin ceiling (sdk-contract §7): zero parks at once, None restores the sidecar's.
    pub fn set_spin_ceiling(&self, d: Option<Duration>) {
        self.shared.spin_ceiling_ns.store(d.map_or(u64::MAX, |d| d.as_nanos().min(u64::MAX as u128) as u64), Relaxed);
    }
    /// sdk-contract §6 `close()`. Also what dropping the client does.
    pub fn close(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        if std::process::id() != self.pid {
            std::mem::forget(self.bg.take()); // a forked child: no background thread to join
            return;
        }
        if let Some(bg) = self.bg.take() {
            self.shared.stop.store(true, Release);
            bg.thread().unpark();
            let _ = bg.join();
        }
    }
}

impl<C, F> Drop for Client<C, F> {
    fn drop(&mut self) {
        self.stop();
    }
}

impl<C: Codec, F: Fallback<C::In, C::Out>> Client<C, F> {
    /// Always returns an output: the model's if it answers within `budget`, else the
    /// fallback's (sdk-contract §1).
    #[inline]
    pub fn infer(&self, input: &C::In, budget: Duration) -> Outcome<C::Out> {
        let t_start = self.sampled().then(monotonic_ns);
        match self.try_model(input, budget.as_nanos().min(u64::MAX as u128) as u64) {
            Ok(o) => {
                if let Some(t) = t_start {
                    self.hists().model.record(monotonic_ns().saturating_sub(t));
                }
                o
            }
            // The slot and the caller entry have already been released.
            Err(reason) => {
                if let Some(h) = &self.on_fallback {
                    h(reason);
                }
                let value = match t_start {
                    None => self.fallback.apply(input, reason),
                    Some(t) => {
                        let t1 = monotonic_ns();
                        let v = self.fallback.apply(input, reason);
                        let t2 = monotonic_ns();
                        let h = self.hists();
                        h.fallback_fn.record(t2.saturating_sub(t1));
                        h.fallback.record(t2.saturating_sub(t));
                        v
                    }
                };
                Outcome { value, source: Source::Fallback { reason } }
            }
        }
    }

    /// Whether this thread times this call: those whose number on this thread (counted over
    /// every client) is a multiple of `sample_every`, the thread's first call among them.
    #[inline]
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
    fn hists(&self) -> &HistShard {
        &self.shared.hists[HINT.with(|h| h.get()) as usize % HIST_SHARDS]
    }

    fn try_model(&self, input: &C::In, budget_ns: u64) -> Result<Outcome<C::Out>, R> {
        let tid = tid();
        let g = match HINT.with(|h| self.shared.callers.enter(&self.shared.published, h, tid)) {
            Ok(g) => g,
            Err(e) => {
                let r = if e == EnterError::NoEntry { R::NoSlot } else { self.shared.unattached_reason() };
                // Threads can share a shard, so an atomic add (bump is for entry-owned counters).
                let shard = HINT.with(|h| h.get()) as usize % SHARDS;
                self.shared.unattached[shard].0[r.index()].fetch_add(1, Relaxed);
                return Err(r);
            }
        };
        let e = g.data();
        let out = call(&self.codec, &self.shared, g.snap(), e, input, budget_ns, self.reserve_ns);
        match &out {
            Ok(o) => {
                bump(&e.model);
                if let Source::Model { model_gen } = o.source {
                    if e.model_gen.load(Relaxed) != model_gen as u64 {
                        e.model_gen.store(model_gen as u64, Relaxed);
                    }
                }
            }
            Err(r) => bump(&e.fallbacks[r.index()]),
        }
        e.record(out.as_ref().err().copied(), budget_ns.min(g.snap().arena.dir.max_deadline_ns));
        out
    }
}

fn gate_reason(g: u64) -> R {
    match word::gate_state(g) {
        gs::INIT => R::GateInit,
        gs::PSI_SHEDDING => R::GateShedding,
        gs::RELOADING => R::GateReloading,
        gs::DRAINING => R::GateDraining,
        _ => R::GateDead,
    }
}

fn status_reason(s: u32) -> R {
    match s {
        st::DEADLINE_EXCEEDED => R::Deadline,
        st::BAD_REQUEST => R::BadRequest,
        st::MODEL_ERROR => R::ModelError,
        st::RESOURCE_EXHAUSTED => R::ResourceExhausted,
        st::STALE_REGISTRY => R::Rebinding,
        _ => R::WorkerFailed,
    }
}

/// The admission check of arena-data-plane §9: when the gate shows a generation the binding
/// was not confirmed under, the registry epoch is read under the seqlock. Unchanged (a
/// promotion, or a reload with the same schema): confirmed. Changed: REBINDING, and the
/// background thread rebinds. Costs no shared read on a normal call.
#[inline]
fn confirm(shared: &Shared, snap: &ArenaSnap, gen: u32) -> Result<(), R> {
    let b = &snap.binding;
    if b.confirmed.load(Relaxed) == gen {
        return Ok(());
    }
    match scz_shm::model::epoch(&snap.arena.ctl.seg(), 64) {
        Some(e) if e == b.epoch && !matches!(b.mode, Mode::Unbound) => {
            b.confirmed.store(gen, Relaxed);
            Ok(())
        }
        _ => {
            shared.rebind.store(true, Release);
            Err(R::Rebinding)
        }
    }
}

/// A wait that ended at its deadline and found the slot done: a late wake-up if the slot was
/// completed more than 1 ms before, which is how a lost wake-up shows (signaling §7).
fn count_late(data: &Seg<'_>, s: &ShmSlot<'_>, e: &CallerData) {
    let done = acc::slot_record::t_done_ns(data, s.rec);
    if done != 0 && monotonic_ns() > done.saturating_add(1_000_000) {
        bump(&e.late_wakes);
    }
}

/// Takes back a READY slot whose call unwound inside the enqueue (the cell is already void).
struct CancelOnUnwind<'a, 'b> {
    s: &'a ShmSlot<'b>,
    ep: u32,
}

impl Drop for CancelOnUnwind<'_, '_> {
    fn drop(&mut self) {
        if let Cancel::NotReady = slot::cancel_if_ready(self.s, self.ep) {
            slot::release(self.s, self.ep); // the sweep failed it meanwhile
        }
    }
}

fn ring_fault(data: &Seg<'_>, dir: &Directory) {
    acc::status_page::add_client_ring_faults(data, dir.status_off as usize, 1);
}

/// sdk-contract §4, from admission to resolution. Every path out of here has released the
/// slot, handed it to the worker, or (after a bounded loop gave up) forgotten it.
#[inline]
fn call<C: Codec>(codec: &C, shared: &Shared, snap: &ArenaSnap, e: &CallerData, input: &C::In, budget_ns: u64, reserve_ns: u64) -> Result<Outcome<C::Out>, R> {
    use acc::slot_record as rec;
    let (ctl, data, dir) = (snap.arena.ctl.seg(), snap.arena.data.seg(), &snap.arena.dir);
    let t0 = monotonic_ns();
    if budget_ns > dir.max_deadline_ns {
        bump(&e.clamped);
    }
    let deadline = t0 + budget_ns.min(dir.max_deadline_ns);
    let wait_until = deadline.saturating_sub(reserve_ns);

    // ── Admission: one clock read and two loads from one cache line.
    let g = acc::control_header::gate(&ctl, 0);
    if word::gate_state(g) != gs::HEALTHY {
        return Err(gate_reason(g));
    }
    if acc::control_header::supervisor_hb_mono_ns(&ctl, 0) + dir.hb_stale_ns < t0 {
        return Err(R::StaleHeartbeat);
    }
    confirm(shared, snap, word::gate_gen(g))?;
    // What the codec writes: opaque bytes, or a frame of one of the registry's templates.
    let frames = match &snap.binding.mode {
        Mode::Raw => None,
        Mode::Frames(reg) => Some((reg, codec.rows(input))),
        Mode::Mismatch => return Err(R::SchemaMismatch),
        Mode::Unbound => {
            shared.rebind.store(true, Release);
            return Err(R::Rebinding);
        }
    };
    if e.shedding(t0) {
        return Err(R::Overloaded); // claims no slot, publishes nothing
    }

    // ── Claim a slot in the smallest class that holds the request: the cached one, then a
    //    bounded probe.
    let class = match frames {
        None => {
            let need = codec.request_len(input);
            dir.layout.slabs.iter().position(|s| s.2.req_cap as usize >= need)
        }
        Some((reg, rows)) => reg.class_for(rows),
    };
    let Some(class) = class else {
        return Err(R::TooLarge);
    };
    let count = dir.layout.slabs[class].2.slot_count;
    let start = match e.cached_slot[class].load(Relaxed) {
        0 => mix(e as *const CallerData as u64) as u32,
        v => v - 1,
    };
    let mut claimed = None;
    for k in 0..(abi::SLOT_PROBE as u32).min(count) {
        let idx = (start.wrapping_add(k)) % count;
        let s = dir.slot(&data, class, idx);
        if let Some(ep) = slot::try_claim(&s) {
            claimed = Some((s, idx, ep));
            break;
        }
    }
    let Some((s, idx, ep)) = claimed else { return Err(R::NoSlot) };
    e.cached_slot[class].store(idx + 1, Relaxed);
    let forget = || e.cached_slot[class].store(idx.wrapping_add(1) % count + 1, Relaxed);

    // ── Encode. A codec error or panic releases the slot.
    let epoch = snap.binding.epoch;
    let (encoded, req_len) = match frames {
        None => {
            let mut w = SlotWriter::request(&s);
            let ok = matches!(catch_unwind(AssertUnwindSafe(|| codec.encode(input, &mut w))), Ok(Ok(())));
            (ok, w.len() as u32)
        }
        Some((reg, rows)) => {
            let t = reg.template(class, abi::direction::REQUEST).expect("a class chosen from its request template");
            // Stamp the region with the template's prefix if it holds another, and set the
            // row count's length fields if it differs (arrow-data-plane §7.1).
            scz_shm::registry::prepare(&data, s.req_off(), t, data.u32_at(s.rec + abi::slot_record::REQ_STAMP), t.stamp(epoch), rows);
            let mut w = FrameWriter { seg: data, region: s.req_off(), tmpl: t, rows };
            let ok = matches!(catch_unwind(AssertUnwindSafe(|| codec.encode_frame(input, &mut w))), Ok(Ok(())));
            (ok, t.frame_cap)
        }
    };
    if !encoded {
        slot::release(&s, ep);
        return Err(R::CodecError);
    }
    let rid = e.next_request.load(Relaxed).wrapping_add(1);
    e.next_request.store(rid, Relaxed);
    let t_enq = monotonic_ns();
    rec::set_request_id(&data, s.rec, rid);
    rec::set_req_len(&data, s.rec, req_len);
    rec::set_t_enqueue_ns(&data, s.rec, t_enq);
    slot::set_ready(&s, ep); // the request region is the worker's from here

    // ── Enqueue: round robin per caller, skipping a full ring.
    let desc = Descriptor {
        request_id: rid,
        slot_index: idx,
        deadline_ns: wait_until,
        slot_epoch: ep,
        slab_class: class as u16,
        enqueue_ns: t_enq,
        flags: 0,
        route: 0,
    };
    let nr = dir.layout.rings.len() as u32;
    let cur = e.ring_cursor.load(Relaxed);
    e.ring_cursor.store(cur.wrapping_add(1), Relaxed);
    // If the call unwinds inside the enqueue, the ring's guard publishes its cell as void, and
    // this one takes the slot back (sdk-contract §4, "Every claimed ring cell is published").
    let unwind = CancelOnUnwind { s: &s, ep };
    let mut sent = false;
    for k in 0..nr {
        match ring::enqueue(&dir.ring(&data, (cur.wrapping_add(k) % nr) as usize), &desc) {
            Ok(()) => {
                sent = true;
                break;
            }
            Err(EnqueueError::Full) => {}
            Err(EnqueueError::Exhausted) => ring_fault(&data, dir),
        }
    }
    std::mem::forget(unwind);
    if !sent {
        // No descriptor exists, but the sweep may have failed the slot meanwhile.
        match slot::cancel_if_ready(&s, ep) {
            Cancel::Cancelled => {}
            Cancel::NotReady => slot::release(&s, ep),
            Cancel::GaveUp => {
                ring_fault(&data, dir);
                forget();
            }
        }
        return Err(R::RingFull);
    }

    // ── Wait: gate re-check, spin, then futex.
    let spin = dir.spin_ns.min(shared.spin_ceiling_ns.load(Relaxed));
    let spec = WaitSpec {
        epoch: ep,
        deadline_ns: wait_until,
        spin_until_ns: monotonic_ns().saturating_add(spin),
        now: monotonic_ns,
        gate: || acc::control_header::gate(&ctl, 0),
    };
    match slot::wait(&s, &spec) {
        Wait::Done(_) => resolve(codec, shared, &data, dir, &s, ep, frames, class, epoch),
        Wait::Late(_) => {
            count_late(&data, &s, e);
            resolve(codec, shared, &data, dir, &s, ep, frames, class, epoch)
        }
        Wait::Failed(_) => {
            slot::release(&s, ep);
            Err(R::WorkerFailed)
        }
        Wait::Cancelled(g) => Err(gate_reason(g)), // the cancel freed the slot
        Wait::Timeout => match slot::abandon(&s, ep) {
            Abandon::Freed => Err(R::Deadline),
            Abandon::Abandoned => {
                forget(); // the worker, or the sweep, frees it
                Err(R::Deadline)
            }
            Abandon::Done(_) => {
                count_late(&data, &s, e); // the worker won the race
                resolve(codec, shared, &data, dir, &s, ep, frames, class, epoch)
            }
            Abandon::Failed(_) => {
                slot::release(&s, ep);
                Err(R::WorkerFailed)
            }
            Abandon::GaveUp => {
                ring_fault(&data, dir);
                forget();
                Err(R::WorkerFailed)
            }
        },
        Wait::GaveUp => {
            ring_fault(&data, dir);
            forget();
            Err(R::WorkerFailed)
        }
    }
}

/// A DONE slot: read each result field once, check, decode, release. A response that fails
/// its checks is refused: counted for the supervisor (client_rejects), and the call falls back.
#[allow(clippy::too_many_arguments)]
fn resolve<C: Codec>(
    codec: &C,
    shared: &Shared,
    data: &Seg<'_>,
    dir: &Directory,
    s: &ShmSlot<'_>,
    ep: u32,
    frames: Option<(&scz_shm::registry::Registry, u32)>,
    class: usize,
    epoch: u32,
) -> Result<Outcome<C::Out>, R> {
    use acc::slot_record as rec;
    let status = rec::status(data, s.rec);
    let model_gen = rec::model_gen(data, s.rec);
    let resp_len = rec::resp_len(data, s.rec) as usize;
    if status != st::OK {
        slot::release(s, ep);
        if status == st::STALE_REGISTRY {
            shared.rebind.store(true, Release);
        }
        return Err(status_reason(status));
    }
    let refuse = || {
        acc::status_page::add_client_rejects(data, dir.status_off as usize, 1);
        slot::release(s, ep);
        Err(R::WorkerFailed)
    };
    let out = match frames {
        None => {
            let Some(r) = SlotReader::response(s, resp_len) else { return refuse() };
            catch_unwind(AssertUnwindSafe(|| codec.decode(&r)))
        }
        Some((reg, rows)) => {
            // arrow-data-plane §7.3: the response must be this template's, under this
            // registry epoch, the template's frame length, and as many rows as were asked.
            let Some(t) = reg.template(class, abi::direction::RESPONSE) else { return refuse() };
            let stamp = rec::resp_stamp(data, s.rec);
            if stamp != t.stamp(epoch) || resp_len != t.frame_cap as usize || scz_shm::registry::rows(data, s.resp_off(), t) != rows as u64 {
                return refuse();
            }
            let r = FrameReader { seg: *data, region: s.resp_off(), tmpl: t, rows };
            catch_unwind(AssertUnwindSafe(|| codec.decode_frame(&r)))
        }
    };
    slot::release(s, ep);
    match out {
        Ok(Ok(v)) => Ok(Outcome { value: v, source: Source::Model { model_gen } }),
        _ => Err(R::CodecError),
    }
}
