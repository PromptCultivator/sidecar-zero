//! What the benchmark's programs share (bench §2, §5): the model scorer every baseline serves
//! with, the reference fallback every host uses, latency histograms, the reference inputs, and
//! the clock.

#[cfg(feature = "ort")]
use scz_worker_ort::{signature, Options, Session, Shape};

/// `CLOCK_MONOTONIC` in nanoseconds: the clock the supervisor, the kill agent, and every
/// host use, so their timestamps can be compared.
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec to write.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub const FEATURES: usize = 256;
pub type Row = [f32; FEATURES];

/// The reference fallback (bench §2, overview §1.3): a linear rule over the 256 features, the
/// same weights in every host language: `w[i] = ((i * 37) mod 17 − 8) / 1000`, no bias.
#[inline]
pub fn fallback(x: &Row) -> f32 {
    let mut s = 0.0f32;
    for (i, v) in x.iter().enumerate() {
        s += (((i * 37) % 17) as f32 - 8.0) / 1000.0 * v;
    }
    s
}

/// The reference rows of models/gen.py (`reference-features.f32`: rows of 256 little-endian
/// float32).
pub fn read_rows(path: &str) -> Result<Vec<Row>, String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if b.is_empty() || b.len() % (FEATURES * 4) != 0 {
        return Err(format!("{path}: not rows of {FEATURES} float32"));
    }
    Ok(b.as_chunks::<{ FEATURES * 4 }>()
        .0
        .iter()
        .map(|row| {
            let mut r = [0f32; FEATURES];
            for (j, c) in row.as_chunks::<4>().0.iter().enumerate() {
                r[j] = f32::from_le_bytes(*c);
            }
            r
        })
        .collect())
}

#[cfg(feature = "ort")]
/// The fraud model under ONNX Runtime, with the settings SidecarZero's worker uses (one
/// intra-op and one inter-op thread, memory profile A): what systems C, D, and E run, so that
/// differences between systems are not differences in model execution (bench §2).
pub struct Scorer {
    session: Session,
    input: Shape,
    output: Shape,
}

#[cfg(feature = "ort")]
impl Scorer {
    pub fn open(path: &str) -> Result<Scorer, String> {
        let bytes: &'static [u8] = Box::leak(std::fs::read(path).map_err(|e| format!("{path}: {e}"))?.into_boxed_slice());
        scz_worker_ort::tune_allocator();
        let session = Session::open(bytes, &Options::default())?;
        let (_, ins, outs) = signature(&session)?;
        let (Some(input), Some(output)) = (ins.into_iter().next(), outs.into_iter().next()) else {
            return Err("the model needs one input and one output".into());
        };
        Ok(Scorer { session, input, output })
    }

    /// One row's score. Safe to call from several threads at once.
    pub fn score(&self, x: &Row) -> Result<f32, String> {
        let mut out = [0f32; 1];
        let i = self.input.tensor(x.as_ptr() as *mut u8, 1);
        let o = self.output.tensor(out.as_mut_ptr() as *mut u8, 1);
        self.session.run(&[i], &[o], false).map_err(|e| format!("{e:?}"))?;
        Ok(out[0])
    }
}

/// A latency histogram: exact below 128 ns, then 64 sub-buckets per power of two (at most
/// 1.6% error), up to 2^40 ns. Recording allocates nothing.
#[derive(Clone)]
pub struct Hist {
    counts: Vec<u64>,
    pub count: u64,
    pub min: u64,
    pub max: u64,
}

const EXACT: u64 = 128;
const SUB: u64 = 64;
const BUCKETS: usize = (EXACT + (40 - 7 + 1) * SUB) as usize;

impl Default for Hist {
    fn default() -> Self {
        Hist { counts: vec![0; BUCKETS], count: 0, min: u64::MAX, max: 0 }
    }
}

impl Hist {
    fn index(v: u64) -> usize {
        if v < EXACT {
            return v as usize;
        }
        let e = (63 - v.leading_zeros() as u64).min(40);
        let m = ((v >> (e - 6)) - SUB).min(SUB - 1);
        (EXACT + (e - 7) * SUB + m) as usize
    }
    fn lower(i: usize) -> u64 {
        let i = i as u64;
        if i < EXACT {
            return i;
        }
        let (e, m) = (7 + (i - EXACT) / SUB, (i - EXACT) % SUB);
        (SUB + m) << (e - 6)
    }
    #[inline]
    pub fn record(&mut self, v: u64) {
        self.counts[Self::index(v)] += 1;
        self.count += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }
    pub fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        self.count += o.count;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }
    /// The value at quantile `q` (0..=1): the lower edge of its bucket, or the maximum.
    pub fn quantile(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Self::lower(i).min(self.max).max(self.min);
            }
        }
        self.max
    }
    pub fn json(&self) -> String {
        let b: Vec<String> = self.counts.iter().enumerate().filter(|(_, &c)| c > 0).map(|(i, c)| format!("[{},{}]", Self::lower(i), c)).collect();
        format!(
            "{{\"count\":{},\"min\":{},\"max\":{},\"p50\":{},\"p90\":{},\"p99\":{},\"p999\":{},\"buckets\":[{}]}}",
            self.count,
            if self.count == 0 { 0 } else { self.min },
            self.max,
            self.quantile(0.5),
            self.quantile(0.9),
            self.quantile(0.99),
            self.quantile(0.999),
            b.join(",")
        )
    }
}

/// What a call ended as (bench §5, "Outcome").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Model,
    /// The fallback answered; the reason's index into `REASONS`.
    Fallback(u8),
    Error,
    Timeout,
}

/// Fallback reasons: SidecarZero's (sdk-contract §3.3), then the baselines' wrapper's.
pub const REASONS: [&str; 24] = [
    "NOT_CONNECTED",
    "ABI_MISMATCH",
    "SCHEMA_MISMATCH",
    "REBINDING",
    "GATE_INIT",
    "GATE_SHEDDING",
    "GATE_RELOADING",
    "GATE_DEAD",
    "GATE_DRAINING",
    "STALE_HEARTBEAT",
    "CLOCK_FAULT",
    "OVERLOADED",
    "TOO_LARGE",
    "NO_SLOT",
    "CODEC_ERROR",
    "RING_FULL",
    "DEADLINE",
    "WORKER_FAILED",
    "MODEL_ERROR",
    "BAD_REQUEST",
    "RESOURCE_EXHAUSTED",
    // A baseline call that failed (connection refused or reset, a server error) or took longer
    // than its budget: the wrapper of bench §3 fairness rule 1 calls the fallback.
    "TRANSPORT_ERROR",
    "TRANSPORT_TIMEOUT",
    "OTHER",
];

pub fn reason_index(name: &str) -> u8 {
    REASONS.iter().position(|r| *r == name).unwrap_or(REASONS.len() - 1) as u8
}

/// Everything one load loop records, per thread and then merged (bench §5).
#[derive(Clone, Default)]
pub struct Record {
    /// From the intended send time, so stalls are not hidden (coordinated omission).
    pub latency: Hist,
    pub model_latency: Hist,
    pub fallback_latency: Hist,
    /// From call entry to the fallback being entered, for calls that fell back.
    pub fallback_entry: Hist,
    /// The same, for calls that fell back at admission (the gate or the breaker): the
    /// switchover claim of overview §1.3.
    pub admission_entry: Hist,
    /// How late each call was sent: the load generator's own accuracy (bench §8 step 1).
    pub send_lag: Hist,
    pub model: u64,
    pub fallback: [u64; REASONS.len()],
    pub error: u64,
    pub timeout: u64,
    /// Per 100 ms from the start: (model, fallback, error or timeout, maximum latency).
    pub timeline: Vec<(u64, u64, u64, u64)>,
}

/// Admission-phase reasons (sdk-contract §3.3): decided before a slot is claimed.
pub fn is_admission(reason: u8) -> bool {
    matches!(
        REASONS[reason as usize],
        "NOT_CONNECTED"
            | "ABI_MISMATCH"
            | "SCHEMA_MISMATCH"
            | "REBINDING"
            | "GATE_INIT"
            | "GATE_SHEDDING"
            | "GATE_RELOADING"
            | "GATE_DEAD"
            | "GATE_DRAINING"
            | "STALE_HEARTBEAT"
            | "CLOCK_FAULT"
            | "OVERLOADED"
    )
}

impl Record {
    pub fn add(&mut self, out: Outcome, latency_ns: u64, entry_ns: Option<u64>, since_start_ns: u64) {
        self.latency.record(latency_ns);
        let slot = (since_start_ns / 100_000_000) as usize;
        if self.timeline.len() <= slot {
            self.timeline.resize(slot + 1, (0, 0, 0, 0));
        }
        let t = &mut self.timeline[slot];
        t.3 = t.3.max(latency_ns);
        match out {
            Outcome::Model => {
                self.model += 1;
                self.model_latency.record(latency_ns);
                t.0 += 1;
            }
            Outcome::Fallback(r) => {
                self.fallback[r as usize] += 1;
                self.fallback_latency.record(latency_ns);
                if let Some(e) = entry_ns {
                    self.fallback_entry.record(e);
                    if is_admission(r) {
                        self.admission_entry.record(e);
                    }
                }
                t.1 += 1;
            }
            Outcome::Error => {
                self.error += 1;
                t.2 += 1;
            }
            Outcome::Timeout => {
                self.timeout += 1;
                t.2 += 1;
            }
        }
    }
    pub fn merge(&mut self, o: &Record) {
        self.latency.merge(&o.latency);
        self.model_latency.merge(&o.model_latency);
        self.fallback_latency.merge(&o.fallback_latency);
        self.fallback_entry.merge(&o.fallback_entry);
        self.admission_entry.merge(&o.admission_entry);
        self.send_lag.merge(&o.send_lag);
        self.model += o.model;
        for (a, b) in self.fallback.iter_mut().zip(&o.fallback) {
            *a += b;
        }
        self.error += o.error;
        self.timeout += o.timeout;
        if self.timeline.len() < o.timeline.len() {
            self.timeline.resize(o.timeline.len(), (0, 0, 0, 0));
        }
        for (a, b) in self.timeline.iter_mut().zip(&o.timeline) {
            a.0 += b.0;
            a.1 += b.1;
            a.2 += b.2;
            a.3 = a.3.max(b.3);
        }
    }
    /// The record as the JSON object every benchmark program writes.
    pub fn json(&self) -> String {
        let fb: Vec<String> = REASONS.iter().zip(&self.fallback).filter(|(_, &n)| n > 0).map(|(r, n)| format!("\"{r}\":{n}")).collect();
        let tl: Vec<String> = self.timeline.iter().map(|t| format!("[{},{},{},{}]", t.0, t.1, t.2, t.3)).collect();
        format!(
            "{{\"model\":{},\"fallback\":{{{}}},\"error\":{},\"timeout\":{},\"latency_ns\":{},\"model_latency_ns\":{},\"fallback_latency_ns\":{},\"fallback_entry_ns\":{},\"admission_entry_ns\":{},\"send_lag_ns\":{},\"timeline_100ms\":[{}]}}",
            self.model,
            fb.join(","),
            self.error,
            self.timeout,
            self.latency.json(),
            self.model_latency.json(),
            self.fallback_latency.json(),
            self.fallback_entry.json(),
            self.admission_entry.json(),
            self.send_lag.json(),
            tl.join(",")
        )
    }
}

/// Process CPU time (user + system) in nanoseconds, from `getrusage`.
pub fn cpu_ns() -> u64 {
    // SAFETY: a valid rusage to write.
    let mut r: libc::rusage = unsafe { core::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
    let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    (us(r.ru_utime) + us(r.ru_stime)) * 1_000
}

/// A cheap per-thread random number generator, for Poisson arrivals.
pub fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Wait until `t` (monotonic ns): sleep while more than 2.5 ms away, then spin, so a schedule
/// is kept to a few microseconds. A sleep can overshoot by a millisecond (a VM's timer), and an
/// overshoot is a late send, which the latency from the intended time would charge to the
/// system under test.
pub fn wait_until(t: u64) {
    loop {
        let now = now_ns();
        if now >= t {
            return;
        }
        let left = t - now;
        if left > 2_500_000 {
            std::thread::sleep(std::time::Duration::from_nanos(left - 2_000_000));
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_quantiles_are_within_two_percent() {
        let mut h = Hist::default();
        for v in 1..=100_000u64 {
            h.record(v * 10);
        }
        for (q, want) in [(0.5, 500_000.0), (0.9, 900_000.0), (0.99, 990_000.0)] {
            let got = h.quantile(q) as f64;
            assert!((got - want).abs() / want < 0.02, "q{q}: {got} vs {want}");
        }
        assert_eq!(h.max, 1_000_000);
        let mut small = Hist::default();
        small.record(5);
        assert_eq!(small.quantile(0.99), 5);
    }
}
