//! scz-bench-host: the benchmark's host application (bench §4), over any of the five systems.
//!
//!   scz-bench-host inproc --system=A|B|C|D|E [--variant=NAME] [--scenario=NAME]
//!                  [--rate=50000] [--duration-s=30] [--warmup-s=5] [--threads=8]
//!                  [--arrival=even|poisson] [--budget-us=2000] [--seed=1] --rows=PATH --out=PATH
//!                  [--socket=PATH] [--triton=URL] [--http=ADDR:PORT] [--uds=PATH] [--model=PATH]
//!                  [--no-spin]
//!   scz-bench-host serve --system=… --listen=ADDR:PORT [--budget-us=2000] --rows=PATH [targets]
//!
//! `inproc` is the in-process mode of bench §4.2: an open-loop load loop inside this process,
//! calling the predictor directly. Each of `--threads` caller threads keeps its own schedule
//! (evenly spaced, offset so that together they make the pattern, or Poisson), and latency is
//! measured from the *intended* send time. It writes one JSON result (bench/README.md).
//!
//! `serve` is the host of the service mode (bench §4.1): `GET /score?r=N` scores reference row N
//! and answers it with `X-Source: model` or `fallback` and `X-Reason`; `GET /healthz` always
//! answers 200.

mod systems;

use scz_bench_common::{cpu_ns, now_ns, read_rows, splitmix, wait_until, Outcome, Record, Row, REASONS};
use std::alloc::{GlobalAlloc, Layout, System as Sys};
use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;
use systems::{caller, shared, System, Targets};

// ── allocation, counted only inside calls ─────────────────────────────────────────────────

struct Counting;
thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|c| {
            if c.get() {
                BYTES.with(|b| b.set(b.get() + l.size() as u64));
            }
        });
        // SAFETY: the caller's contract is ours.
        unsafe { Sys.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract is ours.
        unsafe { Sys.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let _ = COUNTING.try_with(|c| {
            if c.get() {
                BYTES.with(|b| b.set(b.get() + n as u64));
            }
        });
        // SAFETY: the caller's contract is ours.
        unsafe { Sys.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

extern "C" fn on_term(_: libc::c_int) {
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(0) };
}

struct Opts {
    mode: String,
    system: System,
    variant: String,
    scenario: String,
    rate: f64,
    duration: Duration,
    warmup: Duration,
    threads: usize,
    poisson: bool,
    budget: Duration,
    seed: u64,
    rows: String,
    out: String,
    listen: String,
    t: Targets,
}

fn opts() -> Result<Opts, String> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("usage: scz-bench-host inproc|serve --system=… (see the source)")?;
    let mut o = Opts {
        mode,
        system: System::A,
        variant: "default".into(),
        scenario: "steady".into(),
        rate: 50_000.0,
        duration: Duration::from_secs(30),
        warmup: Duration::from_secs(5),
        threads: 8,
        poisson: false,
        budget: Duration::from_micros(2000),
        seed: 1,
        rows: String::new(),
        out: String::new(),
        listen: "0.0.0.0:8080".into(),
        t: Targets::default(),
    };
    for a in args {
        let (k, v) = a.split_once('=').unwrap_or((&a, ""));
        let num = |v: &str| v.parse::<f64>().map_err(|_| format!("{k}: not a number: {v}"));
        match k {
            "--system" => o.system = System::parse(v).ok_or(format!("--system: {v}"))?,
            "--variant" => o.variant = v.into(),
            "--scenario" => o.scenario = v.into(),
            "--rate" => o.rate = num(v)?,
            "--duration-s" => o.duration = Duration::from_secs_f64(num(v)?),
            "--warmup-s" => o.warmup = Duration::from_secs_f64(num(v)?),
            "--threads" => o.threads = num(v)? as usize,
            "--arrival" => o.poisson = v == "poisson",
            "--budget-us" => o.budget = Duration::from_micros(num(v)? as u64),
            "--seed" => o.seed = num(v)? as u64,
            "--rows" => o.rows = v.into(),
            "--out" => o.out = v.into(),
            "--listen" => o.listen = v.into(),
            "--socket" => o.t.socket = v.into(),
            "--triton" => o.t.triton = v.into(),
            "--http" => o.t.http = v.into(),
            "--uds" => o.t.uds = v.into(),
            "--model" => o.t.model = v.into(),
            "--no-spin" => o.t.no_spin = true,
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    Ok(o)
}

/// One thread's share of the run.
struct ThreadOut {
    rec: Record,
    bytes: u64,
    calls: u64,
    /// Changes between model answers and fallbacks: (time, to_model, calls in the run before it,
    /// the reason the run of fallbacks started with).
    transitions: Vec<(u64, bool, u64, u8)>,
}

fn inproc(o: Opts) -> Result<(), String> {
    let rows = Arc::new(read_rows(&o.rows)?);
    let sh = Arc::new(shared(o.system, &o.t)?);
    let t0 = now_ns() + 200_000_000; // every thread has started by then
    let measure_from = t0 + o.warmup.as_nanos() as u64;
    let end = measure_from + o.duration.as_nanos() as u64;
    let period = (o.threads as f64 * 1e9 / o.rate) as u64; // each thread's mean gap
    let cpu0 = cpu_ns();
    let handles: Vec<_> = (0..o.threads)
        .map(|i| {
            let (rows, sh, t, sys) = (rows.clone(), sh.clone(), o.t.clone(), o.system);
            let (budget, poisson, threads, rate, mut seed) = (o.budget, o.poisson, o.threads as u64, o.rate, o.seed ^ (i as u64 * 0x9E37));
            std::thread::spawn(move || {
                let mut c = caller(sys, &t, &sh, budget, i);
                let mut out = ThreadOut { rec: Record::default(), bytes: 0, calls: 0, transitions: Vec::new() };
                // Evenly spaced: thread i takes slots i, i + T, i + 2T, … of the whole schedule.
                let mut next = t0 + (i as f64 * 1e9 / rate) as u64;
                let (mut k, mut last_model, mut run, mut run_reason) = (0u64, true, 0u64, 0u8);
                while next < end {
                    let row: &Row = &rows[(k as usize * threads as usize + i) % rows.len()];
                    wait_until(next);
                    let sent = now_ns();
                    COUNTING.with(|f| f.set(true));
                    let called = c.call(row, budget);
                    COUNTING.with(|f| f.set(false));
                    let t1 = now_ns();
                    std::hint::black_box(called.value);
                    if next >= measure_from {
                        out.rec.add(called.outcome, t1 - next, called.entry_ns, next - measure_from);
                        out.rec.send_lag.record(sent - next);
                        out.calls += 1;
                        let model = called.outcome == Outcome::Model;
                        if model != last_model {
                            let reason = if let Outcome::Fallback(r) = called.outcome { r } else { run_reason };
                            out.transitions.push((t1, model, run, run_reason));
                            run = 0;
                            run_reason = reason;
                            last_model = model;
                        }
                        run += 1;
                    }
                    k += 1;
                    next = if poisson {
                        // Exponential gaps with mean `period`, from a uniform in (0, 1].
                        let u = ((splitmix(&mut seed) >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
                        next + (-u.ln() * period as f64) as u64
                    } else {
                        t0 + ((k * threads + i as u64) as f64 * 1e9 / rate) as u64
                    };
                }
                out.bytes = BYTES.with(|b| b.get());
                out
            })
        })
        .collect();
    let (mut rec, mut bytes, mut calls, mut transitions) = (Record::default(), 0u64, 0u64, Vec::new());
    for (i, h) in handles.into_iter().enumerate() {
        let t = h.join().map_err(|_| "a caller thread panicked".to_string())?;
        rec.merge(&t.rec);
        bytes += t.bytes;
        calls += t.calls;
        transitions.extend(t.transitions.into_iter().map(|x| (x.0, i, x.1, x.2, x.3)));
    }
    let cpu = cpu_ns() - cpu0;
    transitions.sort();
    let tr: Vec<String> = transitions.iter().map(|t| format!("[{},{},{},{},\"{}\"]", t.0, t.1, u8::from(t.2), t.3, REASONS[t.4 as usize])).collect();
    let json = format!(
        "{{\"host\":\"rust\",\"mode\":\"inproc\",\"system\":\"{:?}\",\"variant\":\"{}\",\"scenario\":\"{}\",\"rate\":{},\"arrival\":\"{}\",\"threads\":{},\"duration_s\":{},\"warmup_s\":{},\"budget_us\":{},\"start_ns\":{measure_from},\"end_ns\":{end},\"calls\":{calls},\"cpu_ns\":{cpu},\"alloc_bytes_per_call\":{:.3},\"record\":{},\"transitions\":[{}]}}\n",
        o.system,
        o.variant,
        o.scenario,
        o.rate,
        if o.poisson { "poisson" } else { "even" },
        o.threads,
        o.duration.as_secs_f64(),
        o.warmup.as_secs_f64(),
        o.budget.as_micros(),
        bytes as f64 / calls.max(1) as f64,
        rec.json(),
        tr.join(",")
    );
    std::fs::write(&o.out, json).map_err(|e| format!("{}: {e}", o.out))?;
    let p = |q| rec.latency.quantile(q) as f64 / 1e3;
    eprintln!(
        "scz-bench-host: {:?}/{} {}: {calls} calls, model {}, fallback {}, error {}; p50 {:.1} µs, p99 {:.1} µs, p99.9 {:.1} µs; send lag p99 {:.1} µs; {:.2} B/call",
        o.system,
        o.variant,
        o.scenario,
        rec.model,
        rec.fallback.iter().sum::<u64>(),
        rec.error + rec.timeout,
        p(0.5),
        p(0.99),
        p(0.999),
        rec.send_lag.quantile(0.99) as f64 / 1e3,
        bytes as f64 / calls.max(1) as f64
    );
    Ok(())
}

fn serve(o: Opts) -> Result<(), String> {
    let rows = Arc::new(read_rows(&o.rows)?);
    let sh = Arc::new(shared(o.system, &o.t)?);
    let l = std::net::TcpListener::bind(&o.listen).map_err(|e| format!("{}: {e}", o.listen))?;
    eprintln!("scz-bench-host: {:?} on {}", o.system, o.listen);
    for (n, s) in l.incoming().flatten().enumerate() {
        let (rows, sh, t, sys, budget) = (rows.clone(), sh.clone(), o.t.clone(), o.system, o.budget);
        std::thread::spawn(move || {
            let _ = s.set_nodelay(true);
            let mut c = caller(sys, &t, &sh, budget, n);
            let mut w = s.try_clone().expect("a second handle on the socket");
            let mut r = BufReader::new(s);
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                loop {
                    line.clear();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let reply = if let Some(q) = path.strip_prefix("/score?r=") {
                    let row = &rows[q.parse::<usize>().unwrap_or(0) % rows.len()];
                    let called = c.call(row, budget);
                    let (src, reason) = match called.outcome {
                        Outcome::Model => ("model", ""),
                        Outcome::Fallback(r) => ("fallback", REASONS[r as usize]),
                        _ => ("fallback", "OTHER"),
                    };
                    let body = format!("{}", called.value);
                    format!("HTTP/1.1 200 OK\r\nX-Source: {src}\r\nX-Reason: {reason}\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                } else if path == "/healthz" {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_string()
                } else {
                    "HTTP/1.1 404 X\r\nContent-Length: 0\r\n\r\n".to_string()
                };
                if w.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
        });
    }
    Ok(())
}

fn main() {
    // SAFETY: handlers that only call _exit (a container's PID 1 gets no default action).
    unsafe {
        libc::signal(libc::SIGTERM, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    let r = opts().and_then(|o| match o.mode.as_str() {
        "inproc" => inproc(o),
        "serve" => serve(o),
        m => Err(format!("unknown mode {m}: inproc or serve")),
    });
    if let Err(e) = r {
        eprintln!("scz-bench-host: {e}");
        std::process::exit(1);
    }
}
