//! The arena's invariant check (testing §3, universal assertion 4): every slot FREE with
//! WAITER and OWNER clear, every ring empty with consistent sequence numbers, every wake word
//! at rest. Used by `sczctl dump`, the conformance runner, and the hostile-input tests.

use crate::abi::{consumer_wait as cw, slot_state as ss};
use crate::layout::Directory;
use crate::ring::RingMem;
use crate::seg::Seg;
use crate::slot::SlotMem;
use crate::word;
use core::sync::atomic::Ordering::Relaxed;
use std::fmt;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub slots: [u32; 7],
    /// Each ring's tail: how many cells were ever claimed on it. The conformance runner
    /// compares them across a step to count what was published.
    pub tails: Vec<u64>,
    pub problems: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slots: Vec<String> = (0..7).map(|i| format!("\"{}\":{}", ss::name(i as u32), self.slots[i])).collect();
        let problems: Vec<String> = self.problems.iter().map(|p| format!("\"{}\"", p.replace('"', "'"))).collect();
        let tails: Vec<String> = self.tails.iter().map(u64::to_string).collect();
        write!(f, "{{\"ok\":{},\"slots\":{{{}}},\"tails\":[{}],\"problems\":[{}]}}", self.ok(), slots.join(","), tails.join(","), problems.join(","))
    }
}

/// Check an arena at rest: nothing queued, nothing held, nobody being woken. At most a few
/// problems of each kind are listed.
pub fn arena(_ctl: &Seg<'_>, data: &Seg<'_>, dir: &Directory) -> Report {
    let mut r = Report::default();
    for (class, &(_, _, c)) in dir.layout.slabs.iter().enumerate() {
        for i in 0..c.slot_count {
            let w = dir.slot(data, class, i).word().load(Relaxed);
            let st = word::state(w);
            r.slots[(st as usize).min(6)] += 1;
            if (st != ss::FREE || word::waiter(w) || word::owner(w) != 0) && r.problems.len() < 16 {
                r.problems.push(format!("slot {class}/{i} is {} (waiter {}, owner {})", ss::name(st), word::waiter(w), word::owner(w)));
            }
        }
    }
    for i in 0..dir.layout.rings.len() {
        let ring = dir.ring(data, i);
        let (head, tail) = (ring.head().load(Relaxed), ring.tail().load(Relaxed));
        r.tails.push(tail);
        if head != tail {
            r.problems.push(format!("ring {i} is not empty: head {head}, tail {tail}"));
        }
        let cap = ring.capacity();
        for p in head..head.wrapping_add(cap) {
            let seq = ring.cell(p)[0].load(Relaxed);
            if seq != p {
                r.problems.push(format!("ring {i} cell for position {p} has sequence {seq}"));
                break;
            }
        }
        let wait = ring.consumer_wait().load(Relaxed);
        if wait != cw::AWAKE && wait != cw::PARKED {
            r.problems.push(format!("ring {i}'s consumer_wait is {}", cw::name(wait)));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Config, Geometry, LocalArena};
    use crate::ring;

    #[test]
    fn a_fresh_arena_passes_and_a_used_one_does_not() {
        let a = LocalArena::new(&Geometry::default(), &Config::with_defaults(1, 1)).unwrap();
        assert!(arena(&a.ctl(), &a.data(), &a.dir).ok());
        let data = a.data();
        let s = a.dir.slot(&data, 0, 3);
        crate::slot::try_claim(&s).unwrap();
        ring::enqueue(&a.dir.ring(&data, 0), &ring::Descriptor::default()).unwrap();
        let rep = arena(&a.ctl(), &a.data(), &a.dir);
        assert_eq!(rep.problems.len(), 3, "the claimed slot, the non-empty ring, the published cell: {rep}");
    }
}
