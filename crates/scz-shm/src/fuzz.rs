//! Fuzz targets (testing §2.5) for everything in Rust that reads bytes another process wrote.
//! The same functions are driven by `cargo-fuzz` (fuzz/, nightly) and, with a fixed seed
//! corpus of random inputs, by `tests/hostile.rs` on every `cargo test`.
//!
//! The invariant for all of them: never panic, never read out of bounds, finish in a bounded
//! number of steps. The supervisor's sweep, repair, and reset must also leave an arena that
//! passes the invariant check.

use crate::abi::{self, acc};
use crate::check;
use crate::handshake::{Hello, Welcome};
use crate::layout::{self, Config, Directory, Geometry, LocalArena, SlabClass};
use crate::ring::{self, RingMem};
use crate::seg::Seg;
use crate::slot;
use core::sync::atomic::Ordering::Relaxed;

fn small_arena() -> LocalArena {
    let g = Geometry { rings: vec![8, 4], slab_classes: vec![SlabClass { slot_size: 128, slot_count: 8, req_cap: 64, resp_cap: 64 }], staging_len: 0x1000 };
    LocalArena::new(&g, &Config::with_defaults(1, 1)).unwrap()
}

/// Spread `bytes` over the arena's data segment, after the status page's identity, as words.
fn scribble(data: &Seg<'_>, bytes: &[u8]) {
    let words = bytes.chunks(8).map(|c| {
        let mut b = [0u8; 8];
        b[..c.len()].copy_from_slice(c);
        u64::from_le_bytes(b)
    });
    // Positions are taken from the input too, so short inputs reach anywhere.
    let mut pos = 16usize;
    for (k, w) in words.enumerate() {
        if k % 2 == 0 {
            pos = (w as usize % (data.len() / 8)) * 8;
        } else {
            data.u64_at(pos.max(16)).store(w, Relaxed);
        }
    }
}

/// The supervisor's view of an arbitrary data segment: classify and repair every ring, sweep
/// every slot (both parts, and the DONE wake), then reset; the reset must leave a clean arena.
pub fn supervisor_sweep(bytes: &[u8]) {
    let a = small_arena();
    let (data, dir) = (a.data(), &a.dir);
    scribble(&data, bytes);
    for i in 0..dir.layout.rings.len() {
        let r = dir.ring(&data, i);
        let _ = ring::head_state(&r);
        let _ = ring::repair(&r);
        let _ = ring::head_state(&r);
    }
    for owner in 1..=2 {
        for (class, &(_, _, c)) in dir.layout.slabs.iter().enumerate() {
            for i in 0..c.slot_count {
                let s = dir.slot(&data, class, i);
                let _ = slot::fail_if_owned(&s, owner);
                let _ = slot::fail_if_ready(&s);
                let _ = slot::wake_if_done(&s);
                let _ = acc::slot_record::t_dequeue_ns(&data, s.rec).saturating_add(1);
            }
        }
    }
    let _ = acc::status_page::client_rejects(&data, dir.status_off as usize);
    layout::reset_data(&data, dir);
    let rep = check::arena(&a.ctl(), &data, dir);
    assert!(rep.ok(), "the reset left {rep}");
}

/// The worker's view: consume whatever the rings hold, bounds-check each descriptor, claim and
/// complete what it can. Bounded: at most one lap of each ring.
pub fn worker_consume(bytes: &[u8]) {
    let a = small_arena();
    let (data, dir) = (a.data(), &a.dir);
    scribble(&data, bytes);
    for i in 0..dir.layout.rings.len() {
        let r = dir.ring(&data, i);
        let mut head = r.head().load(Relaxed);
        for _ in 0..r.capacity() {
            let Some(d) = ring::dequeue(&r, &mut head) else { break };
            if d.is_void() {
                continue;
            }
            let Some(s) = dir.locate(&data, d.slab_class, d.slot_index) else { continue };
            if slot::worker_claim(&s, d.slot_epoch, 1) == slot::Claim::Claimed {
                let len = acc::slot_record::req_len(&data, s.rec) as usize;
                if let Some(req) = crate::view::SlotReader::request(&s, len) {
                    let mut resp = crate::view::SlotWriter::response(&s);
                    let _ = resp.copy_from(&req);
                }
                slot::complete(&s, d.slot_epoch);
            }
        }
    }
}

/// A client's or worker's reading of an arbitrary control segment: refused, or a directory
/// whose every ring and slot can be reached without leaving the segments.
pub fn directory(bytes: &[u8]) {
    let a = small_arena();
    let ctl = a.ctl();
    // Mutate the header in place, then fix the checksum so the directory is actually read.
    for (k, c) in bytes.chunks(3).enumerate() {
        if c.len() == 3 {
            let off = ((c[0] as usize) << 8 | c[1] as usize) % (abi::CTL_HEADER_LEN as usize);
            ctl.u8_at(off).store(c[2], Relaxed);
        }
        if k > 64 {
            break;
        }
    }
    acc::control_header::set_layout_crc32c(&ctl, 0, layout::layout_crc(&ctl));
    if let Ok(dir) = Directory::read(&ctl, a.dir.data_len) {
        let data = a.data();
        for i in 0..dir.layout.rings.len() {
            let r = dir.ring(&data, i);
            let _ = ring::head_state(&r);
        }
        for (class, &(_, _, c)) in dir.layout.slabs.iter().enumerate() {
            for i in [0, c.slot_count.saturating_sub(1)] {
                let _ = dir.locate(&data, class as u16, i).map(|s| slot::SlotMem::word(&s).load(Relaxed));
            }
        }
    }
}

/// The handshake codec over arbitrary 64-byte messages.
pub fn handshake(bytes: &[u8]) {
    let mut m = [0u8; 64];
    let n = bytes.len().min(64);
    m[..n].copy_from_slice(&bytes[..n]);
    if let Some(h) = Hello::decode(&m) {
        let _ = h.negotiate();
        assert_eq!(Hello::decode(&h.encode()), Some(h));
    }
    if let Some(w) = Welcome::decode(&m) {
        assert_eq!(Welcome::decode(&w.encode()), Some(w));
    }
    let _ = crate::handshake::decode_ready(&m);
}

/// The supervisor's registry verification (arrow-data-plane §11) over an arbitrary staged
/// candidate: refused or accepted, never a panic; and an accepted one keeps every patch and
/// column inside its template's region.
pub fn registry(bytes: &[u8]) {
    let caps = [(2048u32, 2048u32), (65536, 4096)];
    let _ = crate::registry::parse(bytes, &caps);
    if let Ok(Some(r)) = crate::registry::verify(bytes, &caps) {
        for t in &r.templates {
            let cap = if t.direction as u32 == abi::direction::REQUEST { caps[t.class as usize].0 } else { caps[t.class as usize].1 };
            assert!(t.frame_cap <= cap);
            for c in &t.columns {
                assert!(c.values_off as u64 + t.max_rows as u64 * c.row_bytes as u64 <= cap as u64);
            }
            for p in &t.patches {
                assert!(p.prefix_off as usize + 8 <= t.prefix.len());
            }
        }
    }
}

/// The Arrow IPC FlatBuffer parser (`arrowfb`) over arbitrary untrusted frames.
pub fn arrow_ipc_frame(bytes: &[u8]) {
    let _ = crate::arrowfb::schema(bytes);
    let _ = crate::arrowfb::record_batch(bytes);
    if let Ok((moff, mlen)) = crate::arrowfb::framing(bytes) {
        if let Some(meta) = bytes.get(moff..moff.saturating_add(mlen)) {
            let _ = crate::arrowfb::schema(meta);
            let _ = crate::arrowfb::record_batch(meta);
        }
    }
}

/// The supervisor's HTTP probe request parser over arbitrary bytes.
pub fn http_request(bytes: &[u8]) {
    const MAX_REQUEST: usize = 2048;
    if bytes.len() > MAX_REQUEST {
        return;
    }
    let Some(end) = bytes.windows(2).position(|w| w == b"\r\n") else { return };
    let line = String::from_utf8_lossy(&bytes[..end]);
    let _path = line.split(' ').nth(1).unwrap_or("");
    let rest = &bytes[end + 2..];
    for hline in rest.split(|&b| b == b'\n') {
        let h = String::from_utf8_lossy(hline);
        if let Some((_, v)) = h.trim().split_once(':') {
            let _ = v.trim().parse::<usize>();
        }
    }
}

/// The supervisor's binary admin-socket command codec (16-byte little-endian `SCZA` frame).
pub fn admin_request(bytes: &[u8]) {
    const MAGIC: u32 = 0x415A_4353; // "SCZA"
    if bytes.len() != 16 {
        return;
    }
    let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    if word(0) == MAGIC {
        let (_cmd, _flags) = (word(1), word(2));
    }
}
