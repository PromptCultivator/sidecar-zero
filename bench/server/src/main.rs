//! scz-bench-server: the baselines C and D of bench §3, one native server wrapping ONNX Runtime.
//!
//!   scz-bench-server --model=PATH (--http=ADDR:PORT | --uds=PATH) [--threads=1] [--supervise]
//!
//! `--http`: HTTP/1.1 with keep-alive; `POST /v1/score` with `{"features":[f0, …, f255]}`
//! answers `{"score":s}` (system C). `--uds`: a Unix stream socket; a request is a little-endian
//! u32 byte count (1024) and 256 little-endian float32, an answer the count (4) and one float32,
//! or a count of 0 for a failed one (system D).
//!
//! Both run the model with the settings SidecarZero's worker uses, and with at most `--threads`
//! inferences at once, the worker's consume-thread count (fairness rule 3): each connection has
//! a thread, and takes one of the inference permits for each request. `--supervise` runs the
//! server under a trivial restart loop, so that killing it does not end its container (fairness
//! rule 5); the kill agent then kills the child, which this process starts again at once.

use scz_bench_common::{Row, Scorer, FEATURES};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering::*};
use std::sync::Arc;

extern "C" fn on_term(_: libc::c_int) {
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(0) };
}

/// At most N inferences at once. Taken by spinning, then yielding: an inference is about
/// 10 µs, and a sleeping handoff (a mutex and condition variable) costs a thread wake-up per
/// request, which capped the first version of this server below 50,000 a second.
struct Permits {
    free: AtomicUsize,
}

impl Permits {
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let mut spins = 0u32;
        loop {
            let n = self.free.load(Relaxed);
            if n > 0 && self.free.compare_exchange_weak(n, n - 1, Acquire, Relaxed).is_ok() {
                break;
            }
            spins += 1;
            if spins < 200 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
        let out = f();
        self.free.fetch_add(1, Release);
        out
    }
}

fn parse_features(body: &[u8]) -> Option<Row> {
    let s = std::str::from_utf8(body).ok()?;
    let list = s.split_once('[')?.1.split_once(']')?.0;
    let mut row = [0f32; FEATURES];
    let mut n = 0;
    for v in list.split(',') {
        if n == FEATURES {
            return None;
        }
        row[n] = v.trim().parse().ok()?;
        n += 1;
    }
    (n == FEATURES).then_some(row)
}

fn http_conn(s: std::net::TcpStream, scorer: &Scorer, permits: &Permits) {
    let _ = s.set_nodelay(true);
    let mut w = s.try_clone().expect("a second handle on the socket");
    let mut r = BufReader::new(s);
    let mut line = String::new();
    let mut body = Vec::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let post = line.starts_with("POST /v1/score");
        let mut len = 0usize;
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        body.resize(len, 0);
        if r.read_exact(&mut body).is_err() {
            return;
        }
        let (code, reply) = match (post, parse_features(&body)) {
            (true, Some(row)) => match permits.run(|| scorer.score(&row)) {
                Ok(s) => (200, format!("{{\"score\":{s}}}")),
                Err(e) => (500, format!("{{\"error\":\"{}\"}}", e.replace('"', "'"))),
            },
            (true, None) => (400, "{\"error\":\"expected {\\\"features\\\":[256 numbers]}\"}".to_string()),
            (false, _) => (404, "{}".to_string()),
        };
        let head = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", reply.len());
        if w.write_all(head.as_bytes()).and_then(|_| w.write_all(reply.as_bytes())).is_err() {
            return;
        }
    }
}

fn uds_conn(mut s: std::os::unix::net::UnixStream, scorer: &Scorer, permits: &Permits) {
    let mut req = [0u8; 4 + FEATURES * 4];
    loop {
        if s.read_exact(&mut req).is_err() {
            return;
        }
        if u32::from_le_bytes(req[..4].try_into().unwrap()) as usize != FEATURES * 4 {
            return;
        }
        let mut row = [0f32; FEATURES];
        for (j, c) in req[4..].as_chunks::<4>().0.iter().enumerate() {
            row[j] = f32::from_le_bytes(*c);
        }
        let mut out = [0u8; 8];
        match permits.run(|| scorer.score(&row)) {
            Ok(v) => {
                out[..4].copy_from_slice(&4u32.to_le_bytes());
                out[4..].copy_from_slice(&v.to_le_bytes());
            }
            Err(_) => out[..4].copy_from_slice(&0u32.to_le_bytes()),
        }
        let n = if out[0] == 0 { 4 } else { 8 };
        if s.write_all(&out[..n]).is_err() {
            return;
        }
    }
}

/// The restart loop of fairness rule 5: start the server, and start it again whenever it ends.
fn supervise() -> ! {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--supervise").collect();
    let exe = std::env::current_exe().expect("this program's path");
    loop {
        let t = std::time::Instant::now();
        match std::process::Command::new(&exe).args(&args).status() {
            Ok(st) => eprintln!("scz-bench-server: the server ended ({st}) after {:.1} s; starting it again", t.elapsed().as_secs_f64()),
            Err(e) => {
                eprintln!("scz-bench-server: {e}");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

fn main() {
    // SAFETY: installs handlers that only call _exit: PID 1 of a container gets no default.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    let (mut model, mut http, mut uds, mut threads, mut sup) = (String::new(), None, None, 1usize, false);
    for a in std::env::args().skip(1) {
        let (k, v) = a.split_once('=').unwrap_or((&a, ""));
        match k {
            "--model" => model = v.to_string(),
            "--http" => http = Some(v.to_string()),
            "--uds" => uds = Some(v.to_string()),
            "--threads" => threads = v.parse().unwrap_or(1).max(1),
            "--supervise" => sup = true,
            _ => {
                eprintln!("scz-bench-server: unknown argument {a}");
                std::process::exit(2);
            }
        }
    }
    if sup {
        supervise();
    }
    let scorer = Arc::new(Scorer::open(&model).unwrap_or_else(|e| {
        eprintln!("scz-bench-server: {e}");
        std::process::exit(1)
    }));
    let permits = Arc::new(Permits { free: AtomicUsize::new(threads) });
    if let Some(addr) = http {
        let l = std::net::TcpListener::bind(&addr).unwrap_or_else(|e| panic!("{addr}: {e}"));
        eprintln!("scz-bench-server: HTTP/JSON on {addr}, {threads} inference thread(s)");
        for s in l.incoming().flatten() {
            let (sc, p) = (scorer.clone(), permits.clone());
            std::thread::spawn(move || http_conn(s, &sc, &p));
        }
    } else if let Some(path) = uds {
        let _ = std::fs::remove_file(&path);
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666));
        eprintln!("scz-bench-server: binary frames on {path}, {threads} inference thread(s)");
        for s in l.incoming().flatten() {
            let (sc, p) = (scorer.clone(), permits.clone());
            std::thread::spawn(move || uds_conn(s, &sc, &p));
        }
    } else {
        eprintln!("usage: scz-bench-server --model=PATH (--http=ADDR:PORT | --uds=PATH) [--threads=N] [--supervise]");
        std::process::exit(2);
    }
}
