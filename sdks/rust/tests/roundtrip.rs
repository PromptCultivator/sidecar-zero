//! End to end, on one machine: a real supervisor, the echo worker, and this SDK.
//!
//! The binaries are found next to the test executable, so build the workspace first
//! (`cargo test --workspace` does).

use sidecarzero::{BytesEcho, Client, Codec, CodecError, Endpoint, F32Echo, FallbackReason as R, Options, SlotReader, SlotWriter, Source};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const FREE: usize = 0;

fn bin(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let p = exe.parent().unwrap().parent().unwrap().join(name);
    assert!(p.exists(), "{} not built: run `cargo build --workspace` first", p.display());
    p
}

static N: AtomicU32 = AtomicU32::new(0);

struct Sidecar {
    child: Child,
    dir: PathBuf,
    sock: PathBuf,
}

impl Sidecar {
    fn start(args: &[&str], env: &[(&str, &str)]) -> Sidecar {
        let dir = std::env::temp_dir().join(format!("scz-rt-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        Self::start_in(dir, args, env)
    }
    fn start_in(dir: PathBuf, args: &[&str], env: &[(&str, &str)]) -> Sidecar {
        let sock = dir.join("scz.sock");
        let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("supervisor.log")).unwrap();
        let mut c = Command::new(bin("scz-supervisor"));
        c.arg(format!("--socket={}", sock.display()))
            .arg(format!("--worker={}", bin("scz-worker-echo").display()))
            .arg("--hb-stale-ms=100") // a loaded test machine stalls threads for tens of ms
            .args(args)
            .stderr(Stdio::from(log))
            .stdout(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        Sidecar { child: c.spawn().unwrap(), dir, sock }
    }
    fn endpoint(&self) -> Endpoint {
        Endpoint::Path(self.sock.clone())
    }
    fn worker_pid(&self) -> i32 {
        let id = self.child.id();
        for _ in 0..500 {
            let s = std::fs::read_to_string(format!("/proc/{id}/task/{id}/children")).unwrap_or_default();
            if let Some(p) = s.split_whitespace().next() {
                return p.parse().unwrap();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("no worker");
    }
    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("supervisor.log")).unwrap_or_default()
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.kill();
        if std::thread::panicking() {
            eprintln!("--- supervisor log ---\n{}", self.log());
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn kill9(pid: i32) {
    assert!(Command::new("kill").arg("-9").arg(pid.to_string()).status().unwrap().success());
}

/// Bound is not open: the handshake can finish before the worker is ACTIVE, and calls then
/// fall back with GATE_INIT. Wait until they stop doing that.
fn until_open<C: Codec, F: sidecarzero::Fallback<C::In, C::Out>>(sc: &Sidecar, c: &Client<C, F>, x: &C::In) {
    assert!(c.await_bound(Duration::from_secs(5)), "never bound:\n{}", sc.log());
    let t = Instant::now();
    while matches!(c.infer(x, Duration::from_millis(5)).reason(), Some(R::GateInit | R::NotConnected)) {
        assert!(t.elapsed() < Duration::from_secs(5), "the gate never opened:\n{}", sc.log());
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn echo_client(sc: &Sidecar) -> Client<F32Echo<16>, impl Fn(&[f32; 16], R) -> [f32; 16]> {
    let c = Client::builder(sc.endpoint(), F32Echo::<16>).fallback(|_: &[f32; 16], _| [-1.0; 16]).build();
    until_open(sc, &c, &input(0));
    c
}

fn input(i: u32) -> [f32; 16] {
    core::array::from_fn(|k| (i * 16 + k as u32) as f32)
}

/// Wait until every slot is FREE again (abandoned ones are freed by the worker, later).
fn settle<C, F>(c: &Client<C, F>, total: u32) -> [u32; 7] {
    let mut last = [0; 7];
    for _ in 0..400 {
        last = c.slot_census().unwrap();
        if last[FREE] == total {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    last
}

#[test]
fn round_trips_from_one_thread_and_from_many() {
    let sc = Sidecar::start(&[], &[]);
    let c = echo_client(&sc);
    for i in 0..1000 {
        let out = c.infer(&input(i), Duration::from_millis(50));
        assert_eq!(out.source, Source::Model { model_gen: 1 }, "call {i}");
        assert_eq!(out.value, input(i));
    }
    std::thread::scope(|s| {
        for t in 0..4u32 {
            let c = &c;
            s.spawn(move || {
                for i in 0..2000 {
                    let x = input(t * 100_000 + i);
                    let out = c.infer(&x, Duration::from_millis(50));
                    assert!(out.is_model(), "thread {t} call {i}: {:?}", out.source);
                    assert_eq!(out.value, x);
                }
            });
        }
    });
    let st = c.stats();
    assert!(st.model >= 9000);
    assert!(st.fallback.iter().all(|(r, _)| *r == R::GateInit), "only the probes before the gate opened: {:?}", st.fallback);
    assert_eq!(c.slot_census().unwrap()[FREE], 1024);
}

#[test]
fn no_sidecar_means_an_immediate_fallback() {
    let c = Client::builder(Endpoint::parse("/nonexistent/scz.sock"), F32Echo::<16>).fallback(|_: &[f32; 16], _| [7.0; 16]).build();
    let t = Instant::now();
    let out = c.infer(&input(1), Duration::from_millis(50));
    assert!(t.elapsed() < Duration::from_millis(5));
    assert_eq!(out.source, Source::Fallback { reason: R::NotConnected });
    assert_eq!(out.value, [7.0; 16]);
}

#[test]
fn deadlines_fall_back_on_time_and_leak_no_slot() {
    // Each request takes the worker 50 ms; every call has 1 ms. A client that waited for the
    // worker instead of its deadline would take about 50 ms per call.
    let sc = Sidecar::start(&["--slots=16"], &[("SCZ_ECHO_DELAY_US", "50000")]);
    let c = Client::builder(sc.endpoint(), F32Echo::<16>).fallback(|_: &[f32; 16], _| [-1.0; 16]).build();
    until_open(&sc, &c, &input(0));
    std::thread::sleep(Duration::from_millis(60)); // let the probe's request finish
    let mut times = Vec::new();
    let mut late_answers = 0;
    for i in 0..200 {
        let t = Instant::now();
        let out = c.infer(&input(i), Duration::from_millis(1));
        times.push(t.elapsed());
        match out.source {
            // After eight misses in a row the caller's breaker sheds (sdk-contract §4).
            Source::Fallback { reason: R::Deadline | R::Overloaded } => {}
            // The worker finished while this thread was descheduled past its deadline: the
            // answer is valid (sdk-contract §4, abandon_then_fallback).
            Source::Model { .. } => late_answers += 1,
            // The supervisor's 1 ms heartbeat stalled past 15 ms: possible on a loaded test VM
            // running several sidecars at once, and the right answer when it happens.
            Source::Fallback { reason: R::StaleHeartbeat } => late_answers += 1,
            other => panic!("call {i}: {other:?}"),
        }
    }
    times.sort();
    let (median, worst) = (times[100], times[199]);
    assert!(late_answers <= 4, "{late_answers} calls waited for the worker");
    assert!(median < Duration::from_millis(3), "median {median:?}");
    assert!(worst < Duration::from_millis(25), "worst {worst:?}: that is waiting for the worker, not the deadline");
    let census = settle(&c, 16);
    assert_eq!(census[FREE], 16, "slots left behind: {census:?}");
    // And the model still answers when given the time, once the breaker has stopped shedding
    // (its longest interval is 100 ms, jittered by half).
    std::thread::sleep(Duration::from_millis(200));
    let out = c.infer(&input(9), Duration::from_millis(100));
    assert!(out.is_model(), "{:?}", out.source);
}

#[test]
fn a_killed_worker_becomes_fallbacks_not_hangs() {
    // No standby: the replacement is a cold start, so the gate stays closed for a while.
    let sc = Sidecar::start(&["--slots=16", "--standby=none"], &[("SCZ_ECHO_DELAY_US", "30000")]);
    let c = echo_client(&sc);
    let worker = sc.worker_pid();
    std::thread::scope(|s| {
        let inflight = s.spawn(|| {
            let t = Instant::now();
            (c.infer(&input(1), Duration::from_millis(100)), t.elapsed())
        });
        std::thread::sleep(Duration::from_millis(10));
        kill9(worker);
        let (out, took) = inflight.join().unwrap();
        assert_eq!(out.source, Source::Fallback { reason: R::WorkerFailed });
        assert!(took < Duration::from_millis(80), "the in-flight call waited {took:?}");
    });
    // New calls are refused at admission while the gate is closed...
    let t = Instant::now();
    let out = c.infer(&input(2), Duration::from_millis(100));
    assert!(matches!(out.source, Source::Fallback { reason: R::GateDead } | Source::Model { .. }), "{:?}", out.source);
    assert!(t.elapsed() < Duration::from_millis(40));
    // ... and served again once the replacement is up (phase 2).
    let t = Instant::now();
    while !c.infer(&input(3), Duration::from_millis(100)).is_model() {
        assert!(t.elapsed() < Duration::from_secs(5), "no recovery:\n{}", sc.log());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(settle(&c, 16)[FREE], 16);
}

#[test]
fn the_client_reconnects_after_the_supervisor_restarts() {
    let mut sc = Sidecar::start(&[], &[]);
    let c = echo_client(&sc);
    assert!(c.infer(&input(1), Duration::from_millis(50)).is_model());
    sc.kill();
    let t = Instant::now();
    loop {
        let out = c.infer(&input(2), Duration::from_millis(5));
        if !out.is_model() && c.state() != sidecarzero::ClientState::Bound {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(2), "the client never noticed");
        std::thread::sleep(Duration::from_millis(1));
    }
    let sc2 = Sidecar::start_in(sc.dir.clone(), &[], &[]);
    let t = Instant::now();
    while !c.infer(&input(3), Duration::from_millis(50)).is_model() {
        assert!(t.elapsed() < Duration::from_secs(5), "no reconnect:\n{}", sc2.log());
        std::thread::sleep(Duration::from_millis(5));
    }
    let st = c.stats();
    assert_eq!(st.sessions, 2);
    assert_eq!(st.retired, 1, "the first arena was unmapped before the reconnect");
}

#[test]
fn a_second_client_is_told_busy_until_the_first_leaves() {
    let sc = Sidecar::start(&[], &[]);
    let first = echo_client(&sc);
    let second = Client::builder(sc.endpoint(), F32Echo::<16>).fallback(|_: &[f32; 16], _| [0.0; 16]).build();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(second.infer(&input(1), Duration::from_millis(5)).source, Source::Fallback { reason: R::NotConnected });
    assert!(sc.log().contains("a session exists"));
    drop(first);
    assert!(second.await_bound(Duration::from_secs(3)));
    assert!(second.infer(&input(2), Duration::from_millis(50)).is_model());
}

struct Broken;
impl Codec for Broken {
    type In = u32;
    type Out = u32;
    fn request_len(&self, _: &u32) -> usize {
        4
    }
    fn encode(&self, x: &u32, _: &mut SlotWriter<'_>) -> Result<(), CodecError> {
        if *x == 0 {
            panic!("a codec bug");
        }
        Err(CodecError::Shape)
    }
    fn decode(&self, _: &SlotReader<'_>) -> Result<u32, CodecError> {
        Err(CodecError::Shape)
    }
}

#[test]
fn codec_failures_and_oversized_inputs_fall_back_and_release_the_slot() {
    let sc = Sidecar::start(&["--slots=16"], &[]);
    let c = Client::builder(sc.endpoint(), Broken).fallback(|x: &u32, _| *x).options(Options::default()).build();
    until_open(&sc, &c, &1);
    assert_eq!(c.infer(&1, Duration::from_millis(5)).reason(), Some(R::CodecError));
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    assert_eq!(c.infer(&0, Duration::from_millis(5)).reason(), Some(R::CodecError), "a panicking codec");
    std::panic::set_hook(prev);
    assert_eq!(c.slot_census().unwrap()[FREE], 16);
    let big = Client::builder(sc.endpoint(), BytesEcho::<4096>).fallback(|_: &[u8; 4096], _| [0u8; 4096]).build();
    // Same supervisor: this one is BUSY, so it falls back NotConnected; TooLarge needs a session.
    drop(c);
    until_open(&sc, &big, &[1u8; 4096]);
    assert_eq!(big.infer(&[1u8; 4096], Duration::from_millis(5)).reason(), Some(R::TooLarge));
}
