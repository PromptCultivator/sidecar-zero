//! Loom models of the protocol (testing §2.2). Run with `scripts/dev loom`, which builds with
//! `RUSTFLAGS="--cfg loom"`.
//!
//! Each model runs the crate's own protocol functions, not a copy, over atomics loom owns. A
//! lost wake-up shows as a deadlock, which loom reports: no model has a clock, so a waiter
//! that nobody wakes waits for ever.
//!
//! Store-then-load pairs are written as a release store, a SeqCst fence, and an acquire load,
//! because loom checks SeqCst accesses as if they were AcqRel and would reject the pure form
//! (spike S7).
#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;
use scz_shm::abi::{gate_state as gs, slot_state as ss};
use scz_shm::ring::{self, Descriptor, EnqueueError, LocalRing};
use scz_shm::slot::{self, Abandon, Claim, LocalSlot, Wait, WaitSpec};
use scz_shm::sync::{full_fence, AtomicBool, AtomicU64, Ordering::*};
use scz_shm::word::{gate, owner, slot as word, state};

fn d(id: u64) -> Descriptor {
    Descriptor { request_id: id, slot_index: id as u32, deadline_ns: u64::MAX, slot_epoch: 1, slab_class: 0, enqueue_ns: 0, flags: 0, route: 0 }
}

/// Drain `n` descriptors, parking whenever the ring is empty.
fn consume(r: &LocalRing, n: usize) -> Vec<u64> {
    let mut head = 0;
    let mut got = Vec::new();
    while got.len() < n {
        match ring::dequeue(r, &mut head) {
            Some(x) => got.push(x.request_id),
            None => ring::park(r, head, || false),
        }
    }
    got
}

/// Two producers race for cells while the consumer parks and wakes. Every descriptor arrives
/// exactly once, and no wake-up is lost.
#[test]
fn ring_two_producers_one_parking_consumer() {
    loom::model(|| {
        let r = Arc::new(LocalRing::new(2));
        let ps: Vec<_> = (1..=2)
            .map(|id| {
                let r = r.clone();
                thread::spawn(move || ring::enqueue(&*r, &d(id)).unwrap())
            })
            .collect();
        let mut got = consume(&r, 2);
        for p in ps {
            p.join().unwrap();
        }
        got.sort();
        assert_eq!(got, vec![1, 2]);
    });
}

/// One producer laps a ring of two: cells are reused at `pos + capacity`, the producer sees
/// `Full` and retries, and order is kept.
#[test]
fn ring_wraps_around_under_a_slow_consumer() {
    loom::model(|| {
        let r = Arc::new(LocalRing::new(2));
        let r2 = r.clone();
        let p = thread::spawn(move || {
            for id in 1..=3 {
                loop {
                    match ring::enqueue(&*r2, &d(id)) {
                        Ok(()) => break,
                        Err(EnqueueError::Full) => thread::yield_now(),
                        Err(EnqueueError::Exhausted) => panic!("a consistent ring never exhausts"),
                    }
                }
            }
        });
        assert_eq!(consume(&r, 3), vec![1, 2, 3]);
        p.join().unwrap();
    });
}

fn healthy() -> u64 {
    gate(1, 0, gs::HEALTHY)
}

/// A client parks on its slot while the worker claims and completes it. The client always
/// sees DONE, whether it announced WAITER before or after the worker's claim.
#[test]
fn client_park_and_worker_complete() {
    loom::model(|| {
        let s = Arc::new(LocalSlot::new(0));
        let e = slot::try_claim(&*s).unwrap();
        let s2 = s.clone();
        let w = thread::spawn(move || {
            while slot::worker_claim(&*s2, e, 1) != Claim::Claimed {
                thread::yield_now();
            }
            assert!(slot::complete(&*s2, e));
        });
        slot::set_ready(&*s, e);
        full_fence();
        let spec = WaitSpec { epoch: e, deadline_ns: u64::MAX, spin_until_ns: 0, now: || 0, gate: healthy };
        assert!(matches!(slot::wait(&*s, &spec), Wait::Done(_)));
        w.join().unwrap();
    });
}

/// A worker completes the slot and dies before waking the parked client; the sweep's wake on
/// DONE slots reaches it (found by the TLA+ model, spec A). Without that wake the client
/// would sleep until its deadline, which a model without a clock shows as a deadlock.
#[test]
fn a_worker_that_dies_before_waking_is_covered_by_the_sweep() {
    loom::model(|| {
        let s = Arc::new(LocalSlot::new(0));
        let e = slot::try_claim(&*s).unwrap();
        slot::set_ready(&*s, e);
        let s2 = s.clone();
        let w = thread::spawn(move || {
            while slot::worker_claim(&*s2, e, 1) != Claim::Claimed {
                thread::yield_now();
            }
            // The completing CAS, then death before the wake.
            assert!(matches!(slot::complete_cas(&*s2, e), slot::Completed::Delivered { .. }));
        });
        let s3 = s.clone();
        let client = thread::spawn(move || {
            full_fence();
            let spec = WaitSpec { epoch: e, deadline_ns: u64::MAX, spin_until_ns: 0, now: || 0, gate: healthy };
            slot::wait(&*s3, &spec)
        });
        w.join().unwrap(); // the supervisor learns from the pidfd that the worker has exited
        slot::wake_if_done(&*s);
        assert!(matches!(client.join().unwrap(), Wait::Done(_)));
    });
}

/// The gate re-check after publishing pairs with the supervisor's store-gate-then-sweep
/// (shm-abi §6.3). With no worker at all, the client must leave: either it sees the closed
/// gate and cancels its slot, or the sweep sees READY, fails it, and wakes the client.
#[test]
fn gate_recheck_against_the_sweep() {
    loom::model(|| {
        let s = Arc::new(LocalSlot::new(0));
        let g = Arc::new(AtomicU64::new(healthy()));
        let e = slot::try_claim(&*s).unwrap();
        let (s2, g2) = (s.clone(), g.clone());
        let sup = thread::spawn(move || {
            g2.store(gate(1, 1, gs::DEAD), Release);
            full_fence();
            slot::fail_if_ready(&*s2);
        });
        slot::set_ready(&*s, e);
        full_fence(); // the fence that ends a publish
        let gl = g.clone();
        let spec = WaitSpec { epoch: e, deadline_ns: u64::MAX, spin_until_ns: 0, now: || 0, gate: move || gl.load(Acquire) };
        let out = slot::wait(&*s, &spec);
        sup.join().unwrap();
        match out {
            Wait::Cancelled(_) => assert_eq!(state(s.word.load(Relaxed)), ss::FREE),
            Wait::Failed(_) => assert_eq!(state(s.word.load(Relaxed)), ss::FAILED),
            other => panic!("the client should have left: {other:?}"),
        }
    });
}

/// A client abandons while the worker completes. Exactly one of them frees the slot, or the
/// client receives the answer; no slot is left ABANDONED.
#[test]
fn abandon_against_complete() {
    loom::model(|| {
        let s = Arc::new(LocalSlot::new(0));
        let e = slot::try_claim(&*s).unwrap();
        slot::set_ready(&*s, e);
        assert_eq!(slot::worker_claim(&*s, e, 2), Claim::Claimed);
        let s2 = s.clone();
        let w = thread::spawn(move || slot::complete(&*s2, e));
        let a = slot::abandon(&*s, e);
        let delivered = w.join().unwrap();
        let st = state(s.word.load(Relaxed));
        match a {
            Abandon::Abandoned => assert!(!delivered && st == ss::FREE, "the worker frees an abandoned slot"),
            Abandon::Done(_) => assert!(delivered && st == ss::DONE),
            other => panic!("unexpected {other:?}"),
        }
    });
}

/// A client abandons while the supervisor sweeps the slots of the worker that held it.
/// `OWNER` survives the abandon, so the sweep still finds the slot; it ends FREE, or FAILED
/// with the client told so.
#[test]
fn abandon_against_the_owner_sweep() {
    loom::model(|| {
        let s = Arc::new(LocalSlot::new(0));
        let e = slot::try_claim(&*s).unwrap();
        slot::set_ready(&*s, e);
        assert_eq!(slot::worker_claim(&*s, e, 2), Claim::Claimed);
        let s2 = s.clone();
        let sup = thread::spawn(move || slot::fail_if_owned(&*s2, 2));
        let a = slot::abandon(&*s, e);
        sup.join().unwrap();
        let w = s.word.load(Relaxed);
        match a {
            Abandon::Abandoned => assert_eq!(state(w), ss::FREE),
            Abandon::Failed(_) => assert_eq!(state(w), ss::FAILED),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(owner(w), 0);
    });
}

/// A stale descriptor (its slot cancelled and claimed again) never lets the worker take the
/// new request.
#[test]
fn a_stale_descriptor_against_a_new_claim() {
    loom::model(|| {
        let s = Arc::new(LocalSlot::new(0));
        let e = slot::try_claim(&*s).unwrap();
        slot::set_ready(&*s, e);
        let s2 = s.clone();
        let w = thread::spawn(move || slot::worker_claim(&*s2, e, 1));
        let cancelled = slot::cancel_if_ready(&*s, e) == slot::Cancel::Cancelled;
        let mut e2 = None;
        if cancelled {
            e2 = slot::try_claim(&*s);
            slot::set_ready(&*s, e2.unwrap());
        }
        let c = w.join().unwrap();
        if cancelled {
            assert_eq!(c, Claim::Stale);
            assert_eq!(s.word.load(Relaxed), word(e2.unwrap(), ss::READY));
        } else {
            assert_eq!(c, Claim::Claimed);
        }
    });
}

/// Activation with two warm waiters (testing §2.2, "Standby wait"): the supervisor stores the
/// gate, then bumps and wakes. Both waiters return; none sleeps through it.
#[test]
fn standby_wait_with_two_waiters() {
    use scz_shm::standby::{self, LocalStandby};
    loom::model(|| {
        let m = Arc::new(LocalStandby::new(gate(0, 0, gs::INIT)));
        let ws: Vec<_> = (0..2)
            .map(|_| {
                let m = m.clone();
                thread::spawn(move || standby::wait_for_gen(&*m, 1))
            })
            .collect();
        m.gate.store(gate(1, 0, gs::INIT), Release);
        standby::bump_and_wake(&*m);
        for w in ws {
            w.join().unwrap();
        }
    });
}

mod snapshot {
    use super::*;
    use scz_shm::snapshot::{CallerTable, Published};
    use std::cell::Cell;

    struct Snap(Arc<AtomicBool>);

    /// A request thread enters the current snapshot while the background thread replaces and
    /// tries to reclaim it. A reclaimed snapshot is never in use.
    #[test]
    fn a_snapshot_is_never_freed_under_a_call() {
        loom::model(|| {
            let p = Arc::new(Published::<Snap>::new());
            let t = Arc::new(CallerTable::<()>::new(2));
            let freed = Arc::new(AtomicBool::new(false));
            assert!(p.replace(Some(Box::new(Snap(freed.clone())))).ok().unwrap().is_none());
            let (p2, t2) = (p.clone(), t.clone());
            let caller = thread::spawn(move || {
                let hint = Cell::new(0);
                if let Ok(g) = t2.enter(&*p2, &hint, 7) {
                    assert!(!g.snap().0.load(SeqCst), "a call is inside a freed snapshot");
                }
            });
            let r = p.replace(None).ok().unwrap().unwrap();
            if let Ok(b) = p.reclaim(r, &t, |_| true) {
                b.0.store(true, SeqCst);
            }
            caller.join().unwrap();
        });
    }
}
