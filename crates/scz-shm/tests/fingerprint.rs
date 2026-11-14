//! The schema fingerprint must reproduce abi/golden/fingerprints.txt, which
//! abi/golden/generate.py computes with Python's xxhash from the definition alone.
#![cfg(not(loom))]

use scz_shm::abi::elem_type as et;
use scz_shm::registry::{fingerprint, Column, Schema};

fn columns(text: &str) -> Vec<Column> {
    if text == "-" {
        return vec![];
    }
    text.split(',')
        .map(|c| {
            let (name, rest) = c.split_once(':').unwrap();
            let (ty, size) = rest.trim_end_matches(']').split_once('[').unwrap();
            let nullable = ty.ends_with('?');
            let elem = match ty.trim_end_matches('?') {
                "I8" => et::I8,
                "I16" => et::I16,
                "I32" => et::I32,
                "I64" => et::I64,
                "U8" => et::U8,
                "U16" => et::U16,
                "U32" => et::U32,
                "U64" => et::U64,
                "F16" => et::F16,
                "F32" => et::F32,
                "F64" => et::F64,
                "FIXED_BINARY" => et::FIXED_BINARY,
                t => panic!("unknown type {t}"),
            };
            Column { name: name.to_string(), elem: elem as u8, list_size: size.parse().unwrap(), nullable }
        })
        .collect()
}

#[test]
fn fingerprints_match_the_golden_vectors() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../abi/golden/fingerprints.txt")).unwrap();
    let mut n = 0;
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let mut w = line.split(' ');
        let (want, req, resp) = (w.next().unwrap(), w.next().unwrap(), w.next().unwrap());
        let got = fingerprint(&Schema { request: columns(req), response: columns(resp) });
        assert_eq!(format!("{got:#018x}"), want, "{req} → {resp}");
        n += 1;
    }
    assert!(n >= 8);
}
