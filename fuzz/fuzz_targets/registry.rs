#![no_main]
//! cargo +nightly fuzz run registry (testing §2.5); the target itself is scz_shm::fuzz::registry.
libfuzzer_sys::fuzz_target!(|data: &[u8]| scz_shm::fuzz::registry(data));
