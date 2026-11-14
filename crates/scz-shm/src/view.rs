//! What a codec, or a worker backend, may do with slot memory (sdk-rust §3.2).
//!
//! The other side of a slot is another process, and it can write the slot at any time
//! whatever the protocol says. A plain `&[u8]` over slot memory would promise the compiler
//! that the bytes do not change, so no such reference is ever handed out: views copy in and
//! out through raw pointers, bounds-checked against the region.
//!
//! Phase 1 carries opaque bytes. The Arrow column views of arrow-data-plane §7 replace these
//! in phase 3, over the same rules.

use crate::seg::{Seg, ShmSlot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfBounds;

/// Writes into a region: the request region for a client, the response region for a worker.
pub struct SlotWriter<'a> {
    seg: Seg<'a>,
    off: usize,
    cap: usize,
    len: usize,
}

impl<'a> SlotWriter<'a> {
    pub fn request(slot: &ShmSlot<'a>) -> Self {
        SlotWriter { seg: slot.seg, off: slot.req_off(), cap: slot.req_cap, len: 0 }
    }
    pub fn response(slot: &ShmSlot<'a>) -> Self {
        SlotWriter { seg: slot.seg, off: slot.resp_off(), cap: slot.resp_cap, len: 0 }
    }
    pub fn capacity(&self) -> usize {
        self.cap
    }
    /// One past the highest byte written.
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn write_at(&mut self, at: usize, bytes: &[u8]) -> Result<(), OutOfBounds> {
        let end = at.checked_add(bytes.len()).filter(|&e| e <= self.cap).ok_or(OutOfBounds)?;
        self.seg.copy_in(self.off + at, bytes);
        self.len = self.len.max(end);
        Ok(())
    }
    /// Append after the highest byte written so far.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), OutOfBounds> {
        self.write_at(self.len, bytes)
    }
    pub fn write_u32(&mut self, at: usize, v: u32) -> Result<(), OutOfBounds> {
        self.write_at(at, &v.to_le_bytes())
    }
    pub fn write_u64(&mut self, at: usize, v: u64) -> Result<(), OutOfBounds> {
        self.write_at(at, &v.to_le_bytes())
    }
    pub fn write_f32(&mut self, at: usize, v: f32) -> Result<(), OutOfBounds> {
        self.write_at(at, &v.to_le_bytes())
    }
    /// Copy a whole region read from the same segment (the echo worker).
    pub fn copy_from(&mut self, r: &SlotReader<'_>) -> Result<(), OutOfBounds> {
        if r.len > self.cap || r.seg.as_ptr() != self.seg.as_ptr() {
            return Err(OutOfBounds);
        }
        self.seg.copy_within(r.off, self.off, r.len);
        self.len = self.len.max(r.len);
        Ok(())
    }
}

/// Reads from a region whose length was read once and checked against its capacity.
pub struct SlotReader<'a> {
    seg: Seg<'a>,
    off: usize,
    len: usize,
}

impl<'a> SlotReader<'a> {
    /// The request region of `slot`, `len` bytes long; None if `len` exceeds the region.
    pub fn request(slot: &ShmSlot<'a>, len: usize) -> Option<Self> {
        (len <= slot.req_cap).then_some(SlotReader { seg: slot.seg, off: slot.req_off(), len })
    }
    pub fn response(slot: &ShmSlot<'a>, len: usize) -> Option<Self> {
        (len <= slot.resp_cap).then_some(SlotReader { seg: slot.seg, off: slot.resp_off(), len })
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn copy_to(&self, at: usize, dst: &mut [u8]) -> Result<(), OutOfBounds> {
        at.checked_add(dst.len()).filter(|&e| e <= self.len).ok_or(OutOfBounds)?;
        self.seg.copy_out(self.off + at, dst);
        Ok(())
    }
    pub fn read_u32(&self, at: usize) -> Result<u32, OutOfBounds> {
        let mut b = [0; 4];
        self.copy_to(at, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    pub fn read_u64(&self, at: usize) -> Result<u64, OutOfBounds> {
        let mut b = [0; 8];
        self.copy_to(at, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    pub fn read_f32(&self, at: usize) -> Result<f32, OutOfBounds> {
        self.read_u32(at).map(f32::from_bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Config, Geometry, LocalArena};

    #[test]
    fn views_stay_inside_their_regions() {
        let a = LocalArena::new(&Geometry::default(), &Config::with_defaults(1, 1)).unwrap();
        let data = a.data();
        let s = a.dir.slot(&data, 0, 3);
        let mut w = SlotWriter::request(&s);
        assert_eq!(w.write_at(2040, &[0; 9]), Err(OutOfBounds), "past req_cap");
        w.write(b"hello").unwrap();
        w.write_u32(8, 0xAABBCCDD).unwrap();
        assert_eq!(w.len(), 12);
        let r = SlotReader::request(&s, w.len()).unwrap();
        let mut b = [0; 5];
        r.copy_to(0, &mut b).unwrap();
        assert_eq!(&b, b"hello");
        assert_eq!(r.read_u32(8), Ok(0xAABBCCDD));
        assert_eq!(r.read_u32(9), Err(OutOfBounds), "past the length read from the record");
        let mut resp = SlotWriter::response(&s);
        resp.copy_from(&r).unwrap();
        let rr = SlotReader::response(&s, 12).unwrap();
        rr.copy_to(0, &mut b).unwrap();
        assert_eq!(&b, b"hello");
        assert!(SlotReader::response(&s, 2049).is_none());
    }
}
