//! The request format the conformance agents send and the chaos worker answers (testing §3,
//! worker §7.4). Every agent, in every language, builds requests exactly like [`request`], so
//! the runner can recompute each call's request from what the agent printed and check the
//! answer against [`answer`].
//!
//! A request is little-endian u64 words:
//!
//! | Word | Value |
//! |---|---|
//! | 0 | [`MAGIC`] |
//! | 1 | opcode (`op::*`) |
//! | 2 | the opcode's argument |
//! | 3 | nonce: calling thread << 32 \| sequence number |
//! | 4 | seed |
//! | 5.. | `rows` filler words, `splitmix64(seed ^ nonce ^ i)` |
//!
//! The answer to a request is 8 bytes: [`answer`], the FNV-1a hash of every request byte. A
//! response that belongs to another call, or to an earlier use of the slot, is then a wrong
//! value rather than a plausible one. A request without the magic is echoed, as by the echo
//! worker.

#![forbid(unsafe_code)]

pub const MAGIC: u64 = u64::from_le_bytes(*b"SCZCHAOS");
pub const HEADER_WORDS: usize = 5;

/// Per-request opcodes. Each lands on exactly the call that carries it.
pub mod op {
    /// Answer with the keyed value.
    pub const NORMAL: u64 = 0;
    /// Take `arg` microseconds (spinning up to 2 ms, sleeping beyond), then answer.
    pub const DELAY: u64 = 1;
    /// `SIGKILL` the worker: `arg` 1 during processing (slot claimed, nothing written),
    /// 2 after the response is written but before the slot is `DONE`.
    pub const CRASH: u64 = 2;
    /// Never finish (`arg` 0) or finish after `arg` milliseconds; the heartbeat goes on.
    pub const HANG: u64 = 3;
    /// Complete with status `arg` (shm-abi `status`) and no response.
    pub const STATUS: u64 = 4;
    /// Complete `OK` but with response length `arg`, which the client must refuse.
    pub const BAD_LEN: u64 = 5;
    /// Answer, but wait `arg` milliseconds between completing the slot and waking a parked
    /// client: a stalled waker, which the client's late-wake counter must see.
    pub const STALL_WAKE: u64 = 6;

    pub const ALL: [(&str, u64); 7] =
        [("normal", NORMAL), ("delay", DELAY), ("crash", CRASH), ("hang", HANG), ("status", STATUS), ("bad_len", BAD_LEN), ("stall_wake", STALL_WAKE)];

    pub fn parse(s: &str) -> Option<u64> {
        ALL.iter().find(|(n, _)| *n == s).map(|x| x.1).or_else(|| s.parse().ok())
    }
}

pub fn nonce(thread: u32, seq: u32) -> u64 {
    (thread as u64) << 32 | seq as u64
}

pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The request bytes for one call.
pub fn request(op: u64, arg: u64, thread: u32, seq: u32, seed: u64, rows: usize) -> Vec<u8> {
    let n = nonce(thread, seq);
    let words = [MAGIC, op, arg, n, seed].into_iter().chain((0..rows as u64).map(|i| splitmix64(seed ^ n ^ i)));
    words.flat_map(u64::to_le_bytes).collect()
}

pub fn fnv1a64(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| (h ^ x as u64).wrapping_mul(0x0100_0000_01b3))
}

/// What the chaos worker answers to `req`.
pub fn answer(req: &[u8]) -> u64 {
    fnv1a64(req)
}

/// The opcode and argument of a keyed request; None for anything else.
pub fn parse(req: &[u8]) -> Option<(u64, u64)> {
    if req.len() < HEADER_WORDS * 8 {
        return None;
    }
    let w = |i: usize| u64::from_le_bytes(req[i * 8..i * 8 + 8].try_into().unwrap());
    (w(0) == MAGIC).then(|| (w(1), w(2)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_and_answers_are_keyed() {
        let a = request(op::DELAY, 7, 1, 2, 3, 4);
        assert_eq!(a.len(), (HEADER_WORDS + 4) * 8);
        assert_eq!(parse(&a), Some((op::DELAY, 7)));
        assert_ne!(answer(&a), answer(&request(op::DELAY, 7, 1, 3, 3, 4)));
        assert_eq!(parse(&a[..39]), None);
        assert_eq!(op::parse("bad_len"), Some(op::BAD_LEN));
        // The reference vector other languages check against.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
