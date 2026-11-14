#![no_main]
//! cargo +nightly fuzz run admin (testing §2.5); the target itself is scz_shm::fuzz::admin_request.
libfuzzer_sys::fuzz_target!(|data: &[u8]| scz_shm::fuzz::admin_request(data));
