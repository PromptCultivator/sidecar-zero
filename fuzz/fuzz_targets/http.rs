#![no_main]
//! cargo +nightly fuzz run http (testing §2.5); the target itself is scz_shm::fuzz::http_request.
libfuzzer_sys::fuzz_target!(|data: &[u8]| scz_shm::fuzz::http_request(data));
