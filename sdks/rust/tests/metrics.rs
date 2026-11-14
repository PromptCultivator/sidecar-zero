//! sdk-contract §8: the sampled histograms, the export and its text format, the hook.

mod common;

use common::*;
use sidecarzero::{Client, Endpoint, F32Echo, FallbackReason as R, Options};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn every(n: u32) -> Options {
    Options { sample_every: n, ..Options::default() }
}

#[test]
fn sampled_calls_are_timed_and_exported() {
    let sc = Sidecar::start("scz-worker-echo", &[], &[]);
    let c = Client::builder(sc.endpoint(), F32Echo::<4>).fallback(|x: &[f32; 4], _| *x).options(every(4)).build();
    assert!(c.await_bound(Duration::from_secs(10)));
    // On a thread of its own, so that this thread's sample count starts at zero.
    let c = Arc::new(c);
    let c2 = c.clone();
    std::thread::spawn(move || {
        for _ in 0..400 {
            assert!(c2.infer(&[1.0; 4], Duration::from_millis(50)).is_model());
        }
        // A budget above the sidecar's 100 ms maximum is clamped, and counted.
        assert!(c2.infer(&[1.0; 4], Duration::from_secs(1)).is_model());
    })
    .join()
    .unwrap();
    let s = c.stats();
    assert_eq!(s.model, 401);
    assert_eq!(s.latency_model.count, 101, "the first call, then one in four");
    assert!(s.latency_model.sum_seconds > 0.0 && s.latency_model.cumulative[16] == 101);
    assert_eq!(s.latency_fallback.count, 0);
    assert_eq!(s.clamped, 1);
    assert!(s.model_gen >= 1);
    let text = c.prometheus_text();
    for line in [
        "scz_client_calls_total{source=\"model\"} 401\n",
        "scz_client_calls_total{source=\"fallback\",reason=\"DEADLINE\"} 0\n",
        "scz_client_latency_seconds_count{source=\"model\"} 101\n",
        "scz_client_latency_seconds_bucket{source=\"model\",le=\"+Inf\"} 101\n",
        "scz_client_budget_clamped_total 1\n",
        "scz_client_reconnects_total 0\n",
        "scz_client_state 2\n",
        "# TYPE scz_client_breaker_open gauge\n",
        "# TYPE scz_client_fallback_seconds histogram\n",
    ] {
        assert!(text.contains(line), "missing {line:?} in\n{text}");
    }
    assert_eq!(text.matches("scz_client_calls_total{").count(), 1 + R::ALL.len(), "every reason, zeros included");
}

#[test]
fn the_hook_sees_every_fallback_and_the_fallback_is_timed() {
    // Nothing listens there: every call falls back with NOT_CONNECTED.
    let seen = Arc::new(AtomicU64::new(0));
    let s2 = seen.clone();
    let c = Client::builder(Endpoint::parse("/nonexistent/scz.sock"), F32Echo::<4>)
        .on_fallback(move |r| {
            assert_eq!(r, R::NotConnected);
            s2.fetch_add(1, Ordering::Relaxed);
        })
        .fallback(|x: &[f32; 4], _| {
            std::thread::sleep(Duration::from_micros(200));
            *x
        })
        .options(every(1))
        .build();
    let c = Arc::new(c);
    let c2 = c.clone();
    std::thread::spawn(move || {
        for _ in 0..20 {
            assert!(!c2.infer(&[1.0; 4], Duration::from_millis(1)).is_model());
        }
    })
    .join()
    .unwrap();
    assert_eq!(seen.load(Ordering::Relaxed), 20);
    let s = c.stats();
    assert_eq!((s.fallbacks(R::NotConnected), s.fallback_time.count, s.latency_fallback.count), (20, 20, 20));
    assert!(s.fallback_time.sum_seconds >= 20.0 * 200e-6, "{:?}", s.fallback_time);
    assert!(s.latency_fallback.sum_seconds >= s.fallback_time.sum_seconds);
    assert_eq!(s.fallback_time.cumulative[7], 0, "nothing at or under 200 µs");
}

#[test]
fn sampling_can_be_turned_off() {
    let c = Client::builder(Endpoint::parse("/nonexistent/scz.sock"), F32Echo::<4>).fallback(|x: &[f32; 4], _| *x).options(every(0)).build();
    for _ in 0..100 {
        c.infer(&[1.0; 4], Duration::from_millis(1));
    }
    let s = c.stats();
    assert_eq!((s.fallbacks(R::NotConnected), s.latency_fallback.count, s.fallback_time.count), (100, 0, 0));
}
