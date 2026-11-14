#![no_main]
//! cargo +nightly fuzz run worker_consume (testing §2.5); the target itself is scz_shm::fuzz::worker_consume.
libfuzzer_sys::fuzz_target!(|data: &[u8]| scz_shm::fuzz::worker_consume(data));
