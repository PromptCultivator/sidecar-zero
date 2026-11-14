//! sczctl: talk to the supervisor's admin socket (supervisor §7).
//!
//!   sczctl --admin=PATH status     the gate, the workers, the counters, ring depths, and slot
//!                                  counts, decoded from the fixed-size status struct, as JSON
//!   sczctl --admin=PATH reload     start a model reload (a handover to a fresh worker)
//!   sczctl --admin=PATH reset      ask for a session reset
//!   sczctl --admin=PATH dump       receive read-only descriptors for both segments and
//!                                  print the arena's invariant check

use scz_shm::abi::{gate_reason as gr, gate_state as gs, slot_state as ss};
use scz_shm::handshake::{self, Endpoint};
use scz_shm::word;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::Duration;

const MAGIC: u32 = 0x415A_4353;
const STATUS_LEN: usize = 1024;
const HOLDS: [&str; 6] = ["init", "death", "reload", "reset", "pressure", "shutdown"];

fn connect(admin: &str) -> OwnedFd {
    handshake::connect(&Endpoint::Path(admin.into()), Duration::from_secs(2)).unwrap_or_else(|e| {
        eprintln!("sczctl: {e}");
        std::process::exit(1)
    })
}

fn request(fd: &OwnedFd, code: u32, flags: u32) {
    let mut req = [0u8; 16];
    req[..4].copy_from_slice(&MAGIC.to_le_bytes());
    req[4..8].copy_from_slice(&code.to_le_bytes());
    req[8..12].copy_from_slice(&flags.to_le_bytes());
    // SAFETY: a valid buffer of its own length on a connected socket.
    unsafe { libc::send(fd.as_raw_fd(), req.as_ptr() as *const libc::c_void, req.len(), libc::MSG_NOSIGNAL) };
}

fn recv(fd: &OwnedFd, buf: &mut [u8]) -> usize {
    // SAFETY: a valid buffer of its own length.
    let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
    if n <= 0 {
        eprintln!("sczctl: no reply");
        std::process::exit(1);
    }
    n as usize
}

/// The status struct as JSON (layout: crates/scz-supervisor/src/machine.rs, STATUS_LEN).
fn decode(b: &[u8]) -> String {
    let u32_ = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_ = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let g = u64_(8);
    let workers: Vec<String> = (0..u32_(20).min(2) as usize)
        .map(|k| {
            let o = 24 + k * 16;
            let role = ["?", "Loading", "Warm", "Consumer", "Released"][u32_(o + 8).min(4) as usize];
            let f = u32_(o + 12);
            format!(
                "{{\"gen\":{},\"beat\":{},\"role\":\"{role}\",\"active\":{},\"dying\":{},\"condemned\":{},\"retired\":{}}}",
                u32_(o),
                u32_(o + 4),
                f & 1 != 0,
                f & 2 != 0,
                f & 4 != 0,
                f & 8 != 0
            )
        })
        .collect();
    let counters = ["spawns", "deaths", "promotions", "restarts", "resets", "reloads", "condemned", "recycles", "retired_standbys", "refusal_condemns"];
    let times = ["last_recovery_ms", "max_recovery_ms", "last_promotion_ms", "max_promotion_ms"];
    let flags = ["crashloop", "shedding", "standby_suppressed", "does_not_fit", "reset_stuck", "session_bound", "accepting"];
    let (fl, hl) = (u32_(172), u32_(176));
    let holds: Vec<String> = HOLDS.iter().enumerate().filter(|(i, _)| hl & (1 << i) != 0).map(|(_, h)| format!("\"{h}\"")).collect();
    let rings = u32_(180).min(8) as usize;
    let depths: Vec<String> = (0..rings).map(|i| u32_(184 + i * 4).to_string()).collect();
    let slots: Vec<String> = (0..7).map(|i| format!("\"{}\":{}", ss::name(i as u32), u32_(216 + i * 4))).collect();
    let transitions: Vec<String> = (0..8)
        .map(|k| (u64_(248 + k * 16), u64_(256 + k * 16)))
        .filter(|&(_, at)| at != 0)
        .map(|(w, at)| {
            format!(
                "{{\"state\":\"{}\",\"reason\":\"{}\",\"gen\":{},\"at_ns\":{at}}}",
                gs::name(word::gate_state(w)),
                gr::name(word::gate_reason(w)),
                word::gate_gen(w)
            )
        })
        .collect();
    let cause = String::from_utf8_lossy(&b[376..504]).trim_end_matches('\0').replace('"', "'");
    let hex: String = b[536..568].iter().map(|x| format!("{x:02x}")).collect();
    let opt = |v: u32| if v == u32::MAX { "null".to_string() } else { v.to_string() };
    let model = format!(
        ",\"registry_epoch\":{},\"rollbacks\":{},\"rejected\":{},\"target_model\":{},\"schema_fingerprint\":\"{:#018x}\",\"model_sha256\":\"{hex}\",\"previous_model\":{},\"trial_served_ms\":{},\"trial_deaths\":{},\"model\":{}",
        u32_(512),
        u32_(516),
        u32_(520),
        opt(u32_(524)),
        u64_(528),
        opt(u32_(568)),
        u32_(572),
        u32_(576),
        u32_(580)
    );
    let mut out = format!(
        "{{\"state\":\"{}\",\"reason\":\"{}\",\"gen\":{},\"consumer\":{},\"holds\":[{}],\"workers\":[{}]",
        gs::name(word::gate_state(g)),
        gr::name(word::gate_reason(g)),
        word::gate_gen(g),
        u32_(16),
        holds.join(","),
        workers.join(",")
    );
    for (i, c) in counters.iter().enumerate() {
        out += &format!(",\"{c}\":{}", u64_(56 + i * 8));
    }
    for (i, t) in times.iter().enumerate() {
        out += &format!(",\"{t}\":{:.3}", u64_(136 + i * 8) as f64 / 1e6);
    }
    for (i, f) in flags.iter().enumerate() {
        out += &format!(",\"{f}\":{}", fl & (1 << i) != 0);
    }
    out += &format!(
        ",\"util\":{:.3},\"ring_depth\":[{}],\"slots\":{{{}}},\"transitions\":[{}],\"last_cause\":\"{cause}\"{model}}}",
        u32_(168) as f64 / 1000.0,
        depths.join(","),
        slots.join(","),
        transitions.join(",")
    );
    out
}

fn main() {
    // An operator's `sczctl dump | head` closes the pipe early: end quietly, as a shell tool
    // does, instead of panicking on the next write (Rust ignores SIGPIPE by default).
    // SAFETY: restoring a signal's default disposition, before any thread exists.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let mut admin = None;
    let mut cmd = None;
    let mut force = false;
    for a in std::env::args().skip(1) {
        match a.split_once('=') {
            Some(("--admin", p)) => admin = Some(p.to_string()),
            _ if a == "--force" => force = true,
            _ => cmd = Some(a),
        }
    }
    let (Some(admin), Some(cmd)) = (admin, cmd) else {
        eprintln!("usage: sczctl --admin=PATH status|reload [--force]|reset|dump");
        std::process::exit(2);
    };
    let fd = connect(&admin);
    match cmd.as_str() {
        "status" => {
            request(&fd, 1, 0);
            let mut b = vec![0u8; STATUS_LEN];
            let n = recv(&fd, &mut b);
            if n != STATUS_LEN || u32::from_le_bytes(b[..4].try_into().unwrap()) != 0x5353_4353 {
                eprintln!("sczctl: not a status struct ({n} bytes)");
                std::process::exit(1);
            }
            println!("{}", decode(&b));
        }
        "reload" | "reset" => {
            request(&fd, if cmd == "reload" { 2 } else { 3 }, u32::from(force && cmd == "reload"));
            let mut b = [0u8; 64];
            let n = recv(&fd, &mut b);
            println!("{}", String::from_utf8_lossy(&b[..n]));
        }
        "dump" => {
            request(&fd, 4, 0);
            let Ok(Some((_, fds))) = handshake::recv(fd.as_fd()) else {
                eprintln!("sczctl: no dump");
                std::process::exit(1)
            };
            let [ctl, data] = <[OwnedFd; 2]>::try_from(fds).unwrap_or_else(|_| {
                eprintln!("sczctl: expected two descriptors");
                std::process::exit(1)
            });
            match scz_shm::arena::MappedArena::open_readonly(ctl.as_fd(), data.as_fd()) {
                Ok(a) => {
                    // What this arena says it is: the version and the feature bits its supervisor
                    // wrote, which a newer minor may set (shm-abi §12.5).
                    let (ctl, h) = (a.ctl.seg(), &a.dir);
                    use scz_shm::abi::acc::control_header as ch;
                    println!(
                        "{{\"arena\":{{\"abi_major\":{},\"abi_minor\":{},\"data_len\":{},\"feature_flags\":{}}}}}",
                        ch::abi_major(&ctl, 0),
                        ch::abi_minor(&ctl, 0),
                        h.data_len,
                        ch::feature_flags(&ctl, 0)
                    );
                    let report = scz_shm::check::arena(&a.ctl.seg(), &a.data.seg(), &a.dir);
                    println!("{report}");
                    // The workers' counters (shm-abi status page), one line.
                    let data = a.data.seg();
                    let stats: Vec<String> = (0..scz_shm::abi::BEAT_SLOTS as usize)
                        .map(|b| {
                            use scz_shm::abi::acc::worker_stats as w;
                            let base = a.dir.status_off as usize + scz_shm::abi::status_page::worker_stats::OFF + b * scz_shm::abi::status_page::worker_stats::STRIDE;
                            format!(
                                "{{\"beat\":{b},\"served\":{},\"deadline_drops\":{},\"bad_requests\":{},\"stale_requests\":{},\"model_errors\":{},\"copies\":{},\"zero_copy_misses\":{},\"infer_ns\":{}}}",
                                w::served(&data, base),
                                w::deadline_drops(&data, base),
                                w::bad_requests(&data, base),
                                w::stale_requests(&data, base),
                                w::model_errors(&data, base),
                                w::copies(&data, base),
                                w::zero_copy_misses(&data, base),
                                w::infer_ns(&data, base)
                            )
                        })
                        .collect();
                    println!("{{\"worker_stats\":[{}]}}", stats.join(","));
                }
                Err(e) => {
                    eprintln!("sczctl: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("unknown command {cmd}");
            std::process::exit(2);
        }
    }
}
