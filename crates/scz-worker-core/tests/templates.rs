//! Templates rendered by the worker, written through the fast path, read by a stock Arrow
//! reader (arrow-data-plane §11): at one row, at the most rows, and in between, with the row
//! count changing between writes to the same region. And the verifier against corrupted
//! copies of a real registry: it must refuse or accept, never panic, and whatever it accepts
//! must keep every write inside the region.

use arrow_array::{Array, FixedSizeListArray, Float32Array, Int64Array};
use arrow_buffer::Buffer;
use arrow_ipc::reader::read_record_batch;
use arrow_ipc::{convert::try_fb_to_schema, root_as_message, MetadataVersion};
use scz_shm::abi::{direction, elem_type as et};
use scz_shm::layout::LocalSegment;
use scz_shm::registry::{self, Column, Schema, Template};
use scz_worker_core::render::{render, Signature};
use std::collections::HashMap;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

fn fraud() -> Signature {
    Signature { schema: Schema { request: vec![Column::new("features", et::F32, 256)], response: vec![Column::new("score", et::F32, 1)] }, fixed_batch: None }
}

fn mixed() -> Signature {
    Signature {
        schema: Schema {
            request: vec![Column::new("x", et::F32, 16), Column::new("id", et::I64, 1), Column::new("tag", et::FIXED_BINARY, 3)],
            response: vec![Column::new("p", et::F32, 4), Column::new("flag", et::U8, 1)],
        },
        fixed_batch: None,
    }
}

const CAPS: [(u32, u32); 2] = [(2048, 2048), (65536, 4096)];

/// Read the frame at `seg[..frame_cap]` with arrow-ipc, given the Schema message.
fn read(schema_msg: &[u8], seg: &LocalSegment, t: &Template) -> arrow_array::RecordBatch {
    let (mo, ml) = scz_shm::arrowfb::framing(schema_msg).unwrap();
    let m = root_as_message(&schema_msg[mo..mo + ml]).unwrap();
    let schema = Arc::new(try_fb_to_schema(m.header_as_schema().unwrap()).unwrap());
    let mut frame = vec![0u8; t.frame_cap as usize];
    seg.seg().copy_out(0, &mut frame);
    let (mo, ml) = scz_shm::arrowfb::framing(&frame).unwrap();
    let m = root_as_message(&frame[mo..mo + ml]).unwrap();
    let body = Buffer::from(frame[t.body_off as usize..].to_vec());
    read_record_batch(&body, m.header_as_record_batch().unwrap(), schema, &HashMap::new(), None, &MetadataVersion::V5).unwrap()
}

#[test]
fn every_template_reads_back_at_every_row_count() {
    for sig in [fraud(), mixed()] {
        let reg = render(&sig, &CAPS).unwrap();
        assert_eq!(registry::verify(&reg.to_bytes(), &CAPS).unwrap().as_ref(), Some(&reg));
        for t in &reg.templates {
            let schema_msg = if t.direction as u32 == direction::REQUEST { &reg.in_schema } else { &reg.out_schema };
            let seg = LocalSegment::new(65536);
            let stamp = AtomicU32::new(0);
            let mut counts = vec![1, t.max_rows, t.max_rows.div_ceil(2), 1, t.max_rows];
            counts.dedup();
            for (k, &rows) in counts.iter().enumerate() {
                registry::prepare(&seg.seg(), 0, t, &stamp, t.stamp(7), rows);
                // Distinct values per row and column, written at the template's offsets.
                for (ci, c) in t.columns.iter().enumerate() {
                    let bytes: Vec<u8> = (0..rows as usize * c.row_bytes as usize).map(|b| (b * 7 + ci * 13 + k) as u8).collect();
                    seg.seg().copy_in(c.values_off as usize, &bytes);
                }
                let b = read(schema_msg, &seg, t);
                assert_eq!(b.num_rows(), rows as usize, "template {} at {rows} rows", t.id);
                for (ci, c) in t.columns.iter().enumerate() {
                    let want: Vec<u8> = (0..rows as usize * c.row_bytes as usize).map(|b| (b * 7 + ci * 13 + k) as u8).collect();
                    let col = b.column(ci);
                    let data = match col.as_any().downcast_ref::<FixedSizeListArray>() {
                        Some(l) => l.values().to_data(),
                        None => col.to_data(),
                    };
                    let got = &data.buffers()[0].as_slice()[..want.len()];
                    assert_eq!(got, &want[..], "template {} column {:?} at {rows} rows", t.id, c.column.name);
                }
            }
        }
        // Spot checks of the decoded types.
        let t = reg.template(1, direction::REQUEST).unwrap();
        let seg = LocalSegment::new(65536);
        registry::prepare(&seg.seg(), 0, t, &AtomicU32::new(0), t.stamp(1), 3);
        let b = read(&reg.in_schema, &seg, t);
        assert!(b.column(0).as_any().downcast_ref::<FixedSizeListArray>().is_some());
        if b.num_columns() == 3 {
            assert!(b.column(1).as_any().downcast_ref::<Int64Array>().is_some());
        } else {
            assert!(b.column(0).as_any().downcast_ref::<FixedSizeListArray>().unwrap().values().as_any().downcast_ref::<Float32Array>().is_some());
        }
    }
}

#[test]
fn class_capacities_follow_the_region_sizes() {
    let reg = render(&fraud(), &CAPS).unwrap();
    let small = reg.template(0, direction::REQUEST).unwrap();
    let batch = reg.template(1, direction::REQUEST).unwrap();
    assert_eq!(small.max_rows, 1, "a 2 KB region holds one row of 256 floats");
    assert!(batch.max_rows >= 60, "a 64 KB region holds {} rows", batch.max_rows);
    assert_eq!(reg.template(1, direction::RESPONSE).unwrap().max_rows, batch.max_rows);
    assert_eq!(reg.class_for(1), Some(0));
    assert_eq!(reg.class_for(2), Some(1));
    assert_eq!(reg.class_for(batch.max_rows + 1), None);
    // A fixed batch of one caps every class at one row.
    let one = render(&Signature { fixed_batch: Some(1), ..fraud() }, &CAPS).unwrap();
    assert!(one.templates.iter().all(|t| t.max_rows == 1));
    // A schema the fast path cannot carry is refused.
    let mut nullable = fraud();
    nullable.schema.request[0].nullable = true;
    assert!(render(&nullable, &CAPS).is_err());
    assert!(render(&fraud(), &[(256, 256)]).is_err(), "no class holds a row");
}

#[test]
fn the_verifier_survives_corrupted_registries() {
    let good = render(&mixed(), &CAPS).unwrap().to_bytes();
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let (mut accepted, mut refused) = (0, 0);
    for _ in 0..20_000 {
        let mut b = good.clone();
        for _ in 0..1 + next() % 4 {
            let at = (next() as usize) % b.len();
            b[at] = match next() % 4 {
                0 => 0,
                1 => 0xFF,
                2 => b[at].wrapping_add(1),
                _ => next() as u8,
            };
        }
        if next() % 16 == 0 {
            b.truncate((next() as usize) % b.len());
        }
        let _ = registry::parse(&b, &CAPS);
        match registry::verify(&b, &CAPS) {
            Ok(Some(reg)) => {
                accepted += 1;
                // Whatever is accepted must keep the fast path inside each region.
                for t in &reg.templates {
                    let cap = if t.direction as u32 == direction::REQUEST { CAPS[t.class as usize].0 } else { CAPS[t.class as usize].1 };
                    let seg = LocalSegment::new(cap.next_power_of_two().max(4096) as usize);
                    registry::prepare(&seg.seg(), 0, t, &AtomicU32::new(0), 1, t.max_rows);
                    for c in &t.columns {
                        assert!(c.values_off as u64 + t.max_rows as u64 * c.row_bytes as u64 <= cap as u64);
                    }
                }
            }
            Ok(None) => assert!(b.is_empty(), "only empty bytes are an empty registry"),
            Err(_) => refused += 1,
        }
    }
    assert!(refused > accepted, "{accepted} accepted, {refused} refused");
}
