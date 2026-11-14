//! Metrics and hooks (sdk-contract §8): the sampled histograms, the sink every metric is
//! exported through, and a rendering of them in the Prometheus text format that needs no
//! dependency.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Upper bounds of the histogram buckets, in seconds (sdk-contract §8); a last bucket, +Inf,
/// follows them.
pub const BUCKETS_SECONDS: [f64; 16] = [1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4, 2e-4, 5e-4, 1e-3, 2e-3, 5e-3, 1e-2, 2e-2, 5e-2, 0.1];
const BOUNDS_NS: [u64; 16] =
    [1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000, 20_000_000, 50_000_000, 100_000_000];
/// One call in this many, per calling thread, is timed (sdk-contract §8).
pub const DEFAULT_SAMPLE_EVERY: u32 = 64;
pub(crate) const HIST_SHARDS: usize = 16;

/// One histogram's buckets, summed into by any thread (sampled calls only, so rarely).
#[derive(Default)]
pub(crate) struct Hist {
    buckets: [AtomicU64; 17],
    sum_ns: AtomicU64,
}

impl Hist {
    pub(crate) fn record(&self, ns: u64) {
        let i = BOUNDS_NS.iter().position(|&b| ns <= b).unwrap_or(BOUNDS_NS.len());
        self.buckets[i].fetch_add(1, Relaxed);
        self.sum_ns.fetch_add(ns, Relaxed);
    }
}

/// The three sampled histograms, in one of [`HIST_SHARDS`] shards chosen by the thread.
#[repr(align(64))]
#[derive(Default)]
pub(crate) struct HistShard {
    /// End-to-end `infer` time, by source.
    pub model: Hist,
    pub fallback: Hist,
    /// The fallback function alone.
    pub fallback_fn: Hist,
}

/// A histogram as read: counts per bucket, cumulative as the Prometheus format has them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Histogram {
    /// `cumulative[i]`: durations of at most `BUCKETS_SECONDS[i]`; the last, every duration.
    pub cumulative: [u64; 17],
    pub sum_seconds: f64,
    pub count: u64,
}

impl Histogram {
    pub(crate) fn sum_of<'a>(hs: impl Iterator<Item = &'a Hist>) -> Histogram {
        let (mut raw, mut sum) = ([0u64; 17], 0u64);
        for h in hs {
            for (r, b) in raw.iter_mut().zip(&h.buckets) {
                *r += b.load(Relaxed);
            }
            sum = sum.wrapping_add(h.sum_ns.load(Relaxed));
        }
        let mut cumulative = [0u64; 17];
        let mut acc = 0;
        for (c, r) in cumulative.iter_mut().zip(raw) {
            acc += r;
            *c = acc;
        }
        Histogram { cumulative, sum_seconds: sum as f64 / 1e9, count: acc }
    }
}

/// Where [`Client::export`](crate::Client::export) writes each metric of sdk-contract §8.
/// Adapters for a metrics library implement it; [`PrometheusText`] is the one in the core.
pub trait MetricsSink {
    fn counter(&mut self, name: &str, labels: &[(&str, &str)], value: u64);
    fn gauge(&mut self, name: &str, labels: &[(&str, &str)], value: f64);
    /// `bounds` are [`BUCKETS_SECONDS`]; `h.cumulative` has one more entry, for +Inf.
    fn histogram(&mut self, name: &str, labels: &[(&str, &str)], bounds: &[f64], h: &Histogram);
}

/// The Prometheus text exposition format, for a service that serves `/metrics` itself.
#[derive(Default)]
pub struct PrometheusText {
    out: String,
    last: String,
}

impl PrometheusText {
    pub fn new() -> PrometheusText {
        PrometheusText::default()
    }
    pub fn finish(self) -> String {
        self.out
    }
    fn kind(&mut self, name: &str, kind: &str) {
        if self.last != name {
            let _ = writeln!(self.out, "# TYPE {name} {kind}");
            self.last = name.to_string();
        }
    }
}

fn label_set(labels: &[(&str, &str)], extra: Option<(&str, &str)>) -> String {
    let all: Vec<String> =
        labels.iter().chain(extra.as_ref()).map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))).collect();
    if all.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", all.join(","))
    }
}

impl MetricsSink for PrometheusText {
    fn counter(&mut self, name: &str, labels: &[(&str, &str)], value: u64) {
        self.kind(name, "counter");
        let _ = writeln!(self.out, "{name}{} {value}", label_set(labels, None));
    }
    fn gauge(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.kind(name, "gauge");
        let _ = writeln!(self.out, "{name}{} {value}", label_set(labels, None));
    }
    fn histogram(&mut self, name: &str, labels: &[(&str, &str)], bounds: &[f64], h: &Histogram) {
        self.kind(name, "histogram");
        for (i, c) in h.cumulative.iter().enumerate() {
            let le = bounds.get(i).map_or("+Inf".to_string(), |b| format!("{b}"));
            let _ = writeln!(self.out, "{name}_bucket{} {c}", label_set(labels, Some(("le", &le))));
        }
        let l = label_set(labels, None);
        let _ = writeln!(self.out, "{name}_sum{l} {}", h.sum_seconds);
        let _ = writeln!(self.out, "{name}_count{l} {}", h.count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_lands_in_the_first_bucket_at_least_as_large() {
        let h = Hist::default();
        for ns in [0, 1_000, 1_001, 100_000_000, 100_000_001, u64::MAX / 4] {
            h.record(ns);
        }
        let r = Histogram::sum_of([&h].into_iter());
        assert_eq!(r.cumulative[0], 2, "0 and exactly 1 µs");
        assert_eq!(r.cumulative[1], 3, "1.001 µs");
        assert_eq!(r.cumulative[15], 4, "exactly 100 ms");
        assert_eq!((r.cumulative[16], r.count), (6, 6), "+Inf holds the rest");
    }

    #[test]
    fn the_text_format_names_each_family_once() {
        let mut t = PrometheusText::new();
        t.counter("a_total", &[("source", "fallback"), ("reason", "x\"y")], 3);
        t.counter("a_total", &[("source", "model")], 4);
        t.gauge("b", &[], 1.5);
        let mut h = Histogram { count: 2, sum_seconds: 0.25, ..Default::default() };
        h.cumulative[16] = 2;
        t.histogram("c_seconds", &[], &BUCKETS_SECONDS, &h);
        let s = t.finish();
        assert_eq!(s.matches("# TYPE a_total counter").count(), 1);
        assert!(s.contains("a_total{source=\"fallback\",reason=\"x\\\"y\"} 3\n"), "{s}");
        assert!(s.contains("b 1.5\n") && s.contains("c_seconds_bucket{le=\"0.000001\"} 0\n") && s.contains("c_seconds_bucket{le=\"+Inf\"} 2\n"), "{s}");
        assert!(s.contains("c_seconds_sum 0.25\nc_seconds_count 2\n"), "{s}");
    }
}
