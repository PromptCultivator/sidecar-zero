//! The model block of the control header and the registry it points to, under the model
//! seqlock (shm-abi §3, "The model seqlock").
//!
//! The supervisor is the only writer. Readers (SDKs binding, `sczctl dump`) copy the fields
//! and the registry bytes out between two reads of `model_seq`, with a bounded number of
//! attempts: a reader that gives up stays unbound and tries again later.

use crate::abi::{self, acc::control_header as ch};
use crate::seg::Seg;
use core::sync::atomic::{fence, Ordering::*};

/// Where the registry is placed in the control segment.
pub const REGISTRY_OFF: u32 = abi::CTL_HEADER_LEN as u32;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelBlock {
    pub sha256: [u8; 32],
    pub model_gen: u32,
    pub registry_epoch: u32,
    pub schema_fingerprint: u64,
    pub registry_len: u32,
}

/// Write the block and the registry bytes (supervisor only). `registry` must fit
/// `CTL_REGISTRY_LEN`.
pub fn write(ctl: &Seg<'_>, b: &ModelBlock, registry: &[u8]) {
    assert!(registry.len() <= abi::CTL_REGISTRY_LEN as usize && registry.len() as u32 == b.registry_len);
    let seq = ctl.u64_at(abi::control_header::MODEL_SEQ);
    let s = seq.load(Relaxed);
    seq.store(s.wrapping_add(1) | 1, Relaxed); // odd: an update is in progress
    fence(Release);
    ch::set_model_sha256(ctl, 0, &b.sha256);
    ch::set_model_gen(ctl, 0, b.model_gen);
    ch::set_registry_epoch(ctl, 0, b.registry_epoch);
    ch::set_schema_fingerprint(ctl, 0, b.schema_fingerprint);
    ch::set_registry_off(ctl, 0, REGISTRY_OFF);
    ch::set_registry_len(ctl, 0, b.registry_len);
    for (i, chunk) in registry.chunks(8).enumerate() {
        let mut w = [0u8; 8];
        w[..chunk.len()].copy_from_slice(chunk);
        ctl.u64_at(REGISTRY_OFF as usize + i * 8).store(u64::from_le_bytes(w), Relaxed);
    }
    seq.store((s.wrapping_add(1) | 1).wrapping_add(1), Release); // even again
}

/// Read the block and a copy of the registry, or None if the writer stayed busy for
/// `attempts` tries, or the block names a registry outside the segment.
pub fn read(ctl: &Seg<'_>, attempts: u32) -> Option<(ModelBlock, Vec<u8>)> {
    let seq = ctl.u64_at(abi::control_header::MODEL_SEQ);
    for _ in 0..attempts {
        let s1 = seq.load(Acquire);
        if s1 & 1 == 1 {
            core::hint::spin_loop();
            continue;
        }
        let b = ModelBlock {
            sha256: ch::model_sha256(ctl, 0),
            model_gen: ch::model_gen(ctl, 0),
            registry_epoch: ch::registry_epoch(ctl, 0),
            schema_fingerprint: ch::schema_fingerprint(ctl, 0),
            registry_len: ch::registry_len(ctl, 0),
        };
        let off = ch::registry_off(ctl, 0);
        let len = b.registry_len as usize;
        let fits = len == 0 || (off == REGISTRY_OFF && len <= abi::CTL_REGISTRY_LEN as usize);
        let mut bytes = Vec::new();
        if fits && len > 0 {
            bytes.reserve(len.div_ceil(8) * 8);
            for i in 0..len.div_ceil(8) {
                bytes.extend_from_slice(&ctl.u64_at(off as usize + i * 8).load(Relaxed).to_le_bytes());
            }
            bytes.truncate(len);
        }
        fence(Acquire);
        if seq.load(Relaxed) == s1 {
            return fits.then_some((b, bytes));
        }
    }
    None
}

/// The registry epoch alone, under the seqlock: what a client re-reads when the gate shows a
/// new worker generation (arrow-data-plane §9).
pub fn epoch(ctl: &Seg<'_>, attempts: u32) -> Option<u32> {
    let seq = ctl.u64_at(abi::control_header::MODEL_SEQ);
    for _ in 0..attempts {
        let s1 = seq.load(Acquire);
        if s1 & 1 == 0 {
            let e = ch::registry_epoch(ctl, 0);
            fence(Acquire);
            if seq.load(Relaxed) == s1 {
                return Some(e);
            }
        }
        core::hint::spin_loop();
    }
    None
}
