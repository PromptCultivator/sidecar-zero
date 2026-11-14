//! scz-loadgen: the open-loop load generator of the benchmark's service mode (bench §2, §4.1).
//!
//!   scz-loadgen --target=HOST:PORT --rate=N --duration-s=S [--warmup-s=W] [--conns=32]
//!               [--arrival=even|poisson] [--timeout-ms=1000] [--rows=64] [--seed=1] --out=PATH
//!
//! Requests are scheduled by the clock, not by the previous response, and latency is measured
//! from the intended send time, so a stall shows as latency instead of as requests not sent
//! (coordinated omission). Each of `--conns` keep-alive connections carries an equal share of
//! the schedule. A request is `GET /score?r=N` to the host application (`scz-bench-host
//! serve`), whose `X-Source` and `X-Reason` headers say whether the model or the fallback
//! answered. A refused, reset, or malformed exchange is an `error`, one that takes longer than
//! `--timeout-ms` a `timeout`; the connection is then made again.

use scz_bench_common::{now_ns, reason_index, splitmix, wait_until, Outcome, Record};
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

struct Opts {
    target: String,
    rate: f64,
    duration: Duration,
    warmup: Duration,
    conns: usize,
    poisson: bool,
    timeout: Duration,
    rows: usize,
    seed: u64,
    out: String,
}

fn opts() -> Result<Opts, String> {
    let mut o = Opts {
        target: String::new(),
        rate: 1000.0,
        duration: Duration::from_secs(30),
        warmup: Duration::from_secs(5),
        conns: 32,
        poisson: false,
        timeout: Duration::from_secs(1),
        rows: 64,
        seed: 1,
        out: String::new(),
    };
    for a in std::env::args().skip(1) {
        let (k, v) = a.split_once('=').unwrap_or((&a, ""));
        let num = |v: &str| v.parse::<f64>().map_err(|_| format!("{k}: not a number: {v}"));
        match k {
            "--target" => o.target = v.into(),
            "--rate" => o.rate = num(v)?,
            "--duration-s" => o.duration = Duration::from_secs_f64(num(v)?),
            "--warmup-s" => o.warmup = Duration::from_secs_f64(num(v)?),
            "--conns" => o.conns = num(v)?.max(1.0) as usize,
            "--arrival" => o.poisson = v == "poisson",
            "--timeout-ms" => o.timeout = Duration::from_millis(num(v)? as u64),
            "--rows" => o.rows = num(v)?.max(1.0) as usize,
            "--seed" => o.seed = num(v)? as u64,
            "--out" => o.out = v.into(),
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    if o.target.is_empty() || o.out.is_empty() {
        return Err("--target and --out are required".into());
    }
    Ok(o)
}

type Conn = (BufReader<std::net::TcpStream>, std::net::TcpStream);

fn connect(target: &str, timeout: Duration) -> Option<Conn> {
    let addr = std::net::ToSocketAddrs::to_socket_addrs(target).ok()?.next()?;
    let s = std::net::TcpStream::connect_timeout(&addr, timeout).ok()?;
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let w = s.try_clone().ok()?;
    Some((BufReader::new(s), w))
}

/// One exchange: the outcome, or None if the connection failed (it is then dropped).
fn exchange(c: &mut Conn, row: usize, line: &mut String) -> Result<Outcome, Outcome> {
    let (r, w) = c;
    let fail =
        |e: std::io::Error| if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) { Outcome::Timeout } else { Outcome::Error };
    write!(w, "GET /score?r={row} HTTP/1.1\r\nHost: h\r\n\r\n").map_err(fail)?;
    line.clear();
    r.read_line(line).map_err(fail)?;
    if !line.starts_with("HTTP/1.1 200") {
        return Err(Outcome::Error);
    }
    let (mut model, mut reason, mut len) = (false, String::new(), 0usize);
    loop {
        line.clear();
        if r.read_line(line).map_err(fail)? == 0 {
            return Err(Outcome::Error);
        }
        if line == "\r\n" {
            break;
        }
        let l = line.trim_end();
        if let Some(v) = l.strip_prefix("X-Source: ") {
            model = v == "model";
        } else if let Some(v) = l.strip_prefix("X-Reason: ") {
            reason = v.to_string();
        } else if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    std::io::Read::read_exact(r, &mut body).map_err(fail)?;
    Ok(if model { Outcome::Model } else { Outcome::Fallback(reason_index(&reason)) })
}

fn main() {
    let o = opts().unwrap_or_else(|e| {
        eprintln!("scz-loadgen: {e}");
        std::process::exit(2)
    });
    let t0 = now_ns() + 200_000_000;
    let measure_from = t0 + o.warmup.as_nanos() as u64;
    let end = measure_from + o.duration.as_nanos() as u64;
    let period = o.conns as f64 * 1e9 / o.rate;
    let handles: Vec<_> = (0..o.conns)
        .map(|i| {
            let (target, timeout, rows, poisson, conns, rate, mut seed) =
                (o.target.clone(), o.timeout, o.rows, o.poisson, o.conns as u64, o.rate, o.seed ^ (i as u64 * 0x9E37));
            std::thread::spawn(move || {
                let mut rec = Record::default();
                let mut conn: Option<Conn> = None;
                let mut line = String::new();
                let mut next = t0 + (i as f64 * 1e9 / rate) as u64;
                let mut k = 0u64;
                while next < end {
                    wait_until(next);
                    rec.send_lag.record(now_ns() - next);
                    if conn.is_none() {
                        conn = connect(&target, timeout);
                    }
                    let out = match conn.as_mut() {
                        None => Outcome::Error,
                        Some(c) => match exchange(c, (k as usize * conns as usize + i) % rows, &mut line) {
                            Ok(x) => x,
                            Err(x) => {
                                conn = None;
                                x
                            }
                        },
                    };
                    let t1 = now_ns();
                    if next >= measure_from {
                        rec.add(out, t1 - next, None, next - measure_from);
                    }
                    k += 1;
                    next = if poisson {
                        let u = ((splitmix(&mut seed) >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
                        next + (-u.ln() * period) as u64
                    } else {
                        t0 + ((k * conns + i as u64) as f64 * 1e9 / rate) as u64
                    };
                }
                rec
            })
        })
        .collect();
    let mut rec = Record::default();
    for h in handles {
        rec.merge(&h.join().expect("a connection thread panicked"));
    }
    let json = format!(
        "{{\"mode\":\"service\",\"target\":\"{}\",\"rate\":{},\"arrival\":\"{}\",\"conns\":{},\"duration_s\":{},\"warmup_s\":{},\"timeout_ms\":{},\"start_ns\":{measure_from},\"end_ns\":{end},\"record\":{}}}\n",
        o.target,
        o.rate,
        if o.poisson { "poisson" } else { "even" },
        o.conns,
        o.duration.as_secs_f64(),
        o.warmup.as_secs_f64(),
        o.timeout.as_millis(),
        rec.json()
    );
    std::fs::write(&o.out, json).unwrap_or_else(|e| panic!("{}: {e}", o.out));
    eprintln!(
        "scz-loadgen: {} calls, model {}, fallback {}, error {}, timeout {}; p50 {:.0} µs, p99 {:.0} µs",
        rec.latency.count,
        rec.model,
        rec.fallback.iter().sum::<u64>(),
        rec.error,
        rec.timeout,
        rec.latency.quantile(0.5) as f64 / 1e3,
        rec.latency.quantile(0.99) as f64 / 1e3
    );
}
