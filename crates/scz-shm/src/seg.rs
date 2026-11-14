//! A view of one mapped segment, and the typed rings and slots over it.
//!
//! Every word another process can write is reached as an atomic, so each read is one load
//! (shm-abi §1, "single fetch"). Offsets are checked against the segment's length and the
//! field's alignment; a bad offset is a bug in this crate or a corrupt directory that should
//! have been rejected earlier, so it panics rather than touching memory outside the mapping.

use crate::abi;
use crate::ring::RingMem;
use crate::slot::SlotMem;
use crate::wait;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8};

/// A view of a mapped segment, valid for `'a`: the lifetime of the mapping it came from, so
/// safe code cannot keep a view past the unmap.
#[derive(Clone, Copy)]
pub struct Seg<'a> {
    ptr: *mut u8,
    len: usize,
    _map: PhantomData<&'a [u8]>,
}

// SAFETY: a Seg is a view of shared memory reached only through atomics or explicit copies.
unsafe impl Send for Seg<'_> {}
unsafe impl Sync for Seg<'_> {}

impl<'a> Seg<'a> {
    /// # Safety
    /// `ptr..ptr+len` must stay mapped, readable, and (for writes) writable for `'a`, and
    /// `ptr` must be page-aligned.
    pub unsafe fn new(ptr: *mut u8, len: usize) -> Seg<'a> {
        assert!((ptr as usize).is_multiple_of(4096), "segments are mapped page-aligned");
        Seg { ptr, len, _map: PhantomData }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }
    #[inline]
    fn at<T>(&self, off: usize) -> &T {
        let w = core::mem::size_of::<T>();
        assert!(
            off.is_multiple_of(w) && off.checked_add(w).is_some_and(|end| end <= self.len),
            "offset {off:#x} (width {w}) outside a {} byte segment",
            self.len
        );
        // SAFETY: in bounds and aligned (checked above); the mapping outlives &self by the
        // contract of `new`; T is an atomic integer, valid for any bit pattern.
        unsafe { &*(self.ptr.add(off) as *const T) }
    }
    #[inline]
    pub fn u8_at(&self, off: usize) -> &AtomicU8 {
        self.at(off)
    }
    #[inline]
    pub fn u16_at(&self, off: usize) -> &AtomicU16 {
        self.at(off)
    }
    #[inline]
    pub fn u32_at(&self, off: usize) -> &AtomicU32 {
        self.at(off)
    }
    #[inline]
    pub fn u64_at(&self, off: usize) -> &AtomicU64 {
        self.at(off)
    }
    #[inline]
    pub fn words8_at(&self, off: usize) -> &[AtomicU64; 8] {
        assert!(off.is_multiple_of(64) && off + 64 <= self.len, "cell at {off:#x} outside the segment");
        // SAFETY: as in `at`.
        unsafe { &*(self.ptr.add(off) as *const [AtomicU64; 8]) }
    }
    /// Copy bytes into the segment. For payload regions the caller owns by protocol.
    pub fn copy_in(&self, off: usize, src: &[u8]) {
        assert!(off.checked_add(src.len()).is_some_and(|end| end <= self.len));
        // SAFETY: bounds checked; the region is owned by the caller under the slot protocol.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(off), src.len()) }
    }
    /// Copy bytes out of the segment.
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) {
        assert!(off.checked_add(dst.len()).is_some_and(|end| end <= self.len));
        // SAFETY: as above.
        unsafe { core::ptr::copy_nonoverlapping(self.ptr.add(off), dst.as_mut_ptr(), dst.len()) }
    }
    /// A raw view of a payload region, for codecs that write or read in place.
    ///
    /// # Safety
    /// The caller must own the region under the slot protocol for the lifetime of the slice.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn bytes_mut(&self, off: usize, len: usize) -> &'a mut [u8] {
        assert!(off.checked_add(len).is_some_and(|end| end <= self.len));
        // SAFETY: bounds checked; exclusivity is the caller's obligation.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.add(off), len) }
    }
    /// Copy `len` bytes from one place in the segment to another (the echo worker's whole job).
    pub fn copy_within(&self, src: usize, dst: usize, len: usize) {
        assert!(src.checked_add(len).is_some_and(|e| e <= self.len) && dst.checked_add(len).is_some_and(|e| e <= self.len));
        // SAFETY: bounds checked; `copy` tolerates overlap.
        unsafe { core::ptr::copy(self.ptr.add(src), self.ptr.add(dst), len) }
    }
}

/// One ring in a mapped data segment.
#[derive(Clone, Copy)]
pub struct ShmRing<'a> {
    pub seg: Seg<'a>,
    pub off: usize,
    pub capacity: u64,
}

impl RingMem for ShmRing<'_> {
    fn capacity(&self) -> u64 {
        self.capacity
    }
    fn tail(&self) -> &AtomicU64 {
        self.seg.u64_at(self.off + abi::ring_header::TAIL)
    }
    fn head(&self) -> &AtomicU64 {
        self.seg.u64_at(self.off + abi::ring_header::HEAD)
    }
    fn consumer_wait(&self) -> &AtomicU32 {
        self.seg.u32_at(self.off + abi::ring_header::CONSUMER_WAIT)
    }
    fn cell(&self, pos: u64) -> &[AtomicU64; 8] {
        let i = (pos & (self.capacity - 1)) as usize;
        self.seg.words8_at(self.off + abi::ring_header::CELLS_OFF + i * abi::CELL_LEN as usize)
    }
    fn park(&self, expected: u32) {
        wait::futex_wait(self.consumer_wait(), expected, None);
    }
    fn unpark(&self) {
        wait::futex_wake(self.consumer_wait(), 1);
    }
}

/// One slot: its record in the slot table and its data region.
#[derive(Clone, Copy)]
pub struct ShmSlot<'a> {
    pub seg: Seg<'a>,
    /// Offset of the 64-byte record.
    pub rec: usize,
    /// Offset of the slot's data; the request region starts here.
    pub data: usize,
    pub req_cap: usize,
    pub resp_cap: usize,
}

impl ShmSlot<'_> {
    pub fn req_off(&self) -> usize {
        self.data
    }
    pub fn resp_off(&self) -> usize {
        self.data + self.req_cap
    }
}

impl SlotMem for ShmSlot<'_> {
    fn word(&self) -> &AtomicU64 {
        self.seg.u64_at(self.rec + abi::slot_record::WORD)
    }
    fn wait(&self, expected: u32, deadline_ns: u64) -> bool {
        wait::futex_wait_low32(self.word(), expected, Some(deadline_ns))
    }
    fn wake(&self) {
        wait::futex_wake_low32(self.word(), 1);
    }
}
