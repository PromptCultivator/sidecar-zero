//! The GGUF worker end to end (worker §7.2): a real supervisor, `sidecarzero-worker-gguf` under
//! the full sandbox (seccomp, Landlock, rlimits), bge-small from the dev image, and the Rust
//! SDK. Reference embeddings and tokens come from llama.cpp in this process.
//!
//! Needs the supervisor and sczctl built: `cargo build -p scz-supervisor -p scz-ctl -p
//! scz-worker-gguf`. Each test is skipped, saying so, when the model file is absent. The tests
//! run one at a time: several measure time or memory.

mod common;

use common::*;
use scz_shm::abi::elem_type as et;
use scz_worker_gguf::{l2_normalize, Context, EmbedError, Model};
use sidecarzero::{Client, Codec, CodecError, Column, FallbackReason as R, FrameReader, FrameWriter, Schema, Source};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const MODEL: &str = "/opt/models/bge-small-f16.gguf";

fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The model file, or None (and a message) when it is absent.
fn model_file() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("SCZ_GGUF_MODEL").unwrap_or_else(|_| MODEL.into()));
    if p.exists() {
        Some(p)
    } else {
        eprintln!("SKIPPED: no GGUF model at {} (the dev image has bge-small there: run the tests through scripts/dev)", p.display());
        None
    }
}

/// The model in this process: tokens and reference embeddings.
fn reference_model() -> &'static Model {
    static M: OnceLock<Model> = OnceLock::new();
    // Errors only: the reference's batch leaves llama.cpp to mark every token as an output,
    // which it warns about on each call.
    scz_worker_gguf::init(scz_worker_gguf::LogLevel::Error);
    M.get_or_init(|| Model::load(&model_file().unwrap().display().to_string(), true).unwrap())
}

/// Rows of token ids in, one embedding per row out: the GGUF worker's columns.
#[derive(Clone, Copy)]
struct Embed {
    l: usize,
    d: usize,
}

impl Codec for Embed {
    type In = [Vec<i32>];
    type Out = Vec<Vec<f32>>;
    fn schema(&self) -> Option<Schema> {
        Some(Schema { request: vec![Column::new("tokens", et::I32, self.l as u32)], response: vec![Column::new("embedding", et::F32, self.d as u32)] })
    }
    fn rows(&self, input: &[Vec<i32>]) -> u32 {
        input.len() as u32
    }
    /// Each row padded to `l` with -1, the end of a sequence.
    fn encode_frame(&self, input: &[Vec<i32>], w: &mut FrameWriter<'_>) -> Result<(), CodecError> {
        let mut b = Vec::with_capacity(input.len() * self.l * 4);
        for row in input {
            if row.len() > self.l {
                return Err(CodecError::Shape);
            }
            b.extend((0..self.l).flat_map(|i| row.get(i).copied().unwrap_or(-1).to_le_bytes()));
        }
        w.write_bytes(0, &b)
    }
    fn decode_frame(&self, r: &FrameReader<'_>) -> Result<Vec<Vec<f32>>, CodecError> {
        let mut out = vec![0f32; r.rows() as usize * self.d];
        r.read_f32(0, &mut out)?;
        Ok(out.chunks(self.d).map(<[f32]>::to_vec).collect())
    }
}

type Embedder = Client<Embed, fn(&[Vec<i32>], R) -> Vec<Vec<f32>>>;

fn nothing(_: &[Vec<i32>], _: R) -> Vec<Vec<f32>> {
    vec![]
}

fn client(sc: &Sidecar, l: usize) -> Embedder {
    let d = reference_model().info.n_embd as usize;
    Client::builder(sc.endpoint(), Embed { l, d }).fallback(nothing as fn(&[Vec<i32>], R) -> Vec<Vec<f32>>).build()
}

/// A sidecar serving a copy of the model from `dir`, which is the one directory added to the
/// worker's Landlock view. The maximum deadline is 1 s, so that long inputs can be answered.
fn serve(dir: &Path, extra: &[&str]) -> Sidecar {
    serve_file(dir, &model_file().unwrap(), extra)
}

fn serve_file(dir: &Path, src: &Path, extra: &[&str]) -> Sidecar {
    publish(src, &dir.join("model.gguf"));
    let model = format!("--model={}", dir.join("model.gguf").display());
    let read = format!("--worker-read={}", dir.display());
    let mut args = vec![model.as_str(), read.as_str(), "--max-deadline-ms=1000"];
    args.extend_from_slice(extra);
    Sidecar::start_in(dir.to_path_buf(), "sidecarzero-worker-gguf", &args)
}

/// The sandbox line of the supervisor's log, which must show every layer in force.
fn assert_sandboxed(sc: &Sidecar) {
    let log = sc.log();
    let line = log.lines().find(|l| l.contains("sandbox:")).unwrap_or("").to_string();
    assert!(line.contains("seccomp on") && line.contains("landlock ABI") && line.contains("RLIMIT_NOFILE"), "not the full sandbox: {line}");
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let n = |v: &[f32]| v.iter().map(|x| *x as f64 * *x as f64).sum::<f64>().sqrt();
    dot / (n(a) * n(b))
}

/// A long paragraph: several hundred tokens.
fn long_text() -> String {
    "SidecarZero serves a model to the process next to it through shared memory, and every call has a fallback that answers when the model cannot. ".repeat(30)
}

/// `t` cut to `l` tokens, keeping its last ([SEP]).
fn cut(mut t: Vec<i32>, l: usize) -> Vec<i32> {
    if t.len() > l {
        let last = *t.last().unwrap();
        t.truncate(l - 1);
        t.push(last);
    }
    t
}

/// Sequences of several lengths, the longest exactly `l` tokens.
fn inputs(l: usize) -> Vec<Vec<i32>> {
    let m = reference_model();
    let texts = ["hello", "The quick brown fox jumps over the lazy dog.", "Shared memory between a host and its sidecar, with a mandatory fallback on every call, keeps a slow model from ever stalling the caller beyond its budget."];
    let mut v: Vec<Vec<i32>> = texts.iter().map(|t| cut(m.tokenize(t), l)).collect();
    v.push(cut(m.tokenize(&long_text()), l));
    v
}

/// llama.cpp's own answer for `t`, L2-normalized as the worker's is.
fn reference(t: &[i32]) -> Vec<f32> {
    let mut v = reference_model().reference(t).unwrap();
    l2_normalize(&mut v);
    v
}

fn p50(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[test]
fn embeddings_match_llama_cpp_on_the_same_tokens() {
    let Some(_) = model_file() else { return };
    let _g = serial();
    let l = 128;
    let dir = scratch();
    // Two rings: two consume threads, each with its own llama.cpp context; calls alternate.
    let sc = serve(&dir, &["--rings=2", "--batch-slots=4", "--batch-slot-size=131072", "--batch-resp-cap=65536"]);
    let c = client(&sc, l);
    assert!(c.await_bound(Duration::from_secs(30)), "not bound: {}\n{}", sc.status(), sc.log());
    assert_sandboxed(&sc);
    let inputs = inputs(l);
    let want: Vec<Vec<f32>> = inputs.iter().map(|t| reference(t)).collect();
    let raw = reference_model().reference(&inputs[1]).unwrap();
    let raw_norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
    eprintln!("lengths {:?}; llama.cpp's pooled embedding has norm {raw_norm:.3} before normalizing", inputs.iter().map(Vec::len).collect::<Vec<_>>());
    assert_eq!(inputs[3].len(), l);
    let mut worst = 1f64;
    // One row a call.
    for (t, w) in inputs.iter().zip(&want) {
        let o = c.infer(std::slice::from_ref(t), Duration::from_secs(1));
        assert!(o.is_model(), "{} tokens: {:?}", t.len(), o.source);
        let got = &o.value[0];
        let norm = got.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "not unit length: {norm}");
        worst = worst.min(cosine(got, w));
    }
    // Every sequence in one call (one llama.cpp call for the three short ones, one for the
    // full one), and 24 rows, which take several calls.
    let many: Vec<Vec<i32>> = (0..24).map(|i| inputs[i % inputs.len()].clone()).collect();
    for rows in [&inputs[..], &many[..]] {
        let o = c.infer(rows, Duration::from_secs(1));
        assert!(o.is_model(), "{} rows: {:?}", rows.len(), o.source);
        assert_eq!(o.value.len(), rows.len());
        for (i, got) in o.value.iter().enumerate() {
            worst = worst.min(cosine(got, &want[i % want.len()]));
        }
    }
    eprintln!("lowest cosine similarity to llama.cpp's reference: {worst:.7}");
    assert!(worst >= 0.9999, "an embedding differs from llama.cpp's reference: cosine {worst}");
    assert_eq!(sc.worker_stat("bad_requests") + sc.worker_stat("model_errors"), 0, "{}", sc.dump());
    assert!(sc.worker_stat("copies") >= 6, "the copy path is counted: {}", sc.dump());
}

#[test]
fn a_token_outside_the_vocabulary_is_refused_and_the_worker_keeps_serving() {
    let Some(_) = model_file() else { return };
    let _g = serial();
    let n_vocab = reference_model().info.n_vocab as i32;
    let dir = scratch();
    let sc = serve(&dir, &["--batch-slots=4", "--batch-slot-size=131072", "--batch-resp-cap=65536"]);
    let c = client(&sc, 128);
    assert!(c.await_bound(Duration::from_secs(30)), "{}", sc.status());
    let good = inputs(128)[1].clone();
    assert!(c.infer(std::slice::from_ref(&good), Duration::from_secs(1)).is_model());
    let gen = sc.active_gen();
    let pid = sc.worker_pid(gen);
    let bad: Vec<Vec<Vec<i32>>> = vec![
        vec![vec![101, n_vocab, 102]],                                 // one past the last id
        vec![vec![101, i32::MAX, 102]],                                // far outside
        vec![vec![]],                                                  // no tokens: the row is all padding
        vec![good.clone(), vec![101, n_vocab + 7, 102], good.clone()], // one bad row fails the request
    ];
    for rows in &bad {
        let o = c.infer(rows, Duration::from_secs(1));
        assert_eq!(o.source, Source::Fallback { reason: R::BadRequest }, "{rows:?}");
        let o = c.infer(std::slice::from_ref(&good), Duration::from_secs(1));
        assert!(o.is_model(), "the next call: {:?}", o.source);
    }
    assert_eq!((sc.active_gen(), sc.worker_pid(gen)), (gen, pid), "the same worker serves");
    assert_eq!(sc.stat("deaths"), 0.0);
    assert!(sc.log().contains("OutOfRange"), "the refusal is logged");
}

#[test]
fn a_call_past_its_deadline_is_abandoned_by_the_abort_callback() {
    let Some(_) = model_file() else { return };
    let _g = serial();
    let l = 512;
    let cap_ms = 60u64;
    let long = cut(reference_model().tokenize(&long_text()), l);
    assert_eq!(long.len(), l);

    // In this process first: how long the long input takes on two threads, as the worker runs
    // it, and how far past a deadline the abort callback stops it.
    let mut ctx = Context::new(reference_model(), l as u32, 1, 2).unwrap();
    let mut out = vec![0f32; ctx.n_embd];
    let lens = [l as i32];
    let full_ms = (0..3)
        .map(|_| {
            let t = Instant::now();
            ctx.embed(&long, &lens, u64::MAX, &mut out).unwrap();
            t.elapsed().as_secs_f64() * 1e3
        })
        .fold(f64::MAX, f64::min);
    let mut over = vec![];
    for _ in 0..5 {
        let deadline = scz_shm::wait::monotonic_ns() + 20_000_000;
        assert_eq!(ctx.embed(&long, &lens, deadline, &mut out), Err(EmbedError::Aborted));
        over.push((scz_shm::wait::monotonic_ns() - deadline) as f64 / 1e6);
    }
    ctx.embed(&long, &lens, u64::MAX, &mut out).expect("the context serves after an abort");
    drop(ctx);
    eprintln!(
        "{l} tokens, 2 threads, in process: {full_ms:.1} ms in full; stopped {:.2} ms past a deadline (p50; worst {:.2})",
        p50(&mut over),
        over.last().unwrap()
    );
    assert!(full_ms > 2.0 * cap_ms as f64, "the long input is too fast on this machine for the test to mean anything: {full_ms:.1} ms");

    let dir = scratch();
    let cap = format!("--worker-arg=--max-infer-ms={cap_ms}");
    let sc = serve(&dir, &["--slot-size=16384", "--worker-arg=--max-tokens=512", &cap]);
    let c = client(&sc, l);
    assert!(c.await_bound(Duration::from_secs(30)), "{}", sc.status());
    assert_sandboxed(&sc);
    let short = inputs(l)[1].clone();
    for _ in 0..5 {
        assert!(c.infer(std::slice::from_ref(&short), Duration::from_secs(1)).is_model());
    }
    let gen = sc.active_gen();

    // A budget far below the inference time: the caller falls back at its deadline; the worker
    // gives up at its own, and frees the slot.
    let mut freed = vec![];
    for _ in 0..3 {
        let budget = Duration::from_millis(20);
        let t0 = Instant::now();
        let o = c.infer(std::slice::from_ref(&long), budget);
        assert_eq!(o.source, Source::Fallback { reason: R::Deadline });
        let deadline = t0 + budget;
        let held = |c: &Embedder| c.slot_census().is_some_and(|n| n[3] + n[6] > 0); // PROCESSING, ABANDONED
        while held(&c) {
            assert!(t0.elapsed() < Duration::from_secs(2), "the slot was never freed");
            std::thread::sleep(Duration::from_micros(200));
        }
        let free_at = Instant::now();
        freed.push((free_at - deadline).as_secs_f64() * 1e3);
        assert!((free_at - t0).as_secs_f64() * 1e3 < full_ms, "freed only when the inference would have finished");
        let t = Instant::now();
        let o = c.infer(std::slice::from_ref(&short), Duration::from_secs(1));
        assert!(o.is_model(), "the next call: {:?}", o.source);
        eprintln!("the next call answered in {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
        std::thread::sleep(Duration::from_millis(250)); // let the worker's pace estimate decay
    }
    assert_eq!(sc.worker_stat("deadline_drops"), 0, "the worker ran every long call rather than dropping it: {}", sc.dump());
    // The request's own deadline reaches the backend (BatchView::deadline_ns), so the abort
    // comes at the caller's deadline, not at the worker's cap 40 ms later.
    let freed_p50 = p50(&mut freed.clone());
    assert!(freed_p50 < cap_ms as f64 / 3.0, "the slot was freed {freed_p50:.1} ms after the caller's deadline: {freed:.1?}");

    // A budget above the worker's: the worker answers DEADLINE_EXCEEDED itself, at its own
    // deadline rather than when the inference would have finished.
    let mut answered = vec![];
    for _ in 0..3 {
        let t0 = Instant::now();
        let o = c.infer(std::slice::from_ref(&long), Duration::from_millis(900));
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        assert_eq!(o.source, Source::Fallback { reason: R::Deadline }, "after {ms:.1} ms");
        assert!(ms < full_ms, "not abandoned: {ms:.1} ms");
        answered.push(ms);
        assert!(c.infer(std::slice::from_ref(&short), Duration::from_secs(1)).is_model());
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!(
        "worker deadline {cap_ms} ms; with a 20 ms budget the slot was free {:.1} ms after the caller's deadline (p50 of {freed:.1?}); \
         with a 900 ms budget DEADLINE_EXCEEDED came back after {:.1} ms (p50 of {answered:.1?}); the full inference takes {full_ms:.1} ms",
        p50(&mut freed.clone()),
        p50(&mut answered.clone())
    );
    assert_eq!(sc.active_gen(), gen);
    assert_eq!(sc.stat("deaths"), 0.0, "abandoning a call costs no worker");
}

#[test]
fn a_killed_worker_is_replaced_by_the_warm_standby() {
    let Some(_) = model_file() else { return };
    let _g = serial();
    let dir = scratch();
    let sc = serve(&dir, &[]);
    let c = Arc::new(client(&sc, 128));
    assert!(c.await_bound(Duration::from_secs(30)), "{}", sc.status());
    sc.wait_for("a verified warm standby", Duration::from_secs(30), Sidecar::has_warm_standby);
    std::thread::sleep(Duration::from_millis(150)); // its registry verdict
    let input = inputs(128)[2].clone();
    let before = c.infer(std::slice::from_ref(&input), Duration::from_secs(1));
    assert!(before.is_model());
    let gen = sc.active_gen();
    let pid = sc.worker_pid(gen).expect("the active worker");

    let stop = Arc::new(AtomicBool::new(false));
    let budget = Duration::from_millis(200);
    let h = {
        let (c, stop, input) = (c.clone(), stop.clone(), input.clone());
        std::thread::spawn(move || {
            let mut out = vec![];
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                let o = c.infer(std::slice::from_ref(&input), budget);
                out.push((t.elapsed(), o));
            }
            out
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let promotions = sc.stat("promotions");
    signal_pid(pid, "KILL");
    sc.wait_for("the promotion", Duration::from_secs(10), |s| s.stat("promotions") > promotions && s.gate() == "HEALTHY");
    let new_gen = sc.active_gen();
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let out = h.join().unwrap();

    let slowest = out.iter().map(|(t, _)| *t).max().unwrap();
    let mut reasons = std::collections::BTreeMap::new();
    for (_, o) in &out {
        if let Some(r) = o.reason() {
            *reasons.entry(r.as_str()).or_insert(0) += 1;
        }
    }
    let by_new = out.iter().filter(|(_, o)| o.source == Source::Model { model_gen: new_gen }).count();
    eprintln!(
        "kill -9 of gen {gen} under load: {} calls, {by_new} answered by gen {new_gen}, fallbacks {reasons:?}, slowest call {slowest:?}, promotion {} ms",
        out.len(),
        sc.stat("last_promotion_ms")
    );
    assert!(new_gen != gen);
    assert!(slowest < budget + Duration::from_millis(100), "a call outlived its budget: {slowest:?}");
    assert!(by_new > 0, "the standby answered nothing");
    for (_, o) in &out {
        if let Source::Model { model_gen } = o.source {
            assert!(model_gen == gen || model_gen == new_gen);
            assert!(cosine(&o.value[0], &before.value[0]) > 0.99999, "gen {model_gen} answered differently");
        }
    }
    assert!(sc.stat("deaths") >= 1.0);
}

/// The worker processes of this sidecar, by generation.
fn workers(sc: &Sidecar) -> Vec<(u32, i32)> {
    let id = sc.child.id();
    let kids = std::fs::read_to_string(format!("/proc/{id}/task/{id}/children")).unwrap_or_default();
    kids.split_whitespace()
        .filter_map(|p| p.parse::<i32>().ok())
        .filter_map(|p| {
            let cmd = String::from_utf8_lossy(&std::fs::read(format!("/proc/{p}/cmdline")).ok()?).to_string();
            let gen = cmd.split('\0').find_map(|a| a.strip_prefix("--gen="))?.parse().ok()?;
            cmd.contains("sidecarzero-worker-gguf").then_some((gen, p))
        })
        .collect()
}

/// Memory of the active worker and the standby, once the standby is warm and the active one
/// has served: the mappings of the served model file and the whole process, from /proc, in kB.
/// Printed under `what`.
fn memory(sc: &Sidecar, served: &Path, what: &str) -> Vec<(&'static str, Kb, Kb)> {
    let c = client(sc, 128);
    assert!(c.await_bound(Duration::from_secs(30)), "{}", sc.status());
    sc.wait_for("a verified warm standby", Duration::from_secs(30), Sidecar::has_warm_standby);
    for t in inputs(128).iter().cycle().take(20) {
        assert!(c.infer(std::slice::from_ref(t), Duration::from_secs(1)).is_model());
    }
    let active = sc.active_gen();
    let ws = workers(sc);
    assert_eq!(ws.len(), 2, "the active worker and the standby: {ws:?}");
    let file_kb = std::fs::metadata(served).unwrap().len() / 1024;
    let mut out = vec![];
    for (gen, pid) in ws {
        let (m, r) = (smaps_of(pid, served), rollup(pid));
        let role = if gen == active { "active" } else { "standby" };
        let f = |k: &str, x: &Kb| x.get(k).copied().unwrap_or(0);
        eprintln!(
            "{what}, {role} (gen {gen}): model file {file_kb} kB in {} mapping(s): Rss {} Pss {} Shared_Clean {} Shared_Dirty {} Private {} Anonymous {}; \
             whole process: Rss {} Pss {} Pss_Anon {} Pss_File {} Pss_Shmem {} (kB)",
            f("mappings", &m),
            f("Rss", &m),
            f("Pss", &m),
            f("Shared_Clean", &m),
            f("Shared_Dirty", &m),
            f("Private_Clean", &m) + f("Private_Dirty", &m),
            f("Anonymous", &m),
            f("Rss", &r),
            f("Pss", &r),
            f("Pss_Anon", &r),
            f("Pss_File", &r),
            f("Pss_Shmem", &r),
        );
        out.push((role, m, r));
    }
    out
}

type Kb = std::collections::BTreeMap<String, u64>;

#[test]
fn the_active_worker_and_the_standby_share_the_mapped_weights() {
    let Some(file) = model_file() else { return };
    let _g = serial();
    let dir = scratch();
    let sc = serve(&dir, &[]);
    let file_kb = std::fs::metadata(&file).unwrap().len() / 1024;
    for (role, m, r) in memory(&sc, &dir.join("model.gguf"), "f16") {
        assert!(m["Rss"] * 10 >= file_kb * 9, "{role}: most of the weights are resident through the mapping ({} of {file_kb} kB)", m["Rss"]);
        assert!(m["Pss"] * 10 <= m["Rss"] * 6, "{role}: the mapped weights are not shared (Pss {} of Rss {} kB)", m["Pss"], m["Rss"]);
        assert_eq!(m["Anonymous"], 0, "{role}: no mapped page was copied on write");
        assert!(r["Anonymous"] < file_kb, "{role}: {} kB of private memory, as much as the weights: they were copied", r["Anonymous"]);
    }
}

/// A quantized model, which llama.cpp's CPU backend may repack into a layout of its own: with
/// `--repack=on` (llama.cpp's default) the repacked weights are private to each worker; with
/// `--repack=off` every weight is read from the shared mapping. Measured, with the cost per call.
#[test]
fn quantized_weights_are_shared_unless_repacked() {
    let Some(file) = model_file() else { return };
    let _g = serial();
    let qdir = scratch();
    let q = qdir.join("bge-small-q4_0.gguf");
    scz_worker_gguf::quantize(&file.display().to_string(), &q.display().to_string(), scz_worker_gguf::Quant::Q4_0).unwrap();
    let q_kb = std::fs::metadata(&q).unwrap().len() / 1024;
    let input = cut(reference_model().tokenize(&long_text()), 128);
    let want = reference(&input);
    let mut anon = vec![];
    for repack in ["on", "off"] {
        let dir = scratch();
        let arg = format!("--worker-arg=--repack={repack}");
        let sc = serve_file(&dir, &q, &[&arg]);
        let mem = memory(&sc, &dir.join("model.gguf"), &format!("Q4_0, repack {repack}"));
        let c = client(&sc, 128);
        assert!(c.await_bound(Duration::from_secs(10)));
        let mut ms = vec![];
        let mut cos = 1f64;
        for _ in 0..12 {
            let t = Instant::now();
            let o = c.infer(std::slice::from_ref(&input), Duration::from_secs(1));
            ms.push(t.elapsed().as_secs_f64() * 1e3);
            assert!(o.is_model(), "{:?}", o.source);
            cos = cos.min(cosine(&o.value[0], &want));
        }
        eprintln!("Q4_0 ({q_kb} kB), repack {repack}: 128 tokens in {:.1} ms (p50, 2 threads); cosine to the f16 reference {cos:.4}", p50(&mut ms[2..]));
        let active = mem.iter().find(|x| x.0 == "active").unwrap();
        anon.push(active.2["Anonymous"]);
        if repack == "off" {
            for (role, m, r) in &mem {
                assert!(m["Pss"] * 10 <= m["Rss"] * 6, "{role}: not shared with repacking off (Pss {} of Rss {} kB)", m["Pss"], m["Rss"]);
                assert!(r["Anonymous"] < q_kb, "{role}: {} kB private with repacking off", r["Anonymous"]);
            }
        }
    }
    eprintln!("Q4_0: the active worker's private memory is {} kB with repacking on and {} kB with it off", anon[0], anon[1]);
}

#[test]
fn the_model_opens_under_landlock_only_from_its_directory() {
    let Some(file) = model_file() else { return };
    let _g = serial();
    let start = |rule: Option<String>| {
        let dir = scratch();
        publish(&file, &dir.join("model.gguf"));
        let model = format!("--model={}", dir.join("model.gguf").display());
        let rule = rule.map(|r| r.replace("DIR", &dir.display().to_string()));
        let mut args = vec![model.as_str(), "--standby=none", "--backoff-max-ms=200"];
        if let Some(r) = &rule {
            args.push(r);
        }
        let sc = Sidecar::start_in(dir.clone(), "sidecarzero-worker-gguf", &args);
        (sc, dir)
    };
    let refused = "opening the model at /proc/self/fd/5: Permission denied";
    // The default view, and /proc added to it: llama.cpp's open of /proc/self/fd/N is checked
    // against the model file itself, so neither serves.
    for rule in [None, Some("--worker-read=/proc".to_string())] {
        let (sc, _) = start(rule.clone());
        sc.wait_for("the refused open", Duration::from_secs(20), |s| s.log().contains(refused));
        assert_sandboxed(&sc);
        assert_eq!(sc.gate(), "INIT", "{rule:?}");
    }
    // The model file alone serves, until a reload publishes a new file: the rule names the
    // inode that was there when the supervisor started. With no standby there is memory for
    // one worker (supervisor §5.6): the old worker exits first, the new one is refused, and
    // the previous model, whose descriptor names the inode the rule allows, is loaded again.
    let (sc, dir) = start(Some("--worker-read=DIR/model.gguf".to_string()));
    let c = client(&sc, 128);
    assert!(c.await_bound(Duration::from_secs(30)), "{}", sc.log());
    let gen = sc.active_gen();
    publish(&file, &dir.join("model.gguf"));
    sc.wait_for("the refused reload", Duration::from_secs(20), |s| s.log().contains(refused));
    sc.wait_for("the rollback", Duration::from_secs(30), |s| s.stat("rollbacks") >= 1.0 && s.gate() == "HEALTHY");
    assert!(sc.active_gen() > gen, "a new worker of the previous model serves");
    assert!(c.await_bound(Duration::from_secs(10)));
    assert!(c.infer(std::slice::from_ref(&inputs(128)[0]), Duration::from_secs(1)).is_model());
    drop((c, sc));
    // The model's directory serves, and keeps serving across reloads.
    let (sc, dir) = start(Some("--worker-read=DIR".to_string()));
    let c = client(&sc, 128);
    assert!(c.await_bound(Duration::from_secs(30)), "{}", sc.log());
    let gen = sc.active_gen();
    publish(&file, &dir.join("model.gguf"));
    sc.wait_for("the reload", Duration::from_secs(20), |s| s.active_gen() > gen && s.gate() == "HEALTHY");
    assert!(c.await_bound(Duration::from_secs(10)));
    assert!(c.infer(std::slice::from_ref(&inputs(128)[0]), Duration::from_secs(1)).is_model());
    assert!(!sc.log().contains(refused));
}

#[test]
fn latency_per_call_at_several_lengths() {
    let Some(_) = model_file() else { return };
    let _g = serial();
    let l = 512;
    let long = reference_model().tokenize(&long_text());
    let lengths = [8usize, 32, 128, 512];
    let mut table = vec![];
    for threads in [1u32, 2, 4] {
        let dir = scratch();
        let t = format!("--worker-arg=--threads={threads}");
        // A measurement, not a gate: a budget well past 512 tokens on one thread (about 370 ms)
        // so that a call starved on a busy machine still counts.
        let sc = serve(&dir, &["--slot-size=16384", "--worker-arg=--max-tokens=512", &t, "--standby=none", "--max-deadline-ms=5000"]);
        let c = client(&sc, l);
        assert!(c.await_bound(Duration::from_secs(30)), "{}", sc.status());
        let mut row = vec![];
        for &n in &lengths {
            let input = cut(long.clone(), n);
            assert_eq!(input.len(), n);
            let calls = if n >= 512 { 10 } else { 30 };
            let mut ms = vec![];
            for _ in 0..calls + 2 {
                let t0 = Instant::now();
                let o = c.infer(std::slice::from_ref(&input), Duration::from_secs(5));
                assert!(o.is_model(), "{n} tokens on {threads} thread(s): {:?}", o.source);
                ms.push(t0.elapsed().as_secs_f64() * 1e3);
            }
            row.push(p50(&mut ms[2..]));
        }
        table.push((threads, row));
    }
    eprintln!("p50 per call, end to end, bge-small f16 (ms): tokens {lengths:?}");
    for (threads, row) in &table {
        eprintln!("  {threads} thread(s): {}", row.iter().map(|v| format!("{v:.1}")).collect::<Vec<_>>().join(" | "));
    }
}
