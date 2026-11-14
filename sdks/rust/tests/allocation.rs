//! The allocation gate (testing §7, sdk-contract §5): zero heap allocations per call on every
//! path the gate lists that this SDK has, against a real supervisor. A counting allocator
//! counts the allocations a thread makes while its flag is set, which is only inside `infer`.
//!
//! Paths: a model answer taken while spinning; one taken after parking; a fallback at
//! admission, with no arena and with one (a stale heartbeat); a shed call; an abandon at the
//! deadline; and the first call from a thread that has never called. The Rust SDK has no
//! polling wait.

mod common;

use common::Sidecar;
use sidecarzero::{BytesEcho, Client, Codec, CodecError, Endpoint, FallbackReason as R, SlotReader, SlotWriter, Source};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Duration;

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
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let _ = ON.try_with(|on| {
            if on.get() {
                ALLOCS.with(|a| a.set(a.get() + 1));
            }
        });
        // SAFETY: same contract as ours.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Run `n` calls, counting only what `infer` allocates; returns (allocations, outcomes).
fn measure<C: Codec, F: sidecarzero::Fallback<C::In, C::Out>>(c: &Client<C, F>, input: &C::In, budget: Duration, n: usize) -> (u64, BTreeMap<String, u64>) {
    let mut seen = [0u64; 32];
    let mut model = 0u64;
    let before = ALLOCS.with(|a| a.get());
    for _ in 0..n {
        ON.with(|o| o.set(true));
        let o = c.infer(input, budget);
        ON.with(|o| o.set(false));
        match o.source {
            Source::Model { .. } => model += 1,
            Source::Fallback { reason } => seen[R::ALL.iter().position(|r| *r == reason).unwrap()] += 1,
        }
    }
    let allocs = ALLOCS.with(|a| a.get()) - before;
    let mut m: BTreeMap<String, u64> = R::ALL.iter().zip(seen).filter(|x| x.1 > 0).map(|(r, k)| (r.as_str().to_string(), k)).collect();
    if model > 0 {
        m.insert("MODEL".into(), model);
    }
    (allocs, m)
}

/// The chaos worker's request: magic, opcode, argument, nonce, seed (conformance/proto).
fn chaos(op: u64, arg: u64) -> [u8; 40] {
    let mut b = [0u8; 40];
    for (i, w) in [u64::from_le_bytes(*b"SCZCHAOS"), op, arg, 0, 1].iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    b
}

/// The chaos worker's 8-byte answer, or 0.
struct Keyed;

impl Codec for Keyed {
    type In = [u8; 40];
    type Out = u64;
    fn request_len(&self, _: &[u8; 40]) -> usize {
        40
    }
    fn encode(&self, input: &[u8; 40], w: &mut SlotWriter<'_>) -> Result<(), CodecError> {
        Ok(w.write(input)?)
    }
    fn decode(&self, r: &SlotReader<'_>) -> Result<u64, CodecError> {
        let mut b = [0u8; 8];
        r.copy_to(0, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

#[test]
fn no_path_through_infer_allocates() {
    // ── model answers, spinning and parked, from the echo worker
    let sc = Sidecar::start("scz-worker-echo", &[], &[]);
    let c = Client::builder(sc.endpoint(), BytesEcho::<64>).fallback(|x: &[u8; 64], _| *x).build();
    assert!(c.await_bound(Duration::from_secs(10)), "not bound: {}", sc.log());
    let input = [7u8; 64];
    measure(&c, &input, Duration::from_millis(20), 2_000); // warm up
    let (a, seen) = measure(&c, &input, Duration::from_millis(20), 100_000);
    assert_eq!(seen.get("MODEL"), Some(&100_000), "spinning: {seen:?}");
    assert_eq!(a, 0, "a model answer while spinning allocated");
    c.set_spin_ceiling(Some(Duration::ZERO));
    let (a, seen) = measure(&c, &input, Duration::from_millis(20), 20_000);
    assert_eq!(seen.get("MODEL"), Some(&20_000), "parked: {seen:?}");
    assert_eq!(a, 0, "a model answer after parking allocated");
    c.set_spin_ceiling(None);

    // ── the first call from threads that have never called
    for _ in 0..8 {
        let (a, seen) = std::thread::scope(|s| s.spawn(|| measure(&c, &input, Duration::from_millis(20), 1)).join().unwrap());
        assert_eq!(seen.get("MODEL"), Some(&1));
        assert_eq!(a, 0, "the first call from a new thread allocated");
    }

    // ── a fallback at admission with an arena: the supervisor stopped, its heartbeat stale
    sc.signal("STOP");
    std::thread::sleep(Duration::from_millis(250));
    let (a, seen) = measure(&c, &input, Duration::from_millis(1), 100_000);
    sc.signal("CONT");
    assert_eq!(seen.get("STALE_HEARTBEAT"), Some(&100_000), "{seen:?}");
    assert_eq!(a, 0, "a fallback at admission allocated");
    drop(c);
    drop(sc);

    // ── a fallback with no arena at all
    let none = Client::builder(Endpoint::parse("/nonexistent/scz.sock"), BytesEcho::<64>).fallback(|x: &[u8; 64], _| *x).build();
    let (a, seen) = measure(&none, &input, Duration::from_millis(1), 100_000);
    assert_eq!(seen.get("NOT_CONNECTED"), Some(&100_000), "{seen:?}");
    assert_eq!(a, 0, "a NOT_CONNECTED fallback allocated");
    drop(none);

    // ── abandons at the deadline and shed calls: a 3 ms model against a 1 ms budget
    let sc = Sidecar::start("scz-worker-chaos", &[], &[]);
    let c = Client::builder(sc.endpoint(), Keyed).fallback(|_: &[u8; 40], _| 0u64).build();
    assert!(c.await_bound(Duration::from_secs(10)), "not bound: {}", sc.log());
    let normal = chaos(0, 0);
    measure(&c, &normal, Duration::from_millis(20), 2_000);
    let slow = chaos(1, 3_000);
    let (a, seen) = measure(&c, &slow, Duration::from_millis(1), 2_000);
    assert!(seen.contains_key("DEADLINE") && seen.contains_key("OVERLOADED"), "{seen:?}");
    assert_eq!(a, 0, "an abandon or a shed call allocated ({seen:?})");
}
