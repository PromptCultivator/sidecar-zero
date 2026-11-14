//! Phase 2, end to end: a real supervisor, real workers, and this SDK under faults.
//!
//! The kill loop runs `SCZ_KILLS` kills (default 40), or for `SCZ_SOAK_SECS` seconds when that
//! is set; the exit criterion's hour is `SCZ_SOAK_SECS=3600`.

mod common;

use common::{signal_pid, Sidecar};
use sidecarzero::{Client, F32Echo, FallbackReason as R, Source};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn input(i: u64) -> [f32; 16] {
    core::array::from_fn(|k| (i * 16 + k as u64) as f32)
}

type Echo = Client<F32Echo<16>, fn(&[f32; 16], R) -> [f32; 16]>;

fn fallback(_: &[f32; 16], _: R) -> [f32; 16] {
    [-1.0; 16]
}

fn client(sc: &Sidecar) -> Echo {
    let c: Echo = Client::builder(sc.endpoint(), F32Echo::<16>).fallback(fallback as fn(&[f32; 16], R) -> [f32; 16]).build();
    assert!(c.await_bound(Duration::from_secs(10)), "never served:\n{}", sc.log());
    c
}

/// Callers that run until told to stop, recording the slowest call and what each returned.
struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    worst_ns: Arc<AtomicU64>,
    model: Arc<AtomicU64>,
    calls: Arc<AtomicU64>,
}

impl Load {
    fn start(c: &Arc<Echo>, threads: usize, budget: Duration) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let (worst_ns, model, calls) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let threads = (0..threads)
            .map(|t| {
                let (c, stop, worst_ns, model, calls) = (c.clone(), stop.clone(), worst_ns.clone(), model.clone(), calls.clone());
                std::thread::spawn(move || {
                    let mut i = t as u64 * 1_000_000_000;
                    while !stop.load(Relaxed) {
                        i += 1;
                        let x = input(i);
                        let t0 = Instant::now();
                        let out = c.infer(&x, budget);
                        worst_ns.fetch_max(t0.elapsed().as_nanos() as u64, Relaxed);
                        calls.fetch_add(1, Relaxed);
                        if out.is_model() {
                            assert_eq!(out.value, x, "a model answer to another call's request");
                            model.fetch_add(1, Relaxed);
                        }
                    }
                })
            })
            .collect();
        Load { stop, threads, worst_ns, model, calls }
    }
    fn stop(self) -> (Duration, u64, u64) {
        self.stop.store(true, Relaxed);
        for t in self.threads {
            t.join().expect("a caller panicked");
        }
        (Duration::from_nanos(self.worst_ns.load(Relaxed)), self.model.load(Relaxed), self.calls.load(Relaxed))
    }
}

fn all_slots_free(sc: &Sidecar, c: &Echo, slots: u32) {
    let t = Instant::now();
    loop {
        if let Some(n) = c.slot_census() {
            if n[0] == slots {
                return;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "slots left behind: {n:?}\n{}", sc.status());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn kill_active(sc: &Sidecar) -> u32 {
    sc.wait_for("a healthy gate", Duration::from_secs(10), |s| s.gate() == "HEALTHY");
    let gen = sc.active_gen();
    let pid = sc.worker_pid(gen).expect("the active worker's pid");
    signal_pid(pid, "KILL");
    gen
}

#[test]
fn a_kill_loop_leaves_no_stuck_slot_and_no_hung_call() {
    // Every killed worker had served for less than the stability window, so each kill counts
    // toward the crash-loop backoff (§5.5). Capping it low keeps this a test of promotion and
    // recovery rather than of the backoff, which has a test of its own.
    let sc = Sidecar::start("scz-worker-echo", &["--slots=64", "--backoff-max-ms=100"], &[]);
    let c = Arc::new(client(&sc));
    let budget = Duration::from_millis(20);
    let load = Load::start(&c, 3, budget);
    let soak = std::env::var("SCZ_SOAK_SECS").ok().and_then(|v| v.parse::<u64>().ok());
    let kills = std::env::var("SCZ_KILLS").ok().and_then(|v| v.parse().ok()).unwrap_or(40u32);
    let t0 = Instant::now();
    let mut done = 0u32;
    while soak.map_or(done < kills, |s| t0.elapsed() < Duration::from_secs(s)) {
        kill_active(&sc);
        done += 1;
        std::thread::sleep(Duration::from_millis(30 + (done as u64 * 37) % 120));
    }
    sc.wait_for("recovery", Duration::from_secs(10), |s| s.gate() == "HEALTHY");
    std::thread::sleep(Duration::from_millis(100));
    let (worst, model, calls) = load.stop();
    all_slots_free(&sc, &c, 64);
    eprintln!(
        "kill loop: {done} kills in {:?}; {calls} calls, {model} answered by the model; slowest call {worst:?}; {} promotions, worst {:.2} ms; worst recovery {:.2} ms",
        t0.elapsed(),
        sc.stat("promotions"),
        sc.stat("max_promotion_ms"),
        sc.stat("max_recovery_ms")
    );
    assert!(sc.stat("deaths") >= done as f64);
    assert!(model > 1000, "the model answered only {model} times");
    assert!(c.infer(&input(7), budget).is_model(), "serving after the loop");
    // Every call returns by its budget plus the fallback and scheduling slack.
    assert!(worst < budget + Duration::from_millis(80), "a call hung: {worst:?}");
}

#[test]
fn a_standby_promotion_reopens_the_gate_within_milliseconds() {
    let sc = Sidecar::start("scz-worker-echo", &["--slots=16"], &[]);
    let _c = client(&sc);
    let mut took = Vec::new();
    for _ in 0..8 {
        sc.wait_for("a warm standby", Duration::from_secs(10), |s| s.status().matches("\"role\":\"Warm\"").count() == 1);
        let promotions = sc.stat("promotions");
        kill_active(&sc);
        sc.wait_for("the promotion", Duration::from_secs(5), |s| s.stat("promotions") > promotions && s.gate() == "HEALTHY");
        took.push(sc.stat("last_promotion_ms"));
    }
    took.sort_by(f64::total_cmp);
    eprintln!("gate closed to reopened, from the death lock's report: {took:?} ms");
    // The exit criterion is 5 ms on a quiet machine; a shared VM gets more room.
    assert!(took[took.len() / 2] < 20.0, "median promotion {:.2} ms", took[took.len() / 2]);
}

#[test]
fn a_reload_under_load_costs_no_fallback() {
    let sc = Sidecar::start("scz-worker-echo", &["--slots=64"], &[]);
    let c = Arc::new(client(&sc));
    let load = Load::start(&c, 3, Duration::from_millis(100));
    std::thread::sleep(Duration::from_millis(100));
    for k in 1..=3 {
        let gen = sc.active_gen();
        sc.ctl("reload");
        sc.wait_for("the reload", Duration::from_secs(10), |s| s.active_gen() != gen && s.status().contains("\"holds\":[]") && s.stat("reloads") as u32 == k);
        std::thread::sleep(Duration::from_millis(100));
    }
    let (worst, model, calls) = load.stop();
    let st = c.stats();
    eprintln!("reload under load: {calls} calls, {model} model; slowest {worst:?}; fallbacks {:?}", st.fallback);
    assert_eq!(model, calls, "fallbacks during a same-schema reload: {:?}", st.fallback);
    assert_eq!(sc.stat("deaths"), 0.0, "the old workers released; none died");
    all_slots_free(&sc, &c, 64);
}

#[test]
fn a_hung_inference_is_condemned_after_its_caller_gives_up() {
    let sc = Sidecar::start("scz-worker-chaos", &["--slots=16"], &[("SCZ_CHAOS_HANG_AFTER", "3"), ("SCZ_CHAOS_GEN", "1")]);
    let c = client(&sc);
    sc.wait_for("a warm standby", Duration::from_secs(10), |s| s.status().contains("\"role\":\"Warm\""));
    let mut reasons = Vec::new();
    for i in 0..3 {
        let out = c.infer(&input(i), Duration::from_millis(30));
        reasons.push(out.source);
    }
    assert!(matches!(reasons[2], Source::Fallback { reason: R::Deadline }), "{reasons:?}");
    sc.wait_for("the condemnation and promotion", Duration::from_secs(5), |s| s.stat("condemned") >= 1.0 && s.gate() == "HEALTHY");
    assert!(c.infer(&input(9), Duration::from_millis(50)).is_model());
    all_slots_free(&sc, &c, 16);
}

#[test]
fn a_frozen_worker_is_condemned_and_its_standby_serves() {
    let sc = Sidecar::start("scz-worker-echo", &["--slots=16"], &[]);
    let c = client(&sc);
    sc.wait_for("a warm standby", Duration::from_secs(10), |s| s.status().contains("\"role\":\"Warm\""));
    let gen = sc.active_gen();
    signal_pid(sc.worker_pid(gen).unwrap(), "STOP");
    sc.wait_for("the condemnation", Duration::from_secs(5), |s| s.stat("condemned") >= 1.0 && s.active_gen() != gen && s.gate() == "HEALTHY");
    assert!(c.infer(&input(1), Duration::from_millis(50)).is_model());
}

#[test]
fn a_requested_reset_takes_the_client_out_and_back() {
    let sc = Sidecar::start("scz-worker-echo", &["--slots=16"], &[]);
    let c = client(&sc);
    assert!(c.infer(&input(1), Duration::from_millis(50)).is_model());
    sc.ctl("reset");
    let t = Instant::now();
    while c.stats().sessions < 2 || !c.infer(&input(2), Duration::from_millis(50)).is_model() {
        assert!(t.elapsed() < Duration::from_secs(5), "the client did not come back\n{}", sc.log());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(c.stats().retired, 1, "the old mapping was unmapped before the reset");
    assert!(sc.stat("resets") >= 1.0);
}

#[test]
fn a_model_that_cannot_load_backs_off() {
    let sc = Sidecar::start("scz-worker-chaos", &["--slots=16", "--standby=none"], &[("SCZ_CHAOS_FAIL_LOAD", "1")]);
    let c: Echo = Client::builder(sc.endpoint(), F32Echo::<16>).fallback(fallback as fn(&[f32; 16], R) -> [f32; 16]).build();
    std::thread::sleep(Duration::from_millis(1000));
    let spawns = sc.stat("spawns");
    assert!((3.0..=7.0).contains(&spawns), "50 ms doubling gives about five attempts in a second, not {spawns}");
    assert!(sc.status().contains("\"crashloop\":true"));
    let t = Instant::now();
    let out = c.infer(&input(1), Duration::from_millis(50));
    assert!(!out.is_model());
    assert!(t.elapsed() < Duration::from_millis(5), "a fallback at admission, not a wait");
}

#[test]
fn a_dead_standby_is_replaced_without_touching_the_gate() {
    let sc = Sidecar::start("scz-worker-echo", &["--slots=16"], &[]);
    let _c = client(&sc);
    sc.wait_for("a warm standby", Duration::from_secs(10), |s| s.status().contains("\"role\":\"Warm\""));
    let standby = (1..20).find(|&g| g != sc.active_gen() && sc.worker_pid(g).is_some()).unwrap();
    signal_pid(sc.worker_pid(standby).unwrap(), "KILL");
    sc.wait_for("a new standby", Duration::from_secs(5), |s| s.stat("spawns") >= 3.0 && s.status().contains("\"role\":\"Warm\""));
    assert_eq!(sc.gate(), "HEALTHY");
    assert_eq!(sc.stat("deaths"), 0.0, "a standby's death is not the consumer's");
}

#[test]
fn shutdown_keeps_serving_until_the_host_leaves() {
    let mut sc = Sidecar::start("scz-worker-echo", &["--slots=16"], &[]);
    let c = client(&sc);
    sc.signal("TERM");
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        assert!(c.infer(&input(1), Duration::from_millis(50)).is_model(), "the host is attached: keep serving");
    }
    drop(c);
    let t = Instant::now();
    loop {
        if let Some(st) = sc.child.try_wait().unwrap() {
            assert!(st.success(), "{st:?}");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "no exit after the host left\n{}", sc.log());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_probes_answer() {
    let port = 20000 + (std::process::id() % 20000) as u16;
    let addr = format!("127.0.0.1:{port}");
    let sc = Sidecar::start("scz-worker-echo", &["--slots=16", &format!("--http={addr}")], &[]);
    let _c = client(&sc);
    let get = |path: &str| {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        write!(s, "GET {path} HTTP/1.0\r\n\r\n").unwrap();
        let mut r = String::new();
        s.read_to_string(&mut r).unwrap();
        r
    };
    assert!(get("/livez").starts_with("HTTP/1.0 200"));
    assert!(get("/readyz").starts_with("HTTP/1.0 200"));
    let m = get("/modelz");
    assert!(m.starts_with("HTTP/1.0 200") && m.contains("\"state\":\"HEALTHY\""), "{m}");
    assert!(get("/metrics").contains("scz_gate_state 1"));
    assert!(get("/nope").starts_with("HTTP/1.0 404"));
}
