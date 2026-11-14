#![no_main]
//! cargo +nightly fuzz run handshake (testing §2.5); the target itself is scz_shm::fuzz::handshake.
libfuzzer_sys::fuzz_target!(|data: &[u8]| scz_shm::fuzz::handshake(data));
