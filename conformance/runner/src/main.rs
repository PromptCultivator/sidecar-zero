//! scz-conformance: run scenario files against an SDK's agent (testing §3).
//!
//!   scz-conformance [--bins=DIR] [--crash-bins=DIR] [--agent=PATH] [--crash-agent=PATH]
//!                   [--tolerance-ms=N] PATH...
//!
//! A PATH is a scenario file or a directory of them. `--bins` defaults to the directory this
//! program is in; `--crash-bins` holds the crash-point builds (testing §2.4) that `crash/`
//! scenarios need; `--crash-agent` is the agent under test built with crash points, without
//! which the scenarios that crash the agent skip. Prints one line per scenario, then the suite
//! checks (`scz_conformance::check_suite`): skips against `conformance/skips/<sdk>.toml`, and,
//! when every PATH is a directory, reason and opcode coverage. Exits 1 if a scenario failed or
//! a suite check did. With SCZ_TRAIL set, each passing scenario's steps are printed too.

use scz_conformance::{check_suite, run_file, scenario_key, scenarios, Bins};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_default();
    let mut bins = Bins { dir: exe_dir, crash_dir: None, agent: None, crash_agent: None, tolerance: Duration::from_millis(50) };
    let (mut files, mut full) = (vec![], true);
    for a in std::env::args().skip(1) {
        match a.split_once('=') {
            Some(("--bins", v)) => bins.dir = v.into(),
            Some(("--crash-bins", v)) => bins.crash_dir = Some(v.into()),
            Some(("--agent", v)) => bins.agent = Some(v.into()),
            Some(("--crash-agent", v)) => bins.crash_agent = Some(v.into()),
            Some(("--tolerance-ms", v)) => bins.tolerance = Duration::from_millis(v.parse().expect("--tolerance-ms: a number")),
            _ if a.starts_with("--") => {
                eprintln!("unknown option {a}");
                std::process::exit(2);
            }
            _ => {
                let p = PathBuf::from(&a);
                if p.is_dir() {
                    files.extend(scenarios(&p));
                } else {
                    files.push(p);
                    full = false;
                }
            }
        }
    }
    let (mut failed, mut results, mut sdk) = (0, vec![], String::new());
    for f in &files {
        match run_file(f, &bins) {
            Ok(o) => {
                match &o.skipped {
                    Some(why) => println!("SKIP {} ({why})", o.name),
                    None => println!("PASS {} ({} calls, {:.1} s{})", o.name, o.calls, o.elapsed.as_secs_f64(), late(o.late_wakes)),
                }
                if std::env::var_os("SCZ_TRAIL").is_some() {
                    for t in &o.trail {
                        println!("    {t}");
                    }
                }
                if sdk.is_empty() {
                    sdk = o.sdk.clone();
                }
                results.push((scenario_key(f), o));
            }
            Err(e) => {
                failed += 1;
                println!("FAIL {e}");
            }
        }
    }
    // A failed scenario has no outcome, so coverage is judged only on a clean run.
    let problems = if sdk.is_empty() { vec![] } else { check_suite(&results, &sdk, full && failed == 0) };
    for p in &problems {
        println!("SUITE {p}");
    }
    let (pass, skip) = (results.iter().filter(|r| r.1.skipped.is_none()).count(), results.iter().filter(|r| r.1.skipped.is_some()).count());
    println!("{sdk}: {pass} passed, {skip} skipped, {failed} failed, {} suite problems", problems.len());
    std::process::exit(i32::from(failed > 0 || !problems.is_empty()));
}

fn late(n: u64) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(", {n} late wake-ups")
    }
}
