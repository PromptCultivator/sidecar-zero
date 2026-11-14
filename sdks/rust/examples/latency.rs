//! First latency numbers (phase 1 exit criterion): the Rust client against the echo worker.
//!
//!   latency --socket=PATH [--calls=200000] [--threads=1] [--budget-us=2000]
//!
//! Measures `infer` from entry to return, for three payload sizes, plus the fallback path
//! with no sidecar. Counts heap allocations made during the measured calls, which the SDK
//! contract requires to be zero (sdk-contract §5).

use sidecarzero::{Client, Codec, Endpoint, F32Echo, Fallback, Source};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

/// Counts allocations made by the measuring thread itself, between `start` and `stop`.
struct Counting;
thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let _ = ON.try_with(|on| {
            if on.get() {
                ALLOCS.with(|a| a.set(a.get() + 1));
            }
        });
        // SAFETY: same contract as ours.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: same contract as ours.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

struct Opts {
    socket: String,
    calls: usize,
    threads: usize,
    budget: Duration,
}

fn opts() -> Opts {
    let mut o = Opts { socket: String::new(), calls: 200_000, threads: 1, budget: Duration::from_micros(2000) };
    for a in std::env::args().skip(1) {
        let (k, v) = a.split_once('=').unwrap_or((&a, ""));
        match k {
            "--socket" => o.socket = v.into(),
            "--calls" => o.calls = v.parse().unwrap(),
            "--threads" => o.threads = v.parse().unwrap(),
            "--budget-us" => o.budget = Duration::from_micros(v.parse().unwrap()),
            _ => panic!("unknown argument {a}"),
        }
    }
    assert!(!o.socket.is_empty(), "--socket is required");
    o
}

fn pct(sorted: &[u32], p: f64) -> f64 {
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)] as f64 / 1000.0
}

fn report(name: &str, mut ns: Vec<u32>, model: usize, allocs: u64) {
    ns.sort_unstable();
    println!(
        "{name:<28} {:>9} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>9.1} {:>7.2}% {:>6}",
        ns.len(),
        pct(&ns, 0.50),
        pct(&ns, 0.90),
        pct(&ns, 0.99),
        pct(&ns, 0.999),
        *ns.last().unwrap() as f64 / 1000.0,
        100.0 * model as f64 / ns.len() as f64,
        allocs
    );
}

/// Run `calls` calls on each of `threads` threads; returns per-call times and model count.
fn run<C, F>(c: &Client<C, F>, x: &C::In, calls: usize, threads: usize, budget: Duration) -> (Vec<u32>, usize, u64)
where
    C: Codec + Sync,
    F: Fallback<C::In, C::Out> + Sync,
    C::In: Sync,
{
    // Warm up: page in, fill caches, settle the caller entries.
    for _ in 0..calls.min(20_000) {
        std::hint::black_box(c.infer(x, budget));
    }
    let mut all = Vec::with_capacity(calls * threads);
    let mut model = 0;
    let mut allocs = 0;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut ns = vec![0u32; calls];
                    let mut m = 0;
                    ON.with(|on| on.set(true));
                    for slot in ns.iter_mut() {
                        let t = Instant::now();
                        let out = c.infer(x, budget);
                        *slot = t.elapsed().as_nanos().min(u32::MAX as u128) as u32;
                        if matches!(out.source, Source::Model { .. }) {
                            m += 1;
                        }
                        std::hint::black_box(out.value);
                    }
                    ON.with(|on| on.set(false));
                    let a = ALLOCS.with(|a| a.get());
                    (ns, m, a)
                })
            })
            .collect();
        for h in hs {
            let (ns, m, a) = h.join().unwrap();
            all.extend(ns);
            model += m;
            allocs += a;
        }
    });
    (all, model, allocs)
}

fn bench<const N: usize>(o: &Opts) {
    let c = Client::builder(Endpoint::parse(&o.socket), F32Echo::<N>).fallback(|x: &[f32; N], _| *x).build();
    assert!(c.await_bound(Duration::from_secs(5)), "no sidecar at {}", o.socket);
    let x: [f32; N] = core::array::from_fn(|i| i as f32);
    let t = Instant::now();
    while !c.infer(&x, o.budget).is_model() {
        assert!(t.elapsed() < Duration::from_secs(5), "the gate never opened");
    }
    let (ns, model, allocs) = run(&c, &x, o.calls, o.threads, o.budget);
    report(&format!("model, {} B each way", N * 4), ns, model, allocs);
    let st = c.stats();
    if !st.fallback.is_empty() {
        println!("{:<28} fallbacks: {:?}", "", st.fallback);
    }
}

fn main() {
    let o = opts();
    println!("threads={} calls/thread={} budget={:?}; times in microseconds", o.threads, o.calls, o.budget);
    println!("{:<28} {:>9} {:>8} {:>8} {:>8} {:>8} {:>9} {:>8} {:>6}", "path", "calls", "p50", "p90", "p99", "p99.9", "max", "model", "allocs");
    bench::<4>(&o);
    bench::<64>(&o);
    bench::<256>(&o);
    // The fallback path with nothing to connect to: what every call costs during an outage.
    let c = Client::builder(Endpoint::parse("/nonexistent/scz.sock"), F32Echo::<64>).fallback(|x: &[f32; 64], _| *x).build();
    let x = [0.5f32; 64];
    let (ns, model, allocs) = run(&c, &x, o.calls, o.threads, o.budget);
    report("fallback, not connected", ns, model, allocs);
}
