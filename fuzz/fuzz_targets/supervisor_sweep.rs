#![no_main]
//! cargo +nightly fuzz run supervisor_sweep (testing §2.5); the target itself is scz_shm::fuzz::supervisor_sweep.
libfuzzer_sys::fuzz_target!(|data: &[u8]| scz_shm::fuzz::supervisor_sweep(data));
