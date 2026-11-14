//! The five systems of bench §3, each behind one caller per thread.
//!
//! | | Model through | On failure |
//! |---|---|---|
//! | A | The SidecarZero SDK | The SDK's fallback |
//! | B | Triton over gRPC, tensors in registered system shared memory | The wrapper: the same fallback |
//! | C | The HTTP/JSON baseline server, keep-alive | The wrapper |
//! | D | The Unix-socket baseline server, binary frames | The wrapper |
//! | E | ONNX Runtime in this process | None: the host dies with the model |
//!
//! The wrapper of fairness rule 1 calls the reference fallback whenever a baseline call fails
//! or takes longer than its budget; each baseline socket has the budget as its timeout, and a
//! connection that failed is dropped and made again on the next call.

use scz_bench_common::{fallback, now_ns, reason_index, Outcome, Row, Scorer, FEATURES};
use sidecarzero::{Client, Endpoint, Fallback, FallbackReason, FeatureScore, Source};
use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum System {
    A,
    B,
    C,
    D,
    E,
}

impl System {
    pub fn parse(s: &str) -> Option<System> {
        Some(match s {
            "A" | "sidecarzero" => System::A,
            "B" | "triton" => System::B,
            "C" | "http-json" => System::C,
            "D" | "uds-binary" => System::D,
            "E" | "in-process" => System::E,
            _ => return None,
        })
    }
}

/// Where each system's server is.
#[derive(Clone, Debug, Default)]
pub struct Targets {
    pub socket: String,
    pub triton: String,
    pub http: String,
    pub uds: String,
    pub model: String,
    /// A with its client spin ceiling at zero (the no-spinning variant of bench §3).
    pub no_spin: bool,
}

/// One call's result: the answer, how it ended, and, when the fallback answered, the time from
/// call entry to the fallback being entered.
pub struct Called {
    pub value: f32,
    pub outcome: Outcome,
    pub entry_ns: Option<u64>,
}

pub trait Caller {
    fn call(&mut self, x: &Row, budget: Duration) -> Called;
}

thread_local! {
    /// When the current call entered, and when its fallback was entered (system A).
    static CALL_AT: Cell<u64> = const { Cell::new(0) };
    static FALLBACK_AT: Cell<u64> = const { Cell::new(0) };
}

/// The reference fallback, as the SDK calls it: it notes when it was entered.
#[derive(Clone, Copy)]
pub struct RefFallback;

impl Fallback<Row, f32> for RefFallback {
    #[inline]
    fn apply(&self, x: &Row, _: FallbackReason) -> f32 {
        FALLBACK_AT.with(|t| t.set(now_ns()));
        fallback(x)
    }
}

pub type SczClient = Client<FeatureScore<FEATURES>, RefFallback>;

/// Every system's shared state, made once: A's client, E's model.
pub enum Shared {
    A(Arc<SczClient>),
    E(Arc<Scorer>),
    Other,
}

pub fn shared(sys: System, t: &Targets) -> Result<Shared, String> {
    Ok(match sys {
        System::A => {
            let c = Client::builder(Endpoint::parse(&t.socket), FeatureScore::<FEATURES>::default()).fallback(RefFallback).build();
            if t.no_spin {
                c.set_spin_ceiling(Some(Duration::ZERO));
            }
            if !c.await_bound(Duration::from_secs(30)) {
                eprintln!("scz-bench-host: the sidecar at {} did not bind within 30 s; measuring anyway", t.socket);
            }
            Shared::A(Arc::new(c))
        }
        System::E => Shared::E(Arc::new(Scorer::open(&t.model)?)),
        _ => Shared::Other,
    })
}

/// A caller for one thread.
pub fn caller(sys: System, t: &Targets, sh: &Shared, budget: Duration, thread: usize) -> Box<dyn Caller + Send> {
    match (sys, sh) {
        (System::A, Shared::A(c)) => Box::new(SczCaller(c.clone())),
        (System::E, Shared::E(s)) => Box::new(InProcess(s.clone())),
        (System::C, _) => Box::new(Wrapped::new(HttpJson { addr: t.http.clone(), conn: None, body: String::with_capacity(4096), budget })),
        (System::D, _) => Box::new(Wrapped::new(UdsBinary { path: t.uds.clone(), conn: None, buf: vec![0; 4 + FEATURES * 4], budget })),
        (System::B, _) => triton(t, thread, budget),
        _ => unreachable!("shared state made for another system"),
    }
}

#[cfg(feature = "triton")]
fn triton(t: &Targets, thread: usize, budget: Duration) -> Box<dyn Caller + Send> {
    Box::new(Wrapped::new(TritonCall { endpoint: t.triton.clone(), tag: format!("scz{}t{thread}", std::process::id()), conn: None, budget, next_try: 0 }))
}

#[cfg(not(feature = "triton"))]
fn triton(_: &Targets, _: usize, _: Duration) -> Box<dyn Caller + Send> {
    eprintln!("scz-bench-host: built without the triton feature");
    std::process::exit(2)
}

// ── A ───────────────────────────────────────────────────────────────────────────────────

struct SczCaller(Arc<SczClient>);

impl Caller for SczCaller {
    #[inline]
    fn call(&mut self, x: &Row, budget: Duration) -> Called {
        let t0 = now_ns();
        CALL_AT.with(|c| c.set(t0));
        let o = self.0.infer(x, budget);
        match o.source {
            Source::Model { .. } => Called { value: o.value, outcome: Outcome::Model, entry_ns: None },
            Source::Fallback { reason } => {
                let at = FALLBACK_AT.with(|t| t.get());
                Called { value: o.value, outcome: Outcome::Fallback(reason_index(reason.as_str())), entry_ns: Some(at.saturating_sub(t0)) }
            }
        }
    }
}

// ── E ───────────────────────────────────────────────────────────────────────────────────

struct InProcess(Arc<Scorer>);

impl Caller for InProcess {
    fn call(&mut self, x: &Row, _: Duration) -> Called {
        match self.0.score(x) {
            Ok(v) => Called { value: v, outcome: Outcome::Model, entry_ns: None },
            Err(_) => Called { value: fallback(x), outcome: Outcome::Fallback(reason_index("MODEL_ERROR")), entry_ns: Some(0) },
        }
    }
}

// ── the baselines' wrapper ──────────────────────────────────────────────────────────────

/// A baseline's transport: Ok(score), or Err(true) for a timeout, Err(false) for a failure.
trait Transport {
    fn score(&mut self, x: &Row) -> Result<f32, bool>;
}

struct Wrapped<T>(T);

impl<T> Wrapped<T> {
    fn new(t: T) -> Self {
        Wrapped(t)
    }
}

impl<T: Transport> Caller for Wrapped<T> {
    fn call(&mut self, x: &Row, budget: Duration) -> Called {
        let t0 = now_ns();
        let r = self.0.score(x);
        let late = now_ns() - t0 > budget.as_nanos() as u64;
        match r {
            Ok(v) if !late => Called { value: v, outcome: Outcome::Model, entry_ns: None },
            // Fairness rule 1: an error or a missed deadline calls the same fallback.
            Ok(_) | Err(true) => {
                let e = now_ns() - t0;
                Called { value: fallback(x), outcome: Outcome::Fallback(reason_index("TRANSPORT_TIMEOUT")), entry_ns: Some(e) }
            }
            Err(false) => {
                let e = now_ns() - t0;
                Called { value: fallback(x), outcome: Outcome::Fallback(reason_index("TRANSPORT_ERROR")), entry_ns: Some(e) }
            }
        }
    }
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

// ── C ───────────────────────────────────────────────────────────────────────────────────

struct HttpJson {
    addr: String,
    conn: Option<(BufReader<std::net::TcpStream>, std::net::TcpStream)>,
    body: String,
    budget: Duration,
}

impl HttpJson {
    fn connect(&mut self) -> Result<(), bool> {
        let s = std::net::TcpStream::connect(&self.addr).map_err(|_| false)?;
        let _ = s.set_nodelay(true);
        let _ = s.set_read_timeout(Some(self.budget));
        let _ = s.set_write_timeout(Some(self.budget));
        let w = s.try_clone().map_err(|_| false)?;
        self.conn = Some((BufReader::new(s), w));
        Ok(())
    }
    fn exchange(&mut self, x: &Row) -> Result<f32, std::io::Error> {
        use std::fmt::Write as _;
        self.body.clear();
        self.body.push_str("{\"features\":[");
        for (i, v) in x.iter().enumerate() {
            if i > 0 {
                self.body.push(',');
            }
            let _ = write!(self.body, "{v}");
        }
        self.body.push_str("]}");
        let (r, w) = self.conn.as_mut().unwrap();
        write!(w, "POST /v1/score HTTP/1.1\r\nHost: b\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", self.body.len(), self.body)?;
        let mut line = String::new();
        r.read_line(&mut line)?;
        let ok = line.starts_with("HTTP/1.1 200");
        let mut len = 0usize;
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut b = vec![0u8; len];
        r.read_exact(&mut b)?;
        let s = String::from_utf8_lossy(&b);
        let v = s.split_once(':').and_then(|(_, v)| v.trim_end_matches('}').trim().parse::<f32>().ok());
        match (ok, v) {
            (true, Some(v)) => Ok(v),
            _ => Err(std::io::ErrorKind::InvalidData.into()),
        }
    }
}

impl Transport for HttpJson {
    fn score(&mut self, x: &Row) -> Result<f32, bool> {
        if self.conn.is_none() {
            self.connect()?;
        }
        self.exchange(x).map_err(|e| {
            self.conn = None;
            timed_out(&e)
        })
    }
}

// ── D ───────────────────────────────────────────────────────────────────────────────────

struct UdsBinary {
    path: String,
    conn: Option<std::os::unix::net::UnixStream>,
    buf: Vec<u8>,
    budget: Duration,
}

impl Transport for UdsBinary {
    fn score(&mut self, x: &Row) -> Result<f32, bool> {
        if self.conn.is_none() {
            let s = std::os::unix::net::UnixStream::connect(&self.path).map_err(|_| false)?;
            let _ = s.set_read_timeout(Some(self.budget));
            let _ = s.set_write_timeout(Some(self.budget));
            self.conn = Some(s);
        }
        self.buf[..4].copy_from_slice(&((FEATURES * 4) as u32).to_le_bytes());
        for (j, v) in x.iter().enumerate() {
            self.buf[4 + j * 4..8 + j * 4].copy_from_slice(&v.to_le_bytes());
        }
        let s = self.conn.as_mut().unwrap();
        let mut out = [0u8; 8];
        let r = s.write_all(&self.buf).and_then(|_| s.read_exact(&mut out[..4])).and_then(|_| {
            if u32::from_le_bytes(out[..4].try_into().unwrap()) != 4 {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            s.read_exact(&mut out[4..])
        });
        match r {
            Ok(()) => Ok(f32::from_le_bytes(out[4..].try_into().unwrap())),
            Err(e) => {
                self.conn = None;
                Err(timed_out(&e))
            }
        }
    }
}

// ── B ───────────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "triton")]
struct TritonCall {
    endpoint: String,
    tag: String,
    conn: Option<scz_bench_triton::Triton>,
    budget: Duration,
    /// After a failed connect, the next attempt waits: a connect takes far longer than a call.
    next_try: u64,
}

#[cfg(feature = "triton")]
impl Transport for TritonCall {
    fn score(&mut self, x: &Row) -> Result<f32, bool> {
        if self.conn.is_none() {
            let now = now_ns();
            if now < self.next_try {
                return Err(false);
            }
            match scz_bench_triton::Triton::connect(&self.endpoint, "fraud", &self.tag) {
                Ok(c) => self.conn = Some(c),
                Err(_) => {
                    self.next_try = now + 100_000_000;
                    return Err(false);
                }
            }
        }
        let t0 = now_ns();
        self.conn.as_mut().unwrap().infer(x, self.budget).map_err(|_| now_ns() - t0 >= self.budget.as_nanos() as u64)
    }
}
