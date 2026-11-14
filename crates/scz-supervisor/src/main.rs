//! scz-supervisor: owns the arena, runs the workers, answers the handshake.
//!
//!   scz-supervisor --socket=PATH|@NAME --worker=PATH
//!                  [--rings=1] [--ring-capacity=4096] [--slots=1024] [--slot-size=4096]
//!                  [--standby=warm|none] [--hb-interval-ms=5] [--hb-stale-ms=15]
//!                  [--max-deadline-ms=100] [--load-timeout-ms=300000] [--backoff-max-ms=5000]
//!                  [--http=ADDR:PORT] [--admin=PATH] [--allow-uid=UID] [--core-dumps]
//!                  [--wait-for-model] [--model-copy=none|memfd] [--hugepages]
//!                  [--cgroup=DIR] [--fail-open=true|false]
//!                  [--oom-shield=auto|singleProcess|nestedCgroup|oomScoreAdj|none]
//!                  [--worker-memory-max-mb=N] [--spin-us=50] [--session-grace-ms=20000] [--log-mono]
//!                  [--worker-arg=ARG]... [--worker-read=PATH]... [--worker-socketpair]
//!                  [--pressure-shed=0.85] [--pressure-recover=0.75] [--pressure-hard=0.95] [--pressure-shed-max-ms=2000]
//!
//! One thread, one epoll loop (supervisor §2), plus one watcher thread per consume thread of
//! each worker, which waits for that thread's death lock and writes an eventfd. Every
//! decision is made by [`machine::Core`]; this file is the I/O around it: the arena, the session
//! socket, worker processes with their status pipes and pidfds, the watchers, signals, the
//! HTTP probes, and the admin socket.

mod admin;
mod helper;
mod http;
mod machine;
mod monitor;
mod sandbox;
mod shield;
mod spawn;

use crate::machine::{Config, Core, Effects, Event};
use scz_shm::abi::{self, gate_reason as gr, gate_state as gs, welcome_status as ws};
use scz_shm::arena::OwnedArena;
use scz_shm::handshake::{self, Endpoint, Hello, Welcome};
use scz_shm::layout::{Config as ArenaConfig, Geometry, SlabClass};
use scz_shm::wait::monotonic_ns;
use scz_shm::{deathlock, sys, word};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// With `--log-mono`, every log line carries `CLOCK_MONOTONIC` in nanoseconds after the
/// name: the benchmark lines the supervisor's events up with its kill agent's (bench §5).
static LOG_MONO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

macro_rules! log {
    ($($t:tt)*) => {
        if LOG_MONO.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("scz-supervisor: @{} {}", monotonic_ns(), format_args!($($t)*))
        } else {
            eprintln!("scz-supervisor: {}", format_args!($($t)*))
        }
    };
}

pub struct Args {
    socket: Endpoint,
    worker: PathBuf,
    geometry: Geometry,
    allow_uid: Option<u32>,
    core_dumps: bool,
    standby: bool,
    hb_interval_ms: u64,
    hb_stale_ms: u64,
    max_deadline_ms: u64,
    /// The spin budget written to the arena (signaling §4); None keeps the default.
    spin_us: Option<u64>,
    /// How long an attached host is served after SIGTERM (supervisor §8).
    session_grace_ms: Option<u64>,
    load_timeout_ms: u64,
    backoff_max_ms: Option<u64>,
    http: Option<String>,
    admin: Option<PathBuf>,
    cgroup: Option<PathBuf>,
    fail_open: bool,
    test_fail_arena: u32,
    /// Tests only: refuse every client as if this supervisor spoke ABI major N
    /// (the conformance suite's compatibility scenarios).
    test_abi_major: Option<u16>,
    /// The OOM shield asked for (kubernetes-deploy §5), and the memory.max each worker's own
    /// cgroup gets in nestedCgroup (None: derived from the container's limit).
    oom_shield: shield::Asked,
    worker_memory_max: Option<u64>,
    /// The worker program's own arguments, before the descriptor arguments: a runner such as
    /// `python3 -m sidecarzero.worker my_model` (sdk-python §5).
    worker_args: Vec<String>,
    /// Tests and diagnosis only: false turns the memory monitor, and so Layer 0, off, to show
    /// what the OOM shield alone does against a spike (kubernetes-deploy §10 test 4).
    memory_monitor: bool,
    model: Option<PathBuf>,
    model_pin: Option<[u8; 32]>,
    model_copy: String,
    hugepages: bool,
    wait_for_model: bool,
    rollback: bool,
    rollback_window_ms: u64,
    /// The memory thresholds of supervisor §6, as fractions of the limit: relieve the pressure
    /// at `shed`, shed at once at `hard`, recover below `recover`; and how long shedding may
    /// last before the worker is recycled (the chart's `pressure.*`).
    pressure: Option<(f64, f64, f64)>,
    shed_max_ms: Option<u64>,
    sandbox: sandbox::Config,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        socket: Endpoint::Abstract(String::new()),
        worker: PathBuf::new(),
        geometry: Geometry::default(),
        allow_uid: None,
        core_dumps: false,
        standby: true,
        hb_interval_ms: abi::defaults::HB_INTERVAL_NS / 1_000_000,
        hb_stale_ms: abi::defaults::HB_STALE_NS / 1_000_000,
        max_deadline_ms: abi::defaults::MAX_DEADLINE_NS / 1_000_000,
        spin_us: None,
        session_grace_ms: None,
        load_timeout_ms: 300_000,
        backoff_max_ms: None,
        http: None,
        admin: None,
        cgroup: None,
        fail_open: true,
        test_fail_arena: 0,
        test_abi_major: None,
        oom_shield: shield::Asked::Auto,
        worker_memory_max: None,
        worker_args: Vec::new(),
        memory_monitor: true,
        model: None,
        model_pin: None,
        model_copy: "none".into(),
        hugepages: false,
        wait_for_model: false,
        rollback: true,
        rollback_window_ms: 60_000,
        pressure: None,
        shed_max_ms: None,
        sandbox: sandbox::Config::new(&[]),
    };
    let (mut p_shed, mut p_recover, mut p_hard) = (None, None, None);
    let (mut resp_cap, mut batch, mut batch_size, mut batch_resp) = (None, 0u32, 131_072u32, None);
    let (mut socket, mut worker) = (false, false);
    let mut extra_read: Vec<PathBuf> = Vec::new();
    let (mut rings, mut cap, mut slots, mut slot_size) = (1usize, 4096u64, 1024u32, 4096u32);
    for arg in std::env::args().skip(1) {
        let (k, v) = arg.split_once('=').unwrap_or((arg.as_str(), ""));
        let num = |v: &str| v.parse::<u64>().map_err(|_| format!("{k}: not a number: {v}"));
        let frac = |v: &str| v.parse::<f64>().ok().filter(|f| (0.0..=1.0).contains(f)).ok_or(format!("{k}: not a fraction from 0 to 1: {v}"));
        match k {
            "--pressure-shed" => p_shed = Some(frac(v)?),
            "--pressure-recover" => p_recover = Some(frac(v)?),
            "--pressure-hard" => p_hard = Some(frac(v)?),
            "--pressure-shed-max-ms" => a.shed_max_ms = Some(num(v)?),
            "--socket" => (a.socket, socket) = (Endpoint::parse(v), true),
            "--worker" => (a.worker, worker) = (PathBuf::from(v), true),
            "--rings" => rings = num(v)? as usize,
            "--ring-capacity" => cap = num(v)?,
            "--slots" => slots = num(v)? as u32,
            "--slot-size" => slot_size = num(v)? as u32,
            "--allow-uid" => a.allow_uid = Some(num(v)? as u32),
            "--core-dumps" => a.core_dumps = true,
            "--standby" => a.standby = matches!(v, "warm" | ""),
            "--hb-interval-ms" => a.hb_interval_ms = num(v)?,
            "--hb-stale-ms" => a.hb_stale_ms = num(v)?,
            "--max-deadline-ms" => a.max_deadline_ms = num(v)?,
            "--spin-us" => a.spin_us = Some(num(v)?),
            "--session-grace-ms" => a.session_grace_ms = Some(num(v)?),
            "--load-timeout-ms" => a.load_timeout_ms = num(v)?,
            "--backoff-max-ms" => a.backoff_max_ms = Some(num(v)?),
            "--http" => a.http = Some(v.to_string()),
            "--admin" => a.admin = Some(PathBuf::from(v)),
            "--cgroup" => a.cgroup = Some(PathBuf::from(v)),
            "--fail-open" => a.fail_open = v != "false",
            // Fault injection for the degraded-start tests: fail creating the arena N times.
            "--test-fail-arena" => a.test_fail_arena = num(v)? as u32,
            "--test-abi-major" => a.test_abi_major = Some(num(v)? as u16),
            "--oom-shield" => {
                a.oom_shield = shield::Asked::parse(v).ok_or(format!("{k}: {v} is not auto, singleProcess, nestedCgroup, oomScoreAdj, or none"))?
            }
            "--worker-memory-max-mb" => a.worker_memory_max = Some(num(v)? << 20),
            "--worker-arg" => a.worker_args.push(v.to_string()),
            // Another directory or file the worker's sandbox may read: an interpreter's library,
            // a runner's model module (sdk-python §5).
            "--worker-read" => extra_read.push(PathBuf::from(v)),
            "--worker-socketpair" => a.sandbox.allow_socketpair = v != "false",
            "--memory-monitor" => a.memory_monitor = v != "off" && v != "false",
            "--log-mono" => LOG_MONO.store(true, std::sync::atomic::Ordering::Relaxed),
            "--model" => a.model = Some(PathBuf::from(v)),
            "--model-sha256" => {
                let b: Vec<u8> = (0..v.len()).step_by(2).filter_map(|i| v.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect();
                a.model_pin = Some(b.try_into().map_err(|_| format!("{k}: not 64 hex digits"))?);
            }
            "--rollback" => a.rollback = v != "false",
            "--rollback-window-ms" => a.rollback_window_ms = num(v)?,
            "--resp-cap" => resp_cap = Some(num(v)? as u32),
            "--batch-slots" => batch = num(v)? as u32,
            "--batch-slot-size" => batch_size = num(v)? as u32,
            "--batch-resp-cap" => batch_resp = Some(num(v)? as u32),
            "--worker-rlimit-data-mb" => a.sandbox.rlimit_data = Some(num(v)? << 20),
            "--seccomp" => a.sandbox.seccomp = v != "false",
            "--landlock" => a.sandbox.landlock = v != "false",
            "--wait-for-model" => a.wait_for_model = v != "false",
            "--model-copy" => {
                if !matches!(v, "none" | "memfd") {
                    return Err(format!("{k}: {v} is not none or memfd"));
                }
                a.model_copy = v.to_string();
            }
            "--hugepages" => a.hugepages = v != "false",
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    if !socket || !worker {
        return Err("--socket and --worker are required".into());
    }
    if p_shed.is_some() || p_recover.is_some() || p_hard.is_some() {
        let (shed, recover, hard) = (p_shed.unwrap_or(0.85), p_recover.unwrap_or(0.75), p_hard.unwrap_or(0.95));
        // The gap between recover and shed is the hysteresis that keeps the gate from flapping.
        if !(recover < shed && shed <= hard) {
            return Err(format!("--pressure-recover ({recover}) < --pressure-shed ({shed}) <= --pressure-hard ({hard}) must hold"));
        }
        a.pressure = Some((shed, recover, hard));
    }
    if rings == 0 || rings > deathlock::MAX {
        return Err(format!("--rings must be 1 to {}", deathlock::MAX));
    }
    // A slot is a request region and a response region; the response gets half unless set.
    let class = |size: u32, count: u32, resp: Option<u32>| -> Result<SlabClass, String> {
        let resp = resp.unwrap_or(size / 2 / 64 * 64);
        if resp == 0 || resp >= size || !resp.is_multiple_of(64) {
            return Err(format!("a response region of {resp} bytes in a {size} byte slot"));
        }
        Ok(SlabClass { slot_size: size, slot_count: count, req_cap: size - resp, resp_cap: resp })
    };
    let mut classes = vec![class(slot_size, slots, resp_cap)?];
    if batch > 0 {
        classes.push(class(batch_size, batch, batch_resp)?);
    }
    a.geometry = Geometry { rings: vec![cap; rings], slab_classes: classes, staging_len: 0x10000 };
    a.sandbox.core_dumps = a.core_dumps;
    let rl = a.sandbox.rlimit_data;
    let (sc, ll, sp) = (a.sandbox.seccomp, a.sandbox.landlock, a.sandbox.allow_socketpair);
    a.sandbox = sandbox::Config {
        rlimit_data: rl,
        seccomp: sc,
        landlock: ll,
        core_dumps: a.core_dumps,
        allow_socketpair: sp,
        ..sandbox::Config::new(&[a.worker.as_path(), std::env::current_exe().unwrap_or_default().as_path()])
    };
    a.sandbox.read.extend(extra_read);
    Ok(a)
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

// ── epoll and the other descriptors the loop waits on ───────────────────────────────────

pub struct Epoll(OwnedFd);

impl Epoll {
    fn new() -> io::Result<Epoll> {
        // SAFETY: plain creation.
        let fd = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        Ok(Epoll(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
    pub fn add(&self, fd: BorrowedFd<'_>, token: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event { events: (libc::EPOLLIN | libc::EPOLLRDHUP) as u32, u64: token };
        // SAFETY: a valid event for a valid descriptor.
        cvt(unsafe { libc::epoll_ctl(self.0.as_raw_fd(), libc::EPOLL_CTL_ADD, fd.as_raw_fd(), &mut ev) }).map(|_| ())
    }
    pub fn del(&self, fd: BorrowedFd<'_>) {
        // SAFETY: removing a registered descriptor.
        unsafe { libc::epoll_ctl(self.0.as_raw_fd(), libc::EPOLL_CTL_DEL, fd.as_raw_fd(), core::ptr::null_mut()) };
    }
    fn wait(&self, evs: &mut [libc::epoll_event]) -> io::Result<usize> {
        loop {
            // SAFETY: the buffer is valid for its length.
            let n = unsafe { libc::epoll_wait(self.0.as_raw_fd(), evs.as_mut_ptr(), evs.len() as i32, -1) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

fn timerfd(interval_ns: u64) -> io::Result<OwnedFd> {
    // SAFETY: plain creation, then arming with a valid itimerspec.
    let fd = cvt(unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) })?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let ts = libc::timespec { tv_sec: 0, tv_nsec: interval_ns as libc::c_long };
    let spec = libc::itimerspec { it_interval: ts, it_value: ts };
    cvt(unsafe { libc::timerfd_settime(fd.as_raw_fd(), 0, &spec, core::ptr::null_mut()) })?;
    Ok(fd)
}

/// SIGTERM and SIGINT (shutdown), SIGHUP (reload), and SIGCHLD (reap helpers and adopted grandchildren).
fn signalfd() -> io::Result<OwnedFd> {
    // SAFETY: sigset manipulation on a local, then a signalfd over it.
    unsafe {
        let mut set: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGCHLD] {
            libc::sigaddset(&mut set, s);
        }
        cvt(libc::sigprocmask(libc::SIG_BLOCK, &set, core::ptr::null_mut()))?;
        let fd = cvt(libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC))?;
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn inotify_model(model: Option<&std::path::Path>) -> Option<OwnedFd> {
    let path = model?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(parent.as_os_str())).ok()?;
    // SAFETY: initializing a non-blocking close-on-exec inotify descriptor.
    let fd = cvt(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) }).ok()?;
    // SAFETY: fd is open and we own it exclusively here.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_MODIFY | libc::IN_ATTRIB;
    // SAFETY: `c` is a valid C string for an open inotify descriptor.
    if cvt(unsafe { libc::inotify_add_watch(owned.as_raw_fd(), c.as_ptr(), mask) }).is_err() {
        return None;
    }
    Some(owned)
}

fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: plain creation.
    let fd = cvt(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn drain(fd: BorrowedFd<'_>, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while let Ok(n) = sys::read(fd, buf) {
        if n == 0 {
            break;
        }
        total += n;
        if n < buf.len() {
            break;
        }
    }
    total
}

// ── tokens ──────────────────────────────────────────────────────────────────────────────

const T_LISTEN: u64 = 1;
const T_TIMER: u64 = 2;
const T_SIGNAL: u64 = 3;
const T_LOCKS: u64 = 4;
pub const T_HTTP: u64 = 5;
pub const T_ADMIN: u64 = 6;
const T_INOTIFY: u64 = 7;
const K_STATUS: u64 = 1 << 32;
const K_PIDFD: u64 = 2 << 32;
const K_CONN: u64 = 3 << 32;
pub const K_HTTP: u64 = 4 << 32;
pub const K_ADMIN: u64 = 5 << 32;
const K_HELPER: u64 = 6 << 32;
const KIND: u64 = 0xFFFF_FFFF << 32;

// ── the shell: what the core asks for, and the session socket ───────────────────────────

struct Proc {
    sp: spawn::Spawned,
    watched: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    AwaitHello,
    AwaitReady,
    Bound,
}

struct Conn {
    fd: OwnedFd,
    stage: Stage,
    since: u64,
    session_id: u64,
}

struct Shell<'a> {
    args: &'a Args,
    arena: &'a OwnedArena,
    ep: Epoll,
    listener: OwnedFd,
    procs: HashMap<u32, Proc>,
    reports: Arc<Mutex<Vec<(u32, usize)>>>,
    locks_efd: Arc<OwnedFd>,
    conns: HashMap<u64, Conn>,
    next_conn: u64,
    next_session: u64,
    stub: spawn::Stub,
    models: HashMap<u32, ModelFile>,
    next_model: u32,
    /// The identity of the file the model path named when it was last opened.
    path_seen: Option<Identity>,
    helpers: HashMap<u64, HelperRun>,
    next_helper: u64,
    /// Exit statuses already reaped on SIGCHLD before a worker's pidfd became readable.
    reaped: HashMap<libc::pid_t, i32>,
    /// Events an effect produced, delivered to the core after the current one.
    pending: VecDeque<Event>,
    model_modified: u64,
    shield: shield::Shield,
}

/// An open model file (supervisor §5.1): its descriptor and the identity recorded at open.
struct ModelFile {
    fd: OwnedFd,
    id: Identity,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Identity {
    dev: u64,
    ino: u64,
    size: i64,
    mtime: (i64, i64),
}

fn identity(st: &libc::stat) -> Identity {
    // Device, inode, size, and modification time; not the change time, which a rename or an
    // unlink moves (spike S7).
    Identity { dev: st.st_dev, ino: st.st_ino, size: st.st_size, mtime: (st.st_mtime, st.st_mtime_nsec) }
}

fn fstat(fd: BorrowedFd<'_>) -> io::Result<libc::stat> {
    // SAFETY: fstat into a zeroed struct.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok(st)
}

fn stat_path(p: &std::path::Path) -> io::Result<libc::stat> {
    let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str())).map_err(io::Error::other)?;
    // SAFETY: stat of a valid C string into a zeroed struct.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    cvt(unsafe { libc::stat(c.as_ptr(), &mut st) })?;
    Ok(st)
}

enum HelperKind {
    Hash { model: u32 },
    Verify { gen: u32, bytes: Vec<u8> },
}

struct HelperRun {
    kind: HelperKind,
    h: spawn::Helper,
    buf: Vec<u8>,
    started: u64,
}

const T_HELPER_TIMEOUT_HASH_NS: u64 = 300_000_000_000;
const T_HELPER_TIMEOUT_VERIFY_NS: u64 = 10_000_000_000;

impl Effects for Shell<'_> {
    fn spawn(&mut self, gen: u32, beat: u32, model: u32) -> bool {
        let Some(m) = self.models.get(&model) else {
            log!("worker gen {gen}: model {model} is not open");
            self.pending.push_back(Event::Digest { model, sha: None }); // unusable: stop asking
            return false;
        };
        // A served model must not change in place (§5.1): check before every spawn from it.
        if model != 0 && fstat(m.fd.as_fd()).map(|st| identity(&st)).ok() != Some(m.id) {
            self.model_modified += 1;
            log!("worker gen {gen}: MODEL_MODIFIED: model {model} changed since it was opened; not loading it");
            return false;
        }
        let cg = match shield::worker_procs(&self.shield, beat) {
            Ok(c) => c,
            Err(e) => {
                log!("worker gen {gen}: its cgroup: {e}");
                return false;
            }
        };
        match spawn::spawn(&self.stub, &self.args.worker, &self.args.worker_args, self.arena, gen, beat, m.fd.as_fd(), cg.as_ref().map(|c| c.as_fd())) {
            Ok(sp) => {
                let ok = self.ep.add(sp.status.as_fd(), K_STATUS | gen as u64).and_then(|_| self.ep.add(sp.pidfd.as_fd(), K_PIDFD | gen as u64));
                if let Err(e) = ok {
                    log!("worker gen {gen}: {e}");
                }
                self.procs.insert(gen, Proc { sp, watched: false });
                true
            }
            Err(e) => {
                log!("worker gen {gen}: spawn failed: {e}");
                false
            }
        }
    }
    fn kill(&mut self, gen: u32) {
        if let Some(p) = self.procs.get(&gen) {
            // SAFETY: signalling our own, not yet reaped, child.
            unsafe { libc::kill(p.sp.pid, libc::SIGKILL) };
        }
    }
    fn watch(&mut self, gen: u32, rings: usize) {
        let Some(p) = self.procs.get_mut(&gen) else { return };
        if std::mem::replace(&mut p.watched, true) {
            return;
        }
        for i in 0..rings {
            let (wrk, reports, efd) = (p.sp.wrk.clone(), self.reports.clone(), self.locks_efd.clone());
            let r = std::thread::Builder::new().name(format!("scz-watch-{gen}-{i}")).spawn(move || {
                if deathlock::watch(&wrk.seg(), i) == deathlock::Watch::Died {
                    reports.lock().unwrap_or_else(|e| e.into_inner()).push((gen, i));
                    let one = 1u64.to_ne_bytes();
                    // SAFETY: an 8-byte write to an eventfd that outlives this thread (Arc).
                    unsafe { libc::write(efd.as_raw_fd(), one.as_ptr() as *const libc::c_void, 8) };
                }
            });
            if let Err(e) = r {
                log!("watcher for gen {gen} ring {i}: {e}"); // the pidfd still reports the death
            }
        }
    }
    fn end_session(&mut self) {
        for c in self.conns.values().filter(|c| c.stage == Stage::Bound) {
            // SAFETY: shutting the sending side of a socket we own.
            unsafe { libc::shutdown(c.fd.as_raw_fd(), libc::SHUT_WR) };
        }
    }
    fn log(&mut self, msg: &str) {
        log!("{msg}");
    }
    fn verify(&mut self, gen: u32) -> Option<Result<(Vec<u8>, u64), String>> {
        let Some(beat) = self.procs.get(&gen).map(|p| p.sp.beat) else { return Some(Err("no such worker".into())) };
        let (data, dir) = (self.arena.data.seg(), &self.arena.dir);
        let len = scz_shm::abi::acc::beat::staged_len(&data, dir.beat(beat as usize)) as u64;
        if len == 0 {
            return Some(Ok((vec![], 0))); // a worker without a schema: nothing to verify
        }
        if len > dir.layout.staging_len {
            return Some(Err(format!("staged_len {len} exceeds the staging area")));
        }
        // Copy the candidate out without interpreting it, into a sealed memfd of our own: the
        // worker cannot change it between verification and publication (§2, "Helper processes").
        let mut bytes = vec![0u8; len as usize];
        data.copy_out((dir.layout.staging_off + beat as u64 * dir.layout.staging_len) as usize, &mut bytes);
        let r = (|| -> io::Result<spawn::Helper> {
            let fd = scz_shm::arena::memfd(c"scz-candidate", len)?;
            let mut f = std::fs::File::from(fd.try_clone()?);
            std::io::Write::write_all(&mut f, &bytes)?;
            scz_shm::arena::add_seals(fd.as_fd(), libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL)?;
            // SAFETY: lseek on a descriptor we own; the helper reads from the start.
            unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
            let caps: Vec<String> = dir.layout.slabs.iter().map(|s| format!("{}x{}", s.2.req_cap, s.2.resp_cap)).collect();
            spawn::helper(&self.stub, &["--helper=verify".to_string(), format!("--caps={}", caps.join(","))], fd.as_fd())
        })();
        match r {
            Ok(h) => {
                self.add_helper(HelperKind::Verify { gen, bytes }, h);
                None
            }
            Err(e) => Some(Err(format!("starting the verifier: {e}"))),
        }
    }
    fn release_model(&mut self, model: u32) {
        if model != 0 && self.models.remove(&model).is_some() {
            log!("model {model}: descriptor closed");
        }
    }
}

impl Shell<'_> {
    fn add_helper(&mut self, kind: HelperKind, h: spawn::Helper) {
        let k = self.next_helper;
        self.next_helper += 1;
        if let Err(e) = self.ep.add(h.out.as_fd(), K_HELPER | k) {
            log!("helper: {e}");
        }
        self.helpers.insert(k, HelperRun { kind, h, buf: vec![], started: monotonic_ns() });
    }
    /// Open the model path as a new model; start hashing it.
    fn open_model(&mut self, path: &std::path::Path) -> io::Result<u32> {
        let mut f = std::fs::File::open(path)?;
        let st = fstat(f.as_fd())?;
        let fd = if self.args.model_copy == "memfd" {
            let mfd = scz_shm::arena::memfd(c"scz-model", st.st_size as u64)?;
            let mut dst = std::fs::File::from(mfd.try_clone()?);
            let copied = std::io::copy(&mut f, &mut dst)?;
            if copied != st.st_size as u64 {
                return Err(io::Error::other(format!("short copy to memfd: {copied} != {}", st.st_size)));
            }
            std::io::Seek::rewind(&mut dst)?;
            drop(dst);
            scz_shm::arena::add_seals(mfd.as_fd(), libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL)?;
            mfd
        } else {
            OwnedFd::from(f)
        };
        let model_st = fstat(fd.as_fd())?;
        let id = self.next_model;
        self.next_model += 1;
        self.path_seen = Some(identity(&st));
        let h = spawn::helper(&self.stub, &["--helper=hash".to_string()], fd.as_fd())?;
        self.add_helper(HelperKind::Hash { model: id }, h);
        self.models.insert(id, ModelFile { fd, id: identity(&model_st) });
        log!("model {id}: {} opened ({} bytes); hashing", path.display(), st.st_size);
        Ok(id)
    }
    /// A helper's answer pipe is readable: collect it, and at end of file turn it into an event.
    fn on_helper(&mut self, k: u64) {
        let Some(h) = self.helpers.get_mut(&k) else { return };
        let mut chunk = [0u8; 512];
        loop {
            match sys::read(h.h.out.as_fd(), &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    h.buf.extend_from_slice(&chunk[..n]);
                    if h.buf.len() > 4096 {
                        break;
                    }
                }
                Err(_) => return, // would block: more to come
            }
        }
        let h = self.helpers.remove(&k).unwrap();
        self.ep.del(h.h.out.as_fd());
        self.finish_helper(h, None);
    }
    fn finish_helper(&mut self, h: HelperRun, failure: Option<&str>) {
        // SAFETY: non-blocking reap; SIGCHLD also reaps children in the main loop.
        unsafe { libc::waitpid(h.h.pid, core::ptr::null_mut(), libc::WNOHANG) };
        let line = String::from_utf8_lossy(&h.buf).trim().to_string();
        let why = failure.map(String::from).unwrap_or_else(|| if line.is_empty() { "it exited without answering".into() } else { line.clone() });
        let ev = match h.kind {
            HelperKind::Hash { model } => {
                let d: Vec<u8> = (0..line.len()).step_by(2).filter_map(|i| line.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok())).collect();
                let sha = if failure.is_none() { <[u8; 32]>::try_from(d).ok() } else { None };
                if sha.is_none() {
                    log!("model {model}: hashing failed: {why}");
                }
                Event::Digest { model, sha }
            }
            HelperKind::Verify { gen, bytes } => {
                let result = match line.strip_prefix("OK ") {
                    Some(fp) if failure.is_none() => u64::from_str_radix(fp, 16).map(|fp| (bytes, fp)).map_err(|_| format!("the verifier said {line:?}")),
                    _ => Err(format!("the verifier refused it: {}", why.trim_start_matches("ERR "))),
                };
                Event::Verdict { gen, result }
            }
        };
        self.pending.push_back(ev);
    }
    /// Helpers that have run too long are killed and count as failures.
    fn expire_helpers(&mut self, now: u64) {
        let late: Vec<u64> = self
            .helpers
            .iter()
            .filter(|(_, h)| {
                let limit = if matches!(h.kind, HelperKind::Hash { .. }) { T_HELPER_TIMEOUT_HASH_NS } else { T_HELPER_TIMEOUT_VERIFY_NS };
                now - h.started > limit
            })
            .map(|(&k, _)| k)
            .collect();
        for k in late {
            let h = self.helpers.remove(&k).unwrap();
            self.ep.del(h.h.out.as_fd());
            // SAFETY: killing our own child.
            unsafe { libc::kill(h.h.pid, libc::SIGKILL) };
            self.finish_helper(h, Some("timed out"));
        }
    }
    /// The model path (§5.6): a different file there is a reload; an in-place write is refused.
    fn watch_model_path(&mut self) {
        let Some(path) = self.args.model.clone() else { return };
        let Ok(st) = stat_path(&path) else { return };
        let id = identity(&st);
        if self.path_seen == Some(id) {
            return;
        }
        if self.path_seen.is_some_and(|prev| prev.dev == id.dev && prev.ino == id.ino) {
            self.path_seen = Some(id);
            self.model_modified += 1;
            log!("MODEL_MODIFIED: {} changed in place since it was opened; not loading it", path.display());
            return;
        }
        match self.open_model(&path) {
            Ok(id) => self.pending.push_back(Event::NewModel { model: id }),
            Err(e) => log!("model path {}: {e}", path.display()),
        }
    }
    fn on_listen(&mut self) {
        loop {
            match handshake::accept(self.listener.as_fd()) {
                Ok(fd) => {
                    let _ = sys::set_nonblocking(fd.as_fd());
                    let k = self.next_conn;
                    self.next_conn += 1;
                    if self.ep.add(fd.as_fd(), K_CONN | k).is_ok() {
                        self.conns.insert(k, Conn { fd, stage: Stage::AwaitHello, since: monotonic_ns(), session_id: 0 });
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    log!("accept: {e}");
                    return;
                }
            }
        }
    }

    /// A message or end of stream on a connection; returns a session event for the core.
    fn on_conn(&mut self, k: u64, core: &Core<'_>) -> Option<Event> {
        let c = self.conns.get(&k)?;
        let (stage, session) = (c.stage, c.session_id);
        let msg = match handshake::recv(c.fd.as_fd()) {
            Ok(Some((m, _fds))) => m,
            Ok(None) => return self.close_conn(k),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
            Err(_) => return self.close_conn(k),
        };
        match stage {
            Stage::AwaitHello => self.on_hello(k, &msg, core),
            Stage::AwaitReady => match handshake::decode_ready(&msg) {
                Some(id) if id == self.arena.dir.arena_id => {
                    self.conns.get_mut(&k)?.stage = Stage::Bound;
                    log!("session {session} bound");
                    Some(Event::SessionBound)
                }
                _ => self.close_conn(k),
            },
            Stage::Bound => None,
        }
    }

    fn close_conn(&mut self, k: u64) -> Option<Event> {
        let c = self.conns.remove(&k)?;
        self.ep.del(c.fd.as_fd());
        match c.stage {
            Stage::Bound => {
                log!("session {} closed by the client", c.session_id);
                Some(Event::SessionClosed)
            }
            // A client that was sent the memfds may have mapped the arena: reset as for a session.
            Stage::AwaitReady => Some(Event::SessionClosed),
            Stage::AwaitHello => None,
        }
    }

    fn on_hello(&mut self, k: u64, msg: &handshake::Msg, core: &Core<'_>) -> Option<Event> {
        let fd = self.conns.get(&k)?.fd.as_raw_fd();
        // SAFETY: the descriptor is owned by `self.conns` for the duration of this call.
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let refuse = |status: u32, why: &str| {
            log!("connection {k}: {why}");
            let _ = handshake::send(bfd, &Welcome::refusal(status).encode(), &[]);
        };
        let Some(h) = Hello::decode(msg) else {
            refuse(ws::DENIED, "not a HELLO");
            return self.close_conn(k);
        };
        let cred = handshake::peer_cred(bfd).ok();
        if let (Some(allow), Some(c)) = (self.args.allow_uid, cred) {
            if c.uid != allow {
                refuse(ws::DENIED, &format!("uid {} is not allowed", c.uid));
                return self.close_conn(k);
            }
        }
        let Some(minor) = h.negotiate().filter(|_| self.args.test_abi_major.is_none_or(|m| m == h.abi_major)) else {
            refuse(ws::ABI_MISMATCH, "ABI mismatch");
            return self.close_conn(k);
        };
        let other = self.conns.iter().any(|(&j, c)| j != k && c.stage != Stage::AwaitHello);
        if other || !core.accepting() {
            refuse(ws::BUSY, "a session exists or is being reset");
            return self.close_conn(k);
        }
        self.next_session += 1;
        let w = Welcome {
            status: ws::OK,
            abi_major: abi::ABI_MAJOR,
            abi_minor: minor,
            ctl_len: self.arena.ctl.len() as u32,
            data_len: self.arena.dir.data_len,
            arena_id: self.arena.dir.arena_id,
            session_id: self.next_session,
        };
        let fds = [self.arena.ctl_fd.as_raw_fd(), self.arena.data_fd.as_raw_fd()];
        match handshake::send(bfd, &w.encode(), &fds) {
            Ok(()) => {
                let c = self.conns.get_mut(&k)?;
                c.stage = Stage::AwaitReady;
                c.since = monotonic_ns();
                c.session_id = self.next_session;
                log!("session {} offered to pid {} (sdk {} v{})", self.next_session, h.pid, abi::sdk_lang::name(h.sdk_lang as u32), h.sdk_version);
                None
            }
            Err(_) => self.close_conn(k),
        }
    }

    /// Connections that have not completed the handshake within a second are dropped.
    fn expire_conns(&mut self, now: u64) {
        let stale: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| (c.stage == Stage::AwaitHello || c.stage == Stage::AwaitReady) && c.since + 1_000_000_000 < now)
            .map(|(&k, _)| k)
            .collect();
        for k in stale {
            if let Some(ev) = self.close_conn(k) {
                self.pending.push_back(ev);
            }
        }
    }

    fn reap(&mut self, gen: u32) -> String {
        let Some(p) = self.procs.remove(&gen) else { return String::new() };
        self.ep.del(p.sp.status.as_fd());
        self.ep.del(p.sp.pidfd.as_fd());
        spawn::reap_status(p.sp.pid, self.reaped.remove(&p.sp.pid))
    }
}

/// Each worker's own memory.max in nestedCgroup, when not configured: what the container's
/// limit leaves after the arena and an allowance for the supervisor, shared by the workers
/// that can exist at once. A worker's limit must leave room for everything else, or the
/// container's own limit is reached first and its group kill applies (spike S2).
fn default_worker_max(limit: u64, arena: u64, standby: bool) -> u64 {
    let rest = limit.saturating_sub(arena + scz_shm::layout::CTL_LEN + (64 << 20));
    (rest / if standby { 2 } else { 1 }).max(16 << 20)
}

fn run(args: Args) -> io::Result<()> {
    // SAFETY: prctl with PR_SET_CHILD_SUBREAPER adopts any orphaned grandchild processes so waitpid can reap them.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    let sig = signalfd()?;
    // Supervisor §3 step 3, first the OOM shield, while this process's own /proc entries are
    // still its own to write (non-dumpable makes them root's). The container's cgroup is found
    // once, here: in nestedCgroup the supervisor then lives in a child of it.
    let layout = scz_shm::layout::Layout::compute(&args.geometry).map_err(|e| io::Error::other(format!("{e:?}")))?;
    let cgroup = args.cgroup.clone().or_else(|| shield::own_cgroup().ok());
    let limit = cgroup.as_ref().and_then(|d| std::fs::read_to_string(d.join("memory.max")).ok()).and_then(|s| s.trim().parse::<u64>().ok());
    let worker_max = args.worker_memory_max.or_else(|| limit.map(|l| default_worker_max(l, layout.data_len, args.standby)));
    let sh = shield::settle(args.oom_shield, cgroup.as_deref(), worker_max);
    log!("oom shield: {} ({})", sh.mode.name(), sh.note);
    let _ = shield::SETTLED.set((sh.mode, cgroup.clone()));
    // Then no process with our uid may read our memory or copy our fds.
    // SAFETY: prctl with integer arguments.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) };
    let ep = Epoll::new()?;
    let tick_ns = (args.hb_interval_ms * 1_000_000).clamp(1_000_000, 2_000_000);
    let timer = timerfd(tick_ns)?;
    ep.add(timer.as_fd(), T_TIMER)?;
    ep.add(sig.as_fd(), T_SIGNAL)?;
    // The probes come first: they answer through a degraded start (§3).
    let mut http = match &args.http {
        Some(addr) => Some(http::Http::bind(addr, &ep)?),
        None => None,
    };
    // The memory monitor. Without cgroup v2 files it is off and the supervisor serves anyway.
    let mut monitor = match monitor::Monitor::open(cgroup.clone()).and_then(|m| if args.memory_monitor { Ok(m) } else { Err("--memory-monitor=off".into()) }) {
        Ok(m) => {
            log!("memory monitor: {}", m.describe());
            Some(m)
        }
        Err(e) => {
            log!("memory monitor off: {e}");
            None
        }
    };
    // A fault in the pod's own specification stops the pod (§3): an arena larger than the
    // container's memory limit.
    if let Some(limit) = monitor.as_ref().and_then(|m| m.limit()) {
        let need = layout.data_len + scz_shm::layout::CTL_LEN;
        if need > limit {
            return Err(io::Error::other(format!("the arena needs {} MiB and the memory limit is {} MiB", need >> 20, limit >> 20)));
        }
    }
    let now = monotonic_ns();
    let arena_id = now ^ ((std::process::id() as u64) << 40);
    let mut acfg = ArenaConfig::with_defaults(arena_id, now);
    acfg.hb_interval_ns = args.hb_interval_ms * 1_000_000;
    acfg.hb_stale_ns = args.hb_stale_ms * 1_000_000;
    acfg.max_deadline_ns = args.max_deadline_ms * 1_000_000;
    if let Some(us) = args.spin_us {
        acfg.spin_ns = us * 1_000;
    }
    // The arena and the session socket. A failure here is the node's, not the pod's: with
    // fail-open the supervisor serves its probes degraded and retries on the backoff schedule.
    let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 64];
    let mut scratch = [0u8; 256];
    let (mut fails, mut delay, mut next_try, mut cause) = (args.test_fail_arena, 50_000_000u64, 0u64, String::new());
    let (arena, listener) = loop {
        let now = monotonic_ns();
        if now >= next_try {
            let attempt = if fails > 0 {
                fails -= 1;
                Err("injected failure (--test-fail-arena)".to_string())
            } else {
                create(&args, &acfg)
            };
            match attempt {
                Ok(x) => break x,
                Err(e) if args.fail_open => {
                    log!("degraded start: {e}; retrying in {} ms", delay / 1_000_000);
                    cause = e;
                    next_try = now + delay;
                    delay = (delay * 2).min(5_000_000_000);
                }
                Err(e) => return Err(io::Error::other(e)),
            }
        }
        for ev in shell_free_wait(&ep, &mut evs)? {
            match ev {
                T_TIMER => {
                    drain(timer.as_fd(), &mut scratch[..8]);
                    if let Some(h) = http.as_mut() {
                        h.expire(monotonic_ns(), &ep);
                    }
                }
                T_SIGNAL => {
                    // SAFETY: signalfd_siginfo is plain data; reads are whole structs.
                    let mut si: libc::signalfd_siginfo = unsafe { core::mem::zeroed() };
                    let sz = core::mem::size_of::<libc::signalfd_siginfo>();
                    while unsafe { libc::read(sig.as_raw_fd(), &mut si as *mut _ as *mut libc::c_void, sz) } == sz as isize {
                        if si.ssi_signo == libc::SIGCHLD as u32 {
                            let mut status: libc::c_int = 0;
                            while unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } > 0 {}
                            continue;
                        }
                        log!("stopped while degraded");
                        return Ok(());
                    }
                }
                T_HTTP => {
                    if let Some(h) = http.as_mut() {
                        h.accept(&ep);
                    }
                }
                t if t & KIND == K_HTTP => {
                    if let Some(h) = http.as_mut() {
                        h.serve(t & !KIND, &ep, |path| http::degraded(path, &cause, args.wait_for_model));
                    }
                }
                _ => {}
            }
        }
    };
    log!(
        "arena {arena_id:#018x}: {} KiB data, {} ring(s) of {}, {} slot(s) of {} B",
        arena.dir.data_len >> 10,
        arena.dir.layout.rings.len(),
        args.geometry.rings[0],
        args.geometry.slab_classes[0].slot_count,
        args.geometry.slab_classes[0].slot_size,
    );
    let locks_efd = Arc::new(eventfd()?);
    ep.add(listener.as_fd(), T_LISTEN)?;
    ep.add(locks_efd.as_fd(), T_LOCKS)?;
    let mut admin = match &args.admin {
        Some(path) => Some(admin::Admin::bind(path, &ep)?),
        None => None,
    };
    log!(
        "listening on {}{}{}",
        args.socket,
        args.http.as_deref().map(|h| format!(", probes on {h}")).unwrap_or_default(),
        args.admin.as_ref().map(|a| format!(", admin on {}", a.display())).unwrap_or_default()
    );

    let mut cfg = Config::with_defaults(acfg.hb_interval_ns, acfg.hb_stale_ns, acfg.max_deadline_ns);
    cfg.standby = args.standby;
    cfg.pin = args.model_pin;
    cfg.rollback = args.rollback;
    cfg.rollback_window_ns = args.rollback_window_ms * 1_000_000;
    cfg.load_timeout_ns = args.load_timeout_ms * 1_000_000;
    if let Some(ms) = args.session_grace_ms {
        cfg.session_grace_ns = ms * 1_000_000;
    }
    if let Some(b) = args.backoff_max_ms {
        cfg.backoff_max_ns = b * 1_000_000;
    }
    if let Some((shed, recover, hard)) = args.pressure {
        (cfg.relieve_at, cfg.recover_at, cfg.shed_at) = (shed, recover, hard);
    }
    if let Some(ms) = args.shed_max_ms {
        cfg.shed_max_ns = ms * 1_000_000;
    }
    let mut core = Core::new(cfg, arena.ctl.seg(), arena.data.seg(), &arena.dir, monotonic_ns());
    let mut shell = Shell {
        args: &args,
        arena: &arena,
        ep,
        listener,
        procs: HashMap::new(),
        reports: Arc::new(Mutex::new(Vec::new())),
        locks_efd,
        conns: HashMap::new(),
        next_conn: 0,
        next_session: 0,
        stub: spawn::Stub::new(&args.sandbox),
        models: HashMap::new(),
        next_model: 1,
        path_seen: None,
        helpers: HashMap::new(),
        next_helper: 0,
        reaped: HashMap::new(),
        pending: VecDeque::new(),
        model_modified: 0,
        shield: sh,
    };
    let inotify = inotify_model(args.model.as_deref());
    if let Some(fd) = &inotify {
        if let Err(e) = shell.ep.add(fd.as_fd(), T_INOTIFY) {
            log!("inotify register: {e}");
        }
    }
    log!("sandbox: {}", shell.stub.layers);
    // Model 0 is "no model": /dev/null, for workers that need none (the echo and chaos workers).
    shell.models.insert(0, ModelFile { fd: OwnedFd::from(std::fs::File::open("/dev/null")?), id: Identity { dev: 0, ino: 0, size: 0, mtime: (0, 0) } });
    if let Some(path) = args.model.clone() {
        let model = match shell.open_model(&path) {
            Ok(id) => id,
            Err(e) => {
                // §3: the arena and the socket are up and the gate stays INIT; the path watch
                // picks the model up when it appears.
                log!("degraded: model {}: {e}", path.display());
                core.stats.last_cause = format!("model file {}: {e}", path.display());
                machine::NO_MODEL
            }
        };
        core.handle(monotonic_ns(), Event::NewModel { model }, &mut shell);
    }
    let mut last_tick = monotonic_ns();
    let mut ticks = 0u64;
    loop {
        let n = shell.ep.wait(&mut evs)?;
        for ev in evs[..n].iter().copied() {
            let now = monotonic_ns();
            match ev.u64 {
                T_TIMER => {
                    drain(timer.as_fd(), &mut scratch[..8]);
                    last_tick = now;
                    ticks += 1;
                    core.handle(now, Event::Tick, &mut shell);
                    // Every ~10 ms: the memory readings, and whether the container was throttled.
                    if ticks.is_multiple_of(5) {
                        if let Some(m) = monitor.as_mut() {
                            let pids: Vec<(u32, i32)> = shell.procs.iter().map(|(&g, p)| (g, p.sp.pid)).collect();
                            if let Some(sample) = m.sample(now, &pids) {
                                core.handle(now, Event::Memory(sample), &mut shell);
                            }
                            if m.throttled() {
                                core.handle(now, Event::Throttled, &mut shell);
                            }
                        }
                    }
                    shell.expire_conns(now);
                    shell.expire_helpers(now);
                    if ticks.is_multiple_of(500) {
                        shell.watch_model_path();
                    }
                    if let Some(h) = http.as_mut() {
                        h.expire(now, &shell.ep);
                    }
                    if core.finished() {
                        return shutdown(&mut shell, &args);
                    }
                }
                T_LISTEN => shell.on_listen(),
                T_SIGNAL => {
                    // SAFETY: signalfd_siginfo is plain data; reads are whole structs.
                    let mut si: libc::signalfd_siginfo = unsafe { core::mem::zeroed() };
                    let sz = core::mem::size_of::<libc::signalfd_siginfo>();
                    while unsafe { libc::read(sig.as_raw_fd(), &mut si as *mut _ as *mut libc::c_void, sz) } == sz as isize {
                        if si.ssi_signo == libc::SIGCHLD as u32 {
                            loop {
                                let mut status: libc::c_int = 0;
                                // SAFETY: waitpid(-1, WNOHANG) reaps exited children and adopted grandchildren.
                                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                                if pid <= 0 {
                                    break;
                                }
                                if shell.procs.values().any(|p| p.sp.pid == pid) {
                                    shell.reaped.insert(pid, status);
                                } else if !shell.helpers.values().any(|h| h.h.pid == pid) {
                                    log!("reaped orphan child pid {pid}");
                                }
                            }
                            continue;
                        }
                        let e = if si.ssi_signo == libc::SIGHUP as u32 { Event::Reload } else { Event::Shutdown };
                        log!("signal {}: {:?}", si.ssi_signo, e);
                        core.handle(now, e, &mut shell);
                    }
                }
                T_INOTIFY => {
                    if let Some(fd) = &inotify {
                        let mut ibuf = [0u8; 4096];
                        drain(fd.as_fd(), &mut ibuf);
                    }
                    shell.watch_model_path();
                }
                T_LOCKS => {
                    drain(shell.locks_efd.as_fd(), &mut scratch[..8]);
                    let reports: Vec<(u32, usize)> = std::mem::take(&mut *shell.reports.lock().unwrap_or_else(|e| e.into_inner()));
                    for (gen, ring) in reports {
                        log!("death lock of gen {gen} ring {ring} reported");
                        core.handle(now, Event::LockDied { gen, ring }, &mut shell);
                    }
                }
                T_HTTP => {
                    if let Some(h) = http.as_mut() {
                        h.accept(&shell.ep);
                    }
                }
                T_ADMIN => {
                    if let Some(a) = admin.as_mut() {
                        a.accept(&shell.ep);
                    }
                }
                t if t & KIND == K_STATUS => {
                    let gen = t as u32;
                    let n = match shell.procs.get(&gen) {
                        Some(p) => drain(p.sp.status.as_fd(), &mut scratch),
                        None => continue,
                    };
                    for &b in &scratch[..n] {
                        core.handle(now, Event::Status { gen, byte: b }, &mut shell);
                    }
                    if n == 0 {
                        if let Some(p) = shell.procs.get(&gen) {
                            shell.ep.del(p.sp.status.as_fd()); // end of file: the pidfd follows
                        }
                    }
                }
                t if t & KIND == K_PIDFD => {
                    let gen = t as u32;
                    if let Some(p) = shell.procs.get(&gen) {
                        let n = drain(p.sp.status.as_fd(), &mut scratch); // a last report
                        for &b in &scratch[..n] {
                            core.handle(now, Event::Status { gen, byte: b }, &mut shell);
                        }
                    }
                    let how = shell.reap(gen);
                    log!("worker gen {gen} {how}");
                    core.handle(now, Event::Exited { gen }, &mut shell);
                }
                t if t & KIND == K_CONN => {
                    if let Some(e) = shell.on_conn(t & !KIND, &core) {
                        core.handle(now, e, &mut shell);
                    }
                }
                t if t & KIND == K_HTTP => {
                    if let Some(h) = http.as_mut() {
                        let since = now.saturating_sub(last_tick);
                        let modified = shell.model_modified;
                        h.serve(t & !KIND, &shell.ep, |path| http::running(path, &core, since, modified, args.wait_for_model));
                    }
                }
                t if t & KIND == K_HELPER => shell.on_helper(t & !KIND),
                t if t & KIND == K_ADMIN => {
                    if let Some(a) = admin.as_mut() {
                        let bound = shell.conns.values().any(|c| c.stage == Stage::Bound);
                        if let Some(e) = a.serve(t & !KIND, &shell.ep, &core, &arena, bound) {
                            core.handle(now, e, &mut shell);
                        }
                    }
                }
                _ => {}
            }
            while let Some(e) = shell.pending.pop_front() {
                core.handle(monotonic_ns(), e, &mut shell);
            }
        }
    }
}

/// The arena and the bound session socket, or why not.
fn create(args: &Args, acfg: &ArenaConfig) -> Result<(OwnedArena, OwnedFd), String> {
    let arena = OwnedArena::create_with_opts(&args.geometry, acfg, args.hugepages).map_err(|e| format!("arena: {e}"))?;
    let listener = handshake::listen(&args.socket).map_err(|e| format!("socket {}: {e}", args.socket))?;
    sys::set_nonblocking(listener.as_fd()).map_err(|e| e.to_string())?;
    Ok((arena, listener))
}

/// One epoll wait, returning the tokens (the degraded loop has no shell yet).
fn shell_free_wait(ep: &Epoll, evs: &mut [libc::epoll_event]) -> io::Result<Vec<u64>> {
    let n = ep.wait(evs)?;
    Ok(evs[..n].iter().map(|e| e.u64).collect())
}

fn shutdown(shell: &mut Shell<'_>, args: &Args) -> io::Result<()> {
    let gens: Vec<u32> = shell.procs.keys().copied().collect();
    for g in gens {
        if let Some(p) = shell.procs.get(&g) {
            let _ = sys::wait_readable(p.sp.pidfd.as_fd(), 1000);
        }
        shell.reap(g);
    }
    if let Endpoint::Path(p) = &args.socket {
        let _ = std::fs::remove_file(p);
    }
    log!("stopped");
    Ok(())
}

/// What is published and what a rollback would return to, for /modelz (§5.6 asks it to name
/// both digests after a rollback).
fn model_json(core: &Core<'_>) -> String {
    let m = &core.models;
    let digest = |id: Option<u32>| id.and_then(|i| m.digests.get(&i)).map(|d| format!("\"{}\"", machine::hex(d))).unwrap_or_else(|| "null".into());
    let p = core.published.as_ref();
    format!(
        "{{\"sha256\":{},\"registry_epoch\":{},\"schema_fingerprint\":\"{:#018x}\",\"previous_sha256\":{},\"rollbacks\":{},\"rejected\":{}}}",
        digest(p.map(|p| p.model)),
        p.map_or(0, |p| p.epoch),
        p.map_or(0, |p| p.fingerprint),
        digest(m.previous),
        core.stats.rollbacks,
        core.stats.rejected
    )
}

/// The supervisor's state as JSON, for /modelz and the admin socket's `status`.
pub fn describe(core: &Core<'_>) -> String {
    let g = core.gate();
    let s = &core.stats;
    format!(
        "{{\"state\":\"{}\",\"reason\":\"{}\",\"gen\":{},\"model\":{},\"holds\":{:?},\"workers\":[{}],\"spawns\":{},\"deaths\":{},\"promotions\":{},\"restarts\":{},\"resets\":{},\"reloads\":{},\"condemned\":{},\"crashloop\":{},\"last_recovery_ms\":{:.3},\"max_recovery_ms\":{:.3},\"last_promotion_ms\":{:.3},\"max_promotion_ms\":{:.3},\"oom_shield\":\"{}\",\"last_cause\":\"{}\"}}",
        gs::name(word::gate_state(g)),
        gr::name(word::gate_reason(g)),
        word::gate_gen(g),
        model_json(core),
        core.holds(),
        core.workers.iter().map(|w| format!("{{\"gen\":{},\"beat\":{},\"role\":\"{:?}\",\"active\":{}}}", w.gen, w.beat, w.role, w.active)).collect::<Vec<_>>().join(","),
        s.spawns,
        s.deaths,
        s.promotions,
        s.restarts,
        s.resets,
        s.reloads,
        s.condemned,
        core.crashloop(),
        s.last_recovery_ns as f64 / 1e6,
        s.max_recovery_ns as f64 / 1e6,
        s.last_promotion_ns as f64 / 1e6,
        s.max_promotion_ns as f64 / 1e6,
        shield::SETTLED.get().map_or("none", |s| s.0.name()),
        s.last_cause.replace('"', "'"),
    )
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(kind) = argv.first().and_then(|a| a.strip_prefix("--helper=")) {
        helper::main(kind, &argv[1..]);
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            log!("{e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        log!("fatal: {e}");
        std::process::exit(1);
    }
}
