//! The crash-point harness (testing §2.4): every tag in the source is in `scz_shm::crash::TAGS`,
//! every tag there has a scenario in `conformance/scenarios/crash`, and each of those passes:
//! the process killed at that store, the recovery path run, the universal assertions held.
//!
//! The crash points are compiled in only with the `crashpoints` feature, so this test builds
//! the chaos worker and the Rust agent with it into a target directory of their own
//! (`target/crash`), leaving the ordinary binaries untouched.

use scz_conformance::{crash_at, run_file, scenarios, Bins};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tags_in(dir: &Path, out: &mut BTreeSet<String>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() && !p.ends_with("target") {
            tags_in(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            let src = std::fs::read_to_string(&p).unwrap();
            for part in src.split("crash_point!(\"").skip(1) {
                let tag = part.split('"').next().unwrap();
                if !tag.contains('$') {
                    out.insert(tag.to_string());
                }
            }
        }
    }
}

#[test]
fn every_crash_point_is_listed_and_has_a_scenario() {
    let mut used = BTreeSet::new();
    for d in ["crates", "sdks"] {
        tags_in(&root().join(d), &mut used);
    }
    let listed: BTreeSet<String> = scz_shm::crash::TAGS.iter().map(|t| t.to_string()).collect();
    assert_eq!(used, listed, "the tags in the source and scz_shm::crash::TAGS differ");
    let have: BTreeSet<String> = scenarios(&root().join("conformance/scenarios/crash")).iter().filter_map(|f| crash_at(f)).collect();
    let missing: Vec<&String> = listed.difference(&have).collect();
    assert!(missing.is_empty(), "crash points without a scenario: {missing:?}");
}

#[test]
fn every_crash_point_recovers() {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let profile = profile_dir.file_name().unwrap().to_string_lossy().to_string();
    let target = profile_dir.parent().unwrap().join("crash");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut c = Command::new(cargo);
    c.current_dir(root()).args(["build", "-q", "--target-dir"]).arg(&target).args([
        "-p",
        "scz-worker-core",
        "-p",
        "scz-conformance-agent",
        "--features",
        "scz-worker-core/crashpoints,scz-conformance-agent/crashpoints",
        "--bins",
    ]);
    if profile == "release" {
        c.arg("--release");
    }
    assert!(c.status().unwrap().success(), "building the crash-point binaries failed");
    let tolerance = std::env::var("SCZ_TOLERANCE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    let bins = Bins { dir: profile_dir, crash_dir: Some(target.join(&profile)), agent: None, crash_agent: None, tolerance: Duration::from_millis(tolerance) };
    let filter = std::env::var("SCZ_SCENARIO").ok();
    let mut failures = vec![];
    for f in scenarios(&root().join("conformance/scenarios/crash")) {
        if filter.as_ref().is_some_and(|s| !f.to_string_lossy().contains(s.as_str())) {
            continue;
        }
        match run_file(&f, &bins) {
            Ok(o) => eprintln!("PASS {} ({} calls, {:.2} s)", o.name, o.calls, o.elapsed.as_secs_f64()),
            Err(e) => {
                eprintln!("FAIL {e}");
                failures.push(e.lines().next().unwrap_or_default().to_string());
            }
        }
    }
    assert!(failures.is_empty(), "{} crash scenarios failed:\n{}", failures.len(), failures.join("\n"));
}
