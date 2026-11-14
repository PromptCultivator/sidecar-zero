//! Codecs: how an input becomes a request and a response becomes an output.
//!
//! A codec either declares the model's columns ([`Codec::schema`]) and writes and reads Arrow
//! frames laid out by the registry's templates (arrow-data-plane §7), or declares none and
//! carries opaque bytes, as the echo and chaos workers expect. The client binds a codec with
//! columns only to a registry with the same schema fingerprint (arrow-data-plane §9, strict
//! binding); any other pairing falls back with `SCHEMA_MISMATCH`.

use scz_shm::abi::elem_type as et;
use scz_shm::registry::Template;
pub use scz_shm::registry::{Column, Schema};
use scz_shm::seg::Seg;
pub use scz_shm::view::{OutOfBounds, SlotReader, SlotWriter};

/// A small `Copy` error, so that returning one allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The input or the response does not have the shape the codec expects.
    Shape,
    /// It does not fit the slot.
    Overflow,
}

impl From<OutOfBounds> for CodecError {
    fn from(_: OutOfBounds) -> Self {
        CodecError::Overflow
    }
}

pub trait Codec {
    type In: ?Sized;
    type Out;
    /// The request and response columns, or None for opaque bytes. Called once, when the
    /// client is built.
    fn schema(&self) -> Option<Schema> {
        None
    }

    // ── opaque bytes ─────────────────────────────────────────────────────────────────────
    /// Bytes the request needs; the smallest slot class whose request region holds them is used.
    fn request_len(&self, _input: &Self::In) -> usize {
        0
    }
    fn encode(&self, _input: &Self::In, _w: &mut SlotWriter<'_>) -> Result<(), CodecError> {
        Err(CodecError::Shape)
    }
    fn decode(&self, _r: &SlotReader<'_>) -> Result<Self::Out, CodecError> {
        Err(CodecError::Shape)
    }

    // ── Arrow frames ────────────────────────────────────────────────────────────────────
    /// Rows the request carries; the smallest class whose template holds them is used.
    fn rows(&self, _input: &Self::In) -> u32 {
        1
    }
    /// Write the request's column values. The frame's metadata is already in place.
    fn encode_frame(&self, _input: &Self::In, _w: &mut FrameWriter<'_>) -> Result<(), CodecError> {
        Err(CodecError::Shape)
    }
    /// Read the response's column values.
    fn decode_frame(&self, _r: &FrameReader<'_>) -> Result<Self::Out, CodecError> {
        Err(CodecError::Shape)
    }
}

/// Write `vals` as little-endian f32 at `off`, through a stack buffer: one bounded copy.
fn put_f32s(seg: &Seg<'_>, off: usize, vals: &[f32]) {
    let mut buf = [0u8; 256];
    for (k, chunk) in vals.chunks(64).enumerate() {
        for (j, v) in chunk.iter().enumerate() {
            buf[j * 4..j * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        seg.copy_in(off + k * 256, &buf[..chunk.len() * 4]);
    }
}

fn get_f32s(seg: &Seg<'_>, off: usize, out: &mut [f32]) {
    let mut buf = [0u8; 256];
    for (k, chunk) in out.chunks_mut(64).enumerate() {
        let b = &mut buf[..chunk.len() * 4];
        seg.copy_out(off + k * 256, b);
        for (j, v) in chunk.iter_mut().enumerate() {
            *v = f32::from_le_bytes(b[j * 4..j * 4 + 4].try_into().unwrap());
        }
    }
}

/// The request frame's columns, writable (arrow-data-plane §7.1). Each write is checked
/// against the column's type and the frame's row count.
pub struct FrameWriter<'a> {
    pub(crate) seg: Seg<'a>,
    pub(crate) region: usize,
    pub(crate) tmpl: &'a Template,
    pub(crate) rows: u32,
}

impl FrameWriter<'_> {
    pub fn rows(&self) -> u32 {
        self.rows
    }
    /// Column `col`'s values for every row, row-major: `rows × list_size` floats.
    pub fn write_f32(&mut self, col: usize, values: &[f32]) -> Result<(), CodecError> {
        let c = self.tmpl.columns.get(col).ok_or(CodecError::Shape)?;
        if c.column.elem as u32 != et::F32 || values.len() != self.rows as usize * c.column.list_size as usize {
            return Err(CodecError::Shape);
        }
        put_f32s(&self.seg, self.region + c.values_off as usize, values);
        Ok(())
    }
    /// The values of row `row` alone.
    pub fn write_f32_row(&mut self, col: usize, row: u32, values: &[f32]) -> Result<(), CodecError> {
        let c = self.tmpl.columns.get(col).ok_or(CodecError::Shape)?;
        if c.column.elem as u32 != et::F32 || values.len() != c.column.list_size as usize || row >= self.rows {
            return Err(CodecError::Shape);
        }
        put_f32s(&self.seg, self.region + c.values_off as usize + row as usize * c.row_bytes as usize, values);
        Ok(())
    }
    /// Raw bytes for every row, for any fixed-width type.
    pub fn write_bytes(&mut self, col: usize, bytes: &[u8]) -> Result<(), CodecError> {
        let c = self.tmpl.columns.get(col).ok_or(CodecError::Shape)?;
        if bytes.len() != self.rows as usize * c.row_bytes as usize {
            return Err(CodecError::Shape);
        }
        self.seg.copy_in(self.region + c.values_off as usize, bytes);
        Ok(())
    }
}

/// The response frame's columns (arrow-data-plane §7.3), valid while the call holds the slot.
pub struct FrameReader<'a> {
    pub(crate) seg: Seg<'a>,
    pub(crate) region: usize,
    pub(crate) tmpl: &'a Template,
    pub(crate) rows: u32,
}

impl FrameReader<'_> {
    pub fn rows(&self) -> u32 {
        self.rows
    }
    pub fn read_f32(&self, col: usize, out: &mut [f32]) -> Result<(), CodecError> {
        let c = self.tmpl.columns.get(col).ok_or(CodecError::Shape)?;
        if c.column.elem as u32 != et::F32 || out.len() != self.rows as usize * c.column.list_size as usize {
            return Err(CodecError::Shape);
        }
        get_f32s(&self.seg, self.region + c.values_off as usize, out);
        Ok(())
    }
    /// One f32 value: element `i` of column `col` (row-major).
    pub fn f32(&self, col: usize, i: usize) -> Result<f32, CodecError> {
        let c = self.tmpl.columns.get(col).ok_or(CodecError::Shape)?;
        if c.column.elem as u32 != et::F32 || i >= self.rows as usize * c.column.list_size as usize {
            return Err(CodecError::Shape);
        }
        Ok(f32::from_bits(self.seg.u32_at(self.region + c.values_off as usize + i * 4).load(core::sync::atomic::Ordering::Relaxed)))
    }
    pub fn read_bytes(&self, col: usize, out: &mut [u8]) -> Result<(), CodecError> {
        let c = self.tmpl.columns.get(col).ok_or(CodecError::Shape)?;
        if out.len() != self.rows as usize * c.row_bytes as usize {
            return Err(CodecError::Shape);
        }
        self.seg.copy_out(self.region + c.values_off as usize, out);
        Ok(())
    }
}

// ── prebuilt codecs ─────────────────────────────────────────────────────────────────────

/// One row of `N` float features in, one float score out: the launch workload, a model with
/// input `features: float32[rows, N]` and output `score: float32[rows, 1]` (column names
/// configurable). The request is one `FixedSizeList<float32>[N]` column, the response one
/// `float32` column.
#[derive(Clone, Copy, Debug)]
pub struct FeatureScore<const N: usize> {
    pub input: &'static str,
    pub output: &'static str,
}

impl<const N: usize> Default for FeatureScore<N> {
    fn default() -> Self {
        FeatureScore { input: "features", output: "score" }
    }
}

impl<const N: usize> Codec for FeatureScore<N> {
    type In = [f32; N];
    type Out = f32;
    fn schema(&self) -> Option<Schema> {
        Some(Schema { request: vec![Column::new(self.input, et::F32, N as u32)], response: vec![Column::new(self.output, et::F32, 1)] })
    }
    fn encode_frame(&self, input: &[f32; N], w: &mut FrameWriter<'_>) -> Result<(), CodecError> {
        w.write_f32(0, input)
    }
    fn decode_frame(&self, r: &FrameReader<'_>) -> Result<f32, CodecError> {
        r.f32(0, 0)
    }
}

/// `R` rows of `N` features in, `R` scores out, in one call: the batch slot class.
#[derive(Clone, Copy, Debug)]
pub struct FeatureBatch<const N: usize, const R: usize> {
    pub input: &'static str,
    pub output: &'static str,
}

impl<const N: usize, const R: usize> Default for FeatureBatch<N, R> {
    fn default() -> Self {
        FeatureBatch { input: "features", output: "score" }
    }
}

impl<const N: usize, const R: usize> Codec for FeatureBatch<N, R> {
    type In = [[f32; N]; R];
    type Out = [f32; R];
    fn schema(&self) -> Option<Schema> {
        Some(Schema { request: vec![Column::new(self.input, et::F32, N as u32)], response: vec![Column::new(self.output, et::F32, 1)] })
    }
    fn rows(&self, _: &Self::In) -> u32 {
        R as u32
    }
    fn encode_frame(&self, input: &Self::In, w: &mut FrameWriter<'_>) -> Result<(), CodecError> {
        for (r, row) in input.iter().enumerate() {
            w.write_f32_row(0, r as u32, row)?;
        }
        Ok(())
    }
    fn decode_frame(&self, r: &FrameReader<'_>) -> Result<[f32; R], CodecError> {
        let mut out = [0f32; R];
        r.read_f32(0, &mut out)?;
        Ok(out)
    }
}

/// `[f32; N]` in, `[f32; N]` out, as opaque bytes: what the echo worker answers.
#[derive(Clone, Copy, Debug, Default)]
pub struct F32Echo<const N: usize>;

impl<const N: usize> Codec for F32Echo<N> {
    type In = [f32; N];
    type Out = [f32; N];
    fn request_len(&self, _input: &[f32; N]) -> usize {
        N * 4
    }
    fn encode(&self, input: &[f32; N], w: &mut SlotWriter<'_>) -> Result<(), CodecError> {
        // One bulk copy: on a little-endian host the bytes are the values.
        let mut buf = [0u8; 256];
        for (chunk_i, chunk) in input.chunks(64).enumerate() {
            for (j, v) in chunk.iter().enumerate() {
                buf[j * 4..j * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            w.write_at(chunk_i * 256, &buf[..chunk.len() * 4])?;
        }
        Ok(())
    }
    fn decode(&self, r: &SlotReader<'_>) -> Result<[f32; N], CodecError> {
        if r.len() != N * 4 {
            return Err(CodecError::Shape);
        }
        let mut out = [0f32; N];
        let mut buf = [0u8; 256];
        for (chunk_i, chunk) in out.chunks_mut(64).enumerate() {
            let b = &mut buf[..chunk.len() * 4];
            r.copy_to(chunk_i * 256, b)?;
            for (j, v) in chunk.iter_mut().enumerate() {
                *v = f32::from_le_bytes(b[j * 4..j * 4 + 4].try_into().unwrap());
            }
        }
        Ok(out)
    }
}

/// `[u8; N]` in, `[u8; N]` out, as opaque bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct BytesEcho<const N: usize>;

impl<const N: usize> Codec for BytesEcho<N> {
    type In = [u8; N];
    type Out = [u8; N];
    fn request_len(&self, _: &[u8; N]) -> usize {
        N
    }
    fn encode(&self, input: &[u8; N], w: &mut SlotWriter<'_>) -> Result<(), CodecError> {
        Ok(w.write(input)?)
    }
    fn decode(&self, r: &SlotReader<'_>) -> Result<[u8; N], CodecError> {
        if r.len() != N {
            return Err(CodecError::Shape);
        }
        let mut out = [0u8; N];
        r.copy_to(0, &mut out)?;
        Ok(out)
    }
}
