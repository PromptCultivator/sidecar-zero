//! Publishing an arena to request threads, and retiring it only once no call can still be
//! inside it (sdk-contract §6, "Retiring an old arena"; sdk-rust §3.1).
//!
//! A call claims an entry in a [`CallerTable`] by writing an in-call marker, `(snapshot id <<
//! 32) | thread id`, and then re-checks that the snapshot it named is still current. The
//! background thread replaces the current snapshot and then scans the table; it frees an old
//! snapshot only when no entry names it. Both sides write and then read, each with a full
//! fence between, so at least one of them sees the other (the store-load pairing of shm-abi
//! §4.3): either the call sees the replacement and backs out, or the scan sees the marker and
//! waits. Request threads never unmap; the background thread does.

use crate::sync::{fence, AtomicPtr, AtomicU64, Mutex, Ordering::*};
use std::cell::Cell;

/// How many replaced snapshots may await reclamation at once.
const SLOTS: usize = 16;
/// How many entries a call tries before giving up (sdk-rust §3.1: "a few neighbours").
pub const ENTRY_PROBE: usize = 8;

/// The current snapshot, and the replaced ones not yet freed.
pub struct Published<T> {
    /// Id of the current snapshot; 0 when there is none.
    current: AtomicU64,
    /// `slots[id % SLOTS]` owns snapshot `id` from its publication until it is reclaimed.
    slots: [AtomicPtr<T>; SLOTS],
    next_id: Mutex<u64>,
}

/// A replaced snapshot, still allocated, that [`Published::reclaim`] may free. Dropping the
/// token leaks the snapshot, which is the safe failure: its memory stays mapped.
#[derive(Debug)]
#[must_use]
pub struct Retiring {
    id: u64,
}

impl Retiring {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl<T> Default for Published<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Published<T> {
    pub fn new() -> Self {
        Published { current: AtomicU64::new(0), slots: core::array::from_fn(|_| AtomicPtr::new(core::ptr::null_mut())), next_id: Mutex::new(1) }
    }

    /// Stop publishing in a child forked from this process (sdk-contract §7, "Fork"): its
    /// calls then find no snapshot. Keep the registration for as long as this lives.
    #[cfg(all(target_os = "linux", not(loom)))]
    pub fn clear_in_forked_child(&self) -> Option<crate::fork::Registration> {
        crate::fork::clear_in_child(&self.current)
    }

    /// Whether a snapshot is published. A hint: it may change at once.
    pub fn is_published(&self) -> bool {
        self.current.load(Acquire) != 0
    }

    /// Publish `v`, or clear the reference with `None`, and return a token for the snapshot
    /// that was current. Background thread only. Fails, handing `v` back, if `SLOTS`
    /// snapshots already await reclamation.
    pub fn replace(&self, v: Option<Box<T>>) -> Result<Option<Retiring>, Box<T>> {
        let mut next = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
        let new_id = match v {
            None => 0,
            Some(b) => {
                let mut id = *next;
                let mut tries = 0;
                while !self.slots[id as usize % SLOTS].load(Acquire).is_null() {
                    id = if id == u32::MAX as u64 { 1 } else { id + 1 };
                    tries += 1;
                    if tries == SLOTS {
                        return Err(b);
                    }
                }
                *next = if id == u32::MAX as u64 { 1 } else { id + 1 };
                self.slots[id as usize % SLOTS].store(Box::into_raw(b), Release);
                id
            }
        };
        let old = self.current.swap(new_id, SeqCst);
        fence(SeqCst); // the store of `current` before the scans that follow
        Ok((old != 0).then_some(Retiring { id: old }))
    }

    /// Free a replaced snapshot if no call can still be inside it, returning it so that the
    /// caller drops it (and so unmaps) where it chooses. `alive` says whether a thread id still
    /// exists: a marker left by a thread that has gone is ignored (sdk-rust §3.1).
    pub fn reclaim<E>(&self, r: Retiring, callers: &CallerTable<E>, alive: impl Fn(u32) -> bool) -> Result<Box<T>, Retiring> {
        fence(SeqCst);
        if callers.names(r.id, alive) {
            return Err(r);
        }
        let p = self.slots[r.id as usize % SLOTS].swap(core::ptr::null_mut(), AcqRel);
        assert!(!p.is_null(), "snapshot {} reclaimed twice", r.id);
        // SAFETY: `p` came from Box::into_raw in `replace`; it is no longer current, and no
        // marker names it, so no Guard refers to it and none can be created.
        Ok(unsafe { Box::from_raw(p) })
    }
}

impl<T> Drop for Published<T> {
    fn drop(&mut self) {
        for s in &self.slots {
            let p = s.swap(core::ptr::null_mut(), AcqRel);
            if !p.is_null() {
                // SAFETY: `&mut self`: no Guard borrows this cell any more.
                drop(unsafe { Box::from_raw(p) });
            }
        }
    }
}

/// One caller-state entry: the in-call marker, then whatever the SDK keeps per caller. Each
/// entry has its own cache lines.
#[repr(align(64))]
pub struct Entry<E> {
    in_call: AtomicU64,
    pub data: E,
}

pub struct CallerTable<E> {
    entries: Box<[Entry<E>]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnterError {
    /// Nothing is published, or the snapshot kept changing under the call.
    NoSnapshot,
    /// No entry could be claimed within the probe limit.
    NoEntry,
}

impl<E: Default> CallerTable<E> {
    pub fn new(n: usize) -> Self {
        assert!(n > 0);
        CallerTable { entries: (0..n).map(|_| Entry { in_call: AtomicU64::new(0), data: E::default() }).collect() }
    }
}

impl<E> CallerTable<E> {
    /// Claim an entry for the current snapshot of `p`. `hint` is the thread's index hint; `tid`
    /// its kernel thread id (non-zero).
    pub fn enter<'a, T>(&'a self, p: &'a Published<T>, hint: &Cell<u32>, tid: u32) -> Result<Guard<'a, T, E>, EnterError> {
        let n = self.entries.len();
        for _ in 0..4 {
            let id = p.current.load(SeqCst);
            if id == 0 {
                return Err(EnterError::NoSnapshot);
            }
            let marker = id << 32 | tid as u64;
            let start = hint.get() as usize % n;
            let i = (0..ENTRY_PROBE.min(n)).map(|k| (start + k) % n).find(|&i| self.entries[i].in_call.compare_exchange(0, marker, SeqCst, Relaxed).is_ok());
            let Some(i) = i else { return Err(EnterError::NoEntry) };
            hint.set(i as u32);
            fence(SeqCst); // the marker before the re-check
            if p.current.load(SeqCst) != id {
                self.entries[i].in_call.store(0, Release);
                continue;
            }
            let ptr = p.slots[id as usize % SLOTS].load(Acquire);
            debug_assert!(!ptr.is_null());
            // SAFETY: snapshot `id` was current after our marker was visible, so `reclaim`,
            // which runs only after `current` has moved on, will find the marker and leave
            // the snapshot alone until the guard clears it.
            let snap = unsafe { &*ptr };
            return Ok(Guard { entry: &self.entries[i], snap });
        }
        Err(EnterError::NoSnapshot)
    }

    /// Whether any live caller is inside snapshot `id`.
    pub fn names(&self, id: u64, alive: impl Fn(u32) -> bool) -> bool {
        self.entries.iter().any(|e| {
            let m = e.in_call.load(SeqCst);
            m >> 32 == id && alive(m as u32)
        })
    }

    /// Every entry's data, for summing counters.
    pub fn data(&self) -> impl Iterator<Item = &E> {
        self.entries.iter().map(|e| &e.data)
    }
}

/// A call's hold on one snapshot and one entry. Dropping it clears the marker.
pub struct Guard<'a, T, E> {
    entry: &'a Entry<E>,
    snap: &'a T,
}

impl<'a, T, E> Guard<'a, T, E> {
    pub fn snap(&self) -> &'a T {
        self.snap
    }
    pub fn data(&self) -> &'a E {
        &self.entry.data
    }
}

impl<T, E> Drop for Guard<'_, T, E> {
    fn drop(&mut self) {
        self.entry.in_call.store(0, Release);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[derive(Debug)]
    struct Snap {
        alive: Arc<AtomicBool>,
        v: u64,
    }
    impl Drop for Snap {
        fn drop(&mut self) {
            self.alive.store(false, SeqCst);
        }
    }

    #[test]
    fn a_snapshot_in_use_is_not_reclaimed() {
        let p = Published::<Snap>::new();
        let t = CallerTable::<()>::new(4);
        let alive = Arc::new(AtomicBool::new(true));
        assert!(p.replace(Some(Box::new(Snap { alive: alive.clone(), v: 1 }))).unwrap().is_none());
        let hint = Cell::new(0);
        let g = t.enter(&p, &hint, 77).unwrap();
        assert_eq!(g.snap().v, 1);
        let r = p.replace(None).unwrap().unwrap();
        assert_eq!(t.enter(&p, &hint, 77).err(), Some(EnterError::NoSnapshot));
        let r = p.reclaim(r, &t, |_| true).unwrap_err();
        assert!(alive.load(SeqCst));
        // A marker whose thread has gone does not hold the snapshot.
        let r = p.reclaim(r, &t, |tid| tid != 77).map(drop);
        assert!(r.is_ok());
        assert!(!alive.load(SeqCst));
        core::mem::forget(g); // its snapshot is gone; dropping would only clear the marker
    }

    #[test]
    fn calls_and_replacements_race_without_a_use_after_free() {
        let p = Arc::new(Published::<Snap>::new());
        let t = Arc::new(CallerTable::<()>::new(16));
        let stop = Arc::new(AtomicBool::new(false));
        let mut hs = Vec::new();
        for k in 0..4u32 {
            let (p, t, stop) = (p.clone(), t.clone(), stop.clone());
            hs.push(std::thread::spawn(move || {
                let hint = Cell::new(k);
                let mut seen = 0u64;
                while !stop.load(Relaxed) {
                    if let Ok(g) = t.enter(&p, &hint, 1000 + k) {
                        assert!(g.snap().alive.load(SeqCst), "a call saw a freed snapshot");
                        seen += g.snap().v;
                    }
                }
                seen
            }));
        }
        let mut pending: Vec<Retiring> = Vec::new();
        for v in 0..2000 {
            let s = Box::new(Snap { alive: Arc::new(AtomicBool::new(true)), v });
            if let Some(r) = p.replace(Some(s)).unwrap() {
                pending.push(r);
            }
            pending = pending.into_iter().filter_map(|r| p.reclaim(r, &t, |_| true).err()).collect();
        }
        stop.store(true, Relaxed);
        for h in hs {
            h.join().unwrap();
        }
    }
}
