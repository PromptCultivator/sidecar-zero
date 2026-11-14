//! The upgrade test's next minor version (shm-abi §12.5). Built with the `next-minor` feature,
//! a supervisor, worker, or client uses every allowance that shm-abi §12.4 says a 1.0 reader
//! must tolerate: reserved bytes filled with a pattern, a completion ring directory and ring
//! (shm-abi §12.3) after the regions a 1.0 reader knows, a larger `data_len`, and feature and
//! `HELLO` flag bits no 1.0 reader knows. Without the feature every function here does nothing
//! and every constant is zero, so a release build is the 1.0 layout exactly.

use crate::abi;
#[cfg(not(loom))]
use crate::{layout::Layout, seg::Seg};
#[cfg(not(loom))]
use std::sync::atomic::Ordering::Relaxed;

/// Whether this build is the next minor.
pub const ENABLED: bool = cfg!(feature = "next-minor");

/// The byte reserved space is filled with: easy to spot in a dump.
#[cfg(not(loom))]
const PATTERN: u8 = 0xA5;
const P16: u16 = 0xA5A5;
const P32: u32 = 0xA5A5_A5A5;
const P64: u64 = 0xA5A5_A5A5_A5A5_A5A5;

/// The control header's feature bits: async completion (shm-abi §12.3) and one no version
/// defines.
pub const FEATURE_FLAGS: u64 = if ENABLED { 1 | 1 << 62 } else { 0 };
/// `HELLO.flags`: asking for async mode, and a bit no version defines.
pub const HELLO_FLAGS: u64 = if ENABLED { 1 | 1 << 63 } else { 0 };

const COMPLETION_CAP: u64 = 64;
/// The completion ring directory of shm-abi §12.3: a count, then `{ ring_off u64, capacity
/// u32, cell_stride u32 }` per entry.
#[cfg(not(loom))]
const COMPLETION_COUNT: usize = 0x738;
#[cfg(not(loom))]
const COMPLETION_DIR: usize = 0x740;

// The control header's reserved ranges, and those of each directory entry in use: every byte
// abi/scz_abi.toml gives no field, before the supervisor's counters at 0x800.
#[cfg(not(loom))]
const CTL_RESERVED: [(usize, usize); 7] = [(0x060, 0x080), (0x090, 0x100), (0x140, 0x200), (0x228, 0x400), (0x714, 0x720), (0x720, 0x738), (0x750, 0x800)];
#[cfg(not(loom))]
const RING_DIR_RESERVED: (usize, usize) = (0x14, 0x20);
#[cfg(not(loom))]
const SLAB_DIR_RESERVED: (usize, usize) = (0x26, 0x40);
#[cfg(not(loom))]
const STATUS_RESERVED: [(usize, usize); 2] = [(0x010, 0x040), (0x0D0, 0x800)];
#[cfg(not(loom))]
const RING_HEADER_RESERVED: [(usize, usize); 4] = [(0x04, 0x40), (0x48, 0x80), (0x88, 0xC0), (0xC4, 0x100)];
#[cfg(not(loom))]
const BEAT_RESERVED: [(usize, usize); 2] = [(0x1C, 0x20), (0x28, 0x40)];
#[cfg(not(loom))]
const STATS_RESERVED: (usize, usize) = (0x40, 0x400);

#[cfg(not(loom))]
fn fill(s: &Seg<'_>, base: usize, (a, b): (usize, usize)) {
    for off in base + a..base + b {
        s.u8_at(off).store(PATTERN, Relaxed);
    }
}

#[cfg(not(loom))]
/// Where the next minor's completion ring goes: after the last slab class, on a page boundary.
pub fn completion_off(layout: &Layout) -> u64 {
    let end = layout.slabs.iter().map(|&(_, data, c)| data + c.slot_count as u64 * c.slot_size as u64).max().unwrap_or(0);
    end.div_ceil(4096) * 4096
}

/// The data segment's length for regions ending at `end`: the 1.0 rule, or, in the next minor,
/// room for the completion ring and then twice that, so a reader sees a segment larger than
/// its regions need.
pub fn data_len(end: u64) -> u64 {
    if !ENABLED {
        return end.next_power_of_two();
    }
    let comp = end.div_ceil(4096) * 4096 + abi::RING_HEADER_LEN + COMPLETION_CAP * abi::CELL_LEN;
    comp.next_power_of_two() * 2
}

#[cfg(not(loom))]
/// Fill the control header's reserved bytes and write the completion ring directory. Called
/// before the layout checksum is computed, which therefore covers them.
pub fn fill_control(ctl: &Seg<'_>, layout: &Layout) {
    if !ENABLED {
        return;
    }
    for r in CTL_RESERVED {
        fill(ctl, 0, r);
    }
    for i in 0..layout.rings.len() {
        fill(ctl, abi::control_header::ring_dir::OFF + i * abi::control_header::ring_dir::STRIDE, RING_DIR_RESERVED);
    }
    for i in 0..layout.slabs.len() {
        fill(ctl, abi::control_header::slab_dir::OFF + i * abi::control_header::slab_dir::STRIDE, SLAB_DIR_RESERVED);
    }
    ctl.u16_at(COMPLETION_COUNT).store(1, Relaxed);
    ctl.u64_at(COMPLETION_DIR).store(completion_off(layout), Relaxed);
    ctl.u32_at(COMPLETION_DIR + 8).store(COMPLETION_CAP as u32, Relaxed);
    ctl.u32_at(COMPLETION_DIR + 12).store(abi::CELL_LEN as u32, Relaxed);
}

#[cfg(not(loom))]
/// Fill the status page's reserved bytes, and every ring header's, and lay out the completion
/// ring (cells numbered as in any ring).
pub fn fill_data(data: &Seg<'_>, layout: &Layout) {
    if !ENABLED {
        return;
    }
    for r in STATUS_RESERVED {
        fill(data, layout.status_off as usize, r);
    }
    for &(off, _) in &layout.rings {
        fill_ring_header(data, off as usize);
    }
    let comp = completion_off(layout) as usize;
    for i in 0..COMPLETION_CAP as usize {
        data.u64_at(comp + abi::RING_HEADER_LEN as usize + i * abi::CELL_LEN as usize).store(i as u64, Relaxed);
    }
}

#[cfg(not(loom))]
/// Fill one ring header's reserved bytes (after a reset re-initializes the ring, too).
pub fn fill_ring_header(data: &Seg<'_>, off: usize) {
    if ENABLED {
        for r in RING_HEADER_RESERVED {
            fill(data, off, r);
        }
    }
}

#[cfg(not(loom))]
/// A worker's part: the reserved bytes of its beat and of its statistics block.
pub fn fill_worker(data: &Seg<'_>, status_off: usize, beat_slot: usize) {
    if !ENABLED {
        return;
    }
    let beat = status_off + abi::status_page::beat::OFF + beat_slot * abi::status_page::beat::STRIDE;
    for r in BEAT_RESERVED {
        fill(data, beat, r);
    }
    fill(data, status_off + abi::status_page::worker_stats::OFF + beat_slot * abi::status_page::worker_stats::STRIDE, STATS_RESERVED);
}

/// A client's part, for the cell words that hold reserved bytes: word 2 (0x10), the low half
/// of word 3 (0x18), and the top 16 bits of word 5 (0x2E).
pub const fn cell_words(w3: u64, w5: u64) -> (u64, u64, u64) {
    if ENABLED {
        (P64, w3 | P32 as u64, w5 | (P16 as u64) << 48)
    } else {
        (0, w3, w5)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::layout::{Config, Geometry, LocalArena};

    /// The next minor's arena is what shm-abi §12.5 says, and a reader of this version still
    /// finds its way around it; without the feature, the arena is the plain one.
    #[test]
    fn the_next_minor_uses_every_allowance_and_still_reads() {
        let a = LocalArena::new(&Geometry::default(), &Config::with_defaults(1, 1)).unwrap();
        let (ctl, data) = (a.ctl(), a.data());
        let plain = Layout::compute(&Geometry::default()).unwrap();
        let byte = |s: &Seg<'_>, off: usize| s.u8_at(off).load(Relaxed);
        let ring0 = a.dir.layout.rings[0].0 as usize;
        if ENABLED {
            assert_eq!(crate::abi::acc::control_header::abi_minor(&ctl, 0), abi::ABI_MINOR);
            for (s, off) in [(&ctl, 0x060), (&ctl, 0x3FF), (&ctl, 0x7FF), (&data, 0x010), (&data, 0x7FF), (&data, ring0 + 0x04)] {
                assert_eq!(byte(s, off), PATTERN, "reserved byte at {off:#x}");
            }
            assert_eq!(ctl.u16_at(COMPLETION_COUNT).load(Relaxed), 1);
            assert!(a.dir.data_len >= 2 * (completion_off(&plain) + abi::RING_HEADER_LEN + COMPLETION_CAP * abi::CELL_LEN));
        } else {
            assert_eq!(crate::abi::acc::control_header::feature_flags(&ctl, 0), 0);
            for (s, off) in [(&ctl, 0x060), (&ctl, 0x7FF), (&data, 0x010), (&data, ring0 + 0x04)] {
                assert_eq!(byte(s, off), 0, "reserved byte at {off:#x}");
            }
            assert_eq!(a.dir.data_len, plain.data_len);
        }
    }
}
