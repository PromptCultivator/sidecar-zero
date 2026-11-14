//! Rendering a model's templates (worker §3 step 6, arrow-data-plane §2 and §10).
//!
//! Every byte of metadata comes from a real Arrow writer, arrow-ipc at 64-byte alignment: the
//! two Schema messages, and per slot class and direction one RecordBatch frame laid out for
//! the most rows the class can hold. Where its length fields and value buffers sit is then
//! found by walking the rendered flatbuffer ([`scz_shm::registry::frame_layout`]), never by
//! comparing renders. The registry assembled from that is verified with the same
//! [`scz_shm::registry::verify`] the supervisor's helper runs, before it is staged.

use arrow_array::{make_array, ArrayRef, RecordBatch};
use arrow_buffer::Buffer;
use arrow_data::ArrayData;
use arrow_ipc::writer::{write_message, DictionaryTracker, IpcDataGenerator, IpcWriteOptions};
use arrow_ipc::MetadataVersion;
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use scz_shm::abi::{direction, elem_type as et};
use scz_shm::registry::{self, Caps, Column, Registry, Schema, Template};
use std::sync::Arc;

/// A model's columns, and whether its batch dimension is fixed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub schema: Schema,
    /// `Some(b)`: the model takes exactly `b` rows (worker §4, "Rows and the model's batch
    /// dimension"). A request carries at most `b`.
    pub fixed_batch: Option<u32>,
}

/// The Arrow type of a column. A `list_size` above one is a FixedSizeList of non-nullable
/// elements; a scalar column is the element type itself.
pub fn arrow_type(c: &Column) -> Result<DataType, String> {
    let prim = match c.elem as u32 {
        et::I8 => DataType::Int8,
        et::I16 => DataType::Int16,
        et::I32 => DataType::Int32,
        et::I64 => DataType::Int64,
        et::U8 => DataType::UInt8,
        et::U16 => DataType::UInt16,
        et::U32 => DataType::UInt32,
        et::U64 => DataType::UInt64,
        et::F16 => DataType::Float16,
        et::F32 => DataType::Float32,
        et::F64 => DataType::Float64,
        et::FIXED_BINARY => return Ok(DataType::FixedSizeBinary(c.list_size as i32)),
        e => return Err(format!("column {:?}: unknown element type {e}", c.name)),
    };
    Ok(if c.list_size > 1 { DataType::FixedSizeList(Arc::new(Field::new("item", prim, false)), c.list_size as i32) } else { prim })
}

fn arrow_schema(cols: &[Column]) -> Result<ArrowSchema, String> {
    if let Some(c) = cols.iter().find(|c| c.nullable) {
        return Err(format!("column {:?} is nullable, which version 1 does not carry", c.name));
    }
    Ok(ArrowSchema::new(cols.iter().map(|c| Ok(Field::new(&c.name, arrow_type(c)?, false))).collect::<Result<Vec<_>, String>>()?))
}

/// An array of `len` zeros.
fn zeros(dt: &DataType, len: usize) -> ArrayData {
    let b = ArrayData::builder(dt.clone()).len(len);
    let b = match dt {
        DataType::FixedSizeList(f, n) => b.add_child_data(zeros(f.data_type(), len * *n as usize)),
        DataType::FixedSizeBinary(w) => b.add_buffer(Buffer::from(vec![0u8; len * *w as usize])),
        p => b.add_buffer(Buffer::from(vec![0u8; len * p.primitive_width().expect("a fixed-width type")])),
    };
    b.build().expect("a valid array of zeros")
}

fn options() -> IpcWriteOptions {
    IpcWriteOptions::try_new(64, false, MetadataVersion::V5).expect("64-byte alignment is supported")
}

/// A complete encapsulated Schema message.
fn schema_message(s: &ArrowSchema) -> Vec<u8> {
    let enc = IpcDataGenerator::default().schema_to_bytes_with_dictionary_tracker(s, &mut DictionaryTracker::new(false), &options());
    let mut out = vec![];
    write_message(&mut out, enc, &options()).expect("writing to a Vec");
    out
}

/// A complete encapsulated RecordBatch frame of `rows` rows of zeros.
fn frame(s: &Arc<ArrowSchema>, rows: usize) -> Result<Vec<u8>, String> {
    let cols: Vec<ArrayRef> = s.fields().iter().map(|f| make_array(zeros(f.data_type(), rows))).collect();
    let batch = RecordBatch::try_new(s.clone(), cols).map_err(|e| e.to_string())?;
    let (_, enc) =
        IpcDataGenerator::default().encode(&batch, &mut DictionaryTracker::new(false), &options(), &mut Default::default()).map_err(|e| e.to_string())?;
    let mut out = vec![];
    write_message(&mut out, enc, &options()).map_err(|e| e.to_string())?;
    Ok(out)
}

/// The most rows whose frame fits `cap` bytes, by bisection over renders.
fn max_rows(s: &Arc<ArrowSchema>, cols: &[Column], cap: u32) -> Result<u32, String> {
    let per_row: u64 = cols.iter().map(|c| c.row_bytes().unwrap_or(1) as u64).sum::<u64>().max(1);
    let (mut lo, mut hi) = (0u32, (cap as u64 / per_row + 1).min(1 << 24) as u32);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if frame(s, mid as usize)?.len() as u64 <= cap as u64 {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Ok(lo)
}

fn template(id: u16, dir: u32, class: u16, s: &Arc<ArrowSchema>, cols: &[Column], rows: u32) -> Result<Template, String> {
    let f = frame(s, rows as usize)?;
    let (moff, mlen) = scz_shm::arrowfb::framing(&f)?;
    let prefix = f[..moff + mlen].to_vec();
    let l = registry::frame_layout(&prefix, f.len() as u32, cols, rows)?;
    Ok(Template {
        id,
        direction: dir as u8,
        class,
        max_rows: rows,
        body_off: prefix.len() as u32,
        prefix,
        frame_cap: f.len() as u32,
        rows_field_off: l.rows_field_off,
        columns: l.columns,
        patches: l.patches,
    })
}

/// The registry for `sig` over slot classes with region capacities `caps`. A class that cannot
/// hold one row in both directions gets no templates. Verified before it is returned.
pub fn render(sig: &Signature, caps: &Caps) -> Result<Registry, String> {
    let req = Arc::new(arrow_schema(&sig.schema.request)?);
    let resp = Arc::new(arrow_schema(&sig.schema.response)?);
    if sig.schema.request.is_empty() || sig.schema.response.is_empty() {
        return Err("a model with no inputs or no outputs".into());
    }
    let mut templates = vec![];
    for (class, &(req_cap, resp_cap)) in caps.iter().enumerate() {
        let mut rows = max_rows(&req, &sig.schema.request, req_cap)?.min(max_rows(&resp, &sig.schema.response, resp_cap)?);
        if let Some(b) = sig.fixed_batch {
            rows = rows.min(b);
        }
        if rows == 0 {
            continue;
        }
        let c = class as u16;
        templates.push(template(2 * c + 1, direction::REQUEST, c, &req, &sig.schema.request, rows)?);
        templates.push(template(2 * c + 2, direction::RESPONSE, c, &resp, &sig.schema.response, rows)?);
    }
    if templates.is_empty() {
        return Err("no slot class holds one row of this model".into());
    }
    let reg = Registry { schema: sig.schema.clone(), in_schema: schema_message(&req), out_schema: schema_message(&resp), templates };
    let bytes = reg.to_bytes();
    if bytes.len() > scz_shm::abi::CTL_REGISTRY_LEN as usize {
        return Err(format!("a registry of {} bytes, more than the {} the control segment holds", bytes.len(), scz_shm::abi::CTL_REGISTRY_LEN));
    }
    match registry::verify(&bytes, caps)? {
        Some(back) if back == reg => Ok(reg),
        _ => Err("the rendered registry does not read back as written".into()),
    }
}
