//! End-to-end check of the client against a running Triton; `bench/baselines/triton/smoke.sh`
//! runs it in the dev image, joined to Triton's IPC, network and PID namespaces.
//!
//!   smoke ENDPOINT MODELS_DIR [--calls N] [--timeout-ms MS] [--chaos | --await-outage]
//!
//! 1. Scores the 64 reference rows (MODELS_DIR/reference-features.f32) and checks each against
//!    reference-scores.f32 within 1e-5.
//! 2. Times N (default 10,000) sequential single-row calls after 1,000 warm-up calls, checking
//!    every answer, and prints the latency distribution.
//! 3. Runs four threads at once, each with its own client, 1,000 checked calls each.
//! 4. With --chaos (needs Triton's PID namespace and a supervised Triton): freezes tritonserver
//!    for 500 ms with SIGSTOP, then kills it with SIGKILL, calling once per millisecond with the
//!    --timeout-ms budget (default 2) throughout; shows that every call during the outage fails
//!    within its budget and that calls succeed, with the right scores, once Triton is back.
//! 5. Drops the client, and checks that Triton no longer has its regions and /dev/shm no longer
//!    holds them.
//!
//! With --await-outage, after step 1 it prints "awaiting an outage" and calls once per
//! millisecond until calls fail because Triton went away (someone else kills it) and then
//! succeed again, for at most 120 s; then checks the reference rows again. For an unsupervised
//! Triton whose container restarts. Calls that only time out and then succeed again are a
//! stall, not the outage: reported, and the wait goes on.
//!
//! Exits non-zero when any check fails.

use std::time::{Duration, Instant};

use scz_bench_triton::proto::grpc_inference_service_client::GrpcInferenceServiceClient;
use scz_bench_triton::proto::SystemSharedMemoryStatusRequest;
use scz_bench_triton::{Triton, FEATURES};

const ROWS: usize = 64;
const TOLERANCE: f32 = 1e-5;
const MODEL: &str = "fraud";

fn main() {
    if let Err(e) = run() {
        eprintln!("smoke: FAILED: {e}");
        std::process::exit(1);
    }
    println!("smoke: all checks passed");
}

struct Args {
    endpoint: String,
    models: String,
    calls: usize,
    budget: Duration,
    chaos: bool,
    await_outage: bool,
}

fn args() -> Result<Args, String> {
    let mut a = std::env::args().skip(1);
    let (mut pos, mut calls, mut budget, mut chaos, mut await_outage) = (vec![], 10_000, Duration::from_millis(2), false, false);
    while let Some(x) = a.next() {
        let mut num = |what: &str| a.next().and_then(|v| v.parse::<u64>().ok()).ok_or(format!("{what} takes a number"));
        match x.as_str() {
            "--calls" => calls = num("--calls")? as usize,
            "--timeout-ms" => budget = Duration::from_millis(num("--timeout-ms")?),
            "--chaos" => chaos = true,
            "--await-outage" => await_outage = true,
            _ if x.starts_with("--") => return Err(format!("unknown option {x}")),
            _ => pos.push(x),
        }
    }
    let [endpoint, models] =
        <[String; 2]>::try_from(pos).map_err(|_| "usage: smoke ENDPOINT MODELS_DIR [--calls N] [--timeout-ms MS] [--chaos | --await-outage]")?;
    Ok(Args { endpoint, models, calls, budget, chaos, await_outage })
}

fn f32s(path: &str) -> Result<Vec<f32>, String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

fn run() -> Result<(), String> {
    let a = args()?;
    let features = f32s(&format!("{}/reference-features.f32", a.models))?;
    let scores = f32s(&format!("{}/reference-scores.f32", a.models))?;
    if features.len() != ROWS * FEATURES || scores.len() != ROWS {
        return Err(format!("reference files: {} features and {} scores, expected {} and {ROWS}", features.len(), scores.len(), ROWS * FEATURES));
    }
    let rows: Vec<[f32; FEATURES]> = features.as_chunks::<FEATURES>().0.to_vec();
    let t0 = Instant::now();
    let mut t = Triton::connect(&a.endpoint, MODEL, "smoke")?;
    println!("connected to {} and registered both regions in {:.1} ms", a.endpoint, ms(t0.elapsed()));

    // 1. The reference rows.
    let worst = reference(&mut t, &rows, &scores)?;
    println!("reference: {ROWS}/{ROWS} rows match reference-scores.f32 within {TOLERANCE:e} (largest difference {worst:e})");

    if a.await_outage {
        println!("awaiting an outage: calling once per millisecond with a {:?} budget", a.budget);
        let watching = Instant::now();
        loop {
            let mut o = Outage { from_first_failure: true, ..Outage::default() };
            while o.failed == 0 {
                o.call(&mut t, &rows, &scores, a.budget, watching)?;
                if watching.elapsed() > Duration::from_secs(120) {
                    return Err("no outage within 120 s".into());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            o.until_served(&mut t, &rows, &scores, a.budget, watching)?;
            if o.disconnected {
                o.report("outage (from the first failed call)", a.budget)?;
                break;
            }
            println!(
                "a stall, not the outage: {} call(s) timed out, the slowest in {:.2} ms; still waiting",
                o.failed,
                ms(o.failures.iter().copied().max().unwrap_or_default())
            );
        }
        let worst = reference(&mut t, &rows, &scores)?;
        println!("after the outage: {ROWS}/{ROWS} reference rows match (largest difference {worst:e})");
        return Ok(());
    }

    // 2. Latency of sequential single-row calls.
    let second = Duration::from_secs(1);
    for i in 0..1000 {
        t.infer(&rows[i % ROWS], second)?;
    }
    let mut lat = Vec::with_capacity(a.calls);
    let start = Instant::now();
    for i in 0..a.calls {
        let c = Instant::now();
        let s = t.infer(&rows[i % ROWS], second).map_err(|e| format!("call {i}: {e}"))?;
        lat.push(c.elapsed());
        check(i % ROWS, s, scores[i % ROWS])?;
    }
    let wall = start.elapsed();
    lat.sort();
    let p = |q: f64| ms(lat[((q * lat.len() as f64).ceil() as usize).clamp(1, lat.len()) - 1]) * 1e3;
    println!(
        "latency over {} sequential calls (µs): p50 {:.0}  p90 {:.0}  p99 {:.0}  p99.9 {:.0}  max {:.0}  mean {:.0}; {:.0} calls/s",
        lat.len(),
        p(0.50),
        p(0.90),
        p(0.99),
        p(0.999),
        p(1.0),
        ms(wall) * 1e3 / lat.len() as f64,
        lat.len() as f64 / wall.as_secs_f64()
    );

    // 3. Clients on concurrent threads, as the benchmark's host runs them.
    let threads: Vec<_> = (0..4)
        .map(|n| {
            let (endpoint, rows, scores) = (a.endpoint.clone(), rows.clone(), scores.clone());
            std::thread::spawn(move || -> Result<(), String> {
                let mut c = Triton::connect(&endpoint, MODEL, &format!("smoke-t{n}"))?;
                for i in 0..1000 {
                    let r = (i * 7 + n) % ROWS;
                    check(r, c.infer(&rows[r], second).map_err(|e| format!("thread {n}, call {i}: {e}"))?, scores[r])?;
                }
                Ok(())
            })
        })
        .collect();
    for t in threads {
        t.join().map_err(|_| "a client thread panicked".to_owned())??;
    }
    let left: Vec<_> = registered(&a.endpoint)?.into_iter().filter(|n| n.starts_with("scz-smoke-t")).collect();
    if !left.is_empty() {
        return Err(format!("regions still registered after their clients were dropped: {left:?}"));
    }
    println!("concurrency: 4 threads x 1000 calls, one client each, every score right; their 8 regions unregistered on drop");

    if a.chaos {
        // 4a. A freeze: calls time out within their budget, and succeed once Triton runs again.
        let pid = tritonserver()?;
        signal(pid, libc::SIGSTOP)?;
        let frozen = Instant::now();
        let mut o = Outage::default();
        while frozen.elapsed() < Duration::from_millis(500) {
            o.call(&mut t, &rows, &scores, a.budget, frozen)?;
            std::thread::sleep(Duration::from_millis(1));
        }
        signal(pid, libc::SIGCONT)?;
        o.until_served(&mut t, &rows, &scores, a.budget, frozen)?;
        o.report("freeze (SIGSTOP 500 ms, then SIGCONT)", a.budget)?;

        // 4b. A kill: the supervisor restarts tritonserver; the client reconnects and re-registers.
        let pid = tritonserver()?;
        signal(pid, libc::SIGKILL)?;
        let killed = Instant::now();
        let mut o = Outage::default();
        o.until_served(&mut t, &rows, &scores, a.budget, killed)?;
        let again = tritonserver()?;
        if again == pid {
            return Err(format!("tritonserver is still pid {pid} after SIGKILL"));
        }
        o.report(&format!("kill (SIGKILL pid {pid}; restarted as pid {again})"), a.budget)?;
        let worst = reference(&mut t, &rows, &scores)?;
        println!("after the restart: {ROWS}/{ROWS} reference rows match (largest difference {worst:e})");
    }

    // 5. Dropping the client unregisters its regions and unlinks them.
    let ours = [format!("scz-smoke-{}-in", std::process::id()), format!("scz-smoke-{}-out", std::process::id())];
    let shm = |n: &String| std::path::Path::new("/dev/shm").join(n).exists();
    let before = registered(&a.endpoint)?;
    if !ours.iter().all(|n| before.contains(n) && shm(n)) {
        return Err(format!("expected {ours:?} registered and in /dev/shm; Triton has {before:?}"));
    }
    drop(t);
    let after = registered(&a.endpoint)?;
    if ours.iter().any(|n| after.contains(n) || shm(n)) {
        return Err(format!("after the drop, Triton has {after:?}, and /dev/shm: {:?}", ours.iter().map(shm).collect::<Vec<_>>()));
    }
    println!("drop: {ours:?} were registered and in /dev/shm; after the drop, neither (Triton holds {} regions)", after.len());
    Ok(())
}

/// The names of the system shared-memory regions Triton has registered, by its status call.
fn registered(endpoint: &str) -> Result<Vec<String>, String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let mut c = GrpcInferenceServiceClient::connect(endpoint.to_owned()).await.map_err(|e| e.to_string())?;
        let status = c.system_shared_memory_status(SystemSharedMemoryStatusRequest { name: String::new() }).await.map_err(|s| s.to_string())?;
        Ok(status.into_inner().regions.into_keys().collect())
    })
}

fn reference(t: &mut Triton, rows: &[[f32; FEATURES]], scores: &[f32]) -> Result<f32, String> {
    let mut worst = 0f32;
    for (i, row) in rows.iter().enumerate() {
        let s = t.infer(row, Duration::from_secs(1)).map_err(|e| format!("reference row {i}: {e}"))?;
        check(i, s, scores[i])?;
        worst = worst.max((s - scores[i]).abs());
    }
    Ok(worst)
}

fn check(row: usize, got: f32, want: f32) -> Result<(), String> {
    if (got - want).abs() <= TOLERANCE {
        Ok(())
    } else {
        Err(format!("row {row}: score {got}, reference {want}"))
    }
}

/// Calls made from a fault until the first success after it.
#[derive(Default)]
struct Outage {
    failed: usize,
    /// How long each failed call took.
    failures: Vec<Duration>,
    /// From the fault to the first failure, and to the first success after it.
    first_failure: Option<Duration>,
    served: Option<Duration>,
    errors: Vec<String>,
    from_first_failure: bool,
    /// The call that ended the outage, and the longest pause between the end of one call and
    /// the start of the next while it lasted: a stall outside the calls would show there.
    recovered_in: Duration,
    longest_pause: Duration,
    last_end: Option<Instant>,
    /// Whether a call failed for something other than its timeout: the server went away.
    disconnected: bool,
}

impl Outage {
    fn call(&mut self, t: &mut Triton, rows: &[[f32; FEATURES]], scores: &[f32], budget: Duration, fault: Instant) -> Result<bool, String> {
        let i = self.failed % ROWS;
        let c = Instant::now();
        if let (Some(end), true) = (self.last_end, self.failed > 0) {
            self.longest_pause = self.longest_pause.max(c - end);
        }
        let r = t.infer(&rows[i], budget);
        self.last_end = Some(Instant::now());
        match r {
            Ok(s) => {
                check(i, s, scores[i])?;
                if self.failed > 0 && self.served.is_none() {
                    self.served = Some(fault.elapsed());
                    self.recovered_in = c.elapsed();
                }
                Ok(true)
            }
            Err(e) => {
                self.failed += 1;
                self.disconnected |= !e.starts_with("timed out");
                self.failures.push(c.elapsed());
                self.first_failure.get_or_insert(fault.elapsed());
                // The kinds of error, without the details that differ between calls.
                let kind: String = e.chars().take(90).collect();
                if self.errors.len() < 4 && !self.errors.contains(&kind) {
                    self.errors.push(kind);
                }
                Ok(false)
            }
        }
    }

    /// Call once per millisecond until a call succeeds, for at most 120 s.
    fn until_served(&mut self, t: &mut Triton, rows: &[[f32; FEATURES]], scores: &[f32], budget: Duration, fault: Instant) -> Result<(), String> {
        while !self.call(t, rows, scores, budget, fault)? {
            if fault.elapsed() > Duration::from_secs(120) {
                return Err(format!("no successful call within 120 s; {} failed", self.failed));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// For an outage someone else caused, times count from the first failure instead.
    fn offset(&self) -> Duration {
        if self.from_first_failure {
            self.first_failure.unwrap_or_default()
        } else {
            Duration::ZERO
        }
    }

    fn report(&self, what: &str, budget: Duration) -> Result<(), String> {
        let (Some(first), Some(served)) = (self.first_failure, self.served) else {
            return Err(format!("{what}: expected failed calls and then a success; {} failed", self.failed));
        };
        let (first, served) = (first - self.offset(), served - self.offset());
        let mut f = self.failures.clone();
        f.sort();
        let slowest = f[f.len() - 1];
        println!(
            "{what}: {} calls failed, the first {:.1} ms after it, each in {:.2} ms at the median and {:.2} ms at most (budget {:.0} ms); first success {:.1} ms after it",
            self.failed,
            ms(first),
            ms(f[f.len() / 2]),
            ms(slowest),
            ms(budget),
            ms(served)
        );
        println!("    the call that succeeded took {:.2} ms; longest pause between calls {:.2} ms", ms(self.recovered_in), ms(self.longest_pause));
        for e in &self.errors {
            println!("    error: {e}");
        }
        // Tokio's timer fires up to 1 ms after the deadline, and an idle VM wakes late (about 1 ms
        // on Docker Desktop): allow 5 ms past the budget before calling it a hang.
        if slowest > budget + Duration::from_millis(5) {
            return Err(format!("{what}: a failed call took {:.2} ms, past its {budget:?} budget", ms(slowest)));
        }
        Ok(())
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// The pid of tritonserver, in a PID namespace shared with Triton's container.
fn tritonserver() -> Result<i32, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for e in std::fs::read_dir("/proc").map_err(|e| e.to_string())?.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
            if std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim_end() == "tritonserver") {
                return Ok(pid);
            }
        }
        if Instant::now() > deadline {
            return Err("no tritonserver process: run with --pid=container:<Triton's container>".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn signal(pid: i32, sig: i32) -> Result<(), String> {
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid, sig) } != 0 {
        return Err(format!("kill({pid}, {sig}): {}", std::io::Error::last_os_error()));
    }
    Ok(())
}
