//! Packing and unpacking the two contended words: the slot word and the gate word.

use crate::abi::{gate_word as gw, slot_word as sw};

// ── slot word: [ epoch:32 | reserved:21 | OWNER:2 | WAITER:1 | state:8 ] ──

#[inline]
pub const fn state(w: u64) -> u32 {
    ((w & sw::STATE_MASK) >> sw::STATE_SHIFT) as u32
}
#[inline]
pub const fn epoch(w: u64) -> u32 {
    ((w & sw::EPOCH_MASK) >> sw::EPOCH_SHIFT) as u32
}
#[inline]
pub const fn waiter(w: u64) -> bool {
    w & sw::WAITER_MASK != 0
}
#[inline]
pub const fn owner(w: u64) -> u32 {
    ((w & sw::OWNER_MASK) >> sw::OWNER_SHIFT) as u32
}
/// A word with the given epoch and state, `WAITER` and `OWNER` clear.
#[inline]
pub const fn slot(epoch: u32, state: u32) -> u64 {
    ((epoch as u64) << sw::EPOCH_SHIFT) | (((state as u64) << sw::STATE_SHIFT) & sw::STATE_MASK)
}
#[inline]
pub const fn with_owner(w: u64, owner: u32) -> u64 {
    (w & !sw::OWNER_MASK) | (((owner as u64) << sw::OWNER_SHIFT) & sw::OWNER_MASK)
}
#[inline]
pub const fn with_waiter(w: u64) -> u64 {
    w | sw::WAITER_MASK
}
/// The half of the word a futex waits on (little-endian: the half at the word's address).
#[inline]
pub const fn low32(w: u64) -> u32 {
    w as u32
}

// ── gate word: [ worker_gen:32 | shed_permille:16 | reason:8 | state:8 ] ──

#[inline]
pub const fn gate_state(g: u64) -> u32 {
    ((g & gw::STATE_MASK) >> gw::STATE_SHIFT) as u32
}
#[inline]
pub const fn gate_reason(g: u64) -> u32 {
    ((g & gw::REASON_MASK) >> gw::REASON_SHIFT) as u32
}
#[inline]
pub const fn gate_gen(g: u64) -> u32 {
    ((g & gw::WORKER_GEN_MASK) >> gw::WORKER_GEN_SHIFT) as u32
}
#[inline]
pub const fn gate(gen: u32, reason: u32, state: u32) -> u64 {
    ((gen as u64) << gw::WORKER_GEN_SHIFT) | (((reason as u64) << gw::REASON_SHIFT) & gw::REASON_MASK) | (((state as u64) << gw::STATE_SHIFT) & gw::STATE_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{gate_state as gs, slot_state as ss};

    #[test]
    fn slot_word_round_trips() {
        let w = with_waiter(with_owner(slot(0xDEAD_BEEF, ss::PROCESSING), 2));
        assert_eq!(epoch(w), 0xDEAD_BEEF);
        assert_eq!(state(w), ss::PROCESSING);
        assert_eq!(owner(w), 2);
        assert!(waiter(w));
        assert_eq!(low32(w) & 0xFF, ss::PROCESSING);
    }

    #[test]
    fn gate_word_round_trips() {
        let g = gate(7, 3, gs::HEALTHY);
        assert_eq!((gate_gen(g), gate_reason(g), gate_state(g)), (7, 3, gs::HEALTHY));
        assert_eq!(gate_state(0), gs::INIT, "a zeroed gate must read as not healthy");
    }
}
