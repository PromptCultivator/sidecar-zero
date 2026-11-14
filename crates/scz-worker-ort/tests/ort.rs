//! The ONNX Runtime backend against the test models (models/gen.py): the signature it derives,
//! answers identical to the Python runtime's, and the two memory profiles of worker §5.
//!
//! The memory tests measure resident memory, so each runs in a child process of its own.

use scz_shm::abi::elem_type as et;
use scz_shm::registry::Column;
use scz_worker_ort::{signature, trim, Options, Profile, RunError, Session, Shape};
use std::path::PathBuf;
use std::process::Command;

/// The generated models, made on first use (the generator does nothing when they are current).
fn models() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("target/models");
    let ok = Command::new("python3").arg(root.join("models/gen.py")).arg(&dir).status().map(|s| s.success()).unwrap_or(false);
    assert!(ok, "models/gen.py failed: it needs the dev image's Python packages (scripts/dev)");
    dir
}

fn open(name: &str, o: &Options) -> Session {
    let bytes: &'static [u8] = Box::leak(std::fs::read(models().join(name)).unwrap().into_boxed_slice());
    Session::open(bytes, o).unwrap()
}

/// Run `rows` rows through private buffers.
fn run(s: &Session, ins: &[Shape], outs: &[Shape], input: &[f32], rows: u32) -> Result<Vec<f32>, RunError> {
    let mut x: Vec<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
    x.resize(rows as usize * ins[0].row_bytes, 0);
    let mut y = vec![0u8; rows as usize * outs[0].row_bytes];
    let in_place = s.run(&[ins[0].tensor(x.as_mut_ptr(), rows)], &[outs[0].tensor(y.as_mut_ptr(), rows)], false)?;
    assert!(in_place, "the runtime wrote its answer somewhere other than the bound buffer");
    Ok(y.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
}

#[test]
fn the_fraud_model_has_the_launch_signature_and_the_reference_scores() {
    let s = open("fraud.ort", &Options::default());
    let (sig, ins, outs) = signature(&s).unwrap();
    assert_eq!(sig.schema.request, vec![Column::new("features", et::F32, 256)]);
    assert_eq!(sig.schema.response, vec![Column::new("score", et::F32, 1)]);
    assert_eq!(sig.fixed_batch, None);
    assert_eq!((ins[0].row_bytes, outs[0].row_bytes, outs[0].inner.clone()), (1024, 4, vec![1]));

    let text = std::fs::read_to_string(models().join("reference.json")).unwrap();
    let field = |k: &str| -> Vec<f32> {
        let a = text.split(&format!("\"{k}\": [")).nth(1).unwrap();
        a.replace(['[', ']', '}'], "").split(',').take_while(|v| !v.contains('"')).filter_map(|v| v.trim().parse::<f32>().ok()).collect()
    };
    let (x, want) = (field("features"), field("score"));
    assert_eq!((x.len(), want.len()), (64 * 256, 64));
    // All 64 rows at once, then one row at a time: the Python runtime's answers. It ran the
    // ensemble on several threads and this session on one, so the trees' sums may be added in
    // another order; the difference is rounding.
    let got = run(&s, &ins, &outs, &x, 64).unwrap();
    let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    eprintln!("largest difference from the reference: {worst:e}");
    assert!(worst < 1e-5, "scores differ from the reference by up to {worst}");
    for r in [0, 17, 63] {
        assert_eq!(run(&s, &ins, &outs, &x[r * 256..(r + 1) * 256], 1).unwrap(), vec![got[r]], "row {r} alone");
    }
}

#[test]
fn a_fixed_batch_and_a_changed_schema_are_read_from_the_model() {
    let (sig, _, _) = signature(&open("fraud-fixed1.ort", &Options::default())).unwrap();
    assert_eq!(sig.fixed_batch, Some(1));
    let (sig, _, _) = signature(&open("fraud128.ort", &Options::default())).unwrap();
    assert_eq!(sig.schema.request, vec![Column::new("features", et::F32, 128)]);
}

fn resident_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: f64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096.0 / (1 << 20) as f64
}

/// Run in a child process: mixed load on the MLP under one profile, trimming after each cycle.
#[test]
fn memory_child() {
    let Ok(mode) = std::env::var("SCZ_ORT_MEMORY_CHILD") else { return };
    scz_worker_ort::tune_allocator(); // as the worker does at load
    let o = Options { profile: if mode == "B" { Profile::B } else { Profile::A }, arena_max: 64 << 20, prepack: true };
    let s = open("mlp.ort", &o);
    let (_, ins, outs) = signature(&s).unwrap();
    let x = vec![0.5f32; 8192 * 256];
    run(&s, &ins, &outs, &x, 1).unwrap();
    trim();
    let base = resident_mb();
    let mut peak = base;
    let mut after = vec![];
    for cycle in 0..4 {
        for k in 0..40u32 {
            let rows = if k % 10 == 9 { 8192 } else { 1 + (k * 7 + cycle) % 32 };
            run(&s, &ins, &outs, &x, rows).unwrap();
            peak = peak.max(resident_mb());
        }
        // TRIM: profile B shrinks its arena on the next run, then both purge the allocator.
        if o.profile == Profile::B {
            let mut y = vec![0u8; 4];
            let mut one: Vec<u8> = vec![0; 1024];
            s.run(&[ins[0].tensor(one.as_mut_ptr(), 1)], &[outs[0].tensor(y.as_mut_ptr(), 1)], true).unwrap();
        }
        trim();
        after.push(resident_mb());
    }
    // An oversize request: 65,536 rows needs about 200 MB of activations.
    let big = vec![0.5f32; 65536 * 256];
    let oversize = run(&s, &ins, &outs, &big, 65536);
    let healthy = run(&s, &ins, &outs, &x, 4).is_ok();
    trim();
    println!(
        "RESULT base={base:.1} peak={peak:.1} after={after:?} oversize={} healthy={healthy} end={:.1}",
        match &oversize {
            Ok(_) => "ran".to_string(),
            Err(RunError::Exhausted(_)) => "exhausted".to_string(),
            Err(RunError::Model(m)) => format!("model-error:{}", m.replace(' ', "_")),
        },
        resident_mb()
    );
}

fn child(mode: &str) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "memory_child", "--nocapture", "--test-threads=1"])
        .env("SCZ_ORT_MEMORY_CHILD", mode)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let line = text
        .lines()
        .find_map(|l| l.find("RESULT").map(|i| &l[i..]))
        .unwrap_or_else(|| panic!("no result from profile {mode}: {text}{}", String::from_utf8_lossy(&out.stderr)));
    eprintln!("profile {mode}: {line}");
    line.to_string()
}

fn num(line: &str, key: &str) -> f64 {
    line.split(&format!("{key}=")).nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap()
}

fn list(line: &str) -> Vec<f64> {
    line.split("after=[").nth(1).unwrap().split(']').next().unwrap().split(", ").map(|v| v.parse().unwrap()).collect()
}

#[test]
fn profile_a_returns_to_its_baseline_after_each_trim() {
    let r = child("A");
    let base = num(&r, "base");
    // Bounded once trimmed: every cycle, with its 8,192-row batches, ends within a few MB of
    // where it started.
    for a in list(&r) {
        assert!(a < base + 4.0, "profile A did not return to its baseline after TRIM: {r}");
    }
    assert!(r.contains("healthy=true"), "{r}");
}

#[test]
fn profile_b_caps_its_arena_and_survives_an_oversize_request() {
    let r = child("B");
    let base = num(&r, "base");
    // The arena holds at most its 64 MB between trims, and gives it back on the shrinking run.
    assert!(num(&r, "peak") < base + 64.0 + 8.0, "profile B grew past its 64 MB cap: {r}");
    for a in list(&r) {
        assert!(a < base + 4.0, "profile B did not return to its baseline after TRIM: {r}");
    }
    assert!(r.contains("oversize=exhausted"), "the capped arena did not refuse 65,536 rows as RESOURCE_EXHAUSTED: {r}");
    assert!(r.contains("healthy=true"), "the session did not survive the refusal: {r}");
}
