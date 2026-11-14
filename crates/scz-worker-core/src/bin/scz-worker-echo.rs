//! scz-worker-echo: answers every request with its own bytes. The phase 1 worker, used to
//! measure the transport with no model in the way.
//!
//! `SCZ_ECHO_DELAY_US=N` makes every request take at least N microseconds, so tests can
//! drive deadlines.

use scz_shm::abi::status;
use scz_shm::view::{SlotReader, SlotWriter};
use scz_worker_core::Backend;

struct Echo {
    delay: Option<std::time::Duration>,
}

impl Backend for Echo {
    fn infer(&self, req: &SlotReader<'_>, resp: &mut SlotWriter<'_>) -> u32 {
        if let Some(d) = self.delay {
            std::thread::sleep(d);
        }
        match resp.copy_from(req) {
            Ok(()) => status::OK,
            Err(_) => status::RESOURCE_EXHAUSTED,
        }
    }
}

fn main() {
    let delay = std::env::var("SCZ_ECHO_DELAY_US").ok().and_then(|v| v.parse().ok()).map(std::time::Duration::from_micros);
    scz_worker_core::run(Echo { delay })
}
