//! Descriptor rings (shm-abi §4): bounded MPSC queues with a sequence word in every cell.
//!
//! Every function here is written against [`RingMem`], so the same code runs over a mapped
//! data segment and, under loom, over atomics the model owns. Position arithmetic wraps: the
//! words live in memory the host and the worker can write, and no value in them may panic.

use crate::abi::{self, cell_flag, consumer_wait as cw};
use crate::crash_point;
use crate::sync::{full_fence, spin_hint, AtomicU32, AtomicU64, Ordering::*};

/// The memory of one ring, and the futex operations on its wake word.
pub trait RingMem {
    /// A power of two, taken from the control segment's directory, never from the ring.
    fn capacity(&self) -> u64;
    fn tail(&self) -> &AtomicU64;
    fn head(&self) -> &AtomicU64;
    fn consumer_wait(&self) -> &AtomicU32;
    /// The cell at `pos & (capacity − 1)`: eight 64-bit words, word 0 being `seq`.
    fn cell(&self, pos: u64) -> &[AtomicU64; 8];
    /// Sleep while `consumer_wait` holds `expected`. May return early.
    fn park(&self, expected: u32);
    /// Wake the consumer, if it sleeps.
    fn unpark(&self);
}

/// What a descriptor cell carries, apart from `seq`. The slot record is authoritative; only
/// `slab_class`, `slot_index`, `slot_epoch`, and `deadline_ns` may be acted on (§4.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Descriptor {
    pub request_id: u64,
    pub slot_index: u32,
    pub deadline_ns: u64,
    pub slot_epoch: u32,
    pub slab_class: u16,
    pub enqueue_ns: u64,
    pub flags: u32,
    pub route: u32,
}

impl Descriptor {
    pub fn is_void(&self) -> bool {
        self.flags & cell_flag::VOID != 0
    }
}

// The cell's 64 bytes as eight little-endian words (offsets from abi::cell):
//   0 seq · 1 request_id · 2 reserved · 3 [reserved u32 | slot_index u32] · 4 deadline
//   5 [slot_epoch u32 | slab_class u16 | reserved u16] · 6 enqueue_ns · 7 [flags u32 | route u32]
const _: () = {
    assert!(abi::cell::SEQ == 0 && abi::cell::REQUEST_ID == 8 && abi::cell::SLOT_INDEX == 0x1C);
    assert!(abi::cell::DEADLINE_MONO_NS == 0x20 && abi::cell::SLOT_EPOCH == 0x28 && abi::cell::SLAB_CLASS == 0x2C);
    assert!(abi::cell::ENQUEUE_MONO_NS == 0x30 && abi::cell::FLAGS == 0x38 && abi::cell::ROUTE == 0x3C);
};

fn write(cell: &[AtomicU64; 8], d: &Descriptor) {
    // The reserved bytes are zero, or the next minor's pattern (shm-abi §12.5).
    let (w2, w3, w5) = crate::next::cell_words((d.slot_index as u64) << 32, d.slot_epoch as u64 | (d.slab_class as u64) << 32);
    cell[1].store(d.request_id, Relaxed);
    cell[2].store(w2, Relaxed);
    cell[3].store(w3, Relaxed);
    cell[4].store(d.deadline_ns, Relaxed);
    cell[5].store(w5, Relaxed);
    cell[6].store(d.enqueue_ns, Relaxed);
    cell[7].store(d.flags as u64 | (d.route as u64) << 32, Relaxed);
}

fn read(cell: &[AtomicU64; 8]) -> Descriptor {
    let w3 = cell[3].load(Relaxed);
    let w5 = cell[5].load(Relaxed);
    let w7 = cell[7].load(Relaxed);
    Descriptor {
        request_id: cell[1].load(Relaxed),
        slot_index: (w3 >> 32) as u32,
        deadline_ns: cell[4].load(Relaxed),
        slot_epoch: w5 as u32,
        slab_class: (w5 >> 32) as u16,
        enqueue_ns: cell[6].load(Relaxed),
        flags: w7 as u32,
        route: (w7 >> 32) as u32,
    }
}

/// Why an enqueue did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueError {
    /// The ring is full.
    Full,
    /// Every attempt was lost. In a consistent ring that takes 64 other producers in about a
    /// microsecond; in practice it means a corrupt sequence word. Counted as a ring fault.
    Exhausted,
}

/// Fill the cells of a fresh ring. Only before the segment is shared.
pub fn init(r: &impl RingMem) {
    for i in 0..r.capacity() {
        let cell = r.cell(i);
        cell[0].store(i, Relaxed);
        for w in &cell[1..] {
            w.store(0, Relaxed);
        }
    }
    r.tail().store(0, Relaxed);
    r.head().store(0, Relaxed);
    r.consumer_wait().store(cw::AWAKE, Relaxed);
}

/// The test hook of sdk-contract §11 item 11: armed, the next [`enqueue`] in this process
/// panics between claiming a cell and publishing it, as a caller that unwinds there would.
/// Only the conformance agents arm it.
#[cfg(not(loom))]
pub static UNWIND_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Publishes a claimed cell as void if the producer unwinds before publishing it (sdk-contract
/// §4, "Every claimed ring cell is published"): an unpublished cell would stall the ring for
/// every later call. The consumer skips a void cell.
struct VoidOnUnwind<'a, R: RingMem> {
    r: &'a R,
    cell: &'a [AtomicU64; 8],
    pos: u64,
}

impl<R: RingMem> Drop for VoidOnUnwind<'_, R> {
    fn drop(&mut self) {
        write(self.cell, &Descriptor { flags: cell_flag::VOID, ..Descriptor::default() });
        self.cell[0].store(self.pos.wrapping_add(1), Release);
        full_fence();
        wake_if_parked(self.r);
    }
}

/// Producer side (§4.3). Publishes `d` and wakes the consumer if it has parked.
pub fn enqueue<R: RingMem>(r: &R, d: &Descriptor) -> Result<(), EnqueueError> {
    for _ in 0..abi::ENQUEUE_ATTEMPTS {
        let pos = r.tail().load(Relaxed);
        let cell = r.cell(pos);
        let seq = cell[0].load(Acquire);
        if seq == pos {
            if r.tail().compare_exchange_weak(pos, pos.wrapping_add(1), Relaxed, Relaxed).is_ok() {
                crash_point!("ring.enqueue.claimed");
                let guard = VoidOnUnwind { r, cell, pos };
                #[cfg(not(loom))]
                if UNWIND_ONCE.load(Relaxed) && UNWIND_ONCE.swap(false, Relaxed) {
                    panic!("sidecarzero test hook: unwinding between claiming a ring cell and publishing it");
                }
                write(cell, d);
                crash_point!("ring.enqueue.written");
                core::mem::forget(guard);
                cell[0].store(pos.wrapping_add(1), Release); // publish
                full_fence(); // orders the publish before the wait-word load, and before the caller's gate re-check
                wake_if_parked(r);
                return Ok(());
            }
        } else if seq < pos {
            return Err(EnqueueError::Full);
        }
        // Another producer took the cell first, or the word is corrupt: go round again.
        spin_hint();
    }
    Err(EnqueueError::Exhausted)
}

/// The waker rule (signaling §3.1). Called by producers after publishing, and by the
/// supervisor after changing `ctl_req`; both have already issued a full fence.
pub fn wake_if_parked(r: &impl RingMem) {
    let w = r.consumer_wait().load(Acquire);
    if w == cw::AWAKE {
        return;
    }
    if w == cw::PARKED {
        let _ = r.consumer_wait().compare_exchange(cw::PARKED, cw::WAKING, AcqRel, Acquire);
    }
    r.unpark();
}

/// Is the cell at `head` published? (Not `tail != head`: that is also true while a producer
/// has claimed a cell and not yet published it.)
pub fn head_published(r: &impl RingMem, head: u64) -> bool {
    r.cell(head)[0].load(Acquire) == head.wrapping_add(1)
}

/// Consumer side (§4.4). `head` is the consumer's own copy; the header mirrors it.
pub fn dequeue(r: &impl RingMem, head: &mut u64) -> Option<Descriptor> {
    try_dequeue_if(r, head, |_| true)
}

/// Like [`dequeue`], but inspects a published descriptor first and consumes it only when it
/// is void or `pred(&d)` returns `true`.
pub fn try_dequeue_if(r: &impl RingMem, head: &mut u64, pred: impl FnOnce(&Descriptor) -> bool) -> Option<Descriptor> {
    let pos = *head;
    let cell = r.cell(pos);
    if cell[0].load(Acquire) != pos.wrapping_add(1) {
        return None;
    }
    let d = read(cell);
    if !d.is_void() && !pred(&d) {
        return None;
    }
    cell[0].store(pos.wrapping_add(r.capacity()), Release); // step A: the cell is free for the next lap
    crash_point!("ring.dequeue.after_a");
    r.head().store(pos.wrapping_add(1), Release); // step B
    *head = pos.wrapping_add(1);
    Some(d)
}

/// Park the consumer until there is work (signaling §3.1). Returns at once if work arrived
/// between the caller's last look and the announcement, or a control request is pending.
pub fn park(r: &impl RingMem, head: u64, pending: impl Fn() -> bool) {
    r.consumer_wait().store(cw::PARKED, Release); // announce
    full_fence();
    if head_published(r, head) || pending() {
        r.consumer_wait().store(cw::AWAKE, Release);
        return;
    }
    r.park(cw::PARKED);
    r.consumer_wait().store(cw::AWAKE, Release);
}

/// The repair rule (§4.5), run by the supervisor on a ring whose consumer has died, before a
/// successor consumes it. Returns whether `head` was advanced.
pub fn repair(r: &impl RingMem) -> bool {
    // The words are in memory the host and the worker can write: wrapping arithmetic, so that
    // no value read here can make the supervisor panic (supervisor §9).
    let head = r.head().load(Acquire);
    let s = r.cell(head)[0].load(Acquire);
    let lap = head.wrapping_add(r.capacity());
    if s == lap || s == lap.wrapping_add(1) {
        r.head().store(head.wrapping_add(1), Release);
        true
    } else {
        false
    }
}

/// What the supervisor sees at a ring's head (supervisor §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadState {
    /// Nothing queued.
    Empty,
    /// A descriptor is published at the head: the consumer has work.
    Published,
    /// A producer has claimed the head cell and not yet published it.
    Claimed,
    /// The consumer freed the head cell (step A) and has not advanced `head` (step B): only
    /// a consumer that died in between leaves this, and the repair rule fixes it.
    Freed,
    /// None of the values a consistent ring can show: a stray write.
    Inconsistent,
}

/// Classify a ring's head without changing anything. Only `head`, `tail`, and the head cell's
/// sequence number are read; arithmetic wraps (supervisor §9).
pub fn head_state(r: &impl RingMem) -> (HeadState, u64) {
    let head = r.head().load(Acquire);
    let tail = r.tail().load(Acquire);
    let s = r.cell(head)[0].load(Acquire);
    let cap = r.capacity();
    let lap = head.wrapping_add(cap);
    let st = if tail.wrapping_sub(head) > cap.wrapping_add(1) {
        HeadState::Inconsistent
    } else if s == head {
        if tail == head {
            HeadState::Empty
        } else {
            HeadState::Claimed
        }
    } else if s == head.wrapping_add(1) {
        HeadState::Published
    } else if s == lap || s == lap.wrapping_add(1) {
        HeadState::Freed
    } else {
        HeadState::Inconsistent
    };
    (st, head)
}

/// An in-process ring of owned atomics, for tests and models.
pub struct LocalRing {
    pub tail: AtomicU64,
    pub head: AtomicU64,
    pub wait: AtomicU32,
    pub cells: Vec<[AtomicU64; 8]>,
    #[cfg(loom)]
    pub futex: crate::wait::MockFutex,
}

impl LocalRing {
    pub fn new(capacity: u64) -> Self {
        assert!(capacity.is_power_of_two());
        let r = LocalRing {
            tail: AtomicU64::new(0),
            head: AtomicU64::new(0),
            wait: AtomicU32::new(0),
            cells: (0..capacity).map(|_| core::array::from_fn(|_| AtomicU64::new(0))).collect(),
            #[cfg(loom)]
            futex: crate::wait::MockFutex::new(),
        };
        init(&r);
        r
    }
}

impl RingMem for LocalRing {
    fn capacity(&self) -> u64 {
        self.cells.len() as u64
    }
    fn tail(&self) -> &AtomicU64 {
        &self.tail
    }
    fn head(&self) -> &AtomicU64 {
        &self.head
    }
    fn consumer_wait(&self) -> &AtomicU32 {
        &self.wait
    }
    fn cell(&self, pos: u64) -> &[AtomicU64; 8] {
        &self.cells[(pos & (self.capacity() - 1)) as usize]
    }
    fn park(&self, expected: u32) {
        #[cfg(loom)]
        self.futex.wait32(&self.wait, expected);
        #[cfg(not(loom))]
        crate::wait::futex_wait(&self.wait, expected, None);
    }
    fn unpark(&self) {
        #[cfg(loom)]
        self.futex.wake();
        #[cfg(not(loom))]
        crate::wait::futex_wake(&self.wait, 1);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn d(id: u64) -> Descriptor {
        Descriptor { request_id: id, slot_index: id as u32, deadline_ns: 7, slot_epoch: 3, slab_class: 1, enqueue_ns: 9, flags: 0, route: 0 }
    }

    #[test]
    fn fifo_and_full() {
        let r = LocalRing::new(4);
        for i in 0..4 {
            enqueue(&r, &d(i)).unwrap();
        }
        assert_eq!(enqueue(&r, &d(9)), Err(EnqueueError::Full));
        let mut head = 0;
        for i in 0..4 {
            assert_eq!(dequeue(&r, &mut head), Some(d(i)));
        }
        assert_eq!(dequeue(&r, &mut head), None);
        assert_eq!(r.head.load(Relaxed), 4);
        enqueue(&r, &d(10)).unwrap(); // the next lap
        assert_eq!(dequeue(&r, &mut head).unwrap().request_id, 10);
    }

    #[test]
    fn descriptor_fields_survive_the_cell() {
        let r = LocalRing::new(2);
        let x = Descriptor {
            request_id: u64::MAX,
            slot_index: 0xFFFF_FFFE,
            deadline_ns: 1 << 63,
            slot_epoch: 0x8000_0001,
            slab_class: 0xFFFF,
            enqueue_ns: 42,
            flags: cell_flag::VOID,
            route: 0xABCD,
        };
        enqueue(&r, &x).unwrap();
        let got = dequeue(&r, &mut 0).unwrap();
        assert_eq!(got, x);
        assert!(got.is_void());
    }

    #[test]
    fn repair_after_death_between_a_and_b() {
        // The consumer ran step A on position 0 and died before step B.
        let r = LocalRing::new(2);
        enqueue(&r, &d(1)).unwrap();
        r.cells[0][0].store(2, Relaxed); // step A only
        assert!(repair(&r));
        assert_eq!(r.head.load(Relaxed), 1);
        assert!(!repair(&r), "a consistent ring is left alone");
    }

    #[test]
    fn repair_when_the_freed_cell_was_refilled() {
        // The ring was full; the consumer freed cell 0 (step A) and died; a producer then
        // refilled it, so seq is head + capacity + 1. Testing for equality alone would miss it.
        let r = LocalRing::new(2);
        enqueue(&r, &d(1)).unwrap();
        enqueue(&r, &d(2)).unwrap();
        r.cells[0][0].store(2, Relaxed); // step A on position 0
        enqueue(&r, &d(3)).unwrap(); // refills cell 0 for position 2: seq = 3
        assert_eq!(r.cells[0][0].load(Relaxed), 3);
        assert!(repair(&r));
        let mut head = r.head.load(Relaxed);
        assert_eq!(dequeue(&r, &mut head).unwrap().request_id, 2);
        assert_eq!(dequeue(&r, &mut head).unwrap().request_id, 3);
    }

    #[test]
    fn a_corrupt_sequence_word_ends_in_exhausted_not_a_hang() {
        let r = LocalRing::new(2);
        r.cells[0][0].store(1_000_000, Relaxed); // ahead of tail: every producer would retry
        assert_eq!(enqueue(&r, &d(1)), Err(EnqueueError::Exhausted));
    }

    #[test]
    fn park_returns_at_once_when_work_is_published() {
        let r = LocalRing::new(2);
        enqueue(&r, &d(1)).unwrap();
        park(&r, 0, || false); // must not sleep
        assert_eq!(r.wait.load(Relaxed), cw::AWAKE);
    }

    #[test]
    fn try_dequeue_if_respects_predicate() {
        let r = LocalRing::new(4);
        enqueue(&r, &Descriptor { slab_class: 0, request_id: 1, ..d(1) }).unwrap();
        enqueue(&r, &Descriptor { slab_class: 1, request_id: 2, ..d(2) }).unwrap();
        let mut head = 0;
        assert_eq!(try_dequeue_if(&r, &mut head, |x| x.slab_class == 0).unwrap().request_id, 1);
        assert_eq!(head, 1);
        assert!(try_dequeue_if(&r, &mut head, |x| x.slab_class == 0).is_none());
        assert_eq!(head, 1);
        assert_eq!(try_dequeue_if(&r, &mut head, |x| x.slab_class == 1).unwrap().request_id, 2);
        assert_eq!(head, 2);
    }
}
