//! Hostile input on every `cargo test` (testing §2.5): the fuzz targets over a fixed corpus of
//! seeded random inputs. `cargo-fuzz` (fuzz/) searches for new ones on nightly.
#![cfg(not(loom))]

use scz_shm::fuzz;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = (self.next() as usize) % max;
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn corpus(seed: u64, n: usize, max: usize) -> impl Iterator<Item = Vec<u8>> {
    let mut r = Rng(seed | 1);
    (0..n).map(move |_| {
        let mut b = r.bytes(max);
        // Bias some words towards the values the protocol cares about.
        for w in b.chunks_mut(8) {
            if r.next().is_multiple_of(4) {
                let v = [0u64, 1, 2, 3, 4, 5, 6, u64::MAX, u64::MAX - 1, 8, 9, 0x1_0000_0002, 0x2_0000_0303][(r.next() % 13) as usize];
                let vb = v.to_le_bytes();
                let k = w.len();
                w.copy_from_slice(&vb[..k]);
            }
        }
        b
    })
}

#[test]
fn the_sweep_repair_and_reset_survive_any_data_segment() {
    for b in corpus(1, 3000, 4096) {
        fuzz::supervisor_sweep(&b);
    }
}

#[test]
fn the_worker_survives_any_ring_and_slot_contents() {
    for b in corpus(2, 3000, 4096) {
        fuzz::worker_consume(&b);
    }
}

#[test]
fn the_directory_reader_survives_any_control_header() {
    for b in corpus(3, 3000, 600) {
        fuzz::directory(&b);
    }
}

#[test]
fn the_handshake_codec_survives_any_message() {
    for b in corpus(4, 5000, 80) {
        fuzz::handshake(&b);
    }
}

#[test]
fn the_registry_verifier_survives_any_candidate() {
    // Mostly refused at the header; the worker crate's templates test mutates real ones.
    for mut b in corpus(5, 3000, 2048) {
        if b.len() >= 8 {
            b[..8].copy_from_slice(&scz_shm::abi::REGISTRY_MAGIC.to_le_bytes());
        }
        fuzz::registry(&b);
    }
}

#[test]
fn the_arrow_ipc_parser_survives_any_frame() {
    for mut b in corpus(6, 3000, 1024) {
        if b.len() >= 8 {
            b[..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        fuzz::arrow_ipc_frame(&b);
    }
}

#[test]
fn the_http_and_admin_parsers_survive_any_input() {
    for b in corpus(7, 3000, 2200) {
        fuzz::http_request(&b);
    }
    fuzz::http_request(b"GET /startupz HTTP/1.1\r\nContent-Length: 99999999999999999999\r\n\r\n");
    for mut b in corpus(8, 3000, 32) {
        if b.len() == 16 {
            b[..4].copy_from_slice(&0x415A_4353u32.to_le_bytes());
        }
        fuzz::admin_request(&b);
    }
}
