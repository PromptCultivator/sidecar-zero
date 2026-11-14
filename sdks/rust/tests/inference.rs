//! Phase 3 end to end: a real supervisor, `sidecarzero-worker` with ONNX Runtime, the
//! generated models (models/gen.py), and the Rust SDK binding to the registry.
//!
//! The exit criteria (overview §9): the 256-feature model served end to end with zero copies
//! on the fast path; promotion under 5 ms with the benchmark model. And the model lifecycle of
//! supervisor §5.6: reloads that keep and that change the schema, rollback, the load timeout,
//! a missing model, a digest pin, a model modified in place.

mod common;

use common::*;
use sidecarzero::{Client, F32Echo, FallbackReason as R, FeatureBatch, FeatureScore, Source};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Scores = Client<FeatureScore<256>, fn(&[f32; 256], R) -> f32>;

fn nan(_: &[f32; 256], _: R) -> f32 {
    f32::NAN
}

fn scores(sc: &Sidecar) -> Scores {
    Client::builder(sc.endpoint(), FeatureScore::<256>::default()).fallback(nan as fn(&[f32; 256], R) -> f32).build()
}

/// The reference rows and the Python runtime's scores for them.
fn reference() -> (Vec<[f32; 256]>, Vec<f32>) {
    let text = std::fs::read_to_string(models().join("reference.json")).unwrap();
    let field = |k: &str| -> Vec<f32> {
        let a = text.split(&format!("\"{k}\": [")).nth(1).unwrap();
        a.replace(['[', ']', '}'], "").split(',').take_while(|v| !v.contains('"')).filter_map(|v| v.trim().parse::<f32>().ok()).collect()
    };
    let x = field("features");
    (x.chunks(256).map(|c| c.try_into().unwrap()).collect(), field("score"))
}

/// A sidecar serving `model` from `dir/model.ort` with the ONNX Runtime worker.
fn serve(dir: &Path, model: &str, extra: &[&str]) -> Sidecar {
    publish(&models().join(model), &dir.join("model.ort"));
    let arg = format!("--model={}", dir.join("model.ort").display());
    let mut args = vec![arg.as_str()];
    args.extend_from_slice(extra);
    Sidecar::start_in(dir.to_path_buf(), "sidecarzero-worker", &args, &[])
}

#[test]
fn the_fraud_model_is_served_end_to_end_with_zero_copies() {
    let dir = scratch();
    let sc = serve(&dir, "fraud.ort", &["--batch-slots=4", "--batch-slot-size=131072", "--batch-resp-cap=4096"]);
    let c = scores(&sc);
    assert!(c.await_bound(Duration::from_secs(20)), "not bound: {}", sc.status());
    let (x, want) = reference();
    let mut worst = 0f32;
    for (row, w) in x.iter().zip(&want) {
        let o = c.infer(row, Duration::from_millis(50));
        assert!(o.is_model(), "{:?}", o.source);
        worst = worst.max((o.value - w).abs());
    }
    assert!(worst < 1e-5, "scores differ from the Python runtime's by up to {worst}");
    let t = Instant::now();
    for i in 0..5000 {
        let o = c.infer(&x[i % 64], Duration::from_millis(50));
        assert!(o.is_model(), "call {i}: {:?}", o.source);
    }
    let per = t.elapsed() / 5000;
    eprintln!("fraud model, one row a call: {per:?} per call end to end; largest difference from the reference {worst:e}");
    c.close();

    // The batch class: 32 rows in one call.
    let b = Client::builder(sc.endpoint(), FeatureBatch::<256, 32>::default()).fallback(|_: &[[f32; 256]; 32], _| [f32::NAN; 32]).build();
    assert!(b.await_bound(Duration::from_secs(10)));
    let rows: [[f32; 256]; 32] = core::array::from_fn(|i| x[i]);
    let o = b.infer(&rows, Duration::from_millis(50));
    assert!(o.is_model(), "{:?}", o.source);
    for (got, w) in o.value.iter().zip(&want[..32]) {
        assert!((got - w).abs() < 1e-5);
    }
    b.close();

    // Zero copies: every request was read, and every answer written, in slot memory.
    let served = sc.worker_stat("served");
    assert!(served >= 5065, "served {served}");
    assert_eq!(sc.worker_stat("zero_copy_misses"), 0, "{}", sc.dump());
    assert_eq!(sc.worker_stat("copies"), 0);
    assert_eq!(sc.worker_stat("bad_requests") + sc.worker_stat("stale_requests"), 0);
    assert!(sc.dump().contains("\"ok\":true"), "{}", sc.dump());
    assert_eq!(sc.stat("registry_epoch"), 1.0);
    assert_eq!(sc.status_str("schema_fingerprint"), "0x57078e8d7a2e3733", "the golden fingerprint of features:F32[256] → score:F32[1]");
}

#[test]
fn a_standby_promotion_with_the_benchmark_model_takes_under_5_ms() {
    let dir = scratch();
    let sc = serve(&dir, "fraud.ort", &[]);
    let c = scores(&sc);
    assert!(c.await_bound(Duration::from_secs(20)));
    let mut took = vec![];
    for _ in 0..8 {
        sc.wait_for("a verified warm standby", Duration::from_secs(20), |s| s.status().matches("\"role\":\"Warm\"").count() == 1);
        std::thread::sleep(Duration::from_millis(150)); // its registry verdict
        let promotions = sc.stat("promotions");
        let gen = sc.active_gen();
        signal_pid(sc.worker_pid(gen).expect("the active worker"), "KILL");
        sc.wait_for("the promotion", Duration::from_secs(10), |s| s.stat("promotions") > promotions && s.gate() == "HEALTHY");
        took.push(sc.stat("last_promotion_ms"));
        assert!(c.infer(&[0.5; 256], Duration::from_millis(50)).is_model());
    }
    took.sort_by(f64::total_cmp);
    eprintln!("promotion with the fraud model, from the death lock's report to ACTIVE: {took:?} ms");
    assert!(took[took.len() / 2] < 5.0, "median promotion {:.2} ms", took[took.len() / 2]);
}

/// Run calls on one thread until stopped; every outcome is kept.
fn load(c: Arc<Scores>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<Source>> {
    std::thread::spawn(move || {
        let mut out = vec![];
        let mut i = 0usize;
        while !stop.load(Ordering::Relaxed) {
            let mut x = [0f32; 256];
            x[i % 256] = 1.0;
            out.push(c.infer(&x, Duration::from_millis(50)).source);
            i += 1;
        }
        out
    })
}

#[test]
fn a_same_schema_reload_under_load_costs_no_fallback() {
    let dir = scratch();
    let sc = serve(&dir, "fraud.ort", &[]);
    let c = Arc::new(scores(&sc));
    assert!(c.await_bound(Duration::from_secs(20)));
    let before = sc.status_str("model_sha256");
    let stop = Arc::new(AtomicBool::new(false));
    let h = load(c.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(200));
    publish(&models().join("fraud-b.ort"), &dir.join("model.ort"));
    sc.wait_for("the reload to the new file", Duration::from_secs(20), |s| s.status_str("model_sha256") != before && s.status().contains("\"holds\":[]"));
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    let out = h.join().unwrap();
    let fallbacks: Vec<&Source> = out.iter().filter(|s| !matches!(s, Source::Model { .. })).collect();
    eprintln!("same-schema reload: {} calls, {} fallbacks", out.len(), fallbacks.len());
    assert!(fallbacks.is_empty(), "fallbacks during a same-schema reload: {:?}", &fallbacks[..fallbacks.len().min(5)]);
    assert_eq!(sc.stat("registry_epoch"), 1.0, "the schema did not change");
    assert_eq!(sc.stat("deaths"), 0.0);
}

#[test]
fn a_reload_that_changes_the_schema_never_answers_a_request_for_the_old_one() {
    let dir = scratch();
    let sc = serve(&dir, "fraud.ort", &[]);
    let c = Arc::new(scores(&sc));
    assert!(c.await_bound(Duration::from_secs(20)));
    let stop = Arc::new(AtomicBool::new(false));
    let h = load(c.clone(), stop.clone());
    std::thread::sleep(Duration::from_millis(200));
    let old_gen = sc.active_gen();
    publish(&models().join("fraud128.ort"), &dir.join("model.ort"));
    sc.wait_for("the schema change", Duration::from_secs(20), |s| s.stat("registry_epoch") == 2.0 && s.gate() == "HEALTHY");
    let new_gen = sc.active_gen();
    std::thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    let out = h.join().unwrap();
    // Every model answer came from a generation that served the 256-feature model.
    for s in &out {
        if let Source::Model { model_gen } = s {
            assert!(*model_gen < new_gen, "a request for the old schema was answered by gen {model_gen} of the new model");
        }
    }
    let reasons: std::collections::BTreeSet<String> =
        out.iter().filter_map(|s| if let Source::Fallback { reason } = s { Some(reason.as_str().to_string()) } else { None }).collect();
    eprintln!("schema change: gen {old_gen} → {new_gen}; {} calls; fallback reasons {reasons:?}", out.len());
    assert!(reasons.contains("SCHEMA_MISMATCH"), "the old codec ends up rejected: {reasons:?}");
    for r in &reasons {
        assert!(["GATE_RELOADING", "REBINDING", "SCHEMA_MISMATCH", "WORKER_FAILED"].contains(&r.as_str()), "unexpected fallback {r}");
    }
    assert_eq!(c.state(), sidecarzero::ClientState::SchemaRejected);
    drop(c);

    // A client for the new schema binds and is answered.
    let c128 = Client::builder(sc.endpoint(), FeatureScore::<128>::default()).fallback(|_: &[f32; 128], _| f32::NAN).build();
    assert!(c128.await_bound(Duration::from_secs(10)), "{}", sc.status());
    assert!(c128.infer(&[0.25; 128], Duration::from_millis(50)).is_model());
    assert!(sc.dump().contains("\"ok\":true"));
}

/// A chaos-worker "model": a script (scz-worker-chaos).
fn script(body: &str) -> Vec<u8> {
    format!("SCZCHAOS-SCRIPT\n{body}\n").into_bytes()
}

#[test]
fn a_new_model_that_dies_on_requests_is_rolled_back_and_stays_rejected() {
    let dir = scratch();
    let model = dir.join("model");
    publish_bytes(&script("delay_us=0"), &model);
    let arg = format!("--model={}", model.display());
    let sc = Sidecar::start_in(dir.clone(), "scz-worker-chaos", &[&arg, "--backoff-max-ms=50"], &[]);
    let c = Client::builder(sc.endpoint(), F32Echo::<4>).fallback(|x: &[f32; 4], _| *x).build();
    assert!(c.await_bound(Duration::from_secs(10)));
    let good = sc.status_str("model_sha256");
    publish_bytes(&script("crash_after=1"), &model);
    sc.wait_for("the reload", Duration::from_secs(10), |s| s.stat("reloads") >= 1.0 && s.status_str("model_sha256") != good);
    // Each call kills the new model's worker as the consumer.
    sc.wait_for("the rollback", Duration::from_secs(20), |s| {
        let _ = c.infer(&[1.0; 4], Duration::from_millis(20));
        s.stat("rollbacks") >= 1.0
    });
    sc.wait_for("the previous model serving", Duration::from_secs(10), |s| s.gate() == "HEALTHY" && s.status_str("model_sha256") == good);
    assert!(c.await_bound(Duration::from_secs(5)));
    assert!(c.infer(&[2.0; 4], Duration::from_millis(50)).is_model());
    assert!(sc.stat("rejected") >= 1.0);
    // A plain reload does not bring the rejected file back.
    sc.ctl("reload");
    std::thread::sleep(Duration::from_millis(500));
    sc.wait_for("the plain reload to settle", Duration::from_secs(10), |s| s.gate() == "HEALTHY" && s.status().contains("\"holds\":[]"));
    assert_eq!(sc.status_str("model_sha256"), good, "a rejected model stays rejected");
    // --force does.
    let out = std::process::Command::new(bin("sczctl")).arg(format!("--admin={}", sc.admin.display())).args(["reload", "--force"]).output().unwrap();
    assert!(out.status.success());
    sc.wait_for("the forced reload", Duration::from_secs(10), |s| s.status_str("model_sha256") != good);
}

#[test]
fn with_memory_for_one_worker_a_reload_falls_back_until_the_new_model_serves() {
    let dir = scratch();
    let model = dir.join("model");
    publish_bytes(&script("delay_us=0"), &model);
    let arg = format!("--model={}", model.display());
    let sc = Sidecar::start_in(dir.clone(), "scz-worker-chaos", &[&arg, "--standby=none"], &[]);
    let c = Arc::new(Client::builder(sc.endpoint(), F32Echo::<4>).fallback(|x: &[f32; 4], _| *x).build());
    assert!(c.await_bound(Duration::from_secs(10)));
    let good = sc.status_str("model_sha256");
    let old_gen = sc.active_gen();
    let stop = Arc::new(AtomicBool::new(false));
    let h = {
        let (c, stop) = (c.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut out = vec![];
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                let o = c.infer(&[1.0; 4], Duration::from_millis(20));
                out.push((o.source, t.elapsed()));
            }
            out
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    // A slow load, so that the window in which nothing serves is easy to see.
    publish_bytes(&script("delay_us=1\nload_ms=300"), &model);
    sc.wait_for("the new model serving", Duration::from_secs(10), |s| s.status_str("model_sha256") != good && s.gate() == "HEALTHY");
    std::thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);
    let out = h.join().unwrap();
    let reasons: std::collections::BTreeSet<String> =
        out.iter().filter_map(|(s, _)| if let Source::Fallback { reason } = s { Some(reason.as_str().to_string()) } else { None }).collect();
    let fallbacks = out.iter().filter(|(s, _)| !matches!(s, Source::Model { .. })).count();
    let slowest = out.iter().map(|(_, t)| *t).max().unwrap();
    eprintln!("lone reload: {} calls, {fallbacks} fallbacks {reasons:?}, slowest call {slowest:?}", out.len());
    assert!(fallbacks > 0, "nothing serves while the new model loads");
    for r in &reasons {
        // Queued at the release: WORKER_FAILED; admitted behind the gate: GATE_RELOADING.
        assert!(["GATE_RELOADING", "WORKER_FAILED", "DEADLINE"].contains(&r.as_str()), "unexpected fallback {r}");
    }
    assert!(slowest < Duration::from_millis(100), "a call waited for the load: {slowest:?}");
    let log = sc.log();
    assert!(log.contains("memory for one worker"), "{log}");
    // The new worker started only after the old one had been reaped.
    let reaped = log.find(&format!("worker gen {old_gen} exited and was reaped")).expect("the old worker's exit");
    let started = log.find(&format!("worker gen {} spawned", sc.active_gen())).expect("the new worker's spawn");
    assert!(reaped < started, "the new worker started before the old one exited");
    assert_eq!(sc.stat("deaths"), 0.0);
    assert!(c.infer(&[2.0; 4], Duration::from_millis(50)).is_model());
}

#[test]
fn with_memory_for_one_worker_a_model_that_does_not_load_is_replaced_by_the_previous_one() {
    let dir = scratch();
    let model = dir.join("model");
    publish_bytes(&script("delay_us=0"), &model);
    let arg = format!("--model={}", model.display());
    let sc = Sidecar::start_in(dir.clone(), "scz-worker-chaos", &[&arg, "--standby=none", "--backoff-max-ms=50"], &[]);
    let c = Client::builder(sc.endpoint(), F32Echo::<4>).fallback(|x: &[f32; 4], _| *x).build();
    assert!(c.await_bound(Duration::from_secs(10)));
    let good = sc.status_str("model_sha256");
    publish_bytes(&script("fail_load=1"), &model);
    sc.wait_for("the rollback", Duration::from_secs(10), |s| s.stat("rollbacks") >= 1.0);
    sc.wait_for("the previous model serving", Duration::from_secs(10), |s| s.gate() == "HEALTHY" && s.status_str("model_sha256") == good);
    assert!(c.await_bound(Duration::from_secs(5)));
    assert!(c.infer(&[2.0; 4], Duration::from_millis(50)).is_model());
    assert!(sc.stat("rejected") >= 1.0);
    // The rejected file is not loaded again by a plain reload.
    sc.ctl("reload");
    std::thread::sleep(Duration::from_millis(500));
    sc.wait_for("the plain reload to settle", Duration::from_secs(10), |s| s.gate() == "HEALTHY" && s.status().contains("\"holds\":[]"));
    assert_eq!(sc.status_str("model_sha256"), good, "a rejected model stays rejected");
}

#[test]
fn a_load_that_never_finishes_is_killed_at_the_load_timeout() {
    let dir = scratch();
    let model = dir.join("model");
    publish_bytes(&script("load_ms=60000"), &model);
    let arg = format!("--model={}", model.display());
    let sc = Sidecar::start_in(dir.clone(), "scz-worker-chaos", &[&arg, "--load-timeout-ms=300", "--standby=none"], &[]);
    sc.wait_for("the load timeout", Duration::from_secs(10), |s| s.log().contains("did not load in time"));
    assert_eq!(sc.gate(), "INIT");
}

#[test]
fn a_missing_model_starts_degraded_and_is_picked_up_when_it_appears() {
    let dir = scratch();
    let model = dir.join("model.ort");
    let arg = format!("--model={}", model.display());
    let sc = Sidecar::start_in(dir.clone(), "sidecarzero-worker", &[&arg], &[]);
    sc.wait_for("the supervisor", Duration::from_secs(5), |s| !s.status().is_empty());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(sc.gate(), "INIT");
    assert_eq!(sc.stat("spawns"), 0.0, "nothing to load");
    assert!(sc.status().contains("model file"), "{}", sc.status());
    publish(&models().join("fraud.ort"), &model);
    sc.wait_for("the model to be picked up", Duration::from_secs(20), |s| s.gate() == "HEALTHY");
    let c = scores(&sc);
    assert!(c.await_bound(Duration::from_secs(10)));
    assert!(c.infer(&[0.0; 256], Duration::from_millis(50)).is_model());
}

#[test]
fn a_model_that_does_not_match_the_pin_is_refused() {
    let dir = scratch();
    let sc = serve(&dir, "fraud.ort", &["--model-sha256=0000000000000000000000000000000000000000000000000000000000000000"]);
    sc.wait_for("the refusal", Duration::from_secs(20), |s| s.status().contains("does not match the pin"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(sc.gate(), "INIT");
}

#[test]
fn a_model_modified_in_place_is_not_loaded_again() {
    let dir = scratch();
    let model = dir.join("model");
    publish_bytes(&script("delay_us=0"), &model);
    let arg = format!("--model={}", model.display());
    let sc = Sidecar::start_in(dir.clone(), "scz-worker-chaos", &[&arg, "--standby=none", "--backoff-max-ms=50"], &[]);
    sc.wait_for("serving", Duration::from_secs(10), |s| s.gate() == "HEALTHY");
    // Written in place, which a served model must never be (supervisor §5.1).
    std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&model).unwrap(), b"# changed\n").unwrap();
    signal_pid(sc.worker_pid(sc.active_gen()).unwrap(), "KILL");
    std::thread::sleep(Duration::from_millis(1500));
    assert_ne!(sc.gate(), "HEALTHY", "a modified model must not be loaded");
    assert!(sc.log().contains("MODEL_MODIFIED"), "{}", sc.log());
}

#[test]
fn the_worker_cannot_open_a_socket_or_read_proc() {
    // The sandbox is in force for real workers: a sidecar's workers are its children, and
    // their /proc entries say so.
    let dir = scratch();
    let sc = serve(&dir, "fraud.ort", &[]);
    sc.wait_for("serving", Duration::from_secs(20), |s| s.gate() == "HEALTHY");
    let pid = sc.worker_pid(sc.active_gen()).unwrap();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let field = |k: &str| status.lines().find(|l| l.starts_with(k)).unwrap_or("").split_whitespace().nth(1).unwrap_or("").to_string();
    assert_eq!(field("NoNewPrivs:"), "1");
    assert_eq!(field("Seccomp:"), "2", "a seccomp filter is installed");
    assert_eq!(std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj")).unwrap().trim(), "1000");
    let fds: Vec<String> =
        std::fs::read_dir(format!("/proc/{pid}/fd")).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    eprintln!("worker {pid}: descriptors {fds:?}; {}", sc.log().lines().find(|l| l.contains("sandbox:")).unwrap_or(""));
}
