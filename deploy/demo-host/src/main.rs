//! scz-demo-host: the host application of the deployment tests (kubernetes-deploy §10).
//!
//!   scz-demo-host [--socket=PATH] [--codec=features|keyed] [--threads=2] [--budget-us=2000]
//!                 [--interval-us=500] [--http=ADDR:PORT] [--once=N]
//!
//! It calls the sidecar from `--threads` threads, each pausing `--interval-us` between calls,
//! and counts what answered: the model, or the fallback with its reason. Every second it
//! prints the counts so far as one JSON line, and `GET /stats` returns the same line;
//! `GET /healthz` always answers 200, because the host can always answer through its fallback
//! (kubernetes-deploy §6: the host's readiness must not depend on the model).
//!
//! `--codec=features` sends the fraud model's 256 features; `keyed` sends the chaos worker's
//! keyed requests and checks every answer against its own request, counting a wrong one as
//! `wrong`. `--once=N` makes N calls, prints the counts, and exits: a second client process,
//! which the sidecar refuses with `BUSY` (kubernetes-deploy §10 test 12).
//!
//! The socket is `--socket`, else `$SIDECARZERO_SOCKET`, else `/run/sidecarzero/scz.sock`.

use scz_chaos_proto as proto;
use sidecarzero::{Client, Codec, CodecError, Endpoint, Fallback, FallbackReason, FeatureScore, SlotReader, SlotWriter, Source};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Counts {
    model: AtomicU64,
    wrong: AtomicU64,
    fallback: [AtomicU64; 21],
    /// Calls that returned later than their budget plus 50 ms (the suite's shared tolerance).
    late: AtomicU64,
}

impl Counts {
    fn json(&self, started: Instant) -> String {
        let fb: Vec<String> = FallbackReason::ALL
            .iter()
            .zip(&self.fallback)
            .filter(|(_, n)| n.load(Relaxed) > 0)
            .map(|(r, n)| format!("\"{}\":{}", r.as_str(), n.load(Relaxed)))
            .collect();
        let total: u64 = self.model.load(Relaxed) + self.fallback.iter().map(|n| n.load(Relaxed)).sum::<u64>();
        format!(
            "{{\"t_ms\":{},\"calls\":{total},\"model\":{},\"wrong\":{},\"late\":{},\"fallback\":{{{}}}}}",
            started.elapsed().as_millis(),
            self.model.load(Relaxed),
            self.wrong.load(Relaxed),
            self.late.load(Relaxed),
            fb.join(",")
        )
    }
    fn record(&self, src: Source, right: bool, took: Duration, budget: Duration) {
        match src {
            Source::Model { .. } => {
                self.model.fetch_add(1, Relaxed);
                if !right {
                    self.wrong.fetch_add(1, Relaxed);
                }
            }
            Source::Fallback { reason } => {
                let i = FallbackReason::ALL.iter().position(|r| *r == reason).unwrap_or(0);
                self.fallback[i].fetch_add(1, Relaxed);
            }
        }
        if took > budget + Duration::from_millis(50) {
            self.late.fetch_add(1, Relaxed);
        }
    }
}

/// The chaos worker's keyed requests: bytes in, the 8-byte keyed answer out.
struct Keyed;

impl Codec for Keyed {
    type In = [u8];
    type Out = u64;
    fn request_len(&self, input: &[u8]) -> usize {
        input.len()
    }
    fn encode(&self, input: &[u8], w: &mut SlotWriter<'_>) -> Result<(), CodecError> {
        Ok(w.write(input)?)
    }
    fn decode(&self, r: &SlotReader<'_>) -> Result<u64, CodecError> {
        if r.len() != 8 {
            return Err(CodecError::Shape);
        }
        let mut b = [0u8; 8];
        r.copy_to(0, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

/// A constant answer: what a real host would compute from a rule of its own.
#[derive(Clone, Copy)]
struct Constant;

impl Fallback<[u8], u64> for Constant {
    fn apply(&self, _: &[u8], _: FallbackReason) -> u64 {
        0
    }
}

impl Fallback<[f32; 256], f32> for Constant {
    fn apply(&self, _: &[f32; 256], _: FallbackReason) -> f32 {
        0.0
    }
}

struct Opts {
    socket: String,
    keyed: bool,
    threads: u32,
    budget: Duration,
    interval: Duration,
    http: Option<String>,
    once: Option<u64>,
}

fn opts() -> Result<Opts, String> {
    let mut o = Opts {
        socket: std::env::var("SIDECARZERO_SOCKET").unwrap_or_else(|_| "/run/sidecarzero/scz.sock".into()),
        keyed: false,
        threads: 2,
        budget: Duration::from_micros(2000),
        interval: Duration::from_micros(500),
        http: None,
        once: None,
    };
    for a in std::env::args().skip(1) {
        let (k, v) = a.split_once('=').unwrap_or((&a, ""));
        let num = |v: &str| v.parse::<u64>().map_err(|_| format!("{k}: not a number: {v}"));
        match k {
            "--socket" => o.socket = v.to_string(),
            "--codec" => {
                o.keyed = match v {
                    "keyed" => true,
                    "features" => false,
                    _ => return Err(format!("--codec: {v} is not features or keyed")),
                }
            }
            "--threads" => o.threads = num(v)?.max(1) as u32,
            "--budget-us" => o.budget = Duration::from_micros(num(v)?),
            "--interval-us" => o.interval = Duration::from_micros(num(v)?),
            "--http" => o.http = Some(v.to_string()),
            "--once" => o.once = Some(num(v)?),
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    Ok(o)
}

/// `GET /healthz` and `GET /stats`, one request per connection.
fn serve(addr: &str, counts: Arc<Counts>, started: Instant) {
    let l = match std::net::TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("scz-demo-host: {addr}: {e}");
            std::process::exit(2);
        }
    };
    for s in l.incoming() {
        let Ok(mut s) = s else { continue };
        let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
        let mut buf = [0u8; 1024];
        let n = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req.split_whitespace().nth(1).unwrap_or("");
        let (code, body) = match path {
            "/healthz" => (200, "ok".to_string()),
            "/stats" => (200, counts.json(started)),
            _ => (404, "not found".to_string()),
        };
        let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    }
}

/// The container's PID 1 gets no default action for SIGTERM: without a handler the kubelet
/// would wait out the whole grace period before killing it.
extern "C" fn on_term(_: libc::c_int) {
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(0) };
}

fn main() {
    // SAFETY: installs a handler that only calls _exit.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    let o = opts().unwrap_or_else(|e| {
        eprintln!("scz-demo-host: {e}");
        std::process::exit(2)
    });
    let started = Instant::now();
    let counts = Arc::new(Counts::default());
    if let Some(addr) = o.http.clone() {
        let c = counts.clone();
        std::thread::spawn(move || serve(&addr, c, started));
    }
    let ep = Endpoint::parse(&o.socket);
    eprintln!("scz-demo-host: {} threads, budget {:?}, sidecar at {}", o.threads, o.budget, o.socket);
    let calls = o.once.unwrap_or(u64::MAX);
    if o.keyed {
        let c = Arc::new(Client::builder(ep, Keyed).fallback(Constant).build());
        if o.once.is_some() {
            c.await_bound(Duration::from_secs(1));
        }
        run(&o, &counts, started, calls, move |t, s| {
            let req = proto::request(proto::op::NORMAL, 0, t, s as u32, 1, 0);
            let t0 = Instant::now();
            let out = c.infer(&req, o.budget);
            (out.source, out.value == proto::answer(&req), t0.elapsed())
        });
    } else {
        let c = Arc::new(Client::builder(ep, FeatureScore::<256>::default()).fallback(Constant).build());
        if o.once.is_some() {
            c.await_bound(Duration::from_secs(1));
        }
        run(&o, &counts, started, calls, move |t, s| {
            let mut x = [0f32; 256];
            let mut seed = (t as u64) << 32 | s;
            for v in x.iter_mut() {
                seed = proto::splitmix64(seed);
                *v = (seed >> 40) as f32 / (1u64 << 24) as f32 * 4.0 - 2.0;
            }
            let t0 = Instant::now();
            let out = c.infer(&x, o.budget);
            (out.source, true, t0.elapsed())
        });
    }
}

fn run<F: Fn(u32, u64) -> (Source, bool, Duration) + Send + Sync + 'static>(o: &Opts, counts: &Arc<Counts>, started: Instant, calls: u64, call: F) {
    let call = Arc::new(call);
    let workers: Vec<_> = (0..o.threads)
        .map(|t| {
            let (call, counts, budget, interval) = (call.clone(), counts.clone(), o.budget, o.interval);
            std::thread::spawn(move || {
                for s in 0..calls {
                    let (src, right, took) = call(t, s);
                    counts.record(src, right, took, budget);
                    if !interval.is_zero() {
                        std::thread::sleep(interval);
                    }
                }
            })
        })
        .collect();
    if o.once.is_some() {
        for w in workers {
            let _ = w.join();
        }
        println!("{}", counts.json(started));
        return;
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
        println!("{}", counts.json(started));
    }
}
