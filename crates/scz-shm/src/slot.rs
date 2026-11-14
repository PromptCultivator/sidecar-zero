//! The slot protocol (shm-abi §5–6): who may move a slot from which state to which.
//!
//! Every exit from a state more than one party can leave is a compare-and-swap loop, bounded
//! at `CAS_ATTEMPTS`; plain stores are used only out of `CLAIMED`, `DONE`, and `FAILED`,
//! which belong to the client alone (§6.1).

use crate::abi::{self, slot_state as ss, slot_word as sw};
use crate::crash_point;
use crate::sync::{spin_hint, AtomicU64, Ordering::*};
use crate::word::{epoch, low32, owner, slot, state, waiter, with_owner, with_waiter};

/// The slot word, and the futex operations on its low half.
pub trait SlotMem {
    fn word(&self) -> &AtomicU64;
    /// Sleep while the low half equals `expected`, until `deadline_ns`. False on timeout.
    fn wait(&self, expected_low32: u32, deadline_ns: u64) -> bool;
    fn wake(&self);
}

// ── client ──────────────────────────────────────────────────────────────────────────────

/// `FREE → CLAIMED`, with the epoch advanced. Returns the new epoch.
pub fn try_claim(s: &impl SlotMem) -> Option<u32> {
    let w = s.word().load(Acquire);
    if state(w) != ss::FREE {
        return None;
    }
    let e = epoch(w).wrapping_add(1);
    s.word().compare_exchange(w, slot(e, ss::CLAIMED), AcqRel, Acquire).ok().map(|_| e)
}

/// `CLAIMED → READY`. After this the request region belongs to the worker.
pub fn set_ready(s: &impl SlotMem, e: u32) {
    s.word().store(slot(e, ss::READY), Release);
}

/// `CLAIMED`, `DONE`, or `FAILED` `→ FREE`.
pub fn release(s: &impl SlotMem, e: u32) {
    s.word().store(slot(e, ss::FREE), Release);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancel {
    Cancelled,
    /// No longer `READY` at this epoch: the worker or the sweep owns the outcome.
    NotReady,
    /// The word kept changing; something is writing it that should not be.
    GaveUp,
}

/// `READY → FREE`, when the request never reached the worker.
pub fn cancel_if_ready(s: &impl SlotMem, e: u32) -> Cancel {
    for _ in 0..abi::CAS_ATTEMPTS {
        let w = s.word().load(Acquire);
        if state(w) != ss::READY || epoch(w) != e {
            return Cancel::NotReady;
        }
        if s.word().compare_exchange(w, slot(e, ss::FREE), AcqRel, Acquire).is_ok() {
            return Cancel::Cancelled;
        }
    }
    Cancel::GaveUp
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The worker answered; the word as it was.
    Done(u64),
    /// The worker answered, but the waiter found it only after its deadline had passed while
    /// it was parked: either the answer came at the last moment or its wake-up was lost or
    /// late (signaling §7). The answer is valid.
    Late(u64),
    /// The supervisor failed the slot: the worker died, or the queue was flushed.
    Failed(u64),
    /// Cancelled while still queued because the gate was not healthy; the gate value.
    Cancelled(u64),
    /// The deadline passed.
    Timeout,
    /// The word kept changing under the waiter.
    GaveUp,
}

/// The parameters of one wait.
pub struct WaitSpec<N: Fn() -> u64, G: Fn() -> u64> {
    pub epoch: u32,
    /// Absolute monotonic deadline: the moment the caller stops waiting.
    pub deadline_ns: u64,
    /// Spin until this moment (or the deadline, if sooner), then park.
    pub spin_until_ns: u64,
    pub now: N,
    /// Loads the gate word.
    pub gate: G,
}

/// Client side of signaling §3.2. Called right after the descriptor was published; the
/// publish ended in a full fence, so the first gate load here is the re-check that pairs
/// with the supervisor's store-gate-then-sweep (shm-abi §6.3).
pub fn wait<N: Fn() -> u64, G: Fn() -> u64>(s: &impl SlotMem, p: &WaitSpec<N, G>) -> Wait {
    let healthy = |g: u64| crate::word::gate_state(g) == abi::gate_state::HEALTHY;
    let g = (p.gate)();
    if !healthy(g) && cancel_if_ready(s, p.epoch) == Cancel::Cancelled {
        return Wait::Cancelled(g);
    }
    let resolved = |w: u64| -> Option<Wait> {
        if epoch(w) != p.epoch {
            return Some(Wait::GaveUp);
        }
        match state(w) {
            ss::DONE => Some(Wait::Done(w)),
            ss::FAILED => Some(Wait::Failed(w)),
            _ => None,
        }
    };
    // Spin.
    let spin_end = p.spin_until_ns.min(p.deadline_ns);
    while (p.now)() < spin_end {
        let w = s.word().load(Acquire);
        if let Some(r) = resolved(w) {
            return r;
        }
        if state(w) == ss::READY {
            let g = (p.gate)();
            if !healthy(g) && cancel_if_ready(s, p.epoch) == Cancel::Cancelled {
                return Wait::Cancelled(g);
            }
        }
        spin_hint();
    }
    // Park: announce with the WAITER bit, then sleep on the word.
    let mut attempts = 0;
    let mut slept = false;
    loop {
        let w = s.word().load(Acquire);
        if let Some(r) = resolved(w) {
            if slept && matches!(r, Wait::Done(_)) && (p.now)() >= p.deadline_ns {
                return Wait::Late(w);
            }
            return r;
        }
        if (p.now)() >= p.deadline_ns {
            return Wait::Timeout;
        }
        let mut w = w;
        if !waiter(w) {
            if s.word().compare_exchange(w, with_waiter(w), AcqRel, Acquire).is_err() {
                attempts += 1;
                if attempts >= abi::CAS_ATTEMPTS {
                    return Wait::GaveUp;
                }
                continue;
            }
            w = with_waiter(w);
        }
        s.wait(low32(w), p.deadline_ns);
        slept = true;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abandon {
    /// Still queued: freed on the spot.
    Freed,
    /// Being processed: handed to the worker to free. `OWNER` is kept (§6.1).
    Abandoned,
    /// The worker won the race; the answer is valid.
    Done(u64),
    Failed(u64),
    GaveUp,
}

/// What a client does when its deadline has passed (sdk-contract §5).
pub fn abandon(s: &impl SlotMem, e: u32) -> Abandon {
    for _ in 0..abi::CAS_ATTEMPTS {
        let w = s.word().load(Acquire);
        if epoch(w) != e {
            return Abandon::GaveUp;
        }
        match state(w) {
            ss::READY => {
                if s.word().compare_exchange(w, slot(e, ss::FREE), AcqRel, Acquire).is_ok() {
                    return Abandon::Freed;
                }
            }
            ss::PROCESSING => {
                let new = with_owner(slot(e, ss::ABANDONED), owner(w));
                if s.word().compare_exchange(w, new, AcqRel, Acquire).is_ok() {
                    return Abandon::Abandoned;
                }
            }
            ss::DONE => return Abandon::Done(w),
            ss::FAILED => return Abandon::Failed(w),
            _ => return Abandon::GaveUp,
        }
    }
    Abandon::GaveUp
}

// ── worker ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Claimed,
    /// Not `READY` at the descriptor's epoch: cancelled, abandoned, swept, or a stale descriptor.
    Stale,
    GaveUp,
}

/// `READY → PROCESSING`, keeping `WAITER` and setting `OWNER` to the worker's beat slot + 1.
/// A failure caused only by `WAITER` changing is retried (§6.1).
pub fn worker_claim(s: &impl SlotMem, e: u32, owner_id: u32) -> Claim {
    for _ in 0..abi::CAS_ATTEMPTS {
        let w = s.word().load(Acquire);
        if epoch(w) != e || state(w) != ss::READY {
            return Claim::Stale;
        }
        let new = with_owner(slot(e, ss::PROCESSING) | (w & sw::WAITER_MASK), owner_id);
        if s.word().compare_exchange(w, new, AcqRel, Acquire).is_ok() {
            crash_point!("slot.worker.claimed");
            return Claim::Claimed;
        }
    }
    Claim::GaveUp
}

/// What the completing CAS found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completed {
    /// `PROCESSING → DONE`: the client will see the answer. `wake` says it had parked, so a
    /// futex wake is owed.
    Delivered { wake: bool },
    /// `ABANDONED → FREE`: the client had given up.
    Freed,
    /// Not this worker's to complete any more.
    NotOurs,
}

/// The completing CAS alone: `PROCESSING → DONE`, or `ABANDONED → FREE` if the client gave
/// up meanwhile. The response and the record's worker fields must already be written.
/// [`complete`] adds the wake; a model stops between the two to stand for a death there.
pub fn complete_cas(s: &impl SlotMem, e: u32) -> Completed {
    crash_point!("slot.worker.response_written");
    for _ in 0..abi::CAS_ATTEMPTS {
        let w = s.word().load(Acquire);
        if epoch(w) != e {
            return Completed::NotOurs;
        }
        match state(w) {
            ss::ABANDONED => {
                if s.word().compare_exchange(w, slot(e, ss::FREE), AcqRel, Acquire).is_ok() {
                    return Completed::Freed;
                }
            }
            ss::PROCESSING => {
                if s.word().compare_exchange(w, slot(e, ss::DONE), AcqRel, Acquire).is_ok() {
                    return Completed::Delivered { wake: waiter(w) };
                }
            }
            _ => return Completed::NotOurs,
        }
        spin_hint();
    }
    Completed::NotOurs
}

/// Complete the slot and wake the client if it parked. Returns whether the client will see
/// the answer.
pub fn complete(s: &impl SlotMem, e: u32) -> bool {
    complete_then(s, e, || {})
}

/// [`complete`], running `before_wake` between completing the slot and waking its client: the
/// chaos worker's stalled waker (signaling §7, "A lost wake-up does not look like a failure").
pub fn complete_then(s: &impl SlotMem, e: u32, before_wake: impl FnOnce()) -> bool {
    match complete_cas(s, e) {
        Completed::Delivered { wake } => {
            crash_point!("slot.worker.completed");
            before_wake();
            if wake {
                s.wake();
            }
            true
        }
        _ => false,
    }
}

// ── supervisor sweep (§6.3) ─────────────────────────────────────────────────────────────

/// Part 2: `READY → FAILED`, waking a parked client. Returns whether it failed the slot.
pub fn fail_if_ready(s: &impl SlotMem) -> bool {
    for _ in 0..abi::CAS_ATTEMPTS {
        let w = s.word().load(Acquire);
        if state(w) != ss::READY {
            return false;
        }
        if s.word().compare_exchange(w, slot(epoch(w), ss::FAILED), AcqRel, Acquire).is_ok() {
            if waiter(w) {
                s.wake();
            }
            return true;
        }
    }
    false
}

/// Part 2, last: wake the client of a `DONE` slot. Its worker may have died between the
/// completing CAS, which clears `WAITER`, and its own wake, leaving the client asleep with its
/// answer ready until its deadline. Found by the TLA+ model (spec A, `ClientNotStranded`). A
/// wake that finds nobody costs one system call, on a path that runs only at a death.
pub fn wake_if_done(s: &impl SlotMem) -> bool {
    if state(s.word().load(Acquire)) == ss::DONE {
        s.wake();
        true
    } else {
        false
    }
}

/// Part 1: the slots an exited worker held. `PROCESSING → FAILED`, `ABANDONED → FREE`.
pub fn fail_if_owned(s: &impl SlotMem, owner_id: u32) -> bool {
    for _ in 0..abi::CAS_ATTEMPTS {
        let w = s.word().load(Acquire);
        if owner(w) != owner_id {
            return false;
        }
        let new = match state(w) {
            ss::PROCESSING => slot(epoch(w), ss::FAILED),
            ss::ABANDONED => slot(epoch(w), ss::FREE),
            _ => return false,
        };
        if s.word().compare_exchange(w, new, AcqRel, Acquire).is_ok() {
            if waiter(w) && state(new) == ss::FAILED {
                s.wake();
            }
            return true;
        }
    }
    false
}

/// An in-process slot word, for tests and models.
pub struct LocalSlot {
    pub word: AtomicU64,
    #[cfg(loom)]
    pub futex: crate::wait::MockFutex,
}

impl LocalSlot {
    pub fn new(w: u64) -> Self {
        LocalSlot {
            word: AtomicU64::new(w),
            #[cfg(loom)]
            futex: crate::wait::MockFutex::new(),
        }
    }
}

impl SlotMem for LocalSlot {
    fn word(&self) -> &AtomicU64 {
        &self.word
    }
    fn wait(&self, expected: u32, deadline_ns: u64) -> bool {
        #[cfg(loom)]
        {
            let _ = deadline_ns; // models have no clock: a lost wake-up shows as a deadlock
            self.futex.wait_low32(&self.word, expected);
            true
        }
        #[cfg(not(loom))]
        crate::wait::futex_wait_low32(&self.word, expected, Some(deadline_ns))
    }
    fn wake(&self) {
        #[cfg(loom)]
        self.futex.wake();
        #[cfg(not(loom))]
        crate::wait::futex_wake_low32(&self.word, 1);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::wait::monotonic_ns;

    fn spec(e: u32, gate: u64, budget_ns: u64) -> WaitSpec<impl Fn() -> u64, impl Fn() -> u64> {
        let now = monotonic_ns();
        WaitSpec { epoch: e, deadline_ns: now + budget_ns, spin_until_ns: now + 10_000, now: monotonic_ns, gate: move || gate }
    }
    const HEALTHY: u64 = crate::word::gate(1, 0, abi::gate_state::HEALTHY);
    const DEAD: u64 = crate::word::gate(1, 0, abi::gate_state::DEAD);

    #[test]
    fn the_happy_path() {
        let s = LocalSlot::new(0);
        let e = try_claim(&s).unwrap();
        assert_eq!(e, 1);
        assert!(try_claim(&s).is_none(), "one owner at a time");
        set_ready(&s, e);
        assert_eq!(worker_claim(&s, e, 1), Claim::Claimed);
        assert_eq!(owner(s.word.load(Relaxed)), 1);
        assert!(complete(&s, e));
        let w = s.word.load(Relaxed);
        assert_eq!((state(w), owner(w)), (ss::DONE, 0), "DONE clears OWNER");
        assert!(matches!(wait(&s, &spec(e, HEALTHY, 1_000_000)), Wait::Done(_)));
        release(&s, e);
        assert_eq!(try_claim(&s), Some(2));
    }

    #[test]
    fn a_stale_descriptor_is_skipped() {
        let s = LocalSlot::new(0);
        let e = try_claim(&s).unwrap();
        set_ready(&s, e);
        assert_eq!(cancel_if_ready(&s, e), Cancel::Cancelled);
        assert_eq!(worker_claim(&s, e, 1), Claim::Stale);
        let e2 = try_claim(&s).unwrap();
        set_ready(&s, e2);
        assert_eq!(worker_claim(&s, e, 1), Claim::Stale, "the old epoch's descriptor must not claim the new request");
    }

    #[test]
    fn abandon_during_processing_keeps_owner_and_the_worker_frees() {
        let s = LocalSlot::new(0);
        let e = try_claim(&s).unwrap();
        set_ready(&s, e);
        worker_claim(&s, e, 2);
        assert_eq!(abandon(&s, e), Abandon::Abandoned);
        assert_eq!(owner(s.word.load(Relaxed)), 2, "an abandon must carry OWNER across");
        assert!(!complete(&s, e));
        assert_eq!(state(s.word.load(Relaxed)), ss::FREE);
    }

    #[test]
    fn an_unhealthy_gate_cancels_a_queued_request() {
        let s = LocalSlot::new(0);
        let e = try_claim(&s).unwrap();
        set_ready(&s, e);
        assert_eq!(wait(&s, &spec(e, DEAD, 1_000_000)), Wait::Cancelled(DEAD));
        assert_eq!(state(s.word.load(Relaxed)), ss::FREE);
    }

    #[test]
    fn a_wait_times_out_at_its_deadline() {
        let s = LocalSlot::new(0);
        let e = try_claim(&s).unwrap();
        set_ready(&s, e);
        worker_claim(&s, e, 1); // processing, and never completed
        let t0 = monotonic_ns();
        assert_eq!(wait(&s, &spec(e, HEALTHY, 3_000_000)), Wait::Timeout);
        let took = monotonic_ns() - t0;
        assert!((3_000_000..50_000_000).contains(&took), "took {took} ns");
        assert!(waiter(s.word.load(Relaxed)), "it parked");
    }

    #[test]
    fn the_sweep_wakes_a_client_whose_worker_died_before_waking_it() {
        use std::sync::Arc;
        let s = Arc::new(LocalSlot::new(0));
        let e = try_claim(&*s).unwrap();
        set_ready(&*s, e);
        assert_eq!(worker_claim(&*s, e, 1), Claim::Claimed);
        let s2 = s.clone();
        let client = std::thread::spawn(move || {
            let now = monotonic_ns();
            let spec = WaitSpec { epoch: e, deadline_ns: now + 5_000_000_000, spin_until_ns: now, now: monotonic_ns, gate: || HEALTHY };
            let r = wait(&*s2, &spec);
            (r, monotonic_ns() - now)
        });
        while !waiter(s.word.load(Relaxed)) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(20)); // the client is asleep
                                                                  // The worker's completing CAS, and then its death: no wake.
        s.word.store(slot(e, ss::DONE), Release);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!client.is_finished(), "the client sleeps on, though its answer is there");
        assert!(wake_if_done(&*s));
        let (r, took) = client.join().unwrap();
        assert!(matches!(r, Wait::Done(_)));
        assert!(took < 1_000_000_000, "woken by the sweep, not by its 5 s deadline: {took} ns");
    }

    #[test]
    fn the_sweep_fails_queued_and_owned_slots_only() {
        let queued = LocalSlot::new(slot(4, ss::READY));
        let mine = LocalSlot::new(with_owner(slot(4, ss::PROCESSING), 1));
        let theirs = LocalSlot::new(with_owner(slot(4, ss::PROCESSING), 2));
        let gone = LocalSlot::new(with_owner(slot(4, ss::ABANDONED), 1));
        assert!(fail_if_ready(&queued));
        assert!(fail_if_owned(&mine, 1));
        assert!(!fail_if_owned(&theirs, 1));
        assert!(fail_if_owned(&gone, 1));
        assert_eq!(state(queued.word.load(Relaxed)), ss::FAILED);
        assert_eq!(state(mine.word.load(Relaxed)), ss::FAILED);
        assert_eq!(state(theirs.word.load(Relaxed)), ss::PROCESSING);
        assert_eq!(state(gone.word.load(Relaxed)), ss::FREE);
    }
}
