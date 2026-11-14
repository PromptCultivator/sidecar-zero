//! The Rust layout code must reproduce abi/golden/arena.txt, which abi/golden/generate.py
//! writes from the ABI file alone (testing §2.3).
#![cfg(not(loom))]

use scz_shm::layout::{Config, Geometry, LocalArena, SlabClass};
use scz_shm::seg::Seg;
use std::sync::atomic::Ordering::Relaxed;

fn words(name: &str, s: &Seg<'_>, out: &mut Vec<String>) {
    for off in (0..s.len()).step_by(8) {
        let w = s.u64_at(off).load(Relaxed);
        if w != 0 {
            out.push(format!("{name} {off:#08x} {w:#018x}"));
        }
    }
}

#[test]
fn the_rust_layout_matches_the_golden_arena() {
    // abi/scz_abi.toml [golden]
    let g = Geometry { rings: vec![8], slab_classes: vec![SlabClass { slot_size: 4096, slot_count: 16, req_cap: 2048, resp_cap: 2048 }], staging_len: 0x10000 };
    let a = LocalArena::new(&g, &Config::with_defaults(0x0123_4567_89AB_CDEF, 1)).unwrap();
    let mut got = vec![format!("segments control {:#x} data {:#x}", a.ctl().len(), a.data().len())];
    words("control", &a.ctl(), &mut got);
    words("data", &a.data(), &mut got);
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../abi/golden/arena.txt");
    let want: Vec<String> = std::fs::read_to_string(path)
        .expect("abi/golden/arena.txt: run abi/golden/generate.py")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(String::from)
        .collect();
    for (i, (w, g)) in want.iter().zip(&got).enumerate() {
        assert_eq!(g, w, "line {i} differs (left: Rust, right: golden)");
    }
    assert_eq!(got.len(), want.len(), "word counts differ:\nRust:   {got:#?}\ngolden: {want:#?}");
}
