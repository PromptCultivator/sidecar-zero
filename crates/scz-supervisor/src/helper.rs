//! Helper mode (supervisor §2): short-lived children of this binary, sandboxed as a worker is,
//! for the work that must not run in the event loop.
//!
//!   --helper=hash                          fd 3: a model file. Writes its SHA-256, as hex.
//!   --helper=verify --caps=REQxRESP,…      fd 3: a sealed copy of a staged registry. Writes
//!                                          `OK <fingerprint>` or `ERR <why>`.
//!
//! The answer goes to fd 4, one line. A helper that crashes or never answers counts as a
//! failed load; a malformed candidate can crash only the helper.

use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};

fn answer(line: &str) -> ! {
    // SAFETY: fd 4 is the answer pipe the stub placed there.
    let mut out = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(4) });
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    std::process::exit(0)
}

fn input() -> std::fs::File {
    // SAFETY: fd 3 is the input the stub placed there.
    std::fs::File::from(unsafe { OwnedFd::from_raw_fd(3) })
}

pub fn main(kind: &str, args: &[String]) -> ! {
    match kind {
        "hash" => {
            let mut f = input();
            let mut h = Sha256::new();
            match std::io::copy(&mut f, &mut h) {
                Ok(_) => answer(&scz_supervisor_hex(&h.finalize())),
                Err(e) => answer(&format!("ERR {e}")),
            }
        }
        "verify" => {
            let caps: Vec<(u32, u32)> = args
                .iter()
                .find_map(|a| a.strip_prefix("--caps="))
                .unwrap_or("")
                .split(',')
                .filter_map(|c| {
                    let (q, r) = c.split_once('x')?;
                    Some((q.parse().ok()?, r.parse().ok()?))
                })
                .collect();
            let mut bytes = vec![];
            if let Err(e) = std::io::Read::read_to_end(&mut input(), &mut bytes) {
                answer(&format!("ERR reading the candidate: {e}"));
            }
            match scz_shm::registry::verify(&bytes, &caps) {
                Ok(Some(r)) => answer(&format!("OK {:016x}", r.fingerprint())),
                Ok(None) => answer("OK 0000000000000000"),
                Err(e) => answer(&format!("ERR {}", e.replace('\n', " "))),
            }
        }
        k => answer(&format!("ERR unknown helper {k}")),
    }
}

fn scz_supervisor_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
