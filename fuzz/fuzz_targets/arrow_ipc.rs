#![no_main]
//! cargo +nightly fuzz run arrow_ipc (testing §2.5); the target itself is scz_shm::fuzz::arrow_ipc_frame.
libfuzzer_sys::fuzz_target!(|data: &[u8]| scz_shm::fuzz::arrow_ipc_frame(data));
