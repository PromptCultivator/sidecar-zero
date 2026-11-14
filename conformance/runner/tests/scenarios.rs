//! Every scenario in `conformance/scenarios`, against the Rust agent, and the suite checks of
//! testing §3 (`scz_conformance::check_suite`): no scenario skipped unless
//! `conformance/skips/rust.toml` says why, every fallback reason produced by some scenario or
//! exempted in `conformance/coverage.toml`, and every chaos opcode used.
//!
//! SCZ_SCENARIO=substring runs only the matching scenarios (and skips the coverage checks).
//! SCZ_TOLERANCE_MS sets the tolerance of assertion 2 (default 50, the shared-runner value).
//! The other SDKs' agents run through the `scz-conformance` program (`scripts/check`).

use scz_conformance::{check_suite, run_file, scenario_key, scenarios, Bins};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn bins() -> Bins {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let tolerance = std::env::var("SCZ_TOLERANCE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
    Bins { dir, crash_dir: None, agent: None, crash_agent: None, tolerance: Duration::from_millis(tolerance) }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn every_scenario_passes_with_the_rust_agent() {
    let filter = std::env::var("SCZ_SCENARIO").ok();
    let files: Vec<PathBuf> =
        scenarios(&root().join("scenarios")).into_iter().filter(|f| filter.as_ref().is_none_or(|s| f.to_string_lossy().contains(s.as_str()))).collect();
    assert!(!files.is_empty(), "no scenarios");
    let b = bins();
    let (mut failures, mut results) = (vec![], vec![]);
    for f in &files {
        match run_file(f, &b) {
            Ok(o) => {
                match &o.skipped {
                    Some(why) => eprintln!("SKIP {} ({why})", o.name),
                    None => eprintln!("PASS {} ({} calls, {:.2} s, {} late wake-ups)", o.name, o.calls, o.elapsed.as_secs_f64(), o.late_wakes),
                }
                results.push((scenario_key(f), o));
            }
            Err(e) => {
                eprintln!("FAIL {e}");
                failures.push(e.lines().next().unwrap_or_default().to_string());
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} scenarios failed:\n{}", failures.len(), files.len(), failures.join("\n"));
    let problems = check_suite(&results, "rust", filter.is_none());
    assert!(problems.is_empty(), "suite checks:\n{}", problems.join("\n"));
}
