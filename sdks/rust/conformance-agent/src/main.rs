//! scz-agent-rust: the Rust SDK's conformance agent (testing §3, "Agent protocol"), and the
//! reference for every other SDK's agent.
//!
//! It builds one client for `--endpoint=PATH` and obeys one command per line on stdin,
//! answering one line on stdout. Words after the command are `key=value`; unknown keys are
//! ignored, and a key left out keeps its value.
//!
//! | Command | Reply |
//! |---|---|
//! | (on start) | `HELLO sdk=rust version=V caps=C,...` |
//! | `CONFIG threads=N fallback_us=N kind=K fallback=F spin=S codec=C inputs=PATH` | `OK`, or `ERR unsupported` |
//! | `BOUND ms=N` | `BOUND true\|false`: waits for an open gate |
//! | `CALL count=N budget_us=N rows=N op=O arg=N seed=N` | one `R` line per call (below), then `END calls=N` |
//! | `CODEC_FAULT mode=off\|encode\|decode` | `OK` |
//! | `UNWIND` | `OK`: the next call unwinds between claiming a ring cell and publishing it |
//! | `CLOCK_SKEW ns=N` | `ERR unsupported`: this SDK has no cheap clock |
//! | `FORK` | `FORK src=S reason=R status=N`: a forked child's one call, and how the child exited |
//! | `SPAWN` | `SPAWNED pid=N`: a child process that only sleeps |
//! | `STATS` | `STATS model=N sessions=N retired=N late_wakes=N fb.REASON=N ...` |
//! | `PROC` | `PROC fds=N maps=N threads=N` |
//! | `CLOSE` | `CLOSED`; later commands that need the client answer `ERR closed` |
//! | `EXIT` | `BYE`, and the process exits |
//!
//! CONFIG: `threads` calling threads per CALL, each making `count` calls; `fallback_us`, how
//! long the fallback takes; `kind` `platform` (the default) or `per-call` (a new thread for
//! every call); `fallback` `constant` or `throws` (it records the reason it was given and
//! panics); `spin` `on` or `off` (the spin ceiling fixed at zero); `codec` `keyed` (opaque
//! bytes, the default) or `features` (the dense float32 codec over the fraud model's 256
//! features, reading its rows from `inputs`, raw little-endian float32).
//!
//! A call's line is `R t=THREAD s=SEQ` and then one of `src=model gen=G d=DIGEST` (keyed),
//! `src=model gen=G v=BITS,...` (features: each row's score as hex float bits),
//! `src=fallback reason=R`, `src=thrown reason=R` (the fallback panicked), or `src=unwound`
//! (the call an UNWIND armed), followed by `t0=NS t1=NS` from `CLOCK_MONOTONIC`. Keyed requests
//! are built by `scz_chaos_proto::request`, so the runner can recompute each one and check its
//! digest; row k of a features call `s` is input row `(s + k) mod R`.

use scz_chaos_proto as proto;
use scz_shm::wait::monotonic_ns;
use sidecarzero::{Client, Codec, CodecError, Column, Endpoint, Fallback, FallbackReason, FrameReader, FrameWriter, Schema, SlotReader, SlotWriter, Source};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

const ENCODE: u8 = 1;
const DECODE: u8 = 2;

/// Request bytes in, the 8-byte keyed answer out.
struct Keyed {
    fault: Arc<AtomicU8>,
}

impl Codec for Keyed {
    type In = [u8];
    type Out = u64;
    fn request_len(&self, input: &[u8]) -> usize {
        input.len()
    }
    fn encode(&self, input: &[u8], w: &mut SlotWriter<'_>) -> Result<(), CodecError> {
        if self.fault.load(Relaxed) == ENCODE {
            return Err(CodecError::Shape);
        }
        Ok(w.write(input)?)
    }
    fn decode(&self, r: &SlotReader<'_>) -> Result<u64, CodecError> {
        if self.fault.load(Relaxed) == DECODE || r.len() != 8 {
            return Err(CodecError::Shape);
        }
        let mut b = [0u8; 8];
        r.copy_to(0, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

/// The fraud model's codec for any number of rows: `features: float32[rows, 256]` in,
/// `score: float32[rows, 1]` out, written into the registry's frames like the prebuilt
/// `FeatureScore` and `FeatureBatch`, which fix the row count at compile time.
const N: usize = 256;
const MAX_ROWS: usize = 64;

struct Dense {
    fault: Arc<AtomicU8>,
}

impl Codec for Dense {
    type In = [[f32; N]];
    type Out = Scores;
    fn schema(&self) -> Option<Schema> {
        Some(Schema {
            request: vec![Column::new("features", scz_shm::abi::elem_type::F32, N as u32)],
            response: vec![Column::new("score", scz_shm::abi::elem_type::F32, 1)],
        })
    }
    fn rows(&self, input: &Self::In) -> u32 {
        input.len() as u32
    }
    fn encode_frame(&self, input: &Self::In, w: &mut FrameWriter<'_>) -> Result<(), CodecError> {
        if self.fault.load(Relaxed) == ENCODE {
            return Err(CodecError::Shape);
        }
        for (r, row) in input.iter().enumerate() {
            w.write_f32_row(0, r as u32, row)?;
        }
        Ok(())
    }
    fn decode_frame(&self, r: &FrameReader<'_>) -> Result<Scores, CodecError> {
        let rows = r.rows() as usize;
        if self.fault.load(Relaxed) == DECODE || rows > MAX_ROWS {
            return Err(CodecError::Shape);
        }
        let mut s = Scores { n: rows, v: [0.0; MAX_ROWS] };
        r.read_f32(0, &mut s.v[..rows])?;
        Ok(s)
    }
}

#[derive(Clone, Copy)]
struct Scores {
    n: usize,
    v: [f32; MAX_ROWS],
}

/// Takes `fallback_us`, then answers 0; or, with `throws`, records its reason and panics.
#[derive(Clone)]
struct Slow {
    us: Arc<AtomicU64>,
    throws: Arc<AtomicBool>,
}

thread_local! {
    static THROWN: Cell<Option<FallbackReason>> = const { Cell::new(None) };
}

/// The payload of the agent's own panics, which the panic hook keeps quiet about.
struct Thrown;

impl Slow {
    fn wait(&self, reason: FallbackReason) {
        let us = self.us.load(Relaxed);
        if us > 0 {
            std::thread::sleep(Duration::from_micros(us));
        }
        if self.throws.load(Relaxed) {
            THROWN.with(|t| t.set(Some(reason)));
            std::panic::panic_any(Thrown);
        }
    }
}

impl Fallback<[u8], u64> for Slow {
    fn apply(&self, _: &[u8], reason: FallbackReason) -> u64 {
        self.wait(reason);
        0
    }
}

impl Fallback<[[f32; N]], Scores> for Slow {
    fn apply(&self, input: &[[f32; N]], reason: FallbackReason) -> Scores {
        self.wait(reason);
        Scores { n: input.len().min(MAX_ROWS), v: [0.0; MAX_ROWS] }
    }
}

enum AnyClient {
    Keyed(Client<Keyed, Slow>),
    Dense(Client<Dense, Slow>, Arc<Vec<[f32; N]>>),
}

impl AnyClient {
    fn await_bound(&self, d: Duration) -> bool {
        match self {
            AnyClient::Keyed(c) => c.await_bound(d),
            AnyClient::Dense(c, _) => c.await_bound(d),
        }
    }
    fn stats(&self) -> sidecarzero::Stats {
        match self {
            AnyClient::Keyed(c) => c.stats(),
            AnyClient::Dense(c, _) => c.stats(),
        }
    }
    fn set_spin_ceiling(&self, d: Option<Duration>) {
        match self {
            AnyClient::Keyed(c) => c.set_spin_ceiling(d),
            AnyClient::Dense(c, _) => c.set_spin_ceiling(d),
        }
    }
    fn close(self) {
        match self {
            AnyClient::Keyed(c) => c.close(),
            AnyClient::Dense(c, _) => c.close(),
        }
    }
    /// One call, as its protocol line. The fallback's and the test hook's panics are caught here.
    fn call(&self, req: &Req, t: u32, s: u32) -> String {
        let t0 = monotonic_ns();
        let r = catch_unwind(AssertUnwindSafe(|| match self {
            AnyClient::Keyed(c) => {
                let o = c.infer(&proto::request(req.op, req.arg, t, s, req.seed, req.rows), req.budget);
                (o.source, format!("d={:#x}", o.value))
            }
            AnyClient::Dense(c, inputs) => {
                let k = req.rows.max(1);
                let rows: Vec<[f32; N]> = (0..k).map(|i| inputs[(s as usize + i) % inputs.len()]).collect();
                let o = c.infer(&rows[..], req.budget);
                let v: Vec<String> = o.value.v[..o.value.n].iter().map(|x| format!("{:#x}", x.to_bits())).collect();
                (o.source, format!("v={}", v.join(",")))
            }
        }));
        let t1 = monotonic_ns();
        match r {
            Ok((Source::Model { model_gen }, value)) => format!("R t={t} s={s} src=model gen={model_gen} {value} t0={t0} t1={t1}"),
            Ok((Source::Fallback { reason }, _)) => format!("R t={t} s={s} src=fallback reason={} t0={t0} t1={t1}", reason.as_str()),
            Err(p) if p.is::<Thrown>() => {
                let reason = THROWN.with(|x| x.take()).map_or("UNKNOWN", |r| r.as_str());
                format!("R t={t} s={s} src=thrown reason={reason} t0={t0} t1={t1}")
            }
            Err(_) => format!("R t={t} s={s} src=unwound t0={t0} t1={t1}"),
        }
    }
}

#[derive(Clone, Copy)]
struct Req {
    op: u64,
    arg: u64,
    seed: u64,
    rows: usize,
    budget: Duration,
}

fn kv(words: &[&str]) -> HashMap<String, String> {
    words.iter().filter_map(|w| w.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn num(m: &HashMap<String, String>, k: &str, default: u64) -> u64 {
    m.get(k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn proc_counts() -> String {
    let fds = std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0);
    let maps = std::fs::read_to_string("/proc/self/maps").map(|m| m.lines().filter(|l| l.contains("memfd:scz-")).count()).unwrap_or(0);
    let threads = std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0);
    format!("PROC fds={fds} maps={maps} threads={threads}")
}

fn read_inputs(path: &str) -> Result<Vec<[f32; N]>, String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if b.is_empty() || b.len() % (N * 4) != 0 {
        return Err(format!("{path}: not rows of {N} float32"));
    }
    Ok(b.as_chunks::<{ N * 4 }>()
        .0
        .iter()
        .map(|row| {
            let mut r = [0f32; N];
            for (j, c) in row.as_chunks::<4>().0.iter().enumerate() {
                r[j] = f32::from_le_bytes(*c);
            }
            r
        })
        .collect())
}

/// FORK: the child makes one keyed call and reports it through a pipe; the parent waits.
fn fork_once(c: &AnyClient) -> String {
    let mut p = [0i32; 2];
    // SAFETY: a valid two-element array.
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return "ERR pipe".into();
    }
    // SAFETY: the child only calls the client and writes to the pipe, then _exits.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let line = c.call(&Req { op: proto::op::NORMAL, arg: 0, seed: 1, rows: 0, budget: Duration::from_millis(20) }, 0, 0);
        let f: HashMap<String, String> = kv(&line.split_whitespace().collect::<Vec<_>>());
        let msg = format!("src={} reason={}", f.get("src").map_or("?", |s| s), f.get("reason").map_or("-", |s| s));
        // SAFETY: the buffer is valid for its length; _exit skips the parent's destructors.
        unsafe {
            libc::write(p[1], msg.as_ptr() as *const libc::c_void, msg.len());
            libc::_exit(0);
        }
    }
    // SAFETY: closing our copy of the write end.
    unsafe { libc::close(p[1]) };
    let mut buf = [0u8; 256];
    let mut got = 0usize;
    loop {
        // SAFETY: reading into the rest of a valid buffer.
        let n = unsafe { libc::read(p[0], buf[got..].as_mut_ptr() as *mut libc::c_void, buf.len() - got) };
        if n <= 0 {
            break;
        }
        got += n as usize;
    }
    let mut st = 0;
    // SAFETY: waiting for our own child.
    unsafe {
        libc::close(p[0]);
        libc::waitpid(pid, &mut st, 0);
    }
    let status = if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 128 + libc::WTERMSIG(st) };
    format!("FORK {} status={status}", String::from_utf8_lossy(&buf[..got]))
}

struct Config {
    threads: u32,
    per_call: bool,
    spin_off: bool,
    dense: Option<String>,
}

fn build(endpoint: &Endpoint, cfg: &Config, fault: &Arc<AtomicU8>, slow: &Slow) -> Result<AnyClient, String> {
    let c = match &cfg.dense {
        None => AnyClient::Keyed(Client::builder(endpoint.clone(), Keyed { fault: fault.clone() }).fallback(slow.clone()).build()),
        Some(p) => {
            let inputs = Arc::new(read_inputs(p)?);
            AnyClient::Dense(Client::builder(endpoint.clone(), Dense { fault: fault.clone() }).fallback(slow.clone()).build(), inputs)
        }
    };
    c.set_spin_ceiling(cfg.spin_off.then_some(Duration::ZERO));
    Ok(c)
}

fn main() {
    let mut endpoint = None;
    for a in std::env::args().skip(1) {
        if let Some(p) = a.strip_prefix("--endpoint=") {
            endpoint = Some(Endpoint::parse(p));
        }
    }
    let Some(endpoint) = endpoint else {
        eprintln!("usage: scz-agent-rust --endpoint=PATH");
        std::process::exit(2);
    };
    // The fallback's panics and the unwind hook's are expected; report any other.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let p = info.payload();
        let hook = p.downcast_ref::<&str>().is_some_and(|s| s.contains("test hook"));
        if !p.is::<Thrown>() && !hook {
            default_hook(info);
        }
    }));
    let fault = Arc::new(AtomicU8::new(0));
    let slow = Slow { us: Arc::new(AtomicU64::new(0)), throws: Arc::new(AtomicBool::new(false)) };
    let mut cfg = Config { threads: 1, per_call: false, spin_off: false, dense: None };
    let mut client = Some(build(&endpoint, &cfg, &fault, &slow).expect("the keyed client"));
    let out = std::io::stdout();
    let say = |s: &str| {
        let mut o = out.lock();
        let _ = writeln!(o, "{s}");
        let _ = o.flush();
    };
    let crash = if cfg!(feature = "crashpoints") { ",crashpoints" } else { "" };
    say(&format!("HELLO sdk=rust version={} caps=threads,per-call,frames,unwind,throws,nospin,fork,spawn{crash}", env!("CARGO_PKG_VERSION")));

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = words.first() else { continue };
        let m = kv(&words[1..]);
        match (cmd, client.as_ref()) {
            ("CONFIG", _) => {
                let kind = m.get("kind").map(String::as_str);
                if matches!(kind, Some(k) if k != "platform" && k != "per-call") {
                    say("ERR unsupported");
                    continue;
                }
                cfg.threads = num(&m, "threads", cfg.threads as u64).max(1) as u32;
                cfg.per_call = kind.map_or(cfg.per_call, |k| k == "per-call");
                slow.us.store(num(&m, "fallback_us", slow.us.load(Relaxed)), Relaxed);
                if let Some(f) = m.get("fallback") {
                    slow.throws.store(f == "throws", Relaxed);
                }
                if let Some(s) = m.get("spin") {
                    cfg.spin_off = s == "off";
                }
                let dense = match m.get("codec").map(String::as_str) {
                    Some("features") => Some(m.get("inputs").cloned().unwrap_or_default()),
                    Some(_) => None,
                    None => cfg.dense.clone(),
                };
                if dense != cfg.dense && client.is_some() {
                    // Another codec is another client: the old session ends first.
                    cfg.dense = dense;
                    client.take().unwrap().close();
                    match build(&endpoint, &cfg, &fault, &slow) {
                        Ok(c) => client = Some(c),
                        Err(e) => {
                            say(&format!("ERR {e}"));
                            continue;
                        }
                    }
                }
                if let Some(c) = &client {
                    c.set_spin_ceiling(cfg.spin_off.then_some(Duration::ZERO));
                }
                say("OK");
            }
            ("CODEC_FAULT", _) => {
                let mode = match m.get("mode").map(String::as_str) {
                    Some("encode") => ENCODE,
                    Some("decode") => DECODE,
                    _ => 0,
                };
                fault.store(mode, Relaxed);
                say("OK");
            }
            ("UNWIND", _) => {
                scz_shm::ring::UNWIND_ONCE.store(true, Relaxed);
                say("OK");
            }
            ("CLOCK_SKEW", _) => say("ERR unsupported"),
            ("SPAWN", _) => match std::process::Command::new("sleep").arg("600").spawn() {
                Ok(child) => say(&format!("SPAWNED pid={}", child.id())),
                Err(e) => say(&format!("ERR {e}")),
            },
            ("PROC", _) => say(&proc_counts()),
            ("EXIT", _) => {
                say("BYE");
                if let Some(c) = client.take() {
                    c.close();
                }
                std::process::exit(0);
            }
            (_, None) => say("ERR closed"),
            ("BOUND", Some(c)) => say(&format!("BOUND {}", c.await_bound(Duration::from_millis(num(&m, "ms", 5000))))),
            ("FORK", Some(c)) => say(&fork_once(c)),
            ("CALL", Some(c)) => {
                let count = num(&m, "count", 1) as u32;
                let req = Req {
                    op: m.get("op").and_then(|o| proto::op::parse(o)).unwrap_or(proto::op::NORMAL),
                    arg: num(&m, "arg", 0),
                    seed: num(&m, "seed", 1),
                    rows: num(&m, "rows", 0) as usize,
                    budget: Duration::from_micros(num(&m, "budget_us", 20_000)),
                };
                let per_call = cfg.per_call;
                let lines: Vec<String> = std::thread::scope(|sc| {
                    let hs: Vec<_> =
                        (0..cfg.threads)
                            .map(|t| {
                                sc.spawn(move || {
                                    (0..count)
                                        .map(|s| {
                                            if per_call {
                                                std::thread::scope(|one| one.spawn(|| c.call(&req, t, s)).join().unwrap())
                                            } else {
                                                c.call(&req, t, s)
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                })
                            })
                            .collect();
                    hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
                });
                let mut o = out.lock();
                for l in &lines {
                    let _ = writeln!(o, "{l}");
                }
                let _ = writeln!(o, "END calls={}", lines.len());
                let _ = o.flush();
            }
            ("STATS", Some(c)) => {
                let s = c.stats();
                let fb: String = s.fallback.iter().map(|(r, n)| format!(" fb.{}={n}", r.as_str())).collect();
                say(&format!("STATS model={} sessions={} retired={} late_wakes={}{fb}", s.model, s.sessions, s.retired, s.late_wakes));
            }
            ("CLOSE", Some(_)) => {
                if let Some(c) = client.take() {
                    c.close();
                }
                say("CLOSED");
            }
            _ => say(&format!("ERR unknown command {cmd}")),
        }
    }
}
