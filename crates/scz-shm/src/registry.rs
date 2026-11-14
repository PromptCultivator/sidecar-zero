//! The registry (arrow-data-plane §4): the model's columns and, per slot class, one request
//! and one response template, laid out by `abi/scz_abi.toml`'s registry regions.
//!
//! Three parties handle the same bytes:
//!
//! | Who | Calls | When |
//! |---|---|---|
//! | Worker | [`Registry::to_bytes`] | after rendering its templates, to stage the candidate |
//! | Supervisor's verifier helper | [`verify`] | before the candidate is published |
//! | SDK | [`parse`] | when it binds, over the copy it read under the model seqlock |
//!
//! [`parse`] checks everything a fast path relies on: every offset, length, and patch
//! position against the registry, the prefix, and the slot class's region. [`verify`] also
//! walks every prefix and both Schema messages with [`crate::arrowfb`] and checks that they
//! say what the fixed structures say. An empty registry (no bytes) is the raw mode of the echo
//! and chaos workers: no templates, fingerprint 0.
//!
//! Version 1 is the fast tier of arrow-data-plane §6: fixed-width, non-nullable columns.

use crate::abi::{self, column_desc as cd, direction, elem_type as et, patch_desc as pd, patch_kind, registry_header as rh, template_desc as td};
use crate::arrowfb::{self, ArrowType};

pub type Error = String;

fn err<T>(what: impl Into<String>) -> Result<T, Error> {
    Err(what.into())
}

// ── the schema ──────────────────────────────────────────────────────────────────────────

/// One column, as the fingerprint and the SDKs see it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Column {
    pub name: String,
    pub elem: u8,
    /// 1 for a scalar, N for `FixedSizeList<T>[N]`, the byte width for `FIXED_BINARY`.
    pub list_size: u32,
    pub nullable: bool,
}

impl Column {
    pub fn new(name: &str, elem: u32, list_size: u32) -> Column {
        Column { name: name.to_string(), elem: elem as u8, list_size, nullable: false }
    }
    /// Bytes of values per row.
    pub fn row_bytes(&self) -> Option<u32> {
        elem_width(self.elem)?.checked_mul(self.list_size)
    }
}

/// Bytes per element of a physical type; None for an unknown code.
pub fn elem_width(elem: u8) -> Option<u32> {
    Some(match elem as u32 {
        et::I8 | et::U8 | et::FIXED_BINARY => 1,
        et::I16 | et::U16 | et::F16 => 2,
        et::I32 | et::U32 | et::F32 => 4,
        et::I64 | et::U64 | et::F64 => 8,
        _ => return None,
    })
}

/// A model's request and response columns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Schema {
    pub request: Vec<Column>,
    pub response: Vec<Column>,
}

/// arrow-data-plane §9: xxHash3-64, seed 0, over the logical schema. Golden vectors:
/// `abi/golden/fingerprints.txt`.
pub fn fingerprint(s: &Schema) -> u64 {
    let mut b = b"SCZSCHEMA\x01".to_vec();
    for cols in [&s.request, &s.response] {
        b.extend_from_slice(&(cols.len() as u16).to_le_bytes());
        for c in cols {
            b.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
            b.extend_from_slice(c.name.as_bytes());
            b.push(c.elem);
            b.extend_from_slice(&c.list_size.to_le_bytes());
            b.push(u8::from(c.nullable));
        }
    }
    xxhash_rust::xxh3::xxh3_64(&b)
}

/// The column an Arrow field describes, if it is one the fast path carries.
pub fn column_of(f: &arrowfb::Field) -> Result<Column, Error> {
    let prim = |ty: &ArrowType| -> Option<(u32, u32)> {
        Some(match *ty {
            ArrowType::Int { bits: 8, signed: true } => (et::I8, 1),
            ArrowType::Int { bits: 16, signed: true } => (et::I16, 1),
            ArrowType::Int { bits: 32, signed: true } => (et::I32, 1),
            ArrowType::Int { bits: 64, signed: true } => (et::I64, 1),
            ArrowType::Int { bits: 8, signed: false } => (et::U8, 1),
            ArrowType::Int { bits: 16, signed: false } => (et::U16, 1),
            ArrowType::Int { bits: 32, signed: false } => (et::U32, 1),
            ArrowType::Int { bits: 64, signed: false } => (et::U64, 1),
            ArrowType::Float(0) => (et::F16, 1),
            ArrowType::Float(1) => (et::F32, 1),
            ArrowType::Float(2) => (et::F64, 1),
            ArrowType::FixedSizeBinary(w) if w > 0 => (et::FIXED_BINARY, w as u32),
            _ => return None,
        })
    };
    let (elem, list_size) = match (&f.ty, f.children.as_slice()) {
        (ArrowType::FixedSizeList(1), _) => return err(format!("column {:?}: a FixedSizeList of one is written as a scalar column", f.name)),
        (ArrowType::FixedSizeList(n), [child]) if *n > 0 => {
            if child.nullable {
                return err(format!("column {:?}: a nullable list element", f.name));
            }
            match prim(&child.ty) {
                Some((e, 1)) => (e, *n as u32),
                _ => return err(format!("column {:?}: a list of {:?}", f.name, child.ty)),
            }
        }
        (ty, []) => prim(ty).ok_or(format!("column {:?}: type {ty:?} has no fixed-width form", f.name))?,
        (ty, _) => return err(format!("column {:?}: type {ty:?} with children", f.name)),
    };
    Ok(Column { name: f.name.clone(), elem: elem as u8, list_size, nullable: f.nullable })
}

// ── templates ───────────────────────────────────────────────────────────────────────────

/// Where one column's values sit in a region laid out by a template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnLoc {
    pub column: Column,
    pub values_off: u32,
    pub row_bytes: u32,
}

/// A length field in the prefix, and how it follows the row count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Patch {
    pub prefix_off: u32,
    pub kind: u8,
    pub scale: u32,
}

impl Patch {
    pub fn value(&self, rows: u32) -> u64 {
        match self.kind as u32 {
            patch_kind::BITMAP_BYTES => (rows as u64).div_ceil(8),
            _ => rows as u64 * self.scale as u64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    pub id: u16,
    pub direction: u8,
    pub class: u16,
    pub max_rows: u32,
    /// The bytes before the body: continuation marker, metadata size, flatbuffer, padding.
    pub prefix: Vec<u8>,
    pub body_off: u32,
    pub frame_cap: u32,
    pub rows_field_off: u32,
    pub columns: Vec<ColumnLoc>,
    pub patches: Vec<Patch>,
}

impl Template {
    /// `(registry_epoch, low 16 bits) << 16 | template_id`: what a region holds once stamped.
    pub fn stamp(&self, epoch: u32) -> u32 {
        stamp(epoch, self.id)
    }
}

pub fn stamp(epoch: u32, id: u16) -> u32 {
    (epoch & 0xFFFF) << 16 | id as u32
}

/// A parsed registry. `in_schema` and `out_schema` are complete Arrow IPC Schema messages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    pub schema: Schema,
    pub in_schema: Vec<u8>,
    pub out_schema: Vec<u8>,
    pub templates: Vec<Template>,
}

/// The two region capacities of each slot class, in class order.
pub type Caps = [(u32, u32)];

impl Registry {
    pub fn fingerprint(&self) -> u64 {
        if self.templates.is_empty() {
            0
        } else {
            fingerprint(&self.schema)
        }
    }
    pub fn template(&self, class: usize, dir: u32) -> Option<&Template> {
        self.templates.iter().find(|t| t.class as usize == class && t.direction as u32 == dir)
    }
    /// The smallest class whose request template holds `rows` (arrow-data-plane §4).
    pub fn class_for(&self, rows: u32) -> Option<usize> {
        let mut best: Option<&Template> = None;
        for t in self.templates.iter().filter(|t| t.direction as u32 == direction::REQUEST && t.max_rows >= rows) {
            if best.is_none_or(|b| t.max_rows < b.max_rows) {
                best = Some(t);
            }
        }
        best.map(|t| t.class as usize)
    }

    /// The bytes a worker stages.
    pub fn to_bytes(&self) -> Vec<u8> {
        fn up8(v: usize) -> usize {
            v.div_ceil(8) * 8
        }
        let ncols: usize = self.templates.iter().map(|t| t.columns.len()).sum();
        let npatch: usize = self.templates.iter().map(|t| t.patches.len()).sum();
        let mut names: Vec<u8> = vec![];
        let mut name_at = std::collections::HashMap::new();
        for c in self.templates.iter().flat_map(|t| &t.columns) {
            name_at.entry(c.column.name.clone()).or_insert_with(|| {
                let at = names.len();
                names.extend_from_slice(c.column.name.as_bytes());
                at
            });
        }
        let templates_off = rh::SIZE;
        let columns_off = templates_off + self.templates.len() * td::SIZE;
        let patches_off = columns_off + ncols * cd::SIZE;
        let names_off = up8(patches_off + npatch * pd::SIZE);
        let in_off = up8(names_off + names.len());
        let out_off = up8(in_off + self.in_schema.len());
        let mut prefix_at = up8(out_off + self.out_schema.len());
        let mut b = vec![0u8; prefix_at];
        let put16 = |b: &mut Vec<u8>, at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        b[rh::MAGIC..rh::MAGIC + 8].copy_from_slice(&abi::REGISTRY_MAGIC.to_le_bytes());
        put16(&mut b, rh::TEMPLATE_COUNT, self.templates.len() as u16);
        put16(&mut b, rh::TEMPLATE_STRIDE, td::SIZE as u16);
        put16(&mut b, rh::COLUMN_STRIDE, cd::SIZE as u16);
        put16(&mut b, rh::PATCH_STRIDE, pd::SIZE as u16);
        put32(&mut b, rh::IN_SCHEMA_OFF, in_off as u32);
        put32(&mut b, rh::IN_SCHEMA_LEN, self.in_schema.len() as u32);
        put32(&mut b, rh::OUT_SCHEMA_OFF, out_off as u32);
        put32(&mut b, rh::OUT_SCHEMA_LEN, self.out_schema.len() as u32);
        put32(&mut b, rh::TEMPLATES_OFF, templates_off as u32);
        put32(&mut b, rh::COLUMNS_OFF, columns_off as u32);
        put32(&mut b, rh::PATCHES_OFF, patches_off as u32);
        put32(&mut b, rh::NAMES_OFF, names_off as u32);
        put32(&mut b, rh::NAMES_LEN, names.len() as u32);
        put32(&mut b, rh::COLUMN_TOTAL, ncols as u32);
        put32(&mut b, rh::PATCH_TOTAL, npatch as u32);
        b[names_off..names_off + names.len()].copy_from_slice(&names);
        b[in_off..in_off + self.in_schema.len()].copy_from_slice(&self.in_schema);
        b[out_off..out_off + self.out_schema.len()].copy_from_slice(&self.out_schema);
        let (mut col, mut patch) = (0usize, 0usize);
        let mut prefixes = vec![];
        for (k, t) in self.templates.iter().enumerate() {
            let at = templates_off + k * td::SIZE;
            put16(&mut b, at + td::TEMPLATE_ID, t.id);
            b[at + td::DIRECTION] = t.direction;
            put16(&mut b, at + td::SLAB_CLASS, t.class);
            put16(&mut b, at + td::COLUMN_COUNT, t.columns.len() as u16);
            put32(&mut b, at + td::MAX_ROWS, t.max_rows);
            put32(&mut b, at + td::PREFIX_OFF, prefix_at as u32);
            put32(&mut b, at + td::PREFIX_LEN, t.prefix.len() as u32);
            put32(&mut b, at + td::BODY_OFF, t.body_off);
            put32(&mut b, at + td::FRAME_CAP, t.frame_cap);
            put32(&mut b, at + td::FIRST_COLUMN, col as u32);
            put32(&mut b, at + td::FIRST_PATCH, patch as u32);
            put16(&mut b, at + td::PATCH_COUNT, t.patches.len() as u16);
            put32(&mut b, at + td::ROWS_FIELD_OFF, t.rows_field_off);
            for c in &t.columns {
                let at = columns_off + col * cd::SIZE;
                put32(&mut b, at + cd::VALUES_OFF, c.values_off);
                put32(&mut b, at + cd::ROW_BYTES, c.row_bytes);
                put32(&mut b, at + cd::LIST_SIZE, c.column.list_size);
                put32(&mut b, at + cd::NAME_OFF, name_at[&c.column.name] as u32);
                put16(&mut b, at + cd::NAME_LEN, c.column.name.len() as u16);
                b[at + cd::ELEM_TYPE] = c.column.elem;
                b[at + cd::NULLABLE] = u8::from(c.column.nullable);
                col += 1;
            }
            for p in &t.patches {
                let at = patches_off + patch * pd::SIZE;
                put32(&mut b, at + pd::PREFIX_OFF, p.prefix_off);
                put32(&mut b, at + pd::SCALE, p.scale);
                b[at + pd::KIND] = p.kind;
                patch += 1;
            }
            prefixes.push(prefix_at);
            prefix_at = up8(prefix_at + t.prefix.len());
        }
        b.resize(prefix_at, 0);
        for (t, at) in self.templates.iter().zip(prefixes) {
            b[at..at + t.prefix.len()].copy_from_slice(&t.prefix);
        }
        let len = b.len() as u32;
        put32(&mut b, rh::REGISTRY_LEN, len);
        b
    }
}

// ── reading ─────────────────────────────────────────────────────────────────────────────

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn get<const N: usize>(&self, at: usize) -> Result<[u8; N], Error> {
        at.checked_add(N).and_then(|end| self.0.get(at..end)).map(|s| s.try_into().unwrap()).ok_or(format!("read at {at:#x} past the registry"))
    }
    fn u8(&self, at: usize) -> Result<u8, Error> {
        Ok(self.get::<1>(at)?[0])
    }
    fn u16(&self, at: usize) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.get(at)?))
    }
    fn u32(&self, at: usize) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.get(at)?))
    }
    fn u64(&self, at: usize) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.get(at)?))
    }
    fn slice(&self, off: u32, len: u32) -> Result<&'a [u8], Error> {
        let (off, len) = (off as usize, len as usize);
        off.checked_add(len).and_then(|end| self.0.get(off..end)).ok_or(format!("{len} bytes at {off:#x} lie outside the registry"))
    }
    /// The start of `count` entries of `stride` bytes at `off`, checked to fit.
    fn array(&self, off: u32, count: usize, stride: usize, min: usize, align: usize) -> Result<usize, Error> {
        if stride < min || !stride.is_multiple_of(align.min(4)) {
            return err(format!("stride {stride} below {min}"));
        }
        let off = off as usize;
        if !off.is_multiple_of(align) {
            return err(format!("array at {off:#x} is misaligned"));
        }
        match count.checked_mul(stride).and_then(|n| off.checked_add(n)) {
            Some(end) if end <= self.0.len() => Ok(off),
            _ => err(format!("{count} entries at {off:#x} run past the registry")),
        }
    }
}

/// Parse a registry copied out of shared memory, checking everything the fast path relies on
/// (arrow-data-plane §8, "fast path has nothing to trust"). `Ok(None)`: an empty registry.
pub fn parse(bytes: &[u8], caps: &Caps) -> Result<Option<Registry>, Error> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let r = Reader(bytes);
    if r.u64(rh::MAGIC)? != abi::REGISTRY_MAGIC {
        return err("bad magic");
    }
    let len = r.u32(rh::REGISTRY_LEN)? as usize;
    if len != bytes.len() || len > abi::CTL_REGISTRY_LEN as usize {
        return err(format!("registry_len {len} for {} bytes", bytes.len()));
    }
    let nt = r.u16(rh::TEMPLATE_COUNT)? as usize;
    let (ts, cs, ps) = (r.u16(rh::TEMPLATE_STRIDE)? as usize, r.u16(rh::COLUMN_STRIDE)? as usize, r.u16(rh::PATCH_STRIDE)? as usize);
    let (nc, np) = (r.u32(rh::COLUMN_TOTAL)? as usize, r.u32(rh::PATCH_TOTAL)? as usize);
    if nt == 0 || nt > abi::MAX_TEMPLATES as usize || nc > nt * abi::MAX_COLUMNS as usize || np > nt * abi::MAX_PATCHES as usize {
        return err(format!("{nt} templates, {nc} columns, {np} patches"));
    }
    let toff = r.array(r.u32(rh::TEMPLATES_OFF)?, nt, ts, td::SIZE, 8)?;
    let coff = r.array(r.u32(rh::COLUMNS_OFF)?, nc, cs, cd::SIZE, 4)?;
    let poff = r.array(r.u32(rh::PATCHES_OFF)?, np, ps, pd::SIZE, 4)?;
    let names = r.slice(r.u32(rh::NAMES_OFF)?, r.u32(rh::NAMES_LEN)?)?;
    let in_schema = r.slice(r.u32(rh::IN_SCHEMA_OFF)?, r.u32(rh::IN_SCHEMA_LEN)?)?.to_vec();
    let out_schema = r.slice(r.u32(rh::OUT_SCHEMA_OFF)?, r.u32(rh::OUT_SCHEMA_LEN)?)?.to_vec();
    let mut templates: Vec<Template> = vec![];
    for k in 0..nt {
        let at = toff + k * ts;
        let id = r.u16(at + td::TEMPLATE_ID)?;
        let dir = r.u8(at + td::DIRECTION)?;
        let class = r.u16(at + td::SLAB_CLASS)?;
        let what = format!("template {id}");
        if id == 0 || templates.iter().any(|t| t.id == id) {
            return err(format!("{what}: id 0 or repeated"));
        }
        let Some(&(req_cap, resp_cap)) = caps.get(class as usize) else { return err(format!("{what}: no slot class {class}")) };
        let cap = match dir as u32 {
            direction::REQUEST => req_cap,
            direction::RESPONSE => resp_cap,
            _ => return err(format!("{what}: direction {dir}")),
        };
        if templates.iter().any(|t| t.class == class && t.direction == dir) {
            return err(format!("{what}: a second template for class {class}"));
        }
        let max_rows = r.u32(at + td::MAX_ROWS)?;
        let prefix = r.slice(r.u32(at + td::PREFIX_OFF)?, r.u32(at + td::PREFIX_LEN)?)?.to_vec();
        let body_off = r.u32(at + td::BODY_OFF)?;
        let frame_cap = r.u32(at + td::FRAME_CAP)?;
        let rows_field_off = r.u32(at + td::ROWS_FIELD_OFF)?;
        if max_rows == 0 || prefix.len() < 8 || body_off as usize != prefix.len() || !body_off.is_multiple_of(8) || frame_cap < body_off || frame_cap > cap {
            return err(format!("{what}: max_rows {max_rows}, prefix {} bytes, body at {body_off}, frame {frame_cap} in a {cap} byte region", prefix.len()));
        }
        let in_prefix = |off: u32| off.is_multiple_of(8) && off as usize + 8 <= prefix.len();
        if !in_prefix(rows_field_off) {
            return err(format!("{what}: row count field at {rows_field_off} outside the prefix"));
        }
        let (c0, ncol) = (r.u32(at + td::FIRST_COLUMN)? as usize, r.u16(at + td::COLUMN_COUNT)? as usize);
        let (p0, npat) = (r.u32(at + td::FIRST_PATCH)? as usize, r.u16(at + td::PATCH_COUNT)? as usize);
        if c0.checked_add(ncol).is_none_or(|e| e > nc)
            || p0.checked_add(npat).is_none_or(|e| e > np)
            || ncol > abi::MAX_COLUMNS as usize
            || npat > abi::MAX_PATCHES as usize
        {
            return err(format!("{what}: column or patch range out of bounds"));
        }
        let mut columns = vec![];
        for i in c0..c0 + ncol {
            let at = coff + i * cs;
            let name_off = r.u32(at + cd::NAME_OFF)? as usize;
            let name_len = r.u16(at + cd::NAME_LEN)? as usize;
            let name = names.get(name_off..name_off.saturating_add(name_len)).ok_or(format!("{what}: a column name outside the names"))?;
            let name = core::str::from_utf8(name).map_err(|_| format!("{what}: a column name that is not UTF-8"))?;
            let column = Column {
                name: name.to_string(),
                elem: r.u8(at + cd::ELEM_TYPE)?,
                list_size: r.u32(at + cd::LIST_SIZE)?,
                nullable: r.u8(at + cd::NULLABLE)? != 0,
            };
            let values_off = r.u32(at + cd::VALUES_OFF)?;
            let row_bytes = r.u32(at + cd::ROW_BYTES)?;
            if column.nullable || r.u32(at + cd::VALIDITY_OFF)? != 0 || r.u32(at + cd::NULL_COUNT_OFF)? != 0 {
                return err(format!("{what}: column {name:?} is nullable, which version 1 does not carry"));
            }
            if column.list_size == 0 || column.row_bytes() != Some(row_bytes) || row_bytes == 0 {
                return err(format!("{what}: column {name:?} has {row_bytes} bytes per row for type {} × {}", column.elem, column.list_size));
            }
            let end = values_off as u64 + max_rows as u64 * row_bytes as u64;
            if values_off < body_off || !values_off.is_multiple_of(8) || end > frame_cap as u64 {
                return err(format!("{what}: column {name:?} values at {values_off} for {max_rows} rows outside the frame"));
            }
            columns.push(ColumnLoc { column, values_off, row_bytes });
        }
        let mut patches = vec![];
        for i in p0..p0 + npat {
            let at = poff + i * ps;
            let p = Patch { prefix_off: r.u32(at + pd::PREFIX_OFF)?, scale: r.u32(at + pd::SCALE)?, kind: r.u8(at + pd::KIND)? };
            let ok_kind = matches!(p.kind as u32, patch_kind::ROWS_SCALED if p.scale > 0) || p.kind as u32 == patch_kind::BITMAP_BYTES;
            if !ok_kind || !in_prefix(p.prefix_off) || p.prefix_off < 8 {
                return err(format!("{what}: a patch at {} of kind {} outside the prefix", p.prefix_off, p.kind));
            }
            patches.push(p);
        }
        if !patches.iter().any(|p| p.prefix_off == rows_field_off && p.kind as u32 == patch_kind::ROWS_SCALED && p.scale == 1) {
            return err(format!("{what}: no patch rewrites the row count"));
        }
        templates.push(Template { id, direction: dir, class, max_rows, prefix, body_off, frame_cap, rows_field_off, columns, patches });
    }
    // Every class has both templates or neither, with the same row capacity, and every
    // template of a direction carries the same columns.
    let mut schema = Schema::default();
    for t in &templates {
        let other = if t.direction as u32 == direction::REQUEST { direction::RESPONSE } else { direction::REQUEST };
        match templates.iter().find(|u| u.class == t.class && u.direction as u32 == other) {
            Some(u) if u.max_rows == t.max_rows => {}
            _ => return err(format!("template {}: class {} lacks a matching template of the other direction", t.id, t.class)),
        }
        let cols: Vec<Column> = t.columns.iter().map(|c| c.column.clone()).collect();
        let slot = if t.direction as u32 == direction::REQUEST { &mut schema.request } else { &mut schema.response };
        if slot.is_empty() {
            *slot = cols;
        } else if *slot != cols {
            return err(format!("template {}: columns differ from another template of its direction", t.id));
        }
    }
    if schema.request.is_empty() || schema.response.is_empty() {
        return err("a direction without columns");
    }
    Ok(Some(Registry { schema, in_schema, out_schema, templates }))
}

/// Where a frame's length fields and value buffers are, found by walking its metadata
/// (arrow-data-plane §10: positions come from the flatbuffer, never from comparing renders).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameLayout {
    pub rows_field_off: u32,
    pub columns: Vec<ColumnLoc>,
    /// Sorted.
    pub patches: Vec<Patch>,
}

/// Walk a frame rendered at `max_rows` rows of `columns` (non-nullable, fixed width), whose
/// body begins where `prefix` ends and whose whole length is `frame_cap`. Checks that it holds
/// what such a frame must: the row count, a node per column and one more for a list's child,
/// empty validity buffers, value buffers of `max_rows` rows on 8-byte boundaries, no two
/// buffers overlapping, and a body that ends at `frame_cap`.
pub fn frame_layout(prefix: &[u8], frame_cap: u32, columns: &[Column], max_rows: u32) -> Result<FrameLayout, Error> {
    let (moff, mlen) = arrowfb::framing(prefix)?;
    if moff + mlen != prefix.len() {
        return err(format!("the metadata ends at {}, the prefix at {}", moff + mlen, prefix.len()));
    }
    let body_off = prefix.len() as i64;
    let b = arrowfb::record_batch(&prefix[moff..moff + mlen])?;
    let at = |pos: usize| (moff + pos) as u32;
    let Some(rows_pos) = b.length_pos.filter(|_| b.length == max_rows as i64) else {
        return err(format!("the frame holds {} rows, not {max_rows}", b.length));
    };
    if body_off + b.body_len != frame_cap as i64 {
        return err(format!("a body of {} bytes at {body_off} for a frame of {frame_cap}", b.body_len));
    }
    // The IPC layout of a non-nullable fixed-width column: a scalar or binary column is one
    // node and buffers (validity, values); a FixedSizeList is a node for the list and one for
    // its child, and buffers (validity, child validity, values).
    let mut patches = vec![Patch { prefix_off: at(rows_pos), kind: patch_kind::ROWS_SCALED as u8, scale: 1 }];
    let mut locs = vec![];
    let (mut n, mut k) = (0usize, 0usize);
    for c in columns {
        let row_bytes = c.row_bytes().ok_or(format!("column {:?}: unknown type {}", c.name, c.elem))?;
        let list = c.elem as u32 != et::FIXED_BINARY && c.list_size > 1;
        let scales: &[u32] = if list { &[1, c.list_size] } else { &[1] };
        for &s in scales {
            let node = b.nodes.get(n).ok_or("too few field nodes")?;
            if node.length != max_rows as i64 * s as i64 || node.null_count != 0 {
                return err(format!("node {n} holds {} values, {} null", node.length, node.null_count));
            }
            patches.push(Patch { prefix_off: at(node.length_pos), kind: patch_kind::ROWS_SCALED as u8, scale: s });
            n += 1;
        }
        for _ in 0..if list { 2 } else { 1 } {
            let v = b.buffers.get(k).ok_or("too few buffers")?;
            if v.length != 0 {
                return err(format!("a validity buffer of {} bytes in a non-nullable column", v.length));
            }
            k += 1;
        }
        let v = b.buffers.get(k).ok_or("too few buffers")?;
        if v.length != max_rows as i64 * row_bytes as i64 || v.offset % 8 != 0 {
            return err(format!("column {:?}: {} bytes of values at body offset {}", c.name, v.length, v.offset));
        }
        locs.push(ColumnLoc { column: c.clone(), values_off: (body_off + v.offset) as u32, row_bytes });
        patches.push(Patch { prefix_off: at(v.length_pos), kind: patch_kind::ROWS_SCALED as u8, scale: row_bytes });
        k += 1;
    }
    if n != b.nodes.len() || k != b.buffers.len() {
        return err(format!("{} nodes and {} buffers for columns that need {n} and {k}", b.nodes.len(), b.buffers.len()));
    }
    let mut spans: Vec<(i64, i64)> = b.buffers.iter().map(|x| (x.offset, x.offset + x.length)).collect();
    spans.sort();
    if spans.windows(2).any(|w| w[1].0 < w[0].1) {
        return err("buffers overlap");
    }
    patches.sort();
    Ok(FrameLayout { rows_field_off: at(rows_pos), columns: locs, patches })
}

/// A template must say what walking its own prefix finds.
fn check_prefix(t: &Template) -> Result<(), Error> {
    let cols: Vec<Column> = t.columns.iter().map(|c| c.column.clone()).collect();
    let l = frame_layout(&t.prefix, t.frame_cap, &cols, t.max_rows).map_err(|e| format!("template {}: {e}", t.id))?;
    let mut got = t.patches.clone();
    got.sort();
    if l.rows_field_off != t.rows_field_off || l.columns != t.columns || l.patches != got {
        return err(format!("template {}: its descriptors disagree with its frame ({l:?})", t.id));
    }
    Ok(())
}

fn schema_columns(msg: &[u8]) -> Result<Vec<Column>, Error> {
    let (moff, mlen) = arrowfb::framing(msg)?;
    arrowfb::schema(&msg[moff..moff + mlen])?.iter().map(column_of).collect()
}

/// The supervisor's verification of a staged candidate (arrow-data-plane §4): [`parse`],
/// then every prefix and both Schema messages walked and compared with the fixed structures.
/// On success the caller publishes exactly `bytes`, with [`Registry::fingerprint`].
pub fn verify(bytes: &[u8], caps: &Caps) -> Result<Option<Registry>, Error> {
    let Some(reg) = parse(bytes, caps)? else { return Ok(None) };
    if schema_columns(&reg.in_schema).map_err(|e| format!("request schema: {e}"))? != reg.schema.request {
        return err("the request Schema message disagrees with the request columns");
    }
    if schema_columns(&reg.out_schema).map_err(|e| format!("response schema: {e}"))? != reg.schema.response {
        return err("the response Schema message disagrees with the response columns");
    }
    for t in &reg.templates {
        check_prefix(t)?;
    }
    Ok(Some(reg))
}

// ── the fast path ───────────────────────────────────────────────────────────────────────

/// Make the region at `region` a frame of `t` carrying `rows` rows (arrow-data-plane §7.1):
/// copy the prefix in if the region's stamp (`stamp_word`) is not `want`, then rewrite the
/// length fields if the row count differs from the one the frame last held.
pub fn prepare(seg: &crate::seg::Seg<'_>, region: usize, t: &Template, stamp_word: &core::sync::atomic::AtomicU32, want: u32, rows: u32) {
    use core::sync::atomic::Ordering::Relaxed;
    if stamp_word.load(Relaxed) != want {
        seg.copy_in(region, &t.prefix);
        stamp_word.store(want, Relaxed);
    }
    if seg.u64_at(region + t.rows_field_off as usize).load(Relaxed) != rows as u64 {
        for p in &t.patches {
            seg.u64_at(region + p.prefix_off as usize).store(p.value(rows), Relaxed);
        }
    }
}

/// The row count a frame of `t` at `region` holds, read once.
pub fn rows(seg: &crate::seg::Seg<'_>, region: usize, t: &Template) -> u64 {
    seg.u64_at(region + t.rows_field_off as usize).load(core::sync::atomic::Ordering::Relaxed)
}
