//! Where everything sits in the two segments: computing a layout, writing it, and reading it
//! back. The placement rule is `[layout]` in abi/scz_abi.toml; the golden generator
//! implements the same rule on its own, and the golden tests compare the two.

use crate::abi::{self, acc};
use crate::crc::crc32c;
use crate::ring;
use crate::seg::{Seg, ShmRing, ShmSlot};
use core::sync::atomic::Ordering::Relaxed;

/// What a supervisor asks for.
#[derive(Clone, Debug)]
pub struct Geometry {
    pub rings: Vec<u64>, // capacities
    pub slab_classes: Vec<SlabClass>,
    pub staging_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlabClass {
    pub slot_size: u32,
    pub slot_count: u32,
    pub req_cap: u32,
    pub resp_cap: u32,
}

impl Default for Geometry {
    /// The default sizing in shm-abi §2: one ring of 4,096 cells and 1,024 slots of 4 KB.
    fn default() -> Self {
        Geometry { rings: vec![4096], slab_classes: vec![SlabClass { slot_size: 4096, slot_count: 1024, req_cap: 2048, resp_cap: 2048 }], staging_len: 0x10000 }
    }
}

/// Per-arena values that are not layout.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub arena_id: u64,
    pub created_mono_ns: u64,
    pub hb_interval_ns: u64,
    pub hb_stale_ns: u64,
    pub spin_ns: u64,
    pub default_deadline_ns: u64,
    pub max_deadline_ns: u64,
}

impl Config {
    pub fn with_defaults(arena_id: u64, created_mono_ns: u64) -> Config {
        use abi::defaults as d;
        Config {
            arena_id,
            created_mono_ns,
            hb_interval_ns: d::HB_INTERVAL_NS,
            hb_stale_ns: d::HB_STALE_NS,
            spin_ns: d::SPIN_NS,
            default_deadline_ns: d::DEFAULT_DEADLINE_NS,
            max_deadline_ns: d::MAX_DEADLINE_NS,
        }
    }
}

/// Where each region of the data segment starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub status_off: u64,
    pub staging_off: u64,
    pub staging_len: u64,
    pub rings: Vec<(u64, u64)>,            // (offset, capacity)
    pub slabs: Vec<(u64, u64, SlabClass)>, // (table offset, data offset, class)
    pub data_len: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    TooManyRings,
    TooManyClasses,
    BadCapacity(u64),
    BadSlot(&'static str),
    BadMagic,
    AbiMajor(u16),
    AbiMinor(u16),
    BadChecksum,
    BadDirectory(&'static str),
    WrongSize(&'static str),
}

const PAGE: u64 = 4096;
fn up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

impl Layout {
    /// The placement rule of abi/scz_abi.toml `[layout]`.
    pub fn compute(g: &Geometry) -> Result<Layout, LayoutError> {
        if g.rings.is_empty() || g.rings.len() > abi::MAX_RINGS as usize {
            return Err(LayoutError::TooManyRings);
        }
        if g.slab_classes.is_empty() || g.slab_classes.len() > abi::MAX_SLAB_CLASSES as usize {
            return Err(LayoutError::TooManyClasses);
        }
        let mut end = abi::STATUS_PAGE_LEN;
        let staging_off = end;
        end += abi::BEAT_SLOTS * g.staging_len;
        let mut rings = Vec::new();
        for &cap in &g.rings {
            if !cap.is_power_of_two() || !(2..=1 << 20).contains(&cap) {
                return Err(LayoutError::BadCapacity(cap));
            }
            let off = up(end, PAGE);
            rings.push((off, cap));
            end = off + abi::RING_HEADER_LEN + cap * abi::CELL_LEN;
        }
        let mut slabs = Vec::new();
        for c in &g.slab_classes {
            if c.slot_size == 0 || c.slot_size % 64 != 0 || c.slot_count == 0 {
                return Err(LayoutError::BadSlot("slot_size must be a non-zero multiple of 64, and slot_count non-zero"));
            }
            if c.req_cap % 64 != 0 || c.req_cap as u64 + c.resp_cap as u64 > c.slot_size as u64 {
                return Err(LayoutError::BadSlot("req_cap must be a multiple of 64 and req_cap + resp_cap must fit the slot"));
            }
            let table = up(end, PAGE);
            end = table + c.slot_count as u64 * abi::SLOT_RECORD_LEN;
            let data = up(end, PAGE);
            end = data + c.slot_count as u64 * c.slot_size as u64;
            slabs.push((table, data, *c));
        }
        Ok(Layout { status_off: 0, staging_off, staging_len: g.staging_len, rings, slabs, data_len: crate::next::data_len(end) })
    }
}

/// The control segment's length: the header plus space for the registry.
pub const CTL_LEN: u64 = abi::CTL_HEADER_LEN + abi::CTL_REGISTRY_LEN;

/// Write the control header for `layout`. Only before the segment is shared.
pub fn write_control(ctl: &Seg<'_>, layout: &Layout, cfg: &Config) {
    use acc::control_header as h;
    use acc::{data_map as dm, ring_dir_entry as rd, slab_dir_entry as sd};
    h::set_magic(ctl, 0, abi::CTL_MAGIC);
    h::set_abi_major(ctl, 0, abi::ABI_MAJOR);
    h::set_abi_minor(ctl, 0, abi::ABI_MINOR);
    h::set_header_len(ctl, 0, abi::CTL_HEADER_LEN as u32);
    h::set_data_len(ctl, 0, layout.data_len);
    h::set_arena_id(ctl, 0, cfg.arena_id);
    h::set_ctl_len(ctl, 0, ctl.len() as u32);
    h::set_page_size(ctl, 0, PAGE as u32);
    h::set_ring_count(ctl, 0, layout.rings.len() as u16);
    h::set_slab_class_count(ctl, 0, layout.slabs.len() as u16);
    h::set_feature_flags(ctl, 0, crate::next::FEATURE_FLAGS);
    h::set_created_mono_ns(ctl, 0, cfg.created_mono_ns);
    h::set_hb_interval_ns(ctl, 0, cfg.hb_interval_ns);
    h::set_hb_stale_ns(ctl, 0, cfg.hb_stale_ns);
    h::set_spin_ns(ctl, 0, cfg.spin_ns);
    h::set_default_deadline_ns(ctl, 0, cfg.default_deadline_ns);
    h::set_max_deadline_ns(ctl, 0, cfg.max_deadline_ns);
    for (i, &(off, cap)) in layout.rings.iter().enumerate() {
        let b = abi::control_header::ring_dir::OFF + i * abi::control_header::ring_dir::STRIDE;
        rd::set_ring_off(ctl, b, off);
        rd::set_capacity(ctl, b, cap as u32);
        rd::set_kind(ctl, b, abi::ring_kind::MPSC);
        rd::set_cell_stride(ctl, b, abi::CELL_LEN as u32);
    }
    for (i, &(table, data, c)) in layout.slabs.iter().enumerate() {
        let b = abi::control_header::slab_dir::OFF + i * abi::control_header::slab_dir::STRIDE;
        sd::set_table_off(ctl, b, table);
        sd::set_data_off(ctl, b, data);
        sd::set_slot_size(ctl, b, c.slot_size);
        sd::set_slot_count(ctl, b, c.slot_count);
        sd::set_req_cap(ctl, b, c.req_cap);
        sd::set_resp_cap(ctl, b, c.resp_cap);
        sd::set_record_stride(ctl, b, abi::SLOT_RECORD_LEN as u16);
    }
    let b = abi::control_header::data_map::OFF;
    dm::set_status_off(ctl, b, layout.status_off);
    dm::set_staging_off(ctl, b, layout.staging_off);
    dm::set_staging_len(ctl, b, layout.staging_len as u32);
    crate::next::fill_control(ctl, layout);
    h::set_layout_crc32c(ctl, 0, layout_crc(ctl));
}

/// CRC-32C over the header's IDENT, CONFIG, and DIRECTORY, with the checksum field as zero.
pub fn layout_crc(ctl: &Seg<'_>) -> u32 {
    let mut header = [0u8; abi::CTL_HEADER_LEN as usize];
    ctl.copy_out(0, &mut header);
    header[abi::control_header::LAYOUT_CRC32C..abi::control_header::LAYOUT_CRC32C + 4].fill(0);
    let parts: Vec<&[u8]> = abi::control_header::CRC_RANGES.iter().map(|&(a, b)| &header[a..b]).collect();
    crc32c(&parts)
}

/// Initialize a fresh, zeroed data segment. Only before it is shared.
pub fn write_data(data: &Seg<'_>, layout: &Layout, cfg: &Config) {
    use acc::status_page as sp;
    sp::set_magic(data, layout.status_off as usize, abi::DATA_MAGIC);
    sp::set_arena_id(data, layout.status_off as usize, cfg.arena_id);
    for (i, &(off, cap)) in layout.rings.iter().enumerate() {
        data.u32_at(off as usize + abi::ring_header::RING_ID).store(i as u32, Relaxed);
        ring::init(&ShmRing { seg: *data, off: off as usize, capacity: cap });
    }
    crate::next::fill_data(data, layout);
}

/// The session reset's re-initialization (supervisor §4): every ring fresh, every slot FREE
/// with its record cleared and its epoch kept, so that a descriptor from before the reset can
/// never match a slot claimed after it. Whatever the data segment held, the arena afterwards
/// passes the invariant check (`check::arena`).
pub fn reset_data(data: &Seg<'_>, dir: &Directory) {
    for i in 0..dir.layout.rings.len() {
        let r = dir.ring(data, i);
        ring::init(&r);
        crate::next::fill_ring_header(data, dir.layout.rings[i].0 as usize);
    }
    for (class, &(_, _, c)) in dir.layout.slabs.iter().enumerate() {
        for i in 0..c.slot_count {
            let s = dir.slot(data, class, i);
            let w = crate::slot::SlotMem::word(&s);
            let e = crate::word::epoch(w.load(Relaxed));
            for off in (8..abi::SLOT_RECORD_LEN as usize).step_by(8) {
                data.u64_at(s.rec + off).store(0, Relaxed);
            }
            w.store(crate::word::slot(e, abi::slot_state::FREE), Relaxed);
        }
    }
}

/// Everything a client or worker needs to find its way around, read once from the control
/// segment and checked. Nothing in the data segment describes its own layout (shm-abi §1).
#[derive(Clone, Debug)]
pub struct Directory {
    pub arena_id: u64,
    pub data_len: u64,
    pub status_off: u64,
    pub layout: Layout,
    pub hb_interval_ns: u64,
    pub hb_stale_ns: u64,
    pub spin_ns: u64,
    pub default_deadline_ns: u64,
    pub max_deadline_ns: u64,
}

impl Directory {
    pub fn read(ctl: &Seg<'_>, data_len_actual: u64) -> Result<Directory, LayoutError> {
        use acc::control_header as h;
        use acc::{data_map as dm, ring_dir_entry as rd, slab_dir_entry as sd};
        if h::magic(ctl, 0) != abi::CTL_MAGIC {
            return Err(LayoutError::BadMagic);
        }
        if h::abi_major(ctl, 0) != abi::ABI_MAJOR {
            return Err(LayoutError::AbiMajor(h::abi_major(ctl, 0)));
        }
        // Always false while ABI_MINOR_MIN is 0; the rule for a reader that needs a later minor.
        #[allow(clippy::absurd_extreme_comparisons)]
        if h::abi_minor(ctl, 0) < abi::ABI_MINOR_MIN {
            return Err(LayoutError::AbiMinor(h::abi_minor(ctl, 0)));
        }
        if h::layout_crc32c(ctl, 0) != layout_crc(ctl) {
            return Err(LayoutError::BadChecksum);
        }
        let data_len = h::data_len(ctl, 0);
        if data_len != data_len_actual {
            return Err(LayoutError::WrongSize("data segment"));
        }
        if h::ctl_len(ctl, 0) as usize != ctl.len() {
            return Err(LayoutError::WrongSize("control segment"));
        }
        let within = |off: u64, len: u64| off.checked_add(len).is_some_and(|e| e <= data_len) && off.is_multiple_of(64);
        let nrings = h::ring_count(ctl, 0) as usize;
        let nclasses = h::slab_class_count(ctl, 0) as usize;
        if nrings == 0 || nrings > abi::MAX_RINGS as usize || nclasses == 0 || nclasses > abi::MAX_SLAB_CLASSES as usize {
            return Err(LayoutError::BadDirectory("ring or slab class count"));
        }
        let mut rings = Vec::new();
        for i in 0..nrings {
            let b = abi::control_header::ring_dir::OFF + i * abi::control_header::ring_dir::STRIDE;
            let (off, cap) = (rd::ring_off(ctl, b), rd::capacity(ctl, b) as u64);
            if rd::kind(ctl, b) != abi::ring_kind::MPSC
                || rd::cell_stride(ctl, b) as u64 != abi::CELL_LEN
                || !cap.is_power_of_two()
                || !within(off, abi::RING_HEADER_LEN + cap * abi::CELL_LEN)
            {
                return Err(LayoutError::BadDirectory("ring"));
            }
            rings.push((off, cap));
        }
        let mut slabs = Vec::new();
        for i in 0..nclasses {
            let b = abi::control_header::slab_dir::OFF + i * abi::control_header::slab_dir::STRIDE;
            let c = SlabClass {
                slot_size: sd::slot_size(ctl, b),
                slot_count: sd::slot_count(ctl, b),
                req_cap: sd::req_cap(ctl, b),
                resp_cap: sd::resp_cap(ctl, b),
            };
            let (table, data) = (sd::table_off(ctl, b), sd::data_off(ctl, b));
            if sd::record_stride(ctl, b) as u64 != abi::SLOT_RECORD_LEN
                || c.req_cap as u64 + c.resp_cap as u64 > c.slot_size as u64
                || !within(table, c.slot_count as u64 * abi::SLOT_RECORD_LEN)
                || !within(data, c.slot_count as u64 * c.slot_size as u64)
            {
                return Err(LayoutError::BadDirectory("slab class"));
            }
            slabs.push((table, data, c));
        }
        let m = abi::control_header::data_map::OFF;
        let status_off = dm::status_off(ctl, m);
        if !within(status_off, abi::STATUS_PAGE_LEN) {
            return Err(LayoutError::BadDirectory("status page"));
        }
        let layout = Layout { status_off, staging_off: dm::staging_off(ctl, m), staging_len: dm::staging_len(ctl, m) as u64, rings, slabs, data_len };
        Ok(Directory {
            arena_id: h::arena_id(ctl, 0),
            data_len,
            status_off,
            layout,
            hb_interval_ns: h::hb_interval_ns(ctl, 0),
            hb_stale_ns: h::hb_stale_ns(ctl, 0),
            spin_ns: h::spin_ns(ctl, 0),
            default_deadline_ns: h::default_deadline_ns(ctl, 0),
            max_deadline_ns: h::max_deadline_ns(ctl, 0),
        })
    }

    pub fn ring<'a>(&self, data: &Seg<'a>, i: usize) -> ShmRing<'a> {
        let (off, cap) = self.layout.rings[i];
        ShmRing { seg: *data, off: off as usize, capacity: cap }
    }

    pub fn slot<'a>(&self, data: &Seg<'a>, class: usize, index: u32) -> ShmSlot<'a> {
        let (table, d, c) = self.layout.slabs[class];
        assert!(index < c.slot_count);
        ShmSlot {
            seg: *data,
            rec: (table + index as u64 * abi::SLOT_RECORD_LEN) as usize,
            data: (d + index as u64 * c.slot_size as u64) as usize,
            req_cap: c.req_cap as usize,
            resp_cap: c.resp_cap as usize,
        }
    }

    /// The slot a descriptor names, if it names one: its class and index are bounds-checked
    /// against this directory before any slot memory is touched (worker §4 step 1).
    pub fn locate<'a>(&self, data: &Seg<'a>, class: u16, index: u32) -> Option<ShmSlot<'a>> {
        let c = self.layout.slabs.get(class as usize)?.2;
        (index < c.slot_count).then(|| self.slot(data, class as usize, index))
    }

    /// Offset of beat slot `i` in the data segment.
    pub fn beat(&self, i: usize) -> usize {
        assert!(i < abi::BEAT_SLOTS as usize);
        self.status_off as usize + abi::status_page::beat::OFF + i * abi::status_page::beat::STRIDE
    }
}

/// Page-aligned, zeroed memory standing in for a segment: tests and the in-process arena.
pub struct LocalSegment {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl LocalSegment {
    pub fn new(len: usize) -> LocalSegment {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        // SAFETY: non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        LocalSegment { ptr, layout }
    }
    pub fn seg(&self) -> Seg<'_> {
        // SAFETY: the allocation lives as long as the borrow of self.
        unsafe { Seg::new(self.ptr, self.layout.size()) }
    }
}

impl Drop for LocalSegment {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout in `new`.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

/// A complete arena in ordinary memory. No other process can map it; it exists so the
/// protocol can be exercised in one process, on any platform.
pub struct LocalArena {
    pub ctl_mem: LocalSegment,
    pub data_mem: LocalSegment,
    pub dir: Directory,
}

impl LocalArena {
    pub fn new(g: &Geometry, cfg: &Config) -> Result<LocalArena, LayoutError> {
        let layout = Layout::compute(g)?;
        let ctl_mem = LocalSegment::new(CTL_LEN as usize);
        let data_mem = LocalSegment::new(layout.data_len as usize);
        write_data(&data_mem.seg(), &layout, cfg);
        write_control(&ctl_mem.seg(), &layout, cfg);
        let dir = Directory::read(&ctl_mem.seg(), layout.data_len)?;
        Ok(LocalArena { ctl_mem, data_mem, dir })
    }
    pub fn ctl(&self) -> Seg<'_> {
        self.ctl_mem.seg()
    }
    pub fn data(&self) -> Seg<'_> {
        self.data_mem.seg()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sizing_matches_the_abi_doc() {
        // shm-abi §2: "About 4.6 MB, in an 8 MB segment".
        let l = Layout::compute(&Geometry::default()).unwrap();
        assert_eq!(l.data_len, 8 << 20);
        let end = l.slabs[0].1 + 1024 * 4096;
        assert!(end > 4_500_000 && end < 4_800_000, "{end}");
    }

    #[test]
    fn the_directory_reads_back_what_was_written() {
        let g = Geometry {
            rings: vec![8, 16],
            slab_classes: vec![SlabClass { slot_size: 4096, slot_count: 4, req_cap: 2048, resp_cap: 2048 }],
            staging_len: 0x1000,
        };
        let a = LocalArena::new(&g, &Config::with_defaults(42, 1)).unwrap();
        assert_eq!(a.dir.layout, Layout::compute(&g).unwrap());
        assert_eq!(a.dir.arena_id, 42);
    }

    #[test]
    fn a_damaged_header_is_refused() {
        let a = LocalArena::new(&Geometry::default(), &Config::with_defaults(1, 1)).unwrap();
        let ctl = a.ctl();
        let len = a.dir.data_len;
        // Change a ring capacity without fixing the checksum.
        ctl.u32_at(abi::control_header::ring_dir::OFF + abi::ring_dir_entry::CAPACITY).store(1 << 30, Relaxed);
        assert_eq!(Directory::read(&ctl, len).unwrap_err(), LayoutError::BadChecksum);
    }
}
