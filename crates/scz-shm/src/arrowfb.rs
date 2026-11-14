//! A bounds-checked reader for the two Arrow IPC messages a template involves: the Schema
//! message and the RecordBatch message (arrow-data-plane §4, §10).
//!
//! Both sides use it. The worker walks the frames it rendered with a real Arrow writer to
//! find the length fields a row count changes (spike S4: comparing two renders misses fields
//! whose values happen to agree). The supervisor's registry verifier walks what a worker
//! staged before an Arrow library in the host can ever parse it.
//!
//! It is a flatbuffer verifier specialized to these tables, written against the Arrow format
//! files (`Message.fbs`, `Schema.fbs`), with no dependency on an Arrow library or on the
//! flatbuffers crate. Every offset is checked against the buffer, every scalar against its
//! alignment and its table's size, every vtable against its own bounds; strings must be
//! terminated and UTF-8; the walk visits a bounded number of tables to a bounded depth. A
//! field this reader does not know is left alone, as the flatbuffer rules allow; a field
//! whose meaning would change the layout (compression, dictionaries, big-endian data) is an
//! error.

use core::cell::Cell;

/// Why a message was refused. A string: these are diagnostics for a log line and a verdict.
pub type Error = String;

const MAX_TABLES: u32 = 4096;
const MAX_DEPTH: u32 = 16;
/// `MetadataVersion::V5`, the only version written since Arrow 1.0.
pub const METADATA_V5: i16 = 4;
pub const CONTINUATION: u32 = 0xFFFF_FFFF;

/// `MessageHeader` union tags.
pub mod header {
    pub const SCHEMA: u8 = 1;
    pub const RECORD_BATCH: u8 = 3;
}

/// `Type` union tags this reader looks inside.
pub mod type_tag {
    pub const INT: u8 = 2;
    pub const FLOATING_POINT: u8 = 3;
    pub const FIXED_SIZE_BINARY: u8 = 15;
    pub const FIXED_SIZE_LIST: u8 = 16;
}

#[derive(Clone, Copy, Debug)]
struct Table {
    pos: usize,
    vt: usize,
    vt_len: usize,
    len: usize,
}

struct Fb<'a> {
    b: &'a [u8],
    tables: Cell<u32>,
}

fn err<T>(what: impl Into<String>) -> Result<T, Error> {
    Err(what.into())
}

impl<'a> Fb<'a> {
    fn new(b: &'a [u8]) -> Fb<'a> {
        Fb { b, tables: Cell::new(0) }
    }
    fn bytes<const N: usize>(&self, at: usize) -> Result<[u8; N], Error> {
        match at.checked_add(N).and_then(|end| self.b.get(at..end)) {
            Some(s) => Ok(s.try_into().unwrap()),
            None => err(format!("read of {N} bytes at {at:#x} past the end ({:#x})", self.b.len())),
        }
    }
    fn u8(&self, at: usize) -> Result<u8, Error> {
        Ok(self.bytes::<1>(at)?[0])
    }
    fn u16(&self, at: usize) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.bytes(at)?))
    }
    fn u32(&self, at: usize) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.bytes(at)?))
    }
    fn i64(&self, at: usize) -> Result<i64, Error> {
        Ok(i64::from_le_bytes(self.bytes(at)?))
    }

    fn table(&self, pos: usize) -> Result<Table, Error> {
        let n = self.tables.get() + 1;
        if n > MAX_TABLES {
            return err("too many tables");
        }
        self.tables.set(n);
        if !pos.is_multiple_of(4) {
            return err(format!("table at {pos:#x} is not 4-aligned"));
        }
        let soff = i32::from_le_bytes(self.bytes(pos)?) as i64;
        let vt = pos as i64 - soff;
        if vt < 0 || vt % 2 != 0 {
            return err(format!("table at {pos:#x} has a bad vtable offset {soff}"));
        }
        let vt = vt as usize;
        let vt_len = self.u16(vt)? as usize;
        let len = self.u16(vt + 2)? as usize;
        if vt_len < 4 || !vt_len.is_multiple_of(2) || vt + vt_len > self.b.len() {
            return err(format!("vtable at {vt:#x} has a bad length {vt_len}"));
        }
        if len < 4 || pos + len > self.b.len() {
            return err(format!("table at {pos:#x} has a bad length {len}"));
        }
        Ok(Table { pos, vt, vt_len, len })
    }

    /// The position of field `i` if present, checked to hold `size` bytes inside its table,
    /// aligned to `size`.
    fn field(&self, t: &Table, i: usize, size: usize) -> Result<Option<usize>, Error> {
        let slot = 4 + 2 * i;
        if slot + 2 > t.vt_len {
            return Ok(None);
        }
        let o = self.u16(t.vt + slot)? as usize;
        if o == 0 {
            return Ok(None);
        }
        if o < 4 || o + size > t.len {
            return err(format!("field {i} of the table at {:#x} lies outside it", t.pos));
        }
        let at = t.pos + o;
        if !at.is_multiple_of(size) {
            return err(format!("field {i} of the table at {:#x} is misaligned", t.pos));
        }
        Ok(Some(at))
    }
    fn i64_field(&self, t: &Table, i: usize, default: i64) -> Result<(i64, Option<usize>), Error> {
        match self.field(t, i, 8)? {
            Some(at) => Ok((self.i64(at)?, Some(at))),
            None => Ok((default, None)),
        }
    }
    fn i32_field(&self, t: &Table, i: usize, default: i32) -> Result<i32, Error> {
        Ok(match self.field(t, i, 4)? {
            Some(at) => self.u32(at)? as i32,
            None => default,
        })
    }
    fn i16_field(&self, t: &Table, i: usize, default: i16) -> Result<i16, Error> {
        Ok(match self.field(t, i, 2)? {
            Some(at) => self.u16(at)? as i16,
            None => default,
        })
    }
    fn u8_field(&self, t: &Table, i: usize, default: u8) -> Result<u8, Error> {
        Ok(match self.field(t, i, 1)? {
            Some(at) => self.u8(at)?,
            None => default,
        })
    }
    /// Follow a `uoffset` stored at `at`.
    fn follow(&self, at: usize) -> Result<usize, Error> {
        let o = self.u32(at)? as usize;
        match at.checked_add(o) {
            Some(t) if o != 0 && t < self.b.len() => Ok(t),
            _ => err(format!("offset at {at:#x} points outside the buffer")),
        }
    }
    fn offset_field(&self, t: &Table, i: usize) -> Result<Option<usize>, Error> {
        match self.field(t, i, 4)? {
            Some(at) => self.follow(at).map(Some),
            None => Ok(None),
        }
    }
    fn table_field(&self, t: &Table, i: usize) -> Result<Option<Table>, Error> {
        match self.offset_field(t, i)? {
            Some(p) => self.table(p).map(Some),
            None => Ok(None),
        }
    }
    /// A vector field: (position of the first element, element count).
    fn vector_field(&self, t: &Table, i: usize, elem: usize, align: usize) -> Result<Option<(usize, usize)>, Error> {
        let Some(v) = self.offset_field(t, i)? else { return Ok(None) };
        if !v.is_multiple_of(4) {
            return err(format!("vector at {v:#x} is not 4-aligned"));
        }
        let n = self.u32(v)? as usize;
        let data = v + 4;
        if !data.is_multiple_of(align) {
            return err(format!("vector data at {data:#x} is not {align}-aligned"));
        }
        match n.checked_mul(elem).and_then(|x| data.checked_add(x)) {
            Some(end) if end <= self.b.len() => Ok(Some((data, n))),
            _ => err(format!("vector at {v:#x} of {n} elements runs past the end")),
        }
    }
    fn string_field(&self, t: &Table, i: usize) -> Result<Option<&'a str>, Error> {
        let Some((data, n)) = self.vector_field(t, i, 1, 1)? else { return Ok(None) };
        if self.u8(data + n)? != 0 {
            return err(format!("string at {data:#x} is not terminated"));
        }
        core::str::from_utf8(&self.b[data..data + n]).map(Some).map_err(|_| format!("string at {data:#x} is not UTF-8"))
    }
    fn tables_field(&self, t: &Table, i: usize) -> Result<Vec<Table>, Error> {
        let Some((data, n)) = self.vector_field(t, i, 4, 4)? else { return Ok(vec![]) };
        (0..n).map(|k| self.follow(data + 4 * k).and_then(|p| self.table(p))).collect()
    }
    /// `custom_metadata: [KeyValue]`, verified and ignored.
    fn key_values(&self, t: &Table, i: usize) -> Result<(), Error> {
        for kv in self.tables_field(t, i)? {
            self.string_field(&kv, 0)?;
            self.string_field(&kv, 1)?;
        }
        Ok(())
    }

    /// The root `Message` table: (its header type, the header table, bodyLength).
    fn message(&self) -> Result<(u8, Table, i64), Error> {
        let root = self.follow(0)?;
        let m = self.table(root)?;
        let version = self.i16_field(&m, 0, 0)?;
        if version != METADATA_V5 {
            return err(format!("metadata version {version}, not V5"));
        }
        let ty = self.u8_field(&m, 1, 0)?;
        let Some(h) = self.table_field(&m, 2)? else { return err("a message without a header") };
        let (body, _) = self.i64_field(&m, 3, 0)?;
        if body < 0 {
            return err("a negative body length");
        }
        self.key_values(&m, 4)?;
        Ok((ty, h, body))
    }
}

/// The encapsulated-message framing: `0xFFFFFFFF`, then the metadata size, then the
/// flatbuffer. Returns the flatbuffer's offset and length within `frame`.
pub fn framing(frame: &[u8]) -> Result<(usize, usize), Error> {
    let fb = Fb::new(frame);
    if fb.u32(0)? != CONTINUATION {
        return err("no continuation marker");
    }
    let size = fb.u32(4)? as usize;
    if size == 0 || !size.is_multiple_of(8) || 8 + size > frame.len() {
        return err(format!("metadata size {size} does not fit a {} byte frame", frame.len()));
    }
    Ok((8, size))
}

/// One `FieldNode`, with where its two fields sit in the flatbuffer.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldNode {
    pub length: i64,
    pub null_count: i64,
    pub length_pos: usize,
    pub null_count_pos: usize,
}

/// One `Buffer`, with where its length sits.
#[derive(Clone, Debug, PartialEq)]
pub struct BufferRef {
    pub offset: i64,
    pub length: i64,
    pub length_pos: usize,
}

/// A RecordBatch message's layout. Positions are within the flatbuffer.
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    pub length: i64,
    /// None when the writer omitted the field (a length of 0).
    pub length_pos: Option<usize>,
    pub nodes: Vec<FieldNode>,
    pub buffers: Vec<BufferRef>,
    pub body_len: i64,
}

/// Read a RecordBatch message's flatbuffer. Compression and variadic buffers are refused:
/// neither has a fixed layout.
pub fn record_batch(meta: &[u8]) -> Result<Batch, Error> {
    let fb = Fb::new(meta);
    let (ty, rb, body_len) = fb.message()?;
    if ty != header::RECORD_BATCH {
        return err(format!("header type {ty}, not a RecordBatch"));
    }
    let (length, length_pos) = fb.i64_field(&rb, 0, 0)?;
    if length < 0 {
        return err("a negative row count");
    }
    let mut nodes = vec![];
    if let Some((data, n)) = fb.vector_field(&rb, 1, 16, 8)? {
        for k in 0..n {
            let at = data + 16 * k;
            nodes.push(FieldNode { length: fb.i64(at)?, null_count: fb.i64(at + 8)?, length_pos: at, null_count_pos: at + 8 });
        }
    }
    let mut buffers = vec![];
    if let Some((data, n)) = fb.vector_field(&rb, 2, 16, 8)? {
        for k in 0..n {
            let at = data + 16 * k;
            buffers.push(BufferRef { offset: fb.i64(at)?, length: fb.i64(at + 8)?, length_pos: at + 8 });
        }
    }
    if fb.field(&rb, 3, 4)?.is_some() {
        return err("a compressed body");
    }
    if let Some((_, n)) = fb.vector_field(&rb, 4, 8, 8)? {
        if n != 0 {
            return err("variadic buffers");
        }
    }
    for b in &buffers {
        if b.offset < 0 || b.length < 0 || b.offset.checked_add(b.length).is_none_or(|end| end > body_len) {
            return err(format!("buffer at {} of {} bytes outside a body of {body_len}", b.offset, b.length));
        }
    }
    Ok(Batch { length, length_pos, nodes, buffers, body_len })
}

/// A field's type, as far as the fast path cares.
#[derive(Clone, Debug, PartialEq)]
pub enum ArrowType {
    Int {
        bits: i32,
        signed: bool,
    },
    /// 0 half, 1 single, 2 double.
    Float(i16),
    FixedSizeBinary(i32),
    FixedSizeList(i32),
    /// Any other `Type` union tag.
    Other(u8),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub nullable: bool,
    pub ty: ArrowType,
    pub children: Vec<Field>,
}

fn field(fb: &Fb<'_>, t: &Table, depth: u32) -> Result<Field, Error> {
    if depth > MAX_DEPTH {
        return err("fields nested too deeply");
    }
    let name = fb.string_field(t, 0)?.unwrap_or("").to_string();
    let nullable = fb.u8_field(t, 1, 0)? != 0;
    let tag = fb.u8_field(t, 2, 0)?;
    let tt = fb.table_field(t, 3)?;
    let ty = match (tag, tt) {
        (type_tag::INT, Some(x)) => ArrowType::Int { bits: fb.i32_field(&x, 0, 0)?, signed: fb.u8_field(&x, 1, 0)? != 0 },
        (type_tag::FLOATING_POINT, Some(x)) => ArrowType::Float(fb.i16_field(&x, 0, 0)?),
        (type_tag::FIXED_SIZE_BINARY, Some(x)) => ArrowType::FixedSizeBinary(fb.i32_field(&x, 0, 0)?),
        (type_tag::FIXED_SIZE_LIST, Some(x)) => ArrowType::FixedSizeList(fb.i32_field(&x, 0, 0)?),
        (t, _) => ArrowType::Other(t),
    };
    if fb.field(t, 4, 4)?.is_some() {
        return err(format!("field {name:?} is dictionary-encoded"));
    }
    let children = fb.tables_field(t, 5)?.iter().map(|c| field(fb, c, depth + 1)).collect::<Result<_, _>>()?;
    fb.key_values(t, 6)?;
    Ok(Field { name, nullable, ty, children })
}

/// Read a Schema message's flatbuffer: its top-level fields. Big-endian schemas are refused.
pub fn schema(meta: &[u8]) -> Result<Vec<Field>, Error> {
    let fb = Fb::new(meta);
    let (ty, s, body_len) = fb.message()?;
    if ty != header::SCHEMA {
        return err(format!("header type {ty}, not a Schema"));
    }
    if body_len != 0 {
        return err("a schema message with a body");
    }
    if fb.i16_field(&s, 0, 0)? != 0 {
        return err("a big-endian schema");
    }
    let fields = fb.tables_field(&s, 1)?.iter().map(|f| field(&fb, f, 0)).collect::<Result<_, _>>()?;
    fb.key_values(&s, 2)?;
    fb.vector_field(&s, 3, 8, 8)?;
    Ok(fields)
}
