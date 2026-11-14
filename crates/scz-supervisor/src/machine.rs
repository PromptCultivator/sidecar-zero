//! The supervisor's decisions: a state machine over events, on a clock it is given
//! (testing §2.6: "the supervisor's loop takes its clock and its events through an
//! interface"). It reads and writes the arena directly, and asks the shell for everything else
//! through [`Effects`]: spawning and killing workers, starting death-lock watchers, ending a
//! session. Tests run it against an in-process arena and a scripted shell, in virtual time.
//!
//! What it implements, by section of the supervisor document:
//!   §5.2 watching: worker heartbeats (with the stall discount), a hung ring, a slot held too
//!        long, the load, activation, handover, and release timeouts, a stuck head cell, an
//!        inconsistent ring
//!   §5.3 a death: the gate first, then the queued slots, ring repair as each lock reports,
//!        promotion once every ring is repaired, the owner sweep on the exit
//!   §5.4 the warm standby; §5.5 the restart backoff; §5.6 a same-schema reload by handover
//!   §5.7 the gate, written by one function from the holds that remain
//!   §4   session reset, on the client's exit and on request; §8 shutdown
//!   §6   the memory and pressure monitor: utilization and PSI from cgroup samples, and the
//!        relief ladder (retire the standby, hand over to it, shed and TRIM, recycle,
//!        DOES_NOT_FIT)

use scz_shm::abi::{self, acc, ctl_bit, gate_reason as gr, gate_state as gs, slot_state as ss, status_byte as sb};
use scz_shm::layout::Directory;
use scz_shm::ring::{self, HeadState};
use scz_shm::seg::Seg;
use scz_shm::standby::{self, ShmStandby};
use scz_shm::sync::full_fence;
use scz_shm::{slot, word};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;

const MS: u64 = 1_000_000;

/// The admin `status` struct: its length and magic, and the order of the hold bits in it.
pub const STATUS_LEN: usize = 1024;
/// The target of a supervisor whose model file is missing: nothing is spawned until the path
/// watch finds one (§3, "Starting degraded").
pub const NO_MODEL: u32 = u32::MAX;
pub const STATUS_MAGIC: u32 = 0x5353_4353; // "SCSS"
pub const HOLDS: [&str; 6] = ["init", "death", "reload", "reset", "pressure", "shutdown"];
// Byte offsets, for readers: magic 0, version 4, gate 8, consumer 16, worker count 20,
// workers 24 (2 × {gen, beat, role, flags}), counters 56 (spawns, deaths, promotions,
// restarts, resets, reloads, condemned, recycles, retired standbys, refusal condemns: u64),
// times 136 (last and max recovery, last and max promotion: u64 ns), utilization 168
// (per mille), flags 172 (crashloop, shedding, standby suppressed, does not fit, reset stuck,
// session bound, accepting), holds 176 (HOLDS order), rings 180, ring depths 184 (8 × u32),
// slot counts 216 (7 × u32, by state), transitions 248 (8 × {gate u64, at u64}), last cause
// 376 (128 bytes, NUL-padded); version 2 adds, at 512: registry epoch u32, rollbacks u32,
// rejected models u32, target model u32, schema fingerprint u64, published model sha256 (32
// bytes), previous model 568 (u32, u32::MAX for none), trial served ms 572 u32, trial deaths
// 576 u32, published model 580 u32.
// (sczctl decodes it; crates/scz-ctl/src/main.rs.)

#[derive(Clone, Debug)]
pub struct Config {
    /// Keep a warm standby (supervisor §5.4).
    pub standby: bool,
    pub hb_interval_ns: u64,
    pub hb_stale_ns: u64,
    pub max_deadline_ns: u64,
    /// A consume thread or a slot that has not moved for this long is hung (§5.2).
    pub hang_ns: u64,
    pub load_timeout_ns: u64,
    pub activate_timeout_ns: u64,
    pub handover_timeout_ns: u64,
    pub release_grace_ns: u64,
    pub drain_timeout_ns: u64,
    pub backoff_min_ns: u64,
    pub backoff_max_ns: u64,
    /// A worker that has served this long resets the failure count.
    pub stable_ns: u64,
    pub session_grace_ns: u64,
    /// Supervisor §6 thresholds, as fractions of the memory limit and of wall time.
    pub relieve_at: f64,
    pub shed_at: f64,
    pub recover_at: f64,
    pub recover_hold_ns: u64,
    pub shed_max_ns: u64,
    pub psi_some: f64,
    pub psi_full: f64,
    pub psi_window_ns: u64,
    pub growth_factor: f64,
    /// Supervisor §5.2: refused responses that condemn, within one second.
    pub refusals_min: u64,
    pub refusals_ratio: f64,
    /// §5.6: a configured digest pin; a model with another digest is refused.
    pub pin: Option<[u8; 32]>,
    /// §5.6, rollback: keep the previous model until the new one has served this long, and go
    /// back to it if the new one dies as the consumer `rollback_deaths` times before then.
    pub rollback: bool,
    pub rollback_window_ns: u64,
    pub rollback_deaths: u32,
}

impl Config {
    pub fn with_defaults(hb_interval_ns: u64, hb_stale_ns: u64, max_deadline_ns: u64) -> Config {
        Config {
            standby: true,
            hb_interval_ns,
            hb_stale_ns,
            max_deadline_ns,
            hang_ns: 2 * max_deadline_ns,
            load_timeout_ns: 300_000 * MS,
            activate_timeout_ns: 500 * MS,
            handover_timeout_ns: max_deadline_ns + 500 * MS,
            release_grace_ns: 2_000 * MS,
            drain_timeout_ns: max_deadline_ns + 500 * MS,
            backoff_min_ns: 50 * MS,
            backoff_max_ns: 5_000 * MS,
            stable_ns: 10_000 * MS,
            session_grace_ns: 20_000 * MS,
            relieve_at: 0.85,
            shed_at: 0.95,
            recover_at: 0.75,
            recover_hold_ns: 200 * MS,
            shed_max_ns: 2_000 * MS,
            psi_some: 0.10,
            psi_full: 0.05,
            psi_window_ns: 100 * MS,
            growth_factor: 1.25,
            refusals_min: 8,
            refusals_ratio: 0.01,
            pin: None,
            rollback: true,
            rollback_window_ns: 60_000 * MS,
            rollback_deaths: 3,
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What the shell does on the core's behalf.
pub trait Effects {
    /// Start a worker of generation `gen` in beat slot `beat`, loading model `model` (an id the
    /// shell assigned when it opened the file). False if it could not start.
    fn spawn(&mut self, gen: u32, beat: u32, model: u32) -> bool;
    /// SIGKILL it.
    fn kill(&mut self, gen: u32);
    /// Start watching its death locks, one per ring (it has taken them: it reported WARM).
    fn watch(&mut self, gen: u32, rings: usize);
    /// Shut the sending side of the session socket, so the client leaves (supervisor §4).
    fn end_session(&mut self);
    fn log(&mut self, msg: &str);
    /// Verify the registry `gen` staged (arrow-data-plane §4): copy it out and start the
    /// verifier helper, whose answer comes back as `Event::Verdict`. An empty candidate (a
    /// worker without a schema) needs no helper: the answer is returned at once.
    fn verify(&mut self, gen: u32) -> Option<Result<(Vec<u8>, u64), String>>;
    /// The previous model is no longer needed for a rollback; its descriptor may be closed.
    fn release_model(&mut self, _model: u32) {}
}

/// One reading of the container's memory (supervisor §6), taken by the shell every 10 ms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub current: u64,
    /// None when `memory.max` is "max": the utilization rules are off.
    pub max: Option<u64>,
    pub inactive_file: u64,
    /// PSI `some` and `full` total stall, in microseconds; None without PSI.
    pub psi_some_us: Option<u64>,
    pub psi_full_us: Option<u64>,
    /// Each live worker's private memory (resident minus shared), by generation.
    pub private: Vec<(u32, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Tick,
    Memory(Sample),
    /// `cpu.stat`'s `nr_throttled` advanced: the container was throttled, the supervisor too.
    Throttled,
    Status {
        gen: u32,
        byte: u8,
    },
    LockDied {
        gen: u32,
        ring: usize,
    },
    Exited {
        gen: u32,
    },
    SessionBound,
    SessionClosed,
    Reload,
    /// `sczctl reload --force`: reload even a model that was rolled back from (§5.6).
    ForceReload,
    /// The model path names another file now; the shell has opened it as `model` (§5.6).
    NewModel {
        model: u32,
    },
    /// The hasher's answer for `model`: its SHA-256, or None if it could not be read.
    Digest {
        model: u32,
        sha: Option<[u8; 32]>,
    },
    /// The registry verifier's answer for what `gen` staged: the exact bytes it verified and
    /// their schema fingerprint, or why they were refused.
    Verdict {
        gen: u32,
        result: Result<(Vec<u8>, u64), String>,
    },
    RequestReset,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Spawned, not yet WARM.
    Loading,
    /// WARM and parked: a standby, or a reload's new worker before the handover.
    Warm,
    /// The ring consumer: activated (the gate carries its generation), ACTIVE or not yet.
    Consumer,
    /// Reported RELEASED; will exit in its own time.
    Released,
}

#[derive(Clone, Debug)]
pub struct Worker {
    pub gen: u32,
    pub beat: u32,
    pub role: Role,
    pub active: bool,
    spawned_ns: u64,
    activated_ns: u64,
    active_ns: u64,
    /// The supervisor has started handling its death (gate closed, queue failed).
    dying: bool,
    died_ns: u64,
    repaired: Vec<bool>,
    /// Killed on purpose (a retired standby): its exit is not a failure.
    retired: bool,
    /// Its heartbeat has been seen under its own generation at least once.
    beating: bool,
    /// Condemned and sent SIGKILL: the death path takes it from here.
    condemned: bool,
    /// The model it loads.
    pub model: u32,
    /// Its staged registry, verified, and its fingerprint: set once the verdict is in.
    registry: Option<(Vec<u8>, u64)>,
}

/// The fields of a [`Worker`] the per-tick checks read, copied without its registry.
#[derive(Clone, Copy)]
struct Glance {
    gen: u32,
    beat: u32,
    role: Role,
    active: bool,
    dying: bool,
    condemned: bool,
    beating: bool,
    spawned_ns: u64,
    activated_ns: u64,
    active_ns: u64,
}

impl Glance {
    fn of(w: &Worker) -> Glance {
        Glance {
            gen: w.gen,
            beat: w.beat,
            role: w.role,
            active: w.active,
            dying: w.dying,
            condemned: w.condemned,
            beating: w.beating,
            spawned_ns: w.spawned_ns,
            activated_ns: w.activated_ns,
            active_ns: w.active_ns,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reset {
    Idle,
    /// Requested with the client attached: waiting for it to close its end.
    AwaitClose {
        since: u64,
    },
    /// DRAIN is set; waiting for DRAINED, or for the consumer to be gone.
    Draining {
        since: u64,
        drained: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reload {
    Idle,
    /// Waiting for a free beat slot to start the new worker in.
    Starting,
    Loading {
        new: u32,
    },
    HandingOver {
        old: u32,
        new: u32,
        since: u64,
        /// The new model's registry differs: the handover runs behind the gate (§5.6).
        changed: bool,
    },
    Activating {
        new: u32,
    },
    /// Memory for one worker only (§5.6): the old worker is asked to release, behind the gate,
    /// and the new one starts only once it has exited.
    Releasing {
        old: u32,
        since: u64,
    },
    /// The old worker released or died; its exit, which frees its memory, comes next.
    Exiting {
        old: u32,
    },
}

/// Counters for /modelz, the admin socket, and the tests.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub spawns: u64,
    pub deaths: u64,
    pub promotions: u64,
    pub restarts: u64,
    pub resets: u64,
    pub reloads: u64,
    pub condemned: u64,
    /// From a death's first report to the successor's ACTIVE: of the last recovery by any
    /// path, and of the last one that promoted a warm standby.
    pub last_recovery_ns: u64,
    pub max_recovery_ns: u64,
    pub last_promotion_ns: u64,
    pub max_promotion_ns: u64,
    pub last_cause: String,
    /// The memory utilization of the last sample, when there is a limit.
    pub util: f64,
    pub shedding: bool,
    pub standby_suppressed: bool,
    pub recycles: u64,
    pub retired_standbys: u64,
    pub does_not_fit: bool,
    /// A requested reset is waiting for a client that has not closed its socket.
    pub reset_stuck: bool,
    pub refusal_condemns: u64,
    pub rollbacks: u64,
    /// Models refused: a digest that does not match the pin, or rolled back from.
    pub rejected: u64,
    /// Loads killed at the load timeout (§5.2).
    pub load_timeouts: u64,
    /// Handovers (§5.6) that ended with the new worker activated, and those that did not
    /// (the old worker died or was condemned first, or its successor was gone).
    pub handoffs_ok: u64,
    pub handoffs_failed: u64,
    /// Of `resets`, those requested (by the client, or after a ring fault) rather than the
    /// host's exit.
    pub resets_requested: u64,
}

/// What the model block says (shm-abi §3): written before a generation is activated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Published {
    pub model: u32,
    pub sha256: [u8; 32],
    pub registry: Vec<u8>,
    pub fingerprint: u64,
    pub epoch: u32,
}

/// A newly loaded model within its rollback window (§5.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Trial {
    model: u32,
    served_ns: u64,
    deaths: u32,
}

/// Which model new workers load, what is known about each, and what a rollback returns to.
#[derive(Clone, Debug, Default)]
pub struct Models {
    pub target: u32,
    pub digests: BTreeMap<u32, [u8; 32]>,
    /// Refused: unreadable, against the pin, or rolled back from. Not loaded until forced or
    /// replaced by another file.
    pub bad: std::collections::BTreeSet<u32>,
    pub previous: Option<u32>,
    trial: Option<Trial>,
    /// A target set while a reload was running, applied when it ends.
    pending: Option<u32>,
    /// The file the model path named last: what a forced reload loads.
    pub latest: Option<u32>,
}

pub struct Core<'a> {
    pub cfg: Config,
    ctl: Seg<'a>,
    data: Seg<'a>,
    dir: &'a Directory,
    holds: BTreeMap<&'static str, (u32, u32)>,
    gate_gen: u32,
    next_gen: u32,
    pub workers: Vec<Worker>,
    consumer: Option<u32>,
    failures: Vec<u64>,
    next_spawn_ns: u64,
    bound: bool,
    reset: Reset,
    reload: Reload,
    reload_pending: bool,
    /// The running reload had memory for one worker: the old one exited before the new one
    /// started, so a failed load returns to the previous model (§5.6).
    reload_alone: bool,
    shutdown: Option<u64>,
    finished: bool,
    last_tick: u64,
    last_hb: u64,
    grace_until: u64,
    ring_watch: Vec<(HeadState, u64, u64)>,
    held_scan_ns: u64,
    suspects: Vec<(usize, u32, u32)>,
    /// When the death being recovered from was first reported, and whether a standby took over.
    death_at: Option<u64>,
    promoting: bool,
    /// Memory: PSI totals over the last window, when shedding began, when utilization fell
    /// below the recovery line, each worker's private memory when it reported WARM.
    psi: Vec<(u64, u64, u64)>,
    shed_since: Option<u64>,
    below_since: Option<u64>,
    baseline: BTreeMap<u32, u64>,
    private: BTreeMap<u32, u64>,
    trim_at: u64,
    /// Refusals: (time, client_rejects, completions) samples over the last second.
    refusals: Vec<(u64, u64, u64)>,
    /// The last gate words and when they were written, for `sczctl status`.
    transitions: std::collections::VecDeque<(u64, u64)>,
    pub models: Models,
    pub published: Option<Published>,
    last_serve_ns: u64,
    pub stats: Stats,
}

impl<'a> Core<'a> {
    pub fn new(cfg: Config, ctl: Seg<'a>, data: Seg<'a>, dir: &'a Directory, now: u64) -> Core<'a> {
        let rings = dir.layout.rings.len();
        let mut c = Core {
            cfg,
            ctl,
            data,
            dir,
            holds: BTreeMap::new(),
            gate_gen: 0,
            next_gen: 1,
            workers: Vec::new(),
            consumer: None,
            failures: Vec::new(),
            next_spawn_ns: now,
            bound: false,
            reset: Reset::Idle,
            reload: Reload::Idle,
            reload_pending: false,
            reload_alone: false,
            shutdown: None,
            finished: false,
            last_tick: now,
            last_hb: 0,
            grace_until: now,
            ring_watch: vec![(HeadState::Empty, 0, now); rings],
            held_scan_ns: now,
            suspects: Vec::new(),
            death_at: None,
            promoting: false,
            psi: Vec::new(),
            shed_since: None,
            below_since: None,
            baseline: BTreeMap::new(),
            private: BTreeMap::new(),
            trim_at: 0,
            refusals: Vec::new(),
            transitions: std::collections::VecDeque::new(),
            // Model 0 is "no model file": the raw workers' /dev/null, with a digest of zeros.
            models: Models { digests: [(0, [0u8; 32])].into(), ..Default::default() },
            published: None,
            last_serve_ns: now,
            stats: Stats::default(),
        };
        c.holds.insert("init", (gs::INIT, gr::NO_MODEL));
        c.publish(now);
        acc::control_header::set_supervisor_hb_mono_ns(&c.ctl, 0, now);
        c
    }

    // ── queries for the shell ──

    /// May a new session be accepted? (Otherwise the client is told BUSY.)
    pub fn accepting(&self) -> bool {
        !self.bound && self.reset == Reset::Idle && self.shutdown.is_none()
    }
    pub fn finished(&self) -> bool {
        self.finished
    }
    pub fn gate(&self) -> u64 {
        acc::control_header::gate(&self.ctl, 0)
    }
    pub fn healthy(&self) -> bool {
        word::gate_state(self.gate()) == gs::HEALTHY
    }
    pub fn consumer(&self) -> Option<u32> {
        self.consumer
    }
    pub fn worker(&self, gen: u32) -> Option<&Worker> {
        self.workers.iter().find(|w| w.gen == gen)
    }
    /// The clients' counters on the status page: responses refused, and ring faults reported.
    pub fn client_counters(&self) -> (u64, u64) {
        let base = self.dir.status_off as usize;
        (acc::status_page::client_rejects(&self.data, base), acc::status_page::client_ring_faults(&self.data, base))
    }
    pub fn crashloop(&self) -> bool {
        self.failures.len() >= 3
    }
    pub fn holds(&self) -> Vec<&'static str> {
        self.holds.keys().copied().collect()
    }

    // ── the gate (§5.7): paths take holds; one function writes the word ──

    fn hold(&mut self, path: &'static str, state: u32, reason: u32, now: u64) {
        self.holds.insert(path, (state, reason));
        self.publish(now);
    }
    fn release(&mut self, path: &'static str, now: u64) {
        if self.holds.remove(path).is_some() {
            self.publish(now);
        }
    }
    fn publish(&mut self, now: u64) {
        // DRAINING > DEAD > RELOADING > PSI_SHEDDING > INIT: the state numbers follow the rank.
        let w = match self.holds.values().max_by_key(|h| h.0) {
            Some(&(st, reason)) => word::gate(self.gate_gen, reason, st),
            None => word::gate(self.gate_gen, gr::NONE, gs::HEALTHY),
        };
        if acc::control_header::gate(&self.ctl, 0) != w {
            acc::control_header::set_gate(&self.ctl, 0, w);
            acc::control_header::set_gate_changed_mono_ns(&self.ctl, 0, now);
            if self.transitions.len() == 8 {
                self.transitions.pop_front();
            }
            self.transitions.push_back((w, now));
        }
        full_fence(); // the gate store before any sweep that follows (shm-abi §6.3)
    }

    // ── arena operations ──

    fn ring(&self, i: usize) -> scz_shm::seg::ShmRing<'a> {
        self.dir.ring(&self.data, i)
    }
    fn rings(&self) -> usize {
        self.dir.layout.rings.len()
    }
    fn for_each_slot(&self, mut f: impl FnMut(scz_shm::seg::ShmSlot<'a>)) {
        for (class, &(_, _, c)) in self.dir.layout.slabs.iter().enumerate() {
            for i in 0..c.slot_count {
                f(self.dir.slot(&self.data, class, i));
            }
        }
    }
    /// Sweep part 2, steps 2 and 3: fail every READY slot and wake its waiter; wake every DONE
    /// slot, whose worker may have died before waking its client.
    fn fail_queued(&self) -> (u32, u32) {
        let (mut failed, mut woken) = (0, 0);
        self.for_each_slot(|s| {
            if slot::fail_if_ready(&s) {
                failed += 1;
            } else if slot::wake_if_done(&s) {
                woken += 1;
            }
        });
        (failed, woken)
    }
    /// Sweep part 1: the slots the worker in `beat` held.
    fn sweep_owned(&self, beat: u32) -> u32 {
        let mut n = 0;
        self.for_each_slot(|s| {
            if slot::fail_if_owned(&s, beat + 1) {
                n += 1;
            }
        });
        n
    }
    fn set_ctl(&self, bit: u32, on: bool) {
        let v = acc::control_header::ctl_req(&self.ctl, 0);
        acc::control_header::set_ctl_req(&self.ctl, 0, if on { v | bit } else { v & !bit });
        full_fence();
        for i in 0..self.rings() {
            ring::wake_if_parked(&self.ring(i)); // the control wake (signaling §3.1)
        }
        standby::bump_and_wake(&ShmStandby(self.ctl)); // a paused worker waits there
    }
    fn reinit(&self) {
        scz_shm::layout::reset_data(&self.data, self.dir);
        let epoch = acc::control_header::session_epoch(&self.ctl, 0);
        acc::control_header::set_session_epoch(&self.ctl, 0, epoch.wrapping_add(1));
        full_fence();
    }

    // ── workers ──

    fn idx(&self, gen: u32) -> Option<usize> {
        self.workers.iter().position(|w| w.gen == gen)
    }
    fn free_beat(&self) -> Option<u32> {
        (0..abi::BEAT_SLOTS as u32).find(|b| !self.workers.iter().any(|w| w.beat == *b))
    }
    fn spawn(&mut self, now: u64, fx: &mut impl Effects) -> Option<u32> {
        let beat = self.free_beat()?;
        let gen = self.next_gen;
        self.next_gen += 1;
        self.stats.spawns += 1;
        let rings = self.rings();
        self.workers.push(Worker {
            gen,
            beat,
            role: Role::Loading,
            active: false,
            spawned_ns: now,
            activated_ns: 0,
            active_ns: 0,
            dying: false,
            died_ns: 0,
            repaired: vec![false; rings],
            retired: false,
            beating: false,
            condemned: false,
            model: self.models.target,
            registry: None,
        });
        if fx.spawn(gen, beat, self.models.target) {
            fx.log(&format!("worker gen {gen} spawned in beat slot {beat}"));
            Some(gen)
        } else {
            fx.log(&format!("worker gen {gen} failed to start"));
            self.workers.retain(|w| w.gen != gen);
            self.failure(now);
            None
        }
    }
    fn failure(&mut self, now: u64) {
        self.failures.push(now);
        let n = self.failures.len() as u32;
        let delay = (self.cfg.backoff_min_ns << (n - 1).min(16)).min(self.cfg.backoff_max_ns);
        self.next_spawn_ns = now + delay;
    }
    /// Activate a WARM worker: the gate carries its generation, and standby_wait is bumped.
    fn activate(&mut self, gen: u32, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        // The model block before the gate shows the generation (shm-abi §3).
        self.publish_model(i, fx);
        if self.reload_new() == gen {
            self.reload = Reload::Activating { new: gen };
        }
        self.workers[i].role = Role::Consumer;
        self.workers[i].activated_ns = now;
        self.consumer = Some(gen);
        self.gate_gen = gen;
        self.publish(now);
        standby::bump_and_wake(&ShmStandby(self.ctl));
        fx.log(&format!("worker gen {gen} activated"));
    }
    /// Write the model block for the worker about to be activated: its model's digest, its
    /// generation, and its verified registry, with the epoch raised if the registry changed.
    fn publish_model(&mut self, i: usize, fx: &mut impl Effects) {
        let w = &self.workers[i];
        let (registry, fingerprint) = w.registry.clone().unwrap_or_default();
        let sha256 = self.models.digests.get(&w.model).copied().unwrap_or_default();
        let epoch = match &self.published {
            Some(p) if p.registry == registry => p.epoch,
            Some(p) => p.epoch.wrapping_add(1),
            None => 1,
        };
        if self.published.as_ref().is_some_and(|p| p.epoch != epoch) {
            fx.log(&format!("registry epoch {epoch}: the schema changed (fingerprint {fingerprint:#018x})"));
        }
        let block = scz_shm::model::ModelBlock {
            sha256,
            model_gen: w.gen,
            registry_epoch: epoch,
            schema_fingerprint: fingerprint,
            registry_len: registry.len() as u32,
        };
        scz_shm::model::write(&self.ctl, &block, &registry);
        self.published = Some(Published { model: w.model, sha256, registry, fingerprint, epoch });
    }
    /// Whether a WARM worker may be activated: its registry verified, its model's digest
    /// known, and its model the one new workers load (§3 step 9, §5.6).
    fn ready(&self, w: &Worker) -> bool {
        w.role == Role::Warm
            && !w.dying
            && w.registry.is_some()
            && w.model == self.models.target
            && self.models.digests.contains_key(&w.model)
            && !self.models.bad.contains(&w.model)
    }
    /// Move on after a verdict or a digest: a reload's handover, or the activation of a ready
    /// worker when there is no consumer.
    fn advance(&mut self, now: u64, fx: &mut impl Effects) {
        if self.shutdown.is_some() {
            return;
        }
        if let Reload::Loading { new } = self.reload {
            let Some(j) = self.idx(new) else { return };
            if !self.ready(&self.workers[j]) {
                return;
            }
            match self.consumer.filter(|&g| self.worker(g).is_some_and(|w| !w.dying)) {
                Some(old) => {
                    let changed = self.published.as_ref().is_none_or(|p| Some(&p.registry) != self.workers[j].registry.as_ref().map(|r| &r.0));
                    if changed {
                        // Queued requests were encoded for the old schema: callers fall back
                        // from here until the new worker is ACTIVE (§5.6).
                        self.hold("reload", gs::RELOADING, gr::RELOAD, now);
                    }
                    self.set_ctl(ctl_bit::HANDOVER, true);
                    self.reload = Reload::HandingOver { old, new, since: now, changed };
                    fx.log(&format!("handover: gen {old} asked to release its rings{}", if changed { ", behind the gate: the schema changed" } else { "" }));
                }
                None => self.activate(new, now, fx),
            }
            return;
        }
        if self.consumer.is_none() {
            if let Some(g) = self.workers.iter().find(|w| self.ready(w)).map(|w| w.gen) {
                self.activate(g, now, fx);
            }
        }
    }
    fn on_verdict(&mut self, gen: u32, result: Result<(Vec<u8>, u64), String>, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        if self.workers[i].role != Role::Warm || self.workers[i].dying {
            return;
        }
        let refuse = |c: &mut Self, why: String, fx: &mut dyn FnMut(&str)| {
            c.stats.last_cause = why.clone();
            fx(&why);
        };
        let is_new = self.reload_new() == gen;
        let differs = |c: &Self, b: &Vec<u8>| c.published.as_ref().is_some_and(|p| &p.registry != b) && c.consumer.is_some();
        match result {
            Ok((bytes, fp)) if !is_new && self.workers[i].model == self.published.as_ref().map_or(u32::MAX, |p| p.model) && differs(self, &bytes) => {
                // A standby of the serving model must stage what the consumer staged.
                refuse(self, format!("gen {gen} staged a registry unlike the one published for its model"), &mut |m| fx.log(m));
                let _ = fp;
                self.fail_load(gen, now, fx);
            }
            Ok((bytes, fp)) => {
                fx.log(&format!("worker gen {gen}: registry verified ({} bytes, fingerprint {fp:#018x})", bytes.len()));
                self.workers[i].registry = Some((bytes, fp));
                self.advance(now, fx);
            }
            Err(e) => {
                refuse(self, format!("gen {gen}: its registry was refused: {e}"), &mut |m| fx.log(m));
                self.fail_load(gen, now, fx);
            }
        }
    }
    /// A worker that loaded but cannot be used: killed, and counted as a failed load.
    fn fail_load(&mut self, gen: u32, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        self.workers[i].dying = true;
        fx.kill(gen);
        self.failure(now);
        if self.reload_new() == gen {
            self.abort_reload(now, fx);
        }
    }
    /// A reload whose new worker did not load: the old model keeps serving (§5.6). Where the
    /// old worker had to exit first, nothing is serving: the previous model is loaded again at
    /// once, and the one that failed stays rejected, as after a rollback.
    fn abort_reload(&mut self, now: u64, fx: &mut impl Effects) {
        let failed = self.models.target;
        self.reload = Reload::Idle;
        if self.reload_alone {
            self.next_spawn_ns = now;
            let Some(prev) = self.models.previous.take() else {
                fx.log("reload: the new worker did not load, and the old one has exited; loading the model again");
                return;
            };
            if self.models.bad.insert(failed) {
                self.stats.rejected += 1;
            }
            self.models.target = prev;
            self.models.trial = None;
            self.stats.rollbacks += 1;
            self.stats.last_cause = format!("model {failed} did not load, and the worker of model {prev} had already exited to make room");
            fx.log(&format!("rollback: {}; loading model {prev} again", self.stats.last_cause));
            if self.models.latest != Some(failed) {
                fx.release_model(failed);
            }
            return;
        }
        if let Some(prev) = self.models.previous.take() {
            self.models.target = prev;
            self.models.trial = None;
            if failed != prev && self.models.latest != Some(failed) {
                fx.release_model(failed);
            }
        }
        fx.log("reload: the new worker did not load; the old one keeps serving");
    }
    /// Too many deaths of a new model inside its window: load the previous one (§5.6).
    fn rollback(&mut self, now: u64, fx: &mut impl Effects) {
        let (Some(t), Some(prev)) = (self.models.trial.take(), self.models.previous.take()) else { return };
        self.models.bad.insert(t.model);
        self.models.target = prev;
        self.stats.rollbacks += 1;
        self.stats.rejected += 1;
        self.stats.last_cause = format!("model {} died {} times as the consumer within its rollback window", t.model, t.deaths);
        fx.log(&format!("rollback: {}; loading the previous model again", self.stats.last_cause));
        // A warm worker of the rejected model must not be promoted.
        let stale: Vec<u32> = self.workers.iter().filter(|w| w.model == t.model && w.role != Role::Consumer && !w.dying).map(|w| w.gen).collect();
        for g in stale {
            if let Some(i) = self.idx(g) {
                self.workers[i].retired = true;
                self.workers[i].dying = true;
            }
            fx.kill(g);
        }
        // Its descriptor stays open while the path names it: `reload --force` loads it.
        if self.models.latest != Some(t.model) {
            fx.release_model(t.model);
        }
        if matches!(self.reload, Reload::Loading { .. } | Reload::Starting) {
            self.reload = Reload::Idle;
        }
        let _ = now;
    }

    /// Close the gate and kill (§5.2, "condemning").
    fn condemn(&mut self, gen: u32, why: &str, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        if std::mem::replace(&mut self.workers[i].condemned, true) {
            return;
        }
        self.stats.condemned += 1;
        self.stats.last_cause = why.to_string();
        if self.workers[i].role == Role::Consumer && !self.workers[i].dying {
            self.hold("death", gs::DEAD, gr::WORKER_HUNG, now);
        }
        fx.log(&format!("worker gen {gen} condemned: {why}"));
        fx.kill(gen);
    }
    /// §5.3 steps 1–2, once per death: the gate, SIGKILL, the queued slots.
    fn death_begins(&mut self, gen: u32, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        if self.workers[i].dying {
            return;
        }
        self.workers[i].dying = true;
        self.workers[i].died_ns = now;
        self.death_at.get_or_insert(now);
        self.stats.deaths += 1;
        self.hold("death", gs::DEAD, gr::WORKER_EXIT, now);
        fx.kill(gen);
        let (failed, woken) = self.fail_queued();
        fx.log(&format!("worker gen {gen} died as the consumer; gate closed; {failed} queued request(s) failed, {woken} answered woken"));
        // A worker that had served long enough starts the failure count afresh (§5.5).
        if self.workers[i].active && now - self.workers[i].active_ns >= self.cfg.stable_ns {
            self.failures.clear();
        }
        self.failure(now);
        // A new model inside its rollback window (§5.6).
        let model = self.workers[i].model;
        if let Some(t) = self.models.trial.as_mut().filter(|t| t.model == model) {
            t.deaths += 1;
            if t.deaths >= self.cfg.rollback_deaths && self.cfg.rollback {
                self.rollback(now, fx);
            }
        }
        // A handover in progress ends on the crash path.
        if matches!(self.reload, Reload::HandingOver { .. }) {
            self.set_ctl(ctl_bit::HANDOVER, false);
            self.reload = Reload::Idle;
            self.stats.handoffs_failed += 1;
        }
        // Memory for one worker: the new model still waits for this one's exit.
        if let Reload::Releasing { old, .. } = self.reload {
            if old == gen {
                self.set_ctl(ctl_bit::HANDOVER, false);
                self.reload = Reload::Exiting { old };
                self.stats.handoffs_failed += 1;
            }
        }
    }
    /// §5.3 step 3, for one ring, once its consume thread is known to be gone.
    fn repair(&mut self, gen: u32, ring_i: usize, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        if ring_i >= self.workers[i].repaired.len() || self.workers[i].repaired[ring_i] {
            return;
        }
        self.workers[i].repaired[ring_i] = true;
        if ring::repair(&self.ring(ring_i)) {
            fx.log(&format!("ring {ring_i} repaired: the dead consumer had freed its head cell"));
        }
    }
    /// §5.3 step 4: once every ring is repaired, promote a warm worker, or spawn one.
    fn after_repair(&mut self, gen: u32, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        if self.consumer != Some(gen) || self.workers[i].repaired.iter().any(|r| !r) {
            return;
        }
        self.consumer = None;
        if let Some(n) = self.workers.iter().find(|w| self.ready(w)).map(|w| w.gen) {
            self.stats.promotions += 1;
            self.promoting = true;
            self.activate(n, now, fx);
        }
        // Otherwise a worker is spawned once the backoff allows (tick), or a warm one is
        // activated once its verdict is in (advance).
    }

    // ── events ──

    pub fn handle(&mut self, now: u64, ev: Event, fx: &mut impl Effects) {
        match ev {
            Event::Tick => self.tick(now, fx),
            Event::Memory(sample) => self.on_memory(sample, now, fx),
            Event::Throttled => {
                self.grace_until = now + self.cfg.hb_stale_ns;
                for w in &mut self.ring_watch {
                    w.2 = now;
                }
            }
            Event::Status { gen, byte } => self.on_status(gen, byte as u32, now, fx),
            Event::LockDied { gen, ring } => {
                let Some(i) = self.idx(gen) else { return };
                match self.workers[i].role {
                    Role::Consumer => {
                        self.death_begins(gen, now, fx);
                        self.repair(gen, ring, fx);
                        self.after_repair(gen, now, fx);
                    }
                    Role::Warm | Role::Loading => {
                        if !self.workers[i].dying {
                            self.workers[i].dying = true;
                            fx.log(&format!("worker gen {gen} died while warm"));
                            fx.kill(gen);
                        }
                    }
                    Role::Released => {} // it released; its threads are just exiting
                }
            }
            Event::Exited { gen } => self.on_exit(gen, now, fx),
            Event::SessionBound => self.bound = true,
            Event::SessionClosed => {
                let was_bound = std::mem::replace(&mut self.bound, false);
                if self.shutdown.is_none() && (was_bound || matches!(self.reset, Reset::AwaitClose { .. })) {
                    self.begin_drain(now, fx);
                }
            }
            Event::Reload => self.begin_reload(false, now, fx),
            Event::ForceReload => {
                // The file at the model path, even one rolled back from (§5.6).
                if let Some(m) = self.models.latest {
                    self.models.bad.remove(&m);
                    self.models.pending = Some(m);
                }
                self.begin_reload(true, now, fx)
            }
            Event::NewModel { model } => {
                if model != NO_MODEL {
                    // The file the path named before is needed only if something still uses it.
                    if let Some(old) = self.models.latest.replace(model) {
                        let used = old == self.models.target || self.models.previous == Some(old) || self.workers.iter().any(|w| w.model == old);
                        if !used {
                            fx.release_model(old);
                        }
                    }
                }
                if self.reload != Reload::Idle {
                    self.models.pending = Some(model);
                    self.reload_pending = true;
                    return;
                }
                fx.log(&format!("the model path names a new file (model {model})"));
                self.models.bad.remove(&model);
                if self.consumer.is_some() {
                    self.models.pending = Some(model);
                    self.begin_reload(false, now, fx);
                } else {
                    // Nothing is serving: load the new file from now on.
                    let old = std::mem::replace(&mut self.models.target, model);
                    let stale: Vec<u32> = self.workers.iter().filter(|w| w.model != model && !w.dying).map(|w| w.gen).collect();
                    for g in stale {
                        if let Some(i) = self.idx(g) {
                            self.workers[i].retired = true;
                        }
                        fx.kill(g);
                    }
                    if old != model {
                        fx.release_model(old);
                    }
                }
            }
            Event::Digest { model, sha } => {
                match sha {
                    Some(d) if self.cfg.pin.is_some_and(|p| p != d) => {
                        self.models.bad.insert(model);
                        self.stats.rejected += 1;
                        self.stats.last_cause = format!("model {model}'s digest {} does not match the pin", hex(&d));
                        fx.log(&self.stats.last_cause.clone());
                    }
                    Some(d) => {
                        fx.log(&format!("model {model}: sha256 {}", hex(&d)));
                        self.models.digests.insert(model, d);
                    }
                    None => {
                        self.models.bad.insert(model);
                        self.stats.last_cause = format!("model {model} could not be hashed");
                        fx.log(&self.stats.last_cause.clone());
                    }
                }
                if self.models.bad.contains(&model) {
                    // Workers already loading it are of no use.
                    let doomed: Vec<u32> = self.workers.iter().filter(|w| w.model == model && w.role != Role::Consumer && !w.dying).map(|w| w.gen).collect();
                    for g in doomed {
                        if let Some(i) = self.idx(g) {
                            self.workers[i].retired = true;
                        }
                        fx.kill(g);
                        if self.reload_new() == g {
                            self.abort_reload(now, fx);
                        }
                    }
                }
                self.advance(now, fx);
            }
            Event::Verdict { gen, result } => self.on_verdict(gen, result, now, fx),
            Event::RequestReset => self.request_reset("requested", now, fx),
            Event::Shutdown => {
                if self.shutdown.is_none() {
                    self.shutdown = Some(now);
                    fx.log("shutting down");
                }
            }
        }
    }

    /// §5.6: one reload at a time; triggers that arrive meanwhile collapse into one follow-up.
    fn begin_reload(&mut self, force: bool, now: u64, fx: &mut impl Effects) {
        if self.reload != Reload::Idle || self.consumer.is_none() {
            self.reload_pending = true;
            return;
        }
        let serving = self.consumer.and_then(|g| self.worker(g)).map(|w| w.model).unwrap_or(self.models.target);
        let target = self.models.pending.take().unwrap_or(self.models.target);
        if force {
            self.models.bad.remove(&target);
        }
        if self.models.bad.contains(&target) {
            fx.log(&format!("reload refused: model {target} was rejected; `sczctl reload --force` loads it anyway"));
            return;
        }
        self.models.target = target;
        if target != serving {
            // The previous model stays open until the new one has served its window.
            if let Some(old) = self.models.previous.replace(serving) {
                if old != serving {
                    fx.release_model(old);
                }
            }
            self.models.trial = Some(Trial { model: target, served_ns: 0, deaths: 0 });
        }
        self.stats.reloads += 1;
        // Retire the standby: it holds the old model (§5.6).
        let standby: Vec<u32> = self.workers.iter().filter(|w| matches!(w.role, Role::Warm | Role::Loading)).map(|w| w.gen).collect();
        for g in standby {
            if let Some(i) = self.idx(g) {
                self.workers[i].retired = true;
            }
            fx.kill(g);
        }
        // Without memory for two workers the old one goes first, and callers fall back for the
        // new model's load time (§5.6).
        self.reload_alone = !self.cfg.standby || self.stats.standby_suppressed;
        if self.reload_alone {
            let old = self.consumer.expect("checked above");
            self.hold("reload", gs::RELOADING, gr::RELOAD, now);
            if self.worker(old).is_some_and(|w| w.dying) {
                self.reload = Reload::Exiting { old };
            } else {
                self.set_ctl(ctl_bit::HANDOVER, true);
                self.reload = Reload::Releasing { old, since: now };
            }
            fx.log(&format!("reload: memory for one worker: gen {old} releases and exits before model {target} is loaded; callers fall back meanwhile"));
            return;
        }
        self.reload = Reload::Starting;
        fx.log(&format!("reload: starting a new worker with model {target} to hand over to"));
    }

    fn on_status(&mut self, gen: u32, byte: u32, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        match byte {
            sb::WARM => {
                if self.workers[i].role != Role::Loading || self.workers[i].retired {
                    return;
                }
                self.workers[i].role = Role::Warm;
                fx.watch(gen, self.rings());
                if let Some(&p) = self.private.get(&gen) {
                    self.baseline.insert(gen, p);
                }
                // A freshly warmed worker alone at the relief line: the model does not fit (§6).
                if self.workers.len() == 1 && self.stats.util >= self.cfg.relieve_at {
                    self.stats.does_not_fit = true;
                    self.hold("pressure", gs::PSI_SHEDDING, gr::DOES_NOT_FIT, now);
                    fx.log(&format!("worker gen {gen} alone is at {:.0}% of the memory limit: the model does not fit", self.stats.util * 100.0));
                }
                fx.log(&format!("worker gen {gen} WARM after {:.1} ms", (now - self.workers[i].spawned_ns) as f64 / 1e6));
                // Its staged registry is verified before it can be activated (§3 step 9).
                if let Some(r) = fx.verify(gen) {
                    self.on_verdict(gen, r, now, fx);
                }
            }
            sb::ACTIVE => {
                if self.consumer != Some(gen) {
                    return;
                }
                self.workers[i].active = true;
                self.workers[i].active_ns = now;
                // The successor of a dead worker: report how long the death cost the gate.
                if let Some(dead) = self.death_at.take() {
                    let took = now.saturating_sub(dead);
                    self.stats.last_recovery_ns = took;
                    self.stats.max_recovery_ns = self.stats.max_recovery_ns.max(took);
                    if std::mem::take(&mut self.promoting) {
                        self.stats.last_promotion_ns = took;
                        self.stats.max_promotion_ns = self.stats.max_promotion_ns.max(took);
                    }
                    fx.log(&format!("gate reopened {:.2} ms after the death was reported", took as f64 / 1e6));
                }
                self.release("init", now);
                self.release("death", now);
                // A lone reload's hold lasts until the new model's worker is ACTIVE, not the
                // old one's (it may report ACTIVE after the reload began).
                if !matches!(self.reload, Reload::Releasing { .. } | Reload::Exiting { .. }) {
                    self.release("reload", now);
                }
                if let Reload::Activating { new } = self.reload {
                    if new == gen {
                        self.reload = Reload::Idle;
                        fx.log(&format!("reload: gen {gen} is serving"));
                        if !self.cfg.rollback {
                            if let Some(prev) = self.models.previous.take() {
                                fx.release_model(prev);
                            }
                            self.models.trial = None;
                        }
                        if std::mem::take(&mut self.reload_pending) {
                            self.handle(now, Event::Reload, fx);
                        }
                    }
                }
                fx.log(&format!("worker gen {gen} ACTIVE; gate {}", gs::name(word::gate_state(self.gate()))));
            }
            sb::DRAINED => {
                if let Reset::Draining { since, .. } = self.reset {
                    if self.consumer == Some(gen) {
                        self.reset = Reset::Draining { since, drained: true };
                        self.try_finish_reset(now, fx);
                    }
                }
            }
            sb::RELEASED => {
                if let Reload::Releasing { old, .. } = self.reload {
                    if old == gen {
                        // Nothing consumes until the new model has loaded: what is queued fails.
                        self.workers[i].role = Role::Released;
                        self.workers[i].activated_ns = now;
                        self.set_ctl(ctl_bit::HANDOVER, false);
                        self.consumer = None;
                        let (failed, _) = self.fail_queued();
                        self.reload = Reload::Exiting { old };
                        self.stats.handoffs_ok += 1;
                        fx.log(&format!("reload: gen {old} released; {failed} queued request(s) failed; waiting for its exit"));
                    }
                    return;
                }
                let Reload::HandingOver { old, new, changed, .. } = self.reload else { return };
                if old != gen {
                    return;
                }
                self.workers[i].role = Role::Released;
                self.workers[i].activated_ns = now; // the release grace runs from here
                self.set_ctl(ctl_bit::HANDOVER, false);
                let new_ok = self.idx(new).is_some_and(|j| self.workers[j].role == Role::Warm && !self.workers[j].dying);
                if new_ok {
                    self.stats.handoffs_ok += 1;
                } else {
                    self.stats.handoffs_failed += 1;
                }
                if new_ok && changed {
                    // Behind the gate: nothing queued for the old schema reaches the new model.
                    let (failed, _) = self.fail_queued();
                    self.activate(new, now, fx);
                    fx.log(&format!("handover: gen {old} released; {failed} queued request(s) failed; gen {new} activated behind the gate"));
                } else if new_ok {
                    // One store: the generation changes, the state does not (§5.6).
                    self.activate(new, now, fx);
                    fx.log(&format!("handover: gen {old} released; gen {new} activated with the gate open"));
                } else {
                    self.consumer = None;
                    self.hold("reload", gs::RELOADING, gr::RELOAD, now);
                    let (failed, _) = self.fail_queued();
                    self.reload = Reload::Idle;
                    fx.log(&format!("handover: gen {old} released but its successor is gone; {failed} queued failed; spawning"));
                }
            }
            _ => {}
        }
    }

    fn on_exit(&mut self, gen: u32, now: u64, fx: &mut impl Effects) {
        let Some(i) = self.idx(gen) else { return };
        let w = self.workers[i].clone();
        // A consumer whose locks never reported (it died before taking them): all of §5.3 here.
        if w.role == Role::Consumer {
            self.death_begins(gen, now, fx);
            for r in 0..self.rings() {
                self.repair(gen, r, fx);
            }
            self.after_repair(gen, now, fx);
        }
        // Part 1 of the sweep, on the pidfd only.
        let owned = self.sweep_owned(w.beat);
        if owned > 0 {
            fx.log(&format!("worker gen {gen}: {owned} slot(s) it held failed or freed"));
        }
        match w.role {
            Role::Loading if !w.retired => {
                self.stats.last_cause = format!("gen {gen} exited while loading");
                self.failure(now);
            }
            Role::Warm if !w.retired && !w.dying => self.failure(now),
            _ => {}
        }
        if let Reload::Loading { new } = self.reload {
            if new == gen {
                self.abort_reload(now, fx);
            }
        }
        if let Reload::HandingOver { new, .. } = self.reload {
            if new == gen {
                fx.log("reload: the new worker died during the handover");
            }
        }
        self.workers.remove(i);
        self.baseline.remove(&gen);
        self.private.remove(&gen);
        fx.log(&format!("worker gen {gen} exited and was reaped; beat slot {} is free", w.beat));
        // Its memory is free: the new model is loaded now (§5.6).
        if self.reload == (Reload::Exiting { old: gen }) {
            self.reload = Reload::Starting;
            self.spawn_tick(now, fx);
        }
    }

    // ── session reset (§4) ──

    fn request_reset(&mut self, why: &str, now: u64, fx: &mut impl Effects) {
        if self.reset != Reset::Idle || self.shutdown.is_some() {
            return;
        }
        self.stats.resets += 1;
        self.stats.resets_requested += 1;
        self.hold("reset", gs::RELOADING, gr::SESSION_RESET, now);
        let (failed, _) = self.fail_queued();
        fx.log(&format!("session reset requested ({why}); {failed} queued request(s) failed"));
        if self.bound {
            self.reset = Reset::AwaitClose { since: now };
            fx.end_session();
        } else {
            self.begin_drain(now, fx);
        }
    }
    fn begin_drain(&mut self, now: u64, fx: &mut impl Effects) {
        if self.reset != Reset::Idle && !matches!(self.reset, Reset::AwaitClose { .. }) {
            return;
        }
        if !matches!(self.reset, Reset::AwaitClose { .. }) {
            self.stats.resets += 1;
        }
        self.hold("reset", gs::RELOADING, gr::SESSION_RESET, now);
        self.reset = Reset::Draining { since: now, drained: false };
        let live = self.consumer.and_then(|g| self.idx(g)).is_some_and(|i| !self.workers[i].dying);
        if live {
            self.set_ctl(ctl_bit::DRAIN, true);
            fx.log("session reset: draining the worker");
        }
        self.try_finish_reset(now, fx);
    }
    /// The arena is re-initialized only when nothing can touch it: the consumer has paused (or
    /// there is none), and no dying worker, whose runtime may still write a slot until it
    /// exits, is left.
    fn try_finish_reset(&mut self, now: u64, fx: &mut impl Effects) {
        let Reset::Draining { drained, .. } = self.reset else { return };
        let consumer_gone = self.consumer.and_then(|g| self.idx(g)).is_none_or(|i| self.workers[i].dying);
        if (drained || consumer_gone) && self.workers.iter().all(|w| !w.dying) {
            self.finish_reset(now, fx);
        }
    }
    fn finish_reset(&mut self, now: u64, fx: &mut impl Effects) {
        self.reinit();
        self.set_ctl(ctl_bit::DRAIN, false);
        self.reset = Reset::Idle;
        self.stats.reset_stuck = false;
        self.release("reset", now);
        fx.log("session reset: rings and slots re-initialized");
    }

    // ── the tick: timers and the watchdog (§5.2) ──

    fn tick(&mut self, now: u64, fx: &mut impl Effects) {
        // The stall discount: a late tick means the supervisor was stalled too.
        if now.saturating_sub(self.last_tick) > self.cfg.hb_interval_ns {
            self.grace_until = now + self.cfg.hb_stale_ns;
            for w in &mut self.ring_watch {
                w.2 = now;
            }
        }
        self.last_tick = now;
        if now.saturating_sub(self.last_hb) >= self.cfg.hb_interval_ns {
            acc::control_header::set_supervisor_hb_mono_ns(&self.ctl, 0, now);
            self.last_hb = now;
        }
        if self.shutdown_tick(now, fx) {
            return;
        }
        self.failures.retain(|&t| now - t < self.cfg.stable_ns * 6);
        if let Some(c) = self.consumer.and_then(|g| self.worker(g)) {
            if c.active && now - c.active_ns >= self.cfg.stable_ns && !self.failures.is_empty() {
                self.failures.clear();
            }
        }
        self.serve_tick(now, fx);
        self.spawn_tick(now, fx);
        self.trim_tick(now);
        self.watch_refusals(now, fx);
        self.watch_workers(now, fx);
        self.watch_rings(now, fx);
        if now.saturating_sub(self.held_scan_ns) >= 50 * MS {
            self.held_scan_ns = now;
            self.watch_held(now, fx);
        }
        match self.reset {
            Reset::Draining { since, drained: false } if now - since > self.cfg.drain_timeout_ns => {
                if let Some(g) = self.consumer {
                    self.condemn(g, "did not report DRAINED", now, fx);
                    self.reset = Reset::Draining { since: now, drained: false };
                }
            }
            Reset::AwaitClose { since } if now - since > self.cfg.session_grace_ns && !self.stats.reset_stuck => {
                self.stats.reset_stuck = true;
                fx.log("session reset: the client has not closed its socket; the gate stays closed");
            }
            _ => {}
        }
        self.try_finish_reset(now, fx);
    }

    /// The rollback window counts time served: a worker of the new model active with the gate
    /// healthy (§5.6). Loading, or backing off after a death, does not count.
    fn serve_tick(&mut self, now: u64, fx: &mut impl Effects) {
        let dt = now.saturating_sub(self.last_serve_ns);
        self.last_serve_ns = now;
        let Some(t) = self.models.trial else { return };
        let serving = self.healthy() && self.consumer.and_then(|g| self.worker(g)).is_some_and(|w| w.active && w.model == t.model);
        if serving {
            let served = t.served_ns + dt;
            self.models.trial = Some(Trial { served_ns: served, ..t });
            if served >= self.cfg.rollback_window_ns {
                self.models.trial = None;
                if let Some(prev) = self.models.previous.take() {
                    fx.log(&format!("model {} has served its rollback window; the previous model is released", t.model));
                    fx.release_model(prev);
                }
            }
        }
    }

    fn spawn_tick(&mut self, now: u64, fx: &mut impl Effects) {
        // A reload whose model was refused (its digest) before its worker could start.
        if self.reload == Reload::Starting && self.models.bad.contains(&self.models.target) {
            self.abort_reload(now, fx);
        }
        if self.shutdown.is_some() || now < self.next_spawn_ns || self.models.bad.contains(&self.models.target) || self.models.target == NO_MODEL {
            return;
        }
        // Memory for one worker: nothing starts until the old one has exited.
        if matches!(self.reload, Reload::Releasing { .. } | Reload::Exiting { .. }) {
            return;
        }
        let loading = self.workers.iter().any(|w| w.role == Role::Loading);
        let warm = self.workers.iter().any(|w| w.role == Role::Warm && !w.dying);
        if self.reload == Reload::Starting && !loading {
            if let Some(g) = self.spawn(now, fx) {
                self.reload = Reload::Loading { new: g };
            }
            return;
        }
        if self.consumer.is_none() && !loading && !warm {
            let first = self.stats.spawns == 0;
            if self.spawn(now, fx).is_some() && !first {
                self.stats.restarts += 1;
            }
            return;
        }
        let active = self.consumer.and_then(|g| self.worker(g)).is_some_and(|w| w.active);
        if self.cfg.standby && active && self.reload == Reload::Idle && !loading && !warm && self.reset == Reset::Idle && !self.stats.standby_suppressed {
            self.spawn(now, fx);
        }
    }

    fn watch_workers(&mut self, now: u64, fx: &mut impl Effects) {
        // By index, on a copy of the few fields read: this runs every millisecond, and nothing
        // here removes a worker (only an exit does).
        for k in 0..self.workers.len() {
            let w = Glance::of(&self.workers[k]);
            if w.dying || w.condemned {
                continue;
            }
            let beat = self.dir.beat(w.beat as usize);
            let (bgen, hb) = (acc::beat::gen(&self.data, beat), acc::beat::hb_mono_ns(&self.data, beat));
            if bgen == w.gen && !w.beating {
                if let Some(i) = self.idx(w.gen) {
                    self.workers[i].beating = true;
                }
            }
            let stale = bgen == w.gen && now > self.grace_until && hb.saturating_add(self.cfg.hb_stale_ns) < now;
            match w.role {
                Role::Loading if now - w.spawned_ns > self.cfg.load_timeout_ns => {
                    self.stats.load_timeouts += 1;
                    self.stats.last_cause = format!("gen {} did not load in time", w.gen);
                    fx.log(&self.stats.last_cause.clone());
                    fx.kill(w.gen);
                    if let Some(i) = self.idx(w.gen) {
                        self.workers[i].dying = true;
                    }
                }
                Role::Warm if stale => {
                    fx.log(&format!("standby gen {} stopped beating; killed", w.gen));
                    fx.kill(w.gen);
                    if let Some(i) = self.idx(w.gen) {
                        self.workers[i].dying = true;
                    }
                }
                Role::Consumer if stale => self.condemn(w.gen, "heartbeat stale", now, fx),
                Role::Consumer if !w.active && now - w.activated_ns > self.cfg.activate_timeout_ns => {
                    self.condemn(w.gen, "no ACTIVE after activation", now, fx)
                }
                Role::Released if now - w.activated_ns.max(w.spawned_ns) > self.cfg.release_grace_ns => {
                    fx.kill(w.gen);
                }
                _ => {}
            }
        }
        if let Reload::HandingOver { old, since, .. } = self.reload {
            if now - since > self.cfg.handover_timeout_ns {
                self.condemn(old, "no RELEASED within the handover timeout", now, fx);
                if let Reload::HandingOver { new, changed, .. } = self.reload {
                    self.reload = Reload::HandingOver { old, new, since: now, changed };
                }
            }
        }
        if let Reload::Releasing { old, since } = self.reload {
            if now - since > self.cfg.handover_timeout_ns {
                // Its death moves the reload on to waiting for the exit.
                self.condemn(old, "no RELEASED within the handover timeout", now, fx);
                if let Reload::Releasing { .. } = self.reload {
                    self.reload = Reload::Releasing { old, since: now };
                }
            }
        }
    }
    fn reload_new(&self) -> u32 {
        match self.reload {
            Reload::Loading { new } | Reload::HandingOver { new, .. } | Reload::Activating { new } => new,
            _ => 0,
        }
    }

    /// A hung consume thread, a stuck head cell, an inconsistent ring.
    fn watch_rings(&mut self, now: u64, fx: &mut impl Effects) {
        let consumer = self.consumer.and_then(|g| self.worker(g)).filter(|w| w.active && !w.dying).map(|w| w.gen);
        for i in 0..self.rings() {
            let (st, head) = ring::head_state(&self.ring(i));
            let (pst, phead, since) = self.ring_watch[i];
            if st != pst || head != phead {
                self.ring_watch[i] = (st, head, now);
                continue;
            }
            if now - since <= self.cfg.hang_ns || self.reset != Reset::Idle {
                continue;
            }
            match st {
                HeadState::Published | HeadState::Freed => {
                    if let Some(g) = consumer {
                        if self.reload == Reload::Idle {
                            self.condemn(g, &format!("ring {i} has work and its head has not moved"), now, fx);
                        }
                    }
                }
                HeadState::Claimed => self.request_reset(&format!("ring {i}'s head cell claimed and never published"), now, fx),
                HeadState::Inconsistent => self.request_reset(&format!("ring {i} is inconsistent"), now, fx),
                HeadState::Empty => {}
            }
            self.ring_watch[i].2 = now;
        }
    }

    /// A slot the consumer has held in PROCESSING or ABANDONED for too long, seen on two scans
    /// (the worker writes t_dequeue_ns just after its claim, so one scan could see an old one).
    fn watch_held(&mut self, now: u64, fx: &mut impl Effects) {
        let Some(c) = self.consumer.and_then(|g| self.worker(g)).filter(|w| w.active && !w.dying).map(Glance::of) else {
            self.suspects.clear();
            return;
        };
        let mut found = Vec::new();
        let data = self.data;
        let hang = self.cfg.hang_ns;
        let mut idx = 0usize;
        self.for_each_slot(|s| {
            let w = slot::SlotMem::word(&s).load(Relaxed);
            let st = word::state(w);
            if (st == ss::PROCESSING || st == ss::ABANDONED) && word::owner(w) == c.beat + 1 {
                let t = acc::slot_record::t_dequeue_ns(&data, s.rec);
                if t.saturating_add(hang) < now {
                    found.push((idx, word::epoch(w), st));
                }
            }
            idx += 1;
        });
        let confirmed = found.iter().any(|f| self.suspects.contains(f));
        self.suspects = found;
        if confirmed {
            self.condemn(c.gen, "a slot held for longer than the hang limit", now, fx);
            self.suspects.clear();
        }
    }

    /// The admin socket's `status` reply (supervisor §7): a fixed-size, little-endian struct.
    /// Layout in `STATUS_LAYOUT`; `sczctl` decodes it.
    pub fn status_struct(&self, bound: bool) -> [u8; STATUS_LEN] {
        let mut b = [0u8; STATUS_LEN];
        let mut put = |off: usize, v: u64, w: usize| b[off..off + w].copy_from_slice(&v.to_le_bytes()[..w]);
        put(0, STATUS_MAGIC as u64, 4);
        put(4, 2, 4);
        put(8, self.gate(), 8);
        put(16, self.consumer.unwrap_or(0) as u64, 4);
        put(20, self.workers.len().min(2) as u64, 4);
        for (k, w) in self.workers.iter().take(2).enumerate() {
            let role = match w.role {
                Role::Loading => 1,
                Role::Warm => 2,
                Role::Consumer => 3,
                Role::Released => 4,
            };
            let flags = u64::from(w.active) | u64::from(w.dying) << 1 | u64::from(w.condemned) << 2 | u64::from(w.retired) << 3;
            put(24 + k * 16, w.gen as u64, 4);
            put(28 + k * 16, w.beat as u64, 4);
            put(32 + k * 16, role, 4);
            put(36 + k * 16, flags, 4);
        }
        let s = &self.stats;
        for (k, v) in [s.spawns, s.deaths, s.promotions, s.restarts, s.resets, s.reloads, s.condemned, s.recycles, s.retired_standbys, s.refusal_condemns]
            .iter()
            .enumerate()
        {
            put(56 + k * 8, *v, 8);
        }
        for (k, v) in [s.last_recovery_ns, s.max_recovery_ns, s.last_promotion_ns, s.max_promotion_ns].iter().enumerate() {
            put(136 + k * 8, *v, 8);
        }
        put(168, (s.util * 1000.0) as u64, 4);
        let flags = [self.crashloop(), s.shedding, s.standby_suppressed, s.does_not_fit, s.reset_stuck, bound, self.accepting()]
            .iter()
            .enumerate()
            .fold(0u64, |a, (i, &f)| a | u64::from(f) << i);
        put(172, flags, 4);
        let holds = HOLDS.iter().enumerate().fold(0u64, |a, (i, h)| a | u64::from(self.holds.contains_key(h)) << i);
        put(176, holds, 4);
        put(180, self.rings() as u64, 4);
        for i in 0..self.rings().min(8) {
            let r = self.ring(i);
            put(184 + i * 4, ring::RingMem::tail(&r).load(Relaxed).wrapping_sub(ring::RingMem::head(&r).load(Relaxed)), 4);
        }
        let mut counts = [0u64; 7];
        self.for_each_slot(|sl| counts[(word::state(slot::SlotMem::word(&sl).load(Relaxed)) as usize).min(6)] += 1);
        for (k, c) in counts.iter().enumerate() {
            put(216 + k * 4, *c, 4);
        }
        for (k, &(w, at)) in self.transitions.iter().enumerate() {
            put(248 + k * 16, w, 8);
            put(256 + k * 16, at, 8);
        }
        let cause = s.last_cause.as_bytes();
        let n = cause.len().min(127);
        b[376..376 + n].copy_from_slice(&cause[..n]);
        let p = self.published.clone().unwrap_or_default();
        let mut put = |off: usize, v: u64, w: usize| b[off..off + w].copy_from_slice(&v.to_le_bytes()[..w]);
        put(512, p.epoch as u64, 4);
        put(516, s.rollbacks, 4);
        put(520, s.rejected, 4);
        put(524, self.models.target as u64, 4);
        put(528, p.fingerprint, 8);
        put(568, self.models.previous.unwrap_or(u32::MAX) as u64, 4);
        let t = self.models.trial.unwrap_or(Trial { model: 0, served_ns: 0, deaths: 0 });
        put(572, t.served_ns / MS, 4);
        put(576, t.deaths as u64, 4);
        put(580, p.model as u64, 4);
        b[536..568].copy_from_slice(&p.sha256);
        b
    }

    // ── memory and pressure (§6) ──

    fn on_memory(&mut self, m: Sample, now: u64, fx: &mut impl Effects) {
        for &(g, p) in &m.private {
            self.private.insert(g, p);
            // A worker that reported WARM before any sample included it (a small model warms
            // in a couple of milliseconds, inside one 10 ms tick) takes its baseline from the
            // first one. Without it the worker could never count as grown (§6).
            if !self.baseline.contains_key(&g) && self.idx(g).is_some_and(|i| self.workers[i].role != Role::Loading) {
                self.baseline.insert(g, p);
            }
        }
        let util = m.max.filter(|&max| max > 0).map(|max| m.current.saturating_sub(m.inactive_file) as f64 / max as f64);
        self.stats.util = util.unwrap_or(0.0);
        // PSI: the share of the last window spent stalled, from the totals' deltas.
        let (mut some, mut full) = (0.0, 0.0);
        if let (Some(s), Some(f)) = (m.psi_some_us, m.psi_full_us) {
            self.psi.push((now, s, f));
            self.psi.retain(|&(t, _, _)| now - t <= self.cfg.psi_window_ns);
            if let Some(&(t0, s0, f0)) = self.psi.first() {
                let span_us = ((now - t0) / 1000).max(1) as f64;
                if now - t0 >= self.cfg.psi_window_ns / 2 {
                    some = s.saturating_sub(s0) as f64 / span_us;
                    full = f.saturating_sub(f0) as f64 / span_us;
                }
            }
        }
        let u = util.unwrap_or(0.0);
        if self.shutdown.is_some() {
            return;
        }
        if u >= self.cfg.shed_at || full > self.cfg.psi_full {
            self.shed(now, fx);
            self.relieve(true, now, fx);
        } else if u >= self.cfg.relieve_at || some > self.cfg.psi_some {
            self.relieve(false, now, fx);
        }
        if u <= self.cfg.recover_at && full <= self.cfg.psi_full && some <= self.cfg.psi_some {
            let since = *self.below_since.get_or_insert(now);
            if now - since >= self.cfg.recover_hold_ns {
                // Below the recovery line for the hold time: whatever held the gate for memory,
                // shedding or a model that did not fit, no longer applies.
                if self.shed_since.take().is_some() || self.stats.does_not_fit {
                    self.stats.shedding = false;
                    self.stats.does_not_fit = false;
                    self.release("pressure", now);
                    fx.log("memory: recovered; the gate reopens");
                }
                if self.stats.standby_suppressed {
                    self.stats.standby_suppressed = false; // a standby may be started if it fits
                }
            }
        } else {
            self.below_since = None;
        }
        // Shedding cannot latch: after shed_max, recycle the active worker.
        if let Some(since) = self.shed_since {
            if now - since >= self.cfg.shed_max_ns {
                self.shed_since = Some(now);
                self.recycle(now, fx);
            }
        }
    }

    fn shed(&mut self, now: u64, fx: &mut impl Effects) {
        if self.shed_since.is_none() {
            self.shed_since = Some(now);
            self.stats.shedding = true;
            self.hold("pressure", gs::PSI_SHEDDING, gr::MEMORY, now);
            fx.log(&format!("memory: shedding at {:.0}% of the limit", self.stats.util * 100.0));
        }
    }

    /// The relief ladder: retire the standby; hand over to it if the active worker has grown;
    /// without one, shed and ask the worker to TRIM.
    fn relieve(&mut self, urgent: bool, now: u64, fx: &mut impl Effects) {
        if self.reload != Reload::Idle {
            return; // a handover is already moving memory
        }
        // Memory is still being returned by a worker on its way out: wait for it, unless the
        // limit is close.
        if !urgent && self.workers.iter().any(|w| w.retired || w.dying || w.condemned || w.role == Role::Released) {
            return;
        }
        let standby = self.workers.iter().find(|w| w.role == Role::Warm && !w.dying && !w.retired).map(|w| w.gen);
        let active = self.consumer.filter(|g| self.worker(*g).is_some_and(|w| w.active && !w.dying));
        let grown = active.is_some_and(|g| match (self.private.get(&g), self.baseline.get(&g)) {
            (Some(&p), Some(&b)) => b > 0 && p as f64 > self.cfg.growth_factor * b as f64,
            _ => false,
        });
        match (standby, active) {
            (Some(_), Some(a)) if grown && urgent => {
                // At the limit there is no time for a handover: kill, and the standby takes over.
                self.stats.recycles += 1;
                self.condemn(a, "grown, at the memory limit", now, fx);
            }
            (Some(s), Some(a)) if grown => {
                fx.log(&format!("memory: gen {a} has grown; handing over to the standby gen {s}"));
                self.stats.recycles += 1;
                self.set_ctl(ctl_bit::HANDOVER, true);
                self.reload = Reload::HandingOver { old: a, new: s, since: now, changed: false };
            }
            (Some(s), _) => {
                fx.log(&format!("memory: retiring the standby gen {s}"));
                if let Some(i) = self.idx(s) {
                    self.workers[i].retired = true;
                }
                self.stats.retired_standbys += 1;
                self.stats.standby_suppressed = true;
                fx.kill(s);
            }
            (None, _) => {
                self.stats.standby_suppressed = true;
                self.shed(now, fx);
                if now.saturating_sub(self.trim_at) >= 1_000 * MS {
                    self.trim_at = now;
                    self.set_ctl(ctl_bit::TRIM, true);
                }
            }
        }
    }

    /// TRIM follows the control handshake: the bit is cleared once the worker acknowledges it.
    fn trim_tick(&mut self, _now: u64) {
        let req = acc::control_header::ctl_req(&self.ctl, 0);
        if req & ctl_bit::TRIM == 0 {
            return;
        }
        let acked =
            self.consumer.and_then(|g| self.worker(g)).is_none_or(|w| acc::beat::ctl_ack(&self.data, self.dir.beat(w.beat as usize)) & ctl_bit::TRIM != 0);
        if acked {
            self.set_ctl(ctl_bit::TRIM, false);
        }
    }

    /// Recycle the active worker: a handover to the standby if there is one, else a kill and a
    /// cold start. Its memory returns to the kernel with certainty (§6).
    fn recycle(&mut self, now: u64, fx: &mut impl Effects) {
        let Some(a) = self.consumer else { return };
        self.stats.recycles += 1;
        if let Some(s) = self.workers.iter().find(|w| w.role == Role::Warm && !w.dying).map(|w| w.gen) {
            if self.reload == Reload::Idle {
                fx.log(&format!("memory: shedding has lasted too long; handing gen {a} over to gen {s}"));
                self.set_ctl(ctl_bit::HANDOVER, true);
                self.reload = Reload::HandingOver { old: a, new: s, since: now, changed: false };
            }
        } else {
            fx.log(&format!("memory: shedding has lasted too long; recycling gen {a}"));
            self.condemn(a, "recycled under memory pressure", now, fx);
        }
    }

    /// Refused responses (§5.2): at least `refusals_min` within a second, and at least
    /// `refusals_ratio` of what the worker completed in it. The count restarts at a new
    /// generation. Ring faults never condemn; they only prompt the consistency check.
    fn watch_refusals(&mut self, now: u64, fx: &mut impl Effects) {
        let Some(c) = self.consumer.and_then(|g| self.worker(g)).filter(|w| w.active && !w.dying && !w.condemned).map(Glance::of) else {
            self.refusals.clear();
            return;
        };
        let rejects = acc::status_page::client_rejects(&self.data, self.dir.status_off as usize);
        let done = acc::beat::completed(&self.data, self.dir.beat(c.beat as usize));
        if self.refusals.last().is_some_and(|&(t, _, _)| now - t < 10 * MS) {
            return;
        }
        if self.refusals.first().is_some_and(|&(t, _, _)| t < c.active_ns) {
            self.refusals.clear();
        }
        self.refusals.push((now, rejects, done));
        self.refusals.retain(|&(t, _, _)| now - t <= 1_000 * MS);
        let &(_, r0, d0) = self.refusals.first().unwrap();
        let (dr, dd) = (rejects.wrapping_sub(r0), done.wrapping_sub(d0));
        if dr >= self.cfg.refusals_min && dr as f64 >= self.cfg.refusals_ratio * dd as f64 {
            self.stats.refusal_condemns += 1;
            self.refusals.clear();
            self.condemn(c.gen, &format!("{dr} responses refused by clients in a second, of {dd} completed"), now, fx);
        }
    }

    // ── shutdown (§8) ──

    /// Returns true while shutting down (the rest of the tick is skipped).
    fn shutdown_tick(&mut self, now: u64, fx: &mut impl Effects) -> bool {
        let Some(since) = self.shutdown else { return false };
        if self.finished {
            return true;
        }
        if self.bound && now - since < self.cfg.session_grace_ns {
            return true; // keep serving until the host disconnects
        }
        if !self.holds.contains_key("shutdown") {
            self.hold("shutdown", gs::DRAINING, gr::NONE, now);
            self.set_ctl(ctl_bit::DRAIN, true);
            self.shutdown = Some(now);
            return true;
        }
        let drained = self
            .consumer
            .and_then(|g| self.worker(g))
            .is_none_or(|w| acc::beat::ctl_ack(&self.data, self.dir.beat(w.beat as usize)) & ctl_bit::DRAIN != 0 || w.dying);
        if drained || now - since > self.cfg.drain_timeout_ns {
            let gens: Vec<u32> = self.workers.iter().map(|w| w.gen).collect();
            for g in gens {
                fx.kill(g);
            }
            self.finished = true;
            fx.log("shutdown: workers killed");
        }
        true
    }
}

#[cfg(test)]
mod tests {
    //! The core in virtual time (supervisor §12): scripted events, a recorded shell.
    use super::*;
    use scz_shm::layout::{Config as ArenaConfig, Geometry, LocalArena, SlabClass};

    #[derive(Default)]
    struct Fx {
        spawned: Vec<(u32, u32)>,
        killed: Vec<u32>,
        watched: Vec<u32>,
        ended: u32,
        log: Vec<String>,
        models: Vec<(u32, u32)>,
        verifying: Vec<u32>,
        /// Answers for `verify`; without one, an empty registry verifies at once unless
        /// `hold_verdicts` is set, when the test sends `Event::Verdict` itself.
        verdicts: BTreeMap<u32, Result<(Vec<u8>, u64), String>>,
        hold_verdicts: bool,
        released: Vec<u32>,
    }
    impl Effects for Fx {
        fn spawn(&mut self, gen: u32, beat: u32, model: u32) -> bool {
            self.spawned.push((gen, beat));
            self.models.push((gen, model));
            true
        }
        fn verify(&mut self, gen: u32) -> Option<Result<(Vec<u8>, u64), String>> {
            self.verifying.push(gen);
            self.verdicts.get(&gen).cloned().or(if self.hold_verdicts { None } else { Some(Ok((vec![], 0))) })
        }
        fn release_model(&mut self, model: u32) {
            self.released.push(model);
        }
        fn kill(&mut self, gen: u32) {
            self.killed.push(gen);
        }
        fn watch(&mut self, gen: u32, _rings: usize) {
            self.watched.push(gen);
        }
        fn end_session(&mut self) {
            self.ended += 1;
        }
        fn log(&mut self, msg: &str) {
            self.log.push(msg.to_string());
        }
    }

    fn arena() -> LocalArena {
        let g = Geometry { rings: vec![8], slab_classes: vec![SlabClass { slot_size: 128, slot_count: 4, req_cap: 64, resp_cap: 64 }], staging_len: 0x1000 };
        LocalArena::new(&g, &ArenaConfig::with_defaults(1, 1)).unwrap()
    }
    fn cfg(standby: bool) -> Config {
        let mut c = Config::with_defaults(5 * MS, 15 * MS, 100 * MS);
        c.standby = standby;
        c
    }
    fn state(c: &Core<'_>) -> &'static str {
        gs::name(word::gate_state(c.gate()))
    }
    /// What a live worker does each tick: beat under its generation.
    fn beat(a: &LocalArena, b: u32, gen: u32, now: u64) {
        let (d, off) = (a.data(), a.dir.beat(b as usize));
        acc::beat::set_gen(&d, off, gen);
        acc::beat::set_hb_mono_ns(&d, off, now);
    }
    /// Run ticks every millisecond until `until`, with the given workers beating.
    fn run(c: &mut Core<'_>, a: &LocalArena, fx: &mut Fx, now: &mut u64, until: u64, beating: &[(u32, u32)]) {
        while *now < until {
            *now += MS;
            for &(b, g) in beating {
                beat(a, b, g, *now);
            }
            c.handle(*now, Event::Tick, fx);
        }
    }
    fn run_for(c: &mut Core<'_>, a: &LocalArena, fx: &mut Fx, now: &mut u64, dur: u64, beating: &[(u32, u32)]) {
        let until = *now + dur;
        run(c, a, fx, now, until, beating);
    }
    fn warm_and_active(c: &mut Core<'_>, fx: &mut Fx, gen: u32, now: u64) {
        c.handle(now, Event::Status { gen, byte: sb::WARM as u8 }, fx);
        c.handle(now, Event::Status { gen, byte: sb::ACTIVE as u8 }, fx);
    }

    #[test]
    fn start_activate_and_keep_a_standby() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        assert_eq!(state(&c), "INIT");
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        assert_eq!(fx.spawned, vec![(1, 0)]);
        c.handle(now, Event::Status { gen: 1, byte: sb::WARM as u8 }, &mut fx);
        assert_eq!(word::gate_gen(c.gate()), 1, "activated");
        assert_eq!(state(&c), "INIT", "until it reports ACTIVE");
        c.handle(now, Event::Status { gen: 1, byte: sb::ACTIVE as u8 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY");
        run(&mut c, &a, &mut fx, &mut now, 1_003 * MS, &[(0, 1)]);
        assert_eq!(fx.spawned, vec![(1, 0), (2, 1)], "a standby in the other beat slot");
        c.handle(now, Event::Status { gen: 2, byte: sb::WARM as u8 }, &mut fx);
        assert_eq!(word::gate_gen(c.gate()), 1, "the standby waits");
        assert_eq!(c.worker(2).unwrap().role, Role::Warm);
    }

    #[test]
    fn a_death_closes_the_gate_fails_the_queue_and_promotes_the_standby() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        run(&mut c, &a, &mut fx, &mut now, 1_003 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 2, byte: sb::WARM as u8 }, &mut fx);
        // A queued request, and one being processed by gen 1 (owner 1).
        let data = a.data();
        let q = a.dir.slot(&data, 0, 0);
        let e = slot::try_claim(&q).unwrap();
        slot::set_ready(&q, e);
        let p = a.dir.slot(&data, 0, 1);
        let e2 = slot::try_claim(&p).unwrap();
        slot::set_ready(&p, e2);
        assert_eq!(slot::worker_claim(&p, e2, 1), slot::Claim::Claimed);

        now += MS;
        c.handle(now, Event::LockDied { gen: 1, ring: 0 }, &mut fx);
        assert_eq!(state(&c), "DEAD", "the gate closes on the first lock report");
        assert!(fx.killed.contains(&1));
        assert_eq!(word::state(slot::SlotMem::word(&q).load(Relaxed)), ss::FAILED, "queued: failed at once");
        assert_eq!(word::state(slot::SlotMem::word(&p).load(Relaxed)), ss::PROCESSING, "processing: only on the pidfd");
        assert_eq!(word::gate_gen(c.gate()), 2, "every ring repaired: the standby is activated");
        now += 2 * MS;
        c.handle(now, Event::Status { gen: 2, byte: sb::ACTIVE as u8 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY");
        assert_eq!(c.stats.last_promotion_ns, 2 * MS);
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        assert_eq!(word::state(slot::SlotMem::word(&p).load(Relaxed)), ss::FAILED, "the owner sweep, on the exit");
        // A new standby goes into the freed beat slot, after the backoff.
        run_for(&mut c, &a, &mut fx, &mut now, 10 * MS, &[(1, 2)]);
        assert_eq!(fx.spawned.len(), 2);
        run_for(&mut c, &a, &mut fx, &mut now, 60 * MS, &[(1, 2)]);
        assert_eq!(fx.spawned.last(), Some(&(3, 0)));
    }

    #[test]
    fn without_a_standby_the_restart_backs_off() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(false), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        let mut gen = 1;
        let mut gaps = Vec::new();
        for _ in 0..4 {
            warm_and_active(&mut c, &mut fx, gen, now);
            assert_eq!(state(&c), "HEALTHY");
            now += MS;
            c.handle(now, Event::LockDied { gen, ring: 0 }, &mut fx);
            c.handle(now, Event::Exited { gen }, &mut fx);
            assert_eq!(state(&c), "DEAD");
            let died = now;
            let before = fx.spawned.len();
            while fx.spawned.len() == before {
                run_for(&mut c, &a, &mut fx, &mut now, MS, &[]);
            }
            gaps.push((now - died) / MS);
            gen = fx.spawned.last().unwrap().0;
        }
        assert_eq!(gaps, vec![50, 100, 200, 400], "50 ms, doubling");
        assert!(c.crashloop());
    }

    #[test]
    fn the_gate_opens_only_when_no_hold_is_left() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        run(&mut c, &a, &mut fx, &mut now, 1_003 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 2, byte: sb::WARM as u8 }, &mut fx);
        c.handle(now, Event::SessionBound, &mut fx);
        // A requested reset is pending (waiting for the client) when the worker dies.
        c.handle(now, Event::RequestReset, &mut fx);
        assert_eq!(state(&c), "RELOADING");
        assert_eq!(fx.ended, 1);
        c.handle(now, Event::LockDied { gen: 1, ring: 0 }, &mut fx);
        assert_eq!(state(&c), "DEAD");
        c.handle(now, Event::Status { gen: 2, byte: sb::ACTIVE as u8 }, &mut fx);
        assert_eq!(state(&c), "RELOADING", "the promotion is done, the reset is not");
        c.handle(now, Event::SessionClosed, &mut fx);
        c.handle(now, Event::Status { gen: 2, byte: sb::DRAINED as u8 }, &mut fx);
        assert_eq!(state(&c), "RELOADING", "a dying worker is not yet reaped: no re-initialisation under it");
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, MS, &[(1, 2)]);
        assert_eq!(state(&c), "HEALTHY");
        assert!(c.accepting());
    }

    #[test]
    fn a_session_reset_drains_reinitialises_and_resumes() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(false), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        c.handle(now, Event::SessionBound, &mut fx);
        assert!(!c.accepting(), "one session at a time");
        // The host leaves a claimed slot and a published cell behind.
        let data = a.data();
        let s = a.dir.slot(&data, 0, 2);
        let e = slot::try_claim(&s).unwrap();
        ring::enqueue(&a.dir.ring(&data, 0), &ring::Descriptor { slot_index: 2, slot_epoch: e, ..Default::default() }).unwrap();
        let epoch0 = acc::control_header::session_epoch(&a.ctl(), 0);
        c.handle(now, Event::SessionClosed, &mut fx);
        assert_eq!(state(&c), "RELOADING");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0) & ctl_bit::DRAIN, ctl_bit::DRAIN);
        assert!(!c.accepting(), "BUSY until the reset is done");
        c.handle(now, Event::Status { gen: 1, byte: sb::DRAINED as u8 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), 0);
        assert_eq!(word::state(slot::SlotMem::word(&s).load(Relaxed)), ss::FREE);
        assert_eq!(ring::head_state(&a.dir.ring(&data, 0)), (HeadState::Empty, 0));
        assert_eq!(acc::control_header::session_epoch(&a.ctl(), 0), epoch0 + 1);
        assert!(c.accepting());
    }

    #[test]
    fn a_reload_hands_over_with_the_gate_open() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        run(&mut c, &a, &mut fx, &mut now, 1_003 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 2, byte: sb::WARM as u8 }, &mut fx);
        c.handle(now, Event::Reload, &mut fx);
        assert!(fx.killed.contains(&2), "the standby holds the old model: retired");
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[(0, 1)]);
        assert_eq!(fx.spawned.last(), Some(&(3, 1)));
        c.handle(now, Event::Status { gen: 3, byte: sb::WARM as u8 }, &mut fx);
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), ctl_bit::HANDOVER);
        assert_eq!(state(&c), "HEALTHY");
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        assert_eq!(word::gate_gen(c.gate()), 3);
        assert_eq!(state(&c), "HEALTHY", "callers never see the gate change");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), 0);
        c.handle(now, Event::Status { gen: 3, byte: sb::ACTIVE as u8 }, &mut fx);
        c.handle(now, Event::LockDied { gen: 1, ring: 0 }, &mut fx);
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY", "a released worker's exit is not a death");
        assert_eq!(c.stats.deaths, 0);
    }

    /// A core serving model 1, with registry `reg`, from gen 1 (ACTIVE) and a verified standby,
    /// gen 2. Verdicts are sent by the test.
    fn serving<'a>(a: &'a LocalArena, fx: &mut Fx, now: &mut u64, reg: &[u8]) -> Core<'a> {
        fx.hold_verdicts = true;
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, *now);
        c.handle(*now, Event::NewModel { model: 1 }, fx);
        run(&mut c, a, fx, now, *now + MS, &[]);
        assert_eq!(fx.models.last(), Some(&(1, 1)), "gen 1 loads model 1");
        c.handle(*now, Event::Status { gen: 1, byte: sb::WARM as u8 }, fx);
        assert_eq!(word::gate_gen(c.gate()), 0, "not activated before its verdict");
        c.handle(*now, Event::Verdict { gen: 1, result: Ok((reg.to_vec(), 0xAAAA)) }, fx);
        assert_eq!(word::gate_gen(c.gate()), 0, "nor before its model's digest");
        c.handle(*now, Event::Digest { model: 1, sha: Some([1; 32]) }, fx);
        assert_eq!(word::gate_gen(c.gate()), 1, "verified and hashed: activated");
        c.handle(*now, Event::Status { gen: 1, byte: sb::ACTIVE as u8 }, fx);
        run_for(&mut c, a, fx, now, 2 * MS, &[(0, 1)]);
        c.handle(*now, Event::Status { gen: 2, byte: sb::WARM as u8 }, fx);
        c.handle(*now, Event::Verdict { gen: 2, result: Ok((reg.to_vec(), 0xAAAA)) }, fx);
        assert_eq!(c.worker(2).unwrap().role, Role::Warm);
        c
    }

    #[test]
    fn activation_waits_for_the_verdict_and_publishes_the_model_block_first() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let c = serving(&a, &mut fx, &mut now, b"registry one");
        let (b, bytes) = scz_shm::model::read(&a.ctl(), 10).unwrap();
        assert_eq!((b.registry_epoch, b.model_gen, b.schema_fingerprint, b.sha256), (1, 1, 0xAAAA, [1; 32]));
        assert_eq!(bytes, b"registry one");
        assert_eq!(state(&c), "HEALTHY");
    }

    #[test]
    fn a_refused_registry_is_a_failed_load() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        fx.hold_verdicts = true;
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        c.handle(now, Event::Status { gen: 1, byte: sb::WARM as u8 }, &mut fx);
        c.handle(now, Event::Verdict { gen: 1, result: Err("bad prefix".into()) }, &mut fx);
        assert!(fx.killed.contains(&1));
        assert_eq!(word::gate_gen(c.gate()), 0);
        assert!(c.stats.last_cause.contains("bad prefix"));
    }

    #[test]
    fn a_changed_schema_is_handed_over_behind_the_gate() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving(&a, &mut fx, &mut now, b"registry one");
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        assert!(fx.killed.contains(&2), "the standby holds the old model");
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[(0, 1)]);
        assert_eq!(fx.models.last(), Some(&(3, 2)), "gen 3 loads model 2");
        c.handle(now, Event::Status { gen: 3, byte: sb::WARM as u8 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY", "still serving while the new registry is verified");
        // A request queued for the old schema.
        let data = a.data();
        let q = a.dir.slot(&data, 0, 0);
        let e = slot::try_claim(&q).unwrap();
        slot::set_ready(&q, e);
        c.handle(now, Event::Verdict { gen: 3, result: Ok((b"registry two".to_vec(), 0xBBBB)) }, &mut fx);
        assert_eq!(state(&c), "RELOADING", "a changed schema: callers fall back during the handover");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), ctl_bit::HANDOVER);
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        assert_eq!(word::state(slot::SlotMem::word(&q).load(Relaxed)), ss::FAILED, "nothing queued for the old schema reaches the new model");
        let (b, bytes) = scz_shm::model::read(&a.ctl(), 10).unwrap();
        assert_eq!((b.registry_epoch, b.model_gen, b.schema_fingerprint, &bytes[..]), (2, 3, 0xBBBB, &b"registry two"[..]));
        assert_eq!((word::gate_gen(c.gate()), state(&c)), (3, "RELOADING"), "the new generation, behind the gate");
        c.handle(now, Event::Status { gen: 3, byte: sb::ACTIVE as u8 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY");
    }

    #[test]
    fn the_same_schema_keeps_the_epoch_and_the_gate() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving(&a, &mut fx, &mut now, b"registry one");
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 3, byte: sb::WARM as u8 }, &mut fx);
        c.handle(now, Event::Verdict { gen: 3, result: Ok((b"registry one".to_vec(), 0xAAAA)) }, &mut fx);
        assert_eq!(state(&c), "HEALTHY");
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        assert_eq!((word::gate_gen(c.gate()), state(&c)), (3, "HEALTHY"));
        let (b, _) = scz_shm::model::read(&a.ctl(), 10).unwrap();
        assert_eq!((b.registry_epoch, b.sha256), (1, [2; 32]), "the same registry: the epoch stays, the digest is the new model's");
    }

    #[test]
    fn three_deaths_in_the_window_roll_back_and_the_model_stays_rejected() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving(&a, &mut fx, &mut now, b"registry one");
        c.cfg.backoff_max_ns = MS;
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 3, byte: sb::WARM as u8 }, &mut fx);
        c.handle(now, Event::Verdict { gen: 3, result: Ok((b"registry one".to_vec(), 0xAAAA)) }, &mut fx);
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        c.handle(now, Event::Status { gen: 3, byte: sb::ACTIVE as u8 }, &mut fx);
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        // Model 2 dies on real requests, each time it is serving.
        let mut gen = 3;
        for death in 1..=3 {
            c.handle(now, Event::LockDied { gen, ring: 0 }, &mut fx);
            c.handle(now, Event::Exited { gen }, &mut fx);
            if death == 3 {
                break;
            }
            let before = fx.spawned.len();
            run_for(&mut c, &a, &mut fx, &mut now, 20 * MS, &[]);
            let &(g, m) = fx.models.last().unwrap();
            assert!(fx.spawned.len() > before && m == 2, "death {death}: model 2 is loaded again (gen {g}, model {m})");
            gen = g;
            c.handle(now, Event::Status { gen, byte: sb::WARM as u8 }, &mut fx);
            c.handle(now, Event::Verdict { gen, result: Ok((b"registry one".to_vec(), 0xAAAA)) }, &mut fx);
            c.handle(now, Event::Status { gen, byte: sb::ACTIVE as u8 }, &mut fx);
        }
        assert_eq!(c.stats.rollbacks, 1);
        assert!(c.models.bad.contains(&2) && c.models.target == 1);
        assert!(!fx.released.contains(&2), "the path still names it: kept open for a forced reload");
        run_for(&mut c, &a, &mut fx, &mut now, 20 * MS, &[]);
        let &(g, m) = fx.models.last().unwrap();
        assert_eq!(m, 1, "the previous model is loaded again (gen {g})");
        c.handle(now, Event::Status { gen: g, byte: sb::WARM as u8 }, &mut fx);
        c.handle(now, Event::Verdict { gen: g, result: Ok((b"registry one".to_vec(), 0xAAAA)) }, &mut fx);
        c.handle(now, Event::Status { gen: g, byte: sb::ACTIVE as u8 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY");
        assert_eq!(scz_shm::model::read(&a.ctl(), 10).unwrap().0.sha256, [1; 32]);
        // A rejected model is not loaded again unless forced.
        c.models.target = 2;
        let reloads = c.stats.reloads;
        c.handle(now, Event::Reload, &mut fx);
        assert_eq!(c.stats.reloads, reloads, "refused");
        c.handle(now, Event::ForceReload, &mut fx);
        assert_eq!(c.stats.reloads, reloads + 1, "forced");
    }

    #[test]
    fn a_window_served_out_releases_the_previous_model() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving(&a, &mut fx, &mut now, b"registry one");
        c.cfg.rollback_window_ns = 50 * MS;
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 3, byte: sb::WARM as u8 }, &mut fx);
        c.handle(now, Event::Verdict { gen: 3, result: Ok((b"registry one".to_vec(), 0xAAAA)) }, &mut fx);
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        c.handle(now, Event::Status { gen: 3, byte: sb::ACTIVE as u8 }, &mut fx);
        assert_eq!(c.models.previous, Some(1));
        run_for(&mut c, &a, &mut fx, &mut now, 40 * MS, &[(1, 3)]);
        assert!(!fx.released.contains(&1), "not yet: 40 of 50 ms served");
        run_for(&mut c, &a, &mut fx, &mut now, 20 * MS, &[(1, 3)]);
        assert!(fx.released.contains(&1) && c.models.previous.is_none());
    }

    /// A core serving model 1 from gen 1 without a standby: memory for one worker.
    fn serving_alone<'a>(a: &'a LocalArena, fx: &mut Fx, now: &mut u64) -> Core<'a> {
        let mut c = Core::new(cfg(false), a.ctl(), a.data(), &a.dir, *now);
        c.handle(*now, Event::NewModel { model: 1 }, fx);
        c.handle(*now, Event::Digest { model: 1, sha: Some([1; 32]) }, fx);
        run(&mut c, a, fx, now, *now + MS, &[]);
        warm_and_active(&mut c, fx, 1, *now);
        run_for(&mut c, a, fx, now, 5 * MS, &[(0, 1)]);
        assert_eq!((fx.spawned.clone(), state(&c)), (vec![(1, 0)], "HEALTHY"), "no standby");
        c
    }

    #[test]
    fn with_memory_for_one_worker_the_old_one_exits_before_the_new_model_loads() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving_alone(&a, &mut fx, &mut now);
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        assert_eq!(state(&c), "RELOADING", "callers fall back for the whole reload");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), ctl_bit::HANDOVER);
        let data = a.data();
        let q = a.dir.slot(&data, 0, 0);
        let e = slot::try_claim(&q).unwrap();
        slot::set_ready(&q, e);
        run_for(&mut c, &a, &mut fx, &mut now, 5 * MS, &[(0, 1)]);
        assert_eq!(fx.spawned.len(), 1, "nothing starts beside the old worker");
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        assert_eq!(word::state(slot::SlotMem::word(&q).load(Relaxed)), ss::FAILED, "nothing will consume it for a load time");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), 0);
        run_for(&mut c, &a, &mut fx, &mut now, 5 * MS, &[]);
        assert_eq!(fx.spawned.len(), 1, "nor before the old one has exited and freed its memory");
        c.handle(now, Event::LockDied { gen: 1, ring: 0 }, &mut fx);
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        assert_eq!((fx.spawned.last(), fx.models.last()), (Some(&(2, 0)), Some(&(2, 2))), "gen 2 loads model 2 in the freed beat slot");
        assert_eq!(state(&c), "RELOADING");
        warm_and_active(&mut c, &mut fx, 2, now);
        assert_eq!((word::gate_gen(c.gate()), state(&c)), (2, "HEALTHY"));
        assert_eq!(scz_shm::model::read(&a.ctl(), 10).unwrap().0.sha256, [2; 32]);
        assert_eq!((c.stats.deaths, c.stats.rollbacks), (0, 0));
        assert_eq!(c.models.previous, Some(1), "kept for a rollback until the window is served");
    }

    #[test]
    fn with_memory_for_one_worker_a_model_that_does_not_load_is_replaced_by_the_previous_one() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving_alone(&a, &mut fx, &mut now);
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        assert_eq!(fx.models.last(), Some(&(2, 2)));
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        assert_eq!(c.stats.rollbacks, 1);
        assert!(c.models.bad.contains(&2) && c.models.target == 1, "the failed model stays rejected");
        run_for(&mut c, &a, &mut fx, &mut now, MS, &[]);
        assert_eq!(fx.models.last(), Some(&(3, 1)), "the previous model, at once rather than after a backoff");
        warm_and_active(&mut c, &mut fx, 3, now);
        assert_eq!(state(&c), "HEALTHY");
        assert_eq!(scz_shm::model::read(&a.ctl(), 10).unwrap().0.sha256, [1; 32]);
    }

    #[test]
    fn with_memory_for_one_worker_a_model_refused_before_it_starts_returns_to_the_previous_one() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving_alone(&a, &mut fx, &mut now);
        c.cfg.pin = Some([1; 32]);
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        assert!(c.models.bad.contains(&2), "against the pin");
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, MS, &[]);
        assert_eq!(fx.models.last(), Some(&(2, 1)), "not stuck: the previous model is loaded again");
        warm_and_active(&mut c, &mut fx, 2, now);
        assert_eq!(state(&c), "HEALTHY");
    }

    #[test]
    fn a_suppressed_standby_means_memory_for_one_worker() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving(&a, &mut fx, &mut now, b"registry one");
        c.stats.standby_suppressed = true;
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        assert!(fx.killed.contains(&2), "the standby is retired");
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 5 * MS, &[(0, 1)]);
        assert_eq!(fx.spawned.len(), 2, "no new worker beside the old one");
        assert_eq!((state(&c), acc::control_header::ctl_req(&a.ctl(), 0)), ("RELOADING", ctl_bit::HANDOVER));
    }

    #[test]
    fn a_worker_that_will_not_release_for_a_lone_reload_is_condemned_and_the_reload_goes_on() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = serving_alone(&a, &mut fx, &mut now);
        c.handle(now, Event::NewModel { model: 2 }, &mut fx);
        c.handle(now, Event::Digest { model: 2, sha: Some([2; 32]) }, &mut fx);
        let timeout = c.cfg.handover_timeout_ns;
        run_for(&mut c, &a, &mut fx, &mut now, timeout + 2 * MS, &[(0, 1)]);
        assert!(fx.killed.contains(&1), "condemned");
        c.handle(now, Event::LockDied { gen: 1, ring: 0 }, &mut fx);
        assert_eq!(fx.spawned.len(), 1, "the new model waits for the exit");
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 100 * MS, &[]);
        assert_eq!(fx.models.last(), Some(&(2, 2)), "then loads after the backoff");
    }

    #[test]
    fn a_digest_against_the_pin_is_refused() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut cf = cfg(true);
        cf.pin = Some([9; 32]);
        let mut c = Core::new(cf, a.ctl(), a.data(), &a.dir, now);
        c.handle(now, Event::NewModel { model: 1 }, &mut fx);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        c.handle(now, Event::Digest { model: 1, sha: Some([1; 32]) }, &mut fx);
        assert!(fx.killed.contains(&1), "the worker loading it is of no use");
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        let spawned = fx.spawned.len();
        run_for(&mut c, &a, &mut fx, &mut now, 200 * MS, &[]);
        assert_eq!(fx.spawned.len(), spawned, "nothing more is loaded from a refused model");
        assert_eq!(state(&c), "INIT");
        assert!(c.stats.last_cause.contains("pin"));
    }

    #[test]
    fn a_missing_model_file_spawns_nothing_until_one_appears() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        c.handle(now, Event::NewModel { model: NO_MODEL }, &mut fx);
        run(&mut c, &a, &mut fx, &mut now, 1_100 * MS, &[]);
        assert!(fx.spawned.is_empty());
        c.handle(now, Event::NewModel { model: 1 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[]);
        assert_eq!(fx.models, vec![(1, 1)]);
    }

    #[test]
    fn a_worker_that_will_not_release_is_condemned_and_the_new_one_takes_over() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        // Memory for two workers (a standby configured), reloading before the standby starts.
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        c.handle(now, Event::Reload, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 2, byte: sb::WARM as u8 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 700 * MS, &[(0, 1), (1, 2)]);
        assert!(fx.killed.contains(&1), "no RELEASED within the handover timeout");
        c.handle(now, Event::LockDied { gen: 1, ring: 0 }, &mut fx);
        assert_eq!(word::gate_gen(c.gate()), 2, "the crash path activates the warm new worker");
        c.handle(now, Event::Status { gen: 2, byte: sb::ACTIVE as u8 }, &mut fx);
        assert_eq!(state(&c), "HEALTHY");
    }

    #[test]
    fn a_stale_heartbeat_condemns_but_not_across_a_supervisor_stall() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(false), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        run_for(&mut c, &a, &mut fx, &mut now, 10 * MS, &[(0, 1)]);
        // The whole container stalls for 50 ms: no tick, no beat. The first tick after it
        // must not condemn the worker for a heartbeat that went stale during the stall.
        now += 50 * MS;
        c.handle(now, Event::Tick, &mut fx);
        assert!(fx.killed.is_empty());
        run_for(&mut c, &a, &mut fx, &mut now, 10 * MS, &[(0, 1)]);
        assert!(fx.killed.is_empty());
        // The worker alone stops beating (SIGSTOP): condemned after hb_stale.
        run_for(&mut c, &a, &mut fx, &mut now, 20 * MS, &[]);
        assert_eq!(fx.killed, vec![1]);
        assert_eq!(state(&c), "DEAD");
    }

    #[test]
    fn a_load_that_never_finishes_is_killed_and_counted() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut cf = cfg(false);
        cf.load_timeout_ns = 100 * MS;
        let mut c = Core::new(cf, a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_150 * MS, &[(0, 1)]);
        assert_eq!(fx.killed, vec![1]);
        c.handle(now, Event::Exited { gen: 1 }, &mut fx);
        assert_eq!(state(&c), "INIT");
        run_for(&mut c, &a, &mut fx, &mut now, 60 * MS, &[]);
        assert_eq!(fx.spawned.len(), 2, "retried after the backoff");
    }

    fn mem(c: &mut Core<'_>, fx: &mut Fx, now: u64, util: f64, private: &[(u32, u64)]) {
        let sample = Sample {
            current: (util * 1e9) as u64,
            max: Some(1_000_000_000),
            inactive_file: 0,
            psi_some_us: None,
            psi_full_us: None,
            private: private.to_vec(),
        };
        c.handle(now, Event::Memory(sample), fx);
    }

    fn with_standby(c: &mut Core<'_>, a: &LocalArena, fx: &mut Fx, now: &mut u64, private: &[(u32, u64)]) {
        run(c, a, fx, now, *now + MS, &[]);
        mem(c, fx, *now, 0.5, private);
        warm_and_active(c, fx, 1, *now);
        run_for(c, a, fx, now, 2 * MS, &[(0, 1)]);
        mem(c, fx, *now, 0.5, private);
        c.handle(*now, Event::Status { gen: 2, byte: sb::WARM as u8 }, fx);
    }

    #[test]
    fn memory_relief_retires_the_standby_then_sheds_and_recovers() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        with_standby(&mut c, &a, &mut fx, &mut now, &[(1, 100), (2, 100)]);
        mem(&mut c, &mut fx, now, 0.88, &[(1, 100), (2, 100)]);
        assert!(fx.killed.contains(&2), "the standby goes first: it serves nothing");
        assert_eq!(state(&c), "HEALTHY", "retiring the standby costs callers nothing");
        c.handle(now, Event::Exited { gen: 2 }, &mut fx);
        run_for(&mut c, &a, &mut fx, &mut now, 100 * MS, &[(0, 1)]);
        assert_eq!(fx.spawned.len(), 2, "no new standby while it would not fit");
        mem(&mut c, &mut fx, now, 0.88, &[(1, 100)]);
        assert_eq!(state(&c), "PSI_SHEDDING", "no standby left: shed");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), ctl_bit::TRIM);
        acc::beat::set_ctl_ack(&a.data(), a.dir.beat(0), ctl_bit::TRIM); // the worker trims
        run_for(&mut c, &a, &mut fx, &mut now, MS, &[(0, 1)]);
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), 0, "cleared once acknowledged");
        for _ in 0..25 {
            now += 10 * MS;
            mem(&mut c, &mut fx, now, 0.70, &[(1, 100)]);
        }
        assert_eq!(state(&c), "HEALTHY", "below the recovery line for 200 ms");
    }

    #[test]
    fn a_grown_worker_is_handed_over_to_the_standby() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        with_standby(&mut c, &a, &mut fx, &mut now, &[(1, 100), (2, 100)]);
        mem(&mut c, &mut fx, now, 0.88, &[(1, 200), (2, 100)]);
        assert!(!fx.killed.contains(&2), "the standby is kept: the active worker is the one that grew");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), ctl_bit::HANDOVER);
        c.handle(now, Event::Status { gen: 1, byte: sb::RELEASED as u8 }, &mut fx);
        assert_eq!(word::gate_gen(c.gate()), 2);
        assert_eq!(state(&c), "HEALTHY");
    }

    #[test]
    fn a_worker_that_warms_before_its_first_sample_still_gets_a_baseline() {
        // Both workers report WARM before any memory sample includes them, as the chaos and
        // echo workers do in a cluster: the first sample after that is each one's baseline.
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        let until = now + MS;
        run(&mut c, &a, &mut fx, &mut now, until, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        run_for(&mut c, &a, &mut fx, &mut now, 2 * MS, &[(0, 1)]);
        c.handle(now, Event::Status { gen: 2, byte: sb::WARM as u8 }, &mut fx);
        mem(&mut c, &mut fx, now, 0.5, &[(1, 100), (2, 100)]);
        mem(&mut c, &mut fx, now, 0.88, &[(1, 200), (2, 100)]);
        assert!(!fx.killed.contains(&2), "the active worker grew: the standby is kept");
        assert_eq!(acc::control_header::ctl_req(&a.ctl(), 0), ctl_bit::HANDOVER, "and takes over");
    }

    #[test]
    fn shedding_cannot_latch() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(false), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        warm_and_active(&mut c, &mut fx, 1, now);
        mem(&mut c, &mut fx, now, 0.97, &[(1, 100)]);
        assert_eq!(state(&c), "PSI_SHEDDING");
        // Utilization settles between the recovery and relief lines: it would never recover.
        for _ in 0..210 {
            now += 10 * MS;
            beat(&a, 0, 1, now);
            c.handle(now, Event::Tick, &mut fx);
            mem(&mut c, &mut fx, now, 0.80, &[(1, 100)]);
        }
        assert!(fx.killed.contains(&1), "recycled after shed_max");
    }

    #[test]
    fn a_model_that_does_not_fit_says_so() {
        let a = arena();
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(true), a.ctl(), a.data(), &a.dir, now);
        run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
        mem(&mut c, &mut fx, now, 0.90, &[(1, 900)]);
        c.handle(now, Event::Status { gen: 1, byte: sb::WARM as u8 }, &mut fx);
        assert_eq!(gr::name(word::gate_reason(c.gate())), "DOES_NOT_FIT");
        assert!(c.stats.does_not_fit);
    }

    #[test]
    fn refused_responses_condemn_only_when_they_are_a_real_share() {
        for (completed, condemned) in [(100u64, true), (100_000, false)] {
            let a = arena();
            let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
            let mut c = Core::new(cfg(false), a.ctl(), a.data(), &a.dir, now);
            run(&mut c, &a, &mut fx, &mut now, 1_001 * MS, &[]);
            warm_and_active(&mut c, &mut fx, 1, now);
            run_for(&mut c, &a, &mut fx, &mut now, 20 * MS, &[(0, 1)]);
            // Over the next half second the clients refuse 8 responses.
            let (d, st, b) = (a.data(), a.dir.status_off as usize, a.dir.beat(0));
            for k in 1..=50u64 {
                now += 10 * MS;
                beat(&a, 0, 1, now);
                acc::beat::set_completed(&d, b, completed * k / 50);
                acc::status_page::set_client_rejects(&d, st, (8 * k / 50).min(8));
                c.handle(now, Event::Tick, &mut fx);
            }
            assert_eq!(fx.killed.contains(&1), condemned, "{completed} completions");
        }
    }

    /// A small deterministic generator, so a failing sequence replays from its seed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Sim {
        Loading { ready: u64 },
        Warm,
        Active,
        Paused,
        Frozen,
        Dying { lock: u64, exit: u64 },
        Released { exit: u64 },
        Gone,
    }

    /// The core against simulated workers and random faults (testing §2.6). After every event:
    /// HEALTHY only with no holds and a live consumer; never two live consumers; never more
    /// workers than beat slots. After the faults stop, the gate must come back.
    #[test]
    fn random_fault_sequences_keep_the_invariants() {
        let seeds: u64 = std::env::var("SCZ_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
        for seed in 1..=seeds {
            random_sequence(seed);
        }
    }

    fn random_sequence(seed: u64) {
        let a = arena();
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut fx, mut now) = (Fx::default(), 1_000 * MS);
        let mut c = Core::new(cfg(r.below(2) == 0), a.ctl(), a.data(), &a.dir, now);
        let mut sim: BTreeMap<u32, Sim> = BTreeMap::new();
        let (mut seen_spawn, mut seen_kill) = (0, 0);
        let mut bound = false;
        let steps = 400;
        for step in 0..steps + 3000 {
            let faults = step < steps;
            now += 1 + r.below(2 * MS);
            // The shell's side: new spawns and kills become simulated processes.
            while seen_spawn < fx.spawned.len() {
                let (g, _) = fx.spawned[seen_spawn];
                sim.insert(g, Sim::Loading { ready: now + 1 + r.below(10 * MS) });
                seen_spawn += 1;
            }
            while seen_kill < fx.killed.len() {
                let g = fx.killed[seen_kill];
                if let Some(st) = sim.get_mut(&g) {
                    if !matches!(st, Sim::Dying { .. } | Sim::Gone) {
                        let lock = now + r.below(MS);
                        *st = Sim::Dying { lock, exit: lock + r.below(20 * MS) };
                    }
                }
                seen_kill += 1;
            }
            // The workers' side.
            let gate = c.gate();
            let ctl = acc::control_header::ctl_req(&a.ctl(), 0);
            let mut events = Vec::new();
            for (&g, st) in sim.iter_mut() {
                let beat_slot = c.worker(g).map(|w| w.beat);
                match *st {
                    Sim::Loading { ready } if now >= ready => {
                        *st = Sim::Warm;
                        events.push(Event::Status { gen: g, byte: sb::WARM as u8 });
                    }
                    Sim::Warm if word::gate_gen(gate) == g => {
                        *st = Sim::Active;
                        events.push(Event::Status { gen: g, byte: sb::ACTIVE as u8 });
                    }
                    Sim::Active if ctl & ctl_bit::HANDOVER != 0 && c.consumer() == Some(g) => {
                        *st = Sim::Released { exit: now + r.below(5 * MS) };
                        events.push(Event::Status { gen: g, byte: sb::RELEASED as u8 });
                    }
                    Sim::Active if ctl & ctl_bit::DRAIN != 0 => {
                        *st = Sim::Paused;
                        acc::beat::set_ctl_ack(&a.data(), a.dir.beat(beat_slot.unwrap_or(0) as usize), ctl_bit::DRAIN);
                        events.push(Event::Status { gen: g, byte: sb::DRAINED as u8 });
                    }
                    Sim::Paused if ctl & ctl_bit::DRAIN == 0 => *st = Sim::Active,
                    Sim::Dying { lock, exit } if now >= lock && lock != u64::MAX => {
                        *st = Sim::Dying { lock: u64::MAX, exit };
                        events.push(Event::LockDied { gen: g, ring: 0 });
                    }
                    Sim::Dying { lock: u64::MAX, exit } | Sim::Released { exit } if now >= exit => {
                        *st = Sim::Gone;
                        events.push(Event::Exited { gen: g });
                    }
                    _ => {}
                }
                if let Some(b) = beat_slot {
                    if !matches!(st, Sim::Frozen | Sim::Dying { .. } | Sim::Gone) {
                        beat(&a, b, g, now);
                    }
                }
            }
            // Faults.
            if faults && r.below(25) == 0 {
                let live: Vec<u32> = sim.iter().filter(|(_, s)| matches!(s, Sim::Warm | Sim::Active | Sim::Paused)).map(|(&g, _)| g).collect();
                match r.below(8) {
                    0 | 1 if !live.is_empty() => {
                        let g = live[r.below(live.len() as u64) as usize];
                        let lock = now + r.below(MS);
                        sim.insert(g, Sim::Dying { lock, exit: lock + r.below(20 * MS) });
                    }
                    2 if !live.is_empty() => {
                        let g = live[r.below(live.len() as u64) as usize];
                        sim.insert(g, Sim::Frozen);
                    }
                    3 => events.push(Event::Reload),
                    4 => events.push(Event::RequestReset),
                    5 if !bound && c.accepting() => {
                        bound = true;
                        events.push(Event::SessionBound);
                    }
                    6 if bound => {
                        bound = false;
                        events.push(Event::SessionClosed);
                    }
                    7 => {
                        let util = r.below(100) as f64 / 100.0;
                        let private: Vec<(u32, u64)> = sim.keys().map(|&g| (g, 100 + r.below(100))).collect();
                        mem(&mut c, &mut fx, now, util, &private);
                    }
                    _ => {}
                }
            }
            if !faults {
                let private: Vec<(u32, u64)> = sim.keys().map(|&g| (g, 100)).collect();
                mem(&mut c, &mut fx, now, 0.3, &private); // pressure gone; samples keep coming
            }
            // A requested reset waits for the client to leave: the simulated client does.
            if bound && fx.ended > 0 {
                bound = false;
                fx.ended = 0;
                events.push(Event::SessionClosed);
            }
            events.push(Event::Tick);
            for e in events {
                c.handle(now, e, &mut fx);
                check(&c, seed, step);
            }
        }
        assert_eq!(state(&c), "HEALTHY", "seed {seed}: the gate did not come back\n{}", fx.log[fx.log.len().saturating_sub(15)..].join("\n"));
    }

    fn check(c: &Core<'_>, seed: u64, step: usize) {
        let g = c.gate();
        if word::gate_state(g) == gs::HEALTHY {
            assert!(c.holds.is_empty(), "seed {seed} step {step}: HEALTHY with holds {:?}", c.holds);
            let w = c.worker(word::gate_gen(g));
            assert!(w.is_some_and(|w| w.role == Role::Consumer && !w.dying), "seed {seed} step {step}: HEALTHY without a live consumer: {:?}", c.workers);
        }
        let consumers = c.workers.iter().filter(|w| w.role == Role::Consumer && !w.dying && !w.condemned).count();
        assert!(consumers <= 1, "seed {seed} step {step}: {consumers} live consumers: {:?}", c.workers);
        assert!(c.workers.len() <= abi::BEAT_SLOTS as usize, "seed {seed} step {step}: {:?}", c.workers);
    }
}
