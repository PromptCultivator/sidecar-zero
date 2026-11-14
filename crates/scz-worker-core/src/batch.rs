//! What a backend sees of a request and its response (worker §2): the columns of a frame laid
//! out by a template, located by the template's offsets, over slot memory.
//!
//! A request region stays writable by the host while the model runs, so neither view hands
//! out a Rust slice of it: a `&[u8]` over memory another process may write is a data race.
//! They give raw pointers and lengths, for a runtime to read and write in place (zero copy),
//! and copies for code that wants its own bytes. The registry was verified before either
//! exists, so every column lies inside its region.

use scz_shm::registry::{ColumnLoc, Template};
use scz_shm::seg::Seg;

/// The request: `rows` rows of each request column.
pub struct BatchView<'a> {
    pub(crate) seg: Seg<'a>,
    pub(crate) region: usize,
    pub(crate) tmpl: &'a Template,
    pub(crate) rows: u32,
    pub(crate) deadline_ns: u64,
}

impl BatchView<'_> {
    pub fn rows(&self) -> u32 {
        self.rows
    }
    /// When the caller stops waiting (`CLOCK_MONOTONIC`, from the request's descriptor): a
    /// backend that can abandon an inference stops here, since nobody takes the answer later
    /// (worker §4). The caller wrote it, so it is a reason to stop early and never one to run
    /// longer: a backend keeps its own cap as well.
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub fn columns(&self) -> &[ColumnLoc] {
        &self.tmpl.columns
    }
    /// The first byte of column `i`'s values, and their length for this many rows.
    pub fn raw(&self, i: usize) -> (*const u8, usize) {
        let c = &self.tmpl.columns[i];
        // SAFETY: in bounds: the verified template keeps values_off + max_rows × row_bytes
        // inside the region, and rows ≤ max_rows was checked before the view was made.
        (unsafe { self.seg.as_ptr().add(self.region + c.values_off as usize) }, self.rows as usize * c.row_bytes as usize)
    }
    /// Copy column `i`'s values out (for index-like inputs, which must not change after they
    /// were checked: worker §7.1).
    pub fn copy_column(&self, i: usize, dst: &mut [u8]) {
        let c = &self.tmpl.columns[i];
        let n = (self.rows as usize * c.row_bytes as usize).min(dst.len());
        self.seg.copy_out(self.region + c.values_off as usize, &mut dst[..n]);
    }
    /// Whether `p` points into the data segment: the zero-copy assertion of
    /// arrow-data-plane §11.
    pub fn in_arena(&self, p: *const u8) -> bool {
        let base = self.seg.as_ptr() as usize;
        (base..base + self.seg.len()).contains(&(p as usize))
    }
}

/// The response: `rows` rows of each response column, to be written in place.
pub struct BatchSink<'a> {
    pub(crate) seg: Seg<'a>,
    pub(crate) region: usize,
    pub(crate) tmpl: &'a Template,
    pub(crate) rows: u32,
}

impl BatchSink<'_> {
    pub fn rows(&self) -> u32 {
        self.rows
    }
    pub fn columns(&self) -> &[ColumnLoc] {
        &self.tmpl.columns
    }
    /// The first byte of column `i`'s values, writable for this many rows.
    pub fn raw(&mut self, i: usize) -> (*mut u8, usize) {
        let c = &self.tmpl.columns[i];
        // SAFETY: as for BatchView::raw; the worker owns the response region while the slot
        // is PROCESSING.
        (unsafe { self.seg.as_ptr().add(self.region + c.values_off as usize) }, self.rows as usize * c.row_bytes as usize)
    }
    pub fn write_column(&mut self, i: usize, src: &[u8]) {
        let c = &self.tmpl.columns[i];
        let n = (self.rows as usize * c.row_bytes as usize).min(src.len());
        self.seg.copy_in(self.region + c.values_off as usize, &src[..n]);
    }
    pub fn in_arena(&self, p: *const u8) -> bool {
        let base = self.seg.as_ptr() as usize;
        (base..base + self.seg.len()).contains(&(p as usize))
    }
}
