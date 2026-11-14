//! Spike S4, Rust side (arrow-rs).
//!
//!   s4-rust read FRAMES_DIR      read every frame in manifest.tsv, print one JSON line per (frame, mode)
//!   s4-rust render OUT_DIR       write each variant at max_rows with the stock StreamWriter at alignment 8 and 64
//!   s4-rust align-probe FRAMES_DIR   read template frames from deliberately misaligned regions
//!
//! Read modes:
//!   read_record_batch  the path docs/components/03-arrow-data-plane.md section 7.4 names: the region is a
//!                      Buffer::from_custom_allocation over an anonymous shared mmap (page-aligned, like a
//!                      slot), the message is parsed with root_as_message and decoded with
//!                      arrow_ipc::reader::read_record_batch against the registry's schema.
//!   StreamDecoder      the push decoder with require_alignment(true), fed the Schema message and then the
//!                      region. It refuses to copy a misaligned buffer, so success means zero-copy.
//!   StreamReader       the stock io::Read stream reader over Schema message + frame bytes.
//! Every mode then runs ArrayData::validate_full on every column.

use std::collections::HashMap;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;
use std::{env, fs};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type, Int32Type, Int64Type, UInt8Type};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, Float64Array, Int32Array, Int64Array,
    RecordBatch, UInt8Array,
};
use arrow_buffer::Buffer;
use arrow_ipc::convert::try_fb_to_schema;
use arrow_ipc::reader::{read_record_batch, StreamDecoder, StreamReader};
use arrow_ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow_ipc::{root_as_message, MessageHeader, MetadataVersion};
use arrow_schema::{DataType, Field, Schema, SchemaRef};

const LIB: &str = "arrow-rs arrow-ipc 60.1.0";

// ---------------------------------------------------------------- region

/// An anonymous shared mapping standing in for a slot's request region.
struct Mapping {
    addr: usize,
    len: usize,
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.addr as *mut libc::c_void, self.len) };
    }
}

/// Maps a fresh region and copies `bytes` in at `shift` bytes past the page boundary. shift = 0 is
/// what a slot looks like (64-byte aligned); other shifts exist only for the alignment probe.
fn mmap_region(bytes: &[u8], shift: usize) -> Buffer {
    let len = bytes.len() + shift + 1;
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert!(p != libc::MAP_FAILED, "mmap failed");
        let start = (p as *mut u8).add(shift);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), start, bytes.len());
        let owner = Arc::new(Mapping { addr: p as usize, len });
        Buffer::from_custom_allocation(NonNull::new(start).unwrap(), bytes.len(), owner)
    }
}

// ---------------------------------------------------------------- checksum

struct Fnv(u64);
impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf29ce484222325)
    }
    fn feed(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x100000001b3);
        }
    }
}

fn feed_elem(a: &dyn Array, i: usize, h: &mut Fnv) -> Result<(), String> {
    match a.data_type() {
        DataType::Float32 => h.feed(&a.as_primitive::<Float32Type>().value(i).to_le_bytes()),
        DataType::Float64 => h.feed(&a.as_primitive::<Float64Type>().value(i).to_le_bytes()),
        DataType::Int64 => h.feed(&a.as_primitive::<Int64Type>().value(i).to_le_bytes()),
        DataType::Int32 => h.feed(&a.as_primitive::<Int32Type>().value(i).to_le_bytes()),
        DataType::UInt8 => h.feed(&a.as_primitive::<UInt8Type>().value(i).to_le_bytes()),
        other => return Err(format!("unexpected element type {other}")),
    }
    Ok(())
}

/// (null count, checksum) of one column, read through the array accessors.
fn column_summary(col: &ArrayRef) -> Result<(usize, String), String> {
    let mut h = Fnv::new();
    for r in 0..col.len() {
        if col.is_null(r) {
            h.feed(&[0]);
            continue;
        }
        h.feed(&[1]);
        if let DataType::FixedSizeList(_, _) = col.data_type() {
            let row = col.as_fixed_size_list().value(r);
            for j in 0..row.len() {
                feed_elem(row.as_ref(), j, &mut h)?;
            }
        } else {
            feed_elem(col.as_ref(), r, &mut h)?;
        }
    }
    Ok((col.null_count(), format!("{:016x}", h.0)))
}

fn values_ptr(col: &ArrayRef) -> usize {
    let data = col.to_data();
    let leaf = if let DataType::FixedSizeList(_, _) = col.data_type() {
        data.child_data()[0].clone()
    } else {
        data
    };
    leaf.buffers()[0].as_ptr() as usize
}

// ---------------------------------------------------------------- read modes

struct Outcome {
    batch: RecordBatch,
    borrowed: Option<bool>,
    detail: String,
}

fn parse_schema(schema_msg: &[u8]) -> Result<SchemaRef, String> {
    if schema_msg.len() < 8 || schema_msg[0..4] != [0xff; 4] {
        return Err("schema message: no continuation marker".into());
    }
    let n = i32::from_le_bytes(schema_msg[4..8].try_into().unwrap()) as usize;
    let m = root_as_message(&schema_msg[8..8 + n]).map_err(|e| format!("schema message: {e}"))?;
    let s = m.header_as_schema().ok_or("schema message: header is not a Schema")?;
    Ok(Arc::new(try_fb_to_schema(s).map_err(|e| format!("schema message: {e}"))?))
}

fn all_borrowed(batch: &RecordBatch, base: usize, len: usize) -> bool {
    batch.columns().iter().all(|c| {
        let p = values_ptr(c);
        p >= base && p < base + len
    })
}

fn mode_read_record_batch(schema_msg: &[u8], frame: &[u8], shift: usize) -> Result<Outcome, String> {
    let schema = parse_schema(schema_msg)?;
    let region = mmap_region(frame, shift);
    let base = region.as_ptr() as usize;
    let b = region.as_slice();
    if b.len() < 8 || b[0..4] != [0xff; 4] {
        return Err("no continuation marker".into());
    }
    let meta_len = i32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    if 8 + meta_len > b.len() {
        return Err("metadata_size runs past the region".into());
    }
    let message = root_as_message(&b[8..8 + meta_len]).map_err(|e| format!("root_as_message: {e}"))?;
    if message.header_type() != MessageHeader::RecordBatch {
        return Err(format!("header type {:?}", message.header_type()));
    }
    let rb = message.header_as_record_batch().ok_or("no RecordBatch header")?;
    let body_len = message.bodyLength() as usize;
    if 8 + meta_len + body_len > b.len() {
        return Err("bodyLength runs past the region".into());
    }
    let body = region.slice_with_length(8 + meta_len, body_len);
    let detail = format!(
        "metadata={}B body={}B region@{}mod4096 body@{}mod64",
        meta_len,
        body_len,
        base % 4096,
        body.as_ptr() as usize % 64
    );
    let batch = read_record_batch(&body, rb, schema, &HashMap::new(), None, &message.version())
        .map_err(|e| e.to_string())?;
    let borrowed = Some(all_borrowed(&batch, base, frame.len()));
    Ok(Outcome { batch, borrowed, detail })
}

fn mode_stream_decoder(schema_msg: &[u8], frame: &[u8], shift: usize) -> Result<Outcome, String> {
    let mut dec = StreamDecoder::new().with_require_alignment(true);
    let mut s = Buffer::from(schema_msg.to_vec());
    if dec.decode(&mut s).map_err(|e| e.to_string())?.is_some() {
        return Err("a batch came out of the schema message".into());
    }
    let mut region = mmap_region(frame, shift);
    let base = region.as_ptr() as usize;
    let batch = dec
        .decode(&mut region)
        .map_err(|e| e.to_string())?
        .ok_or("decoder wanted more bytes after the whole frame")?;
    let detail = format!("require_alignment=true, {} bytes left undecoded", region.len());
    let borrowed = Some(all_borrowed(&batch, base, frame.len()));
    Ok(Outcome { batch, borrowed, detail })
}

fn mode_stream_reader(schema_msg: &[u8], frame: &[u8]) -> Result<Outcome, String> {
    let mut stream = schema_msg.to_vec();
    stream.extend_from_slice(frame);
    let mut rdr = StreamReader::try_new(Cursor::new(stream), None).map_err(|e| e.to_string())?;
    let batch = rdr.next().ok_or("stream ended before a batch")?.map_err(|e| e.to_string())?;
    let detail = match rdr.next() {
        None => "stream ended cleanly after the frame".to_string(),
        Some(Ok(_)) => "a second batch followed".to_string(),
        Some(Err(e)) => format!("after the frame: {e}"),
    };
    Ok(Outcome { batch, borrowed: None, detail })
}

// ---------------------------------------------------------------- output

fn js(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn report(frame_id: &str, mode: &str, r: Result<Outcome, String>) {
    let head = format!(
        "{{\"reader\":\"rust\",\"lib\":{},\"mode\":{},\"frame\":{}",
        js(LIB),
        js(mode),
        js(frame_id)
    );
    match r {
        Err(e) => println!("{head},\"ok\":false,\"error\":{}}}", js(&e)),
        Ok(o) => {
            let schema = o
                .batch
                .schema()
                .fields()
                .iter()
                .map(|f| format!("{}: {}{}", f.name(), f.data_type(), if f.is_nullable() { "" } else { " not null" }))
                .collect::<Vec<_>>()
                .join("; ");
            let mut vf = "ok".to_string();
            for (i, c) in o.batch.columns().iter().enumerate() {
                if let Err(e) = c.to_data().validate_full() {
                    vf = format!("column {i}: {e}");
                    break;
                }
            }
            let mut cols = Vec::new();
            for (f, c) in o.batch.schema().fields().iter().zip(o.batch.columns()) {
                match column_summary(c) {
                    Ok((nulls, fnv)) => cols.push(format!(
                        "{{\"name\":{},\"nulls\":{},\"fnv\":{}}}",
                        js(f.name()),
                        nulls,
                        js(&fnv)
                    )),
                    Err(e) => {
                        println!("{head},\"ok\":false,\"error\":{}}}", js(&e));
                        return;
                    }
                }
            }
            let borrowed = match o.borrowed {
                Some(b) => format!(",\"borrowed\":{b}"),
                None => String::new(),
            };
            println!(
                "{head},\"ok\":true,\"rows\":{},\"schema\":{},\"validate_full\":{},\"cols\":[{}]{},\"detail\":{}}}",
                o.batch.num_rows(),
                js(&schema),
                js(&vf),
                cols.join(","),
                borrowed,
                js(&o.detail)
            );
        }
    }
}

fn guarded(f: impl FnOnce() -> Result<Outcome, String>) -> Result<Outcome, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            Err(format!("PANIC: {msg}"))
        }
    }
}

fn read_all(dir: &Path) {
    let manifest = fs::read_to_string(dir.join("manifest.tsv")).expect("manifest.tsv");
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let (frame_id, schema_file, frame_file) = (f[0], f[5], f[6]);
        let schema_msg = fs::read(dir.join(schema_file)).expect("schema file");
        let frame = fs::read(dir.join(frame_file)).expect("frame file");
        report(frame_id, "read_record_batch", guarded(|| mode_read_record_batch(&schema_msg, &frame, 0)));
        report(frame_id, "StreamDecoder", guarded(|| mode_stream_decoder(&schema_msg, &frame, 0)));
        report(frame_id, "StreamReader", guarded(|| mode_stream_reader(&schema_msg, &frame)));
    }
}

// ---------------------------------------------------------------- alignment probe

/// What does arrow-rs do when the region is NOT aligned the way a slot is? Reads the mid-row-count
/// template frames from regions shifted by 0, 8, 4 and 1 bytes.
fn align_probe(dir: &Path) {
    let manifest = fs::read_to_string(dir.join("manifest.tsv")).expect("manifest.tsv");
    println!("{LIB}: template frames read from a region that starts `shift` bytes past a page boundary");
    for line in manifest.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f[1] != "matrix" || !(f[0].contains(".r17.") || f[0].contains(".r37.")) {
            continue;
        }
        let schema_msg = fs::read(dir.join(f[5])).expect("schema file");
        let frame = fs::read(dir.join(f[6])).expect("frame file");
        let summary = |o: &Outcome| -> String {
            o.batch.columns().iter().map(|c| column_summary(c).map(|x| x.1).unwrap_or_default()).collect::<Vec<_>>().join(",")
        };
        let reference = guarded(|| mode_read_record_batch(&schema_msg, &frame, 0)).map(|o| summary(&o)).unwrap_or_default();
        for shift in [0usize, 8, 4, 1] {
            let show = |r: Result<Outcome, String>| match r {
                Ok(o) => format!(
                    "accepted, values {}, zero-copy={}",
                    if summary(&o) == reference { "correct" } else { "WRONG" },
                    o.borrowed.unwrap_or(false)
                ),
                Err(e) => format!("REJECTED: {e}"),
            };
            println!("  {:22} shift={shift}", f[0]);
            println!("      read_record_batch (default, require_alignment=false): {}", show(guarded(|| mode_read_record_batch(&schema_msg, &frame, shift))));
            println!("      StreamDecoder with_require_alignment(true):           {}", show(guarded(|| mode_stream_decoder(&schema_msg, &frame, shift))));
        }
    }
}

// ---------------------------------------------------------------- render (writer probe)

fn prim_fields(nullable: bool) -> Vec<Field> {
    vec![
        Field::new("id", DataType::Int64, nullable),
        Field::new("amount", DataType::Float64, nullable),
        Field::new("count", DataType::Int32, nullable),
        Field::new("flag", DataType::UInt8, nullable),
    ]
}

fn variants() -> Vec<(&'static str, RecordBatch)> {
    let mut out = Vec::new();

    // launch: features: FixedSizeList<float32>[256] not null, 32 rows
    let item = Arc::new(Field::new("item", DataType::Float32, false));
    let values = Float32Array::from_iter_values((0..32 * 256).map(|i| i as f32));
    let fsl = FixedSizeListArray::try_new(item.clone(), 256, Arc::new(values), None).unwrap();
    let schema = Arc::new(Schema::new(vec![Field::new(
        "features",
        DataType::FixedSizeList(item, 256),
        false,
    )]));
    out.push(("launch", RecordBatch::try_new(schema, vec![Arc::new(fsl) as ArrayRef]).unwrap()));

    // prims: four non-nullable columns, 100 rows
    let n = 100usize;
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values((0..n).map(|i| i as i64))),
        Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
        Arc::new(Int32Array::from_iter_values((0..n).map(|i| i as i32))),
        Arc::new(UInt8Array::from_iter_values((0..n).map(|i| i as u8))),
    ];
    out.push(("prims", RecordBatch::try_new(Arc::new(Schema::new(prim_fields(false))), cols).unwrap()));

    // nullable: the same columns, nullable, each with some nulls
    let cols: Vec<ArrayRef> = vec![
        Arc::new((0..n).map(|i| (i % 3 != 0).then_some(i as i64)).collect::<Int64Array>()),
        Arc::new((0..n).map(|i| (i % 4 != 1).then_some(i as f64)).collect::<Float64Array>()),
        Arc::new((0..n).map(|i| (i % 5 != 2).then_some(i as i32)).collect::<Int32Array>()),
        Arc::new((0..n).map(|i| (i % 2 != 0).then_some(i as u8)).collect::<UInt8Array>()),
    ];
    out.push(("nullable", RecordBatch::try_new(Arc::new(Schema::new(prim_fields(true))), cols).unwrap()));

    // probe only: nullable columns that happen to hold no nulls when rendered
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values((0..n).map(|i| i as i64))),
        Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
        Arc::new(Int32Array::from_iter_values((0..n).map(|i| i as i32))),
        Arc::new(UInt8Array::from_iter_values((0..n).map(|i| i as u8))),
    ];
    out.push(("nullable-rendered-without-nulls", RecordBatch::try_new(Arc::new(Schema::new(prim_fields(true))), cols).unwrap()));
    out
}

fn render(out: &Path) {
    fs::create_dir_all(out).unwrap();
    for (name, batch) in variants() {
        for align in [8usize, 64] {
            let opts = IpcWriteOptions::try_new(align, false, MetadataVersion::V5).unwrap();
            let mut w = StreamWriter::try_new_with_options(Vec::new(), &batch.schema(), opts).unwrap();
            w.write(&batch).unwrap();
            w.finish().unwrap();
            let bytes = w.into_inner().unwrap();
            let path = out.join(format!("{name}.maxrows.align{align}.stream"));
            fs::write(&path, &bytes).unwrap();
            println!("{LIB}: wrote {} ({} bytes, {} rows)", path.display(), bytes.len(), batch.num_rows());
        }
    }
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = env::args().collect();
    match (args.get(1).map(String::as_str), args.get(2)) {
        (Some("read"), Some(d)) => read_all(Path::new(d)),
        (Some("render"), Some(d)) => render(Path::new(d)),
        (Some("align-probe"), Some(d)) => align_probe(Path::new(d)),
        _ => {
            eprintln!("usage: s4-rust read FRAMES_DIR | s4-rust render OUT_DIR | s4-rust align-probe FRAMES_DIR");
            std::process::exit(2);
        }
    }
}
