//! The conformance runner (testing §3). A scenario is a TOML file: how to start the sidecar
//! and the agent, then a list of steps. The runner drives the agent over its line protocol,
//! the supervisor with signals and `sczctl`, and checks:
//!
//! - every call's outcome against what its step expects, and every model answer against the
//!   keyed value of that call's own request (`scz_chaos_proto`);
//! - the universal assertions, after the last step: every call returned within its budget,
//!   its fallback's time, and the tolerance; the arena passes the invariant check (through the
//!   admin socket's read-only `dump`); the agent's counters equal the outcomes it printed; its
//!   descriptors, arena mappings, and threads are back where they started; neither the agent
//!   nor the supervisor died unless the scenario killed it.
//!
//! The format is documented in `conformance/README.md`.

use scz_chaos_proto as proto;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Where the binaries are.
#[derive(Clone, Debug)]
pub struct Bins {
    /// The supervisor, `sczctl`, the chaos worker, and the Rust agent.
    pub dir: PathBuf,
    /// The same, built with crash points (testing §2.4), for scenarios that set `crash_at`.
    pub crash_dir: Option<PathBuf>,
    /// The agent under test; the Rust agent in `dir` by default.
    pub agent: Option<PathBuf>,
    /// The agent under test built with crash points, for scenarios with `crash_in = "agent"`;
    /// the Rust agent in `crash_dir` when `agent` is not set. Without one those scenarios skip.
    pub crash_agent: Option<PathBuf>,
    /// The tolerance of assertion 2 (testing §3, "Timing rules").
    pub tolerance: Duration,
}

/// What a scenario did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub name: String,
    pub calls: usize,
    /// Fallback reasons seen, with counts; `MODEL` counts model answers.
    pub seen: BTreeMap<String, usize>,
    pub ops: BTreeSet<String>,
    pub crash_at: Option<String>,
    pub skipped: Option<String>,
    pub elapsed: Duration,
    /// The agent's `sdk=` (from `HELLO`).
    pub sdk: String,
    /// Late wake-ups the agents counted (signaling §7; universal assertion 8).
    pub late_wakes: u64,
    /// The steps and what each call step saw.
    pub trail: Vec<String>,
}

// ── the scenario file ───────────────────────────────────────────────────────────────────

struct Scenario {
    name: String,
    seed: u64,
    allow: Vec<String>,
    covers: Vec<String>,
    needs: Vec<String>,
    crash_at: Option<String>,
    crash_in: String,
    crash_gen: u64,
    worker: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    start_sidecar: bool,
    start_agent: bool,
    threads: u64,
    budget_us: u64,
    fallback_us: u64,
    /// The agent's CONFIG beyond threads and fallback time: `kind`, `codec`, `fallback`, `spin`.
    agent_config: AgentConfig,
    /// The scenario stalls wake-ups on purpose, so late wakes are expected (assertion 8).
    stall: bool,
    steps: Vec<toml::Table>,
}

/// The CONFIG keys of protocol v2 a scenario or a `config` step may set. None: not sent, so
/// the agent keeps its own default (Go's goroutines, for example).
#[derive(Clone, Debug, Default)]
struct AgentConfig {
    kind: Option<String>,
    codec: Option<String>,
    fallback: Option<String>,
    spin: Option<String>,
}

impl AgentConfig {
    fn read(t: &toml::Table, base: &AgentConfig) -> Result<AgentConfig, String> {
        let get = |k: &str, allowed: &[&str], cur: &Option<String>| -> Result<Option<String>, String> {
            match t.get(k) {
                None => Ok(cur.clone()),
                Some(v) => {
                    let v = v.as_str().ok_or(format!("{k}: expected a string"))?;
                    if !allowed.contains(&v) {
                        return Err(format!("{k}: {v} is not one of {allowed:?}"));
                    }
                    Ok(Some(v.to_string()))
                }
            }
        };
        Ok(AgentConfig {
            kind: get("kind", &["platform", "virtual", "goroutine", "per-call"], &base.kind)?,
            codec: get("codec", &["keyed", "features"], &base.codec)?,
            fallback: get("fallback", &["constant", "throws"], &base.fallback)?,
            spin: get("spin", &["on", "off"], &base.spin)?,
        })
    }
    fn features(&self) -> bool {
        self.codec.as_deref() == Some("features")
    }
    fn throws(&self) -> bool {
        self.fallback.as_deref() == Some("throws")
    }
}

fn strs(t: &toml::Table, k: &str) -> Result<Vec<String>, String> {
    match t.get(k) {
        None => Ok(vec![]),
        Some(toml::Value::String(s)) => Ok(vec![s.clone()]),
        Some(toml::Value::Array(a)) => a.iter().map(|v| v.as_str().map(String::from).ok_or(format!("{k}: expected strings"))).collect(),
        Some(_) => Err(format!("{k}: expected a string or a list of strings")),
    }
}

fn int(t: &toml::Table, k: &str, default: u64) -> Result<u64, String> {
    match t.get(k) {
        None => Ok(default),
        Some(v) => v.as_integer().filter(|&i| i >= 0).map(|i| i as u64).ok_or(format!("{k}: expected a non-negative integer")),
    }
}

fn string(t: &toml::Table, k: &str, default: &str) -> Result<String, String> {
    match t.get(k) {
        None => Ok(default.to_string()),
        Some(v) => v.as_str().map(String::from).ok_or(format!("{k}: expected a string")),
    }
}

fn boolean(t: &toml::Table, k: &str, default: bool) -> Result<bool, String> {
    match t.get(k) {
        None => Ok(default),
        Some(v) => v.as_bool().ok_or(format!("{k}: expected true or false")),
    }
}

fn check_keys(t: &toml::Table, what: &str, known: &[&str]) -> Result<(), String> {
    match t.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => Err(format!("{what}: unknown key {k}")),
        None => Ok(()),
    }
}

fn table<'a>(t: &'a toml::Table, k: &str) -> Result<Option<&'a toml::Table>, String> {
    match t.get(k) {
        None => Ok(None),
        Some(v) => v.as_table().map(Some).ok_or(format!("{k}: expected a table")),
    }
}

fn parse(text: &str) -> Result<Scenario, String> {
    let t: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
    check_keys(&t, "scenario", &["name", "seed", "allow", "covers", "needs", "crash_at", "crash_in", "crash_gen", "stall", "sidecar", "agent", "step"])?;
    let empty = toml::Table::new();
    let sc = table(&t, "sidecar")?.unwrap_or(&empty);
    check_keys(sc, "[sidecar]", &["worker", "args", "env", "start"])?;
    let ag = table(&t, "agent")?.unwrap_or(&empty);
    check_keys(ag, "[agent]", &["start", "threads", "budget_us", "fallback_us", "kind", "codec", "fallback", "spin"])?;
    let env = strs(sc, "env")?
        .into_iter()
        .map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or(format!("env: {kv} is not KEY=VALUE")))
        .collect::<Result<_, _>>()?;
    let steps = match t.get("step") {
        None => vec![],
        Some(toml::Value::Array(a)) => a.iter().map(|v| v.as_table().cloned().ok_or("step: expected [[step]] tables".to_string())).collect::<Result<_, _>>()?,
        Some(_) => return Err("step: expected [[step]] tables".into()),
    };
    let crash_at = t.get("crash_at").map(|v| v.as_str().map(String::from).ok_or("crash_at: expected a string")).transpose()?;
    Ok(Scenario {
        name: string(&t, "name", "")?,
        seed: int(&t, "seed", 1)?,
        allow: strs(&t, "allow")?,
        covers: strs(&t, "covers")?,
        needs: strs(&t, "needs")?,
        crash_in: string(&t, "crash_in", "worker")?,
        crash_gen: int(&t, "crash_gen", 1)?,
        crash_at,
        worker: string(sc, "worker", "scz-worker-chaos")?,
        args: strs(sc, "args")?,
        env,
        start_sidecar: boolean(sc, "start", true)?,
        start_agent: boolean(ag, "start", true)?,
        threads: int(ag, "threads", 1)?,
        budget_us: int(ag, "budget_us", 20_000)?,
        fallback_us: int(ag, "fallback_us", 0)?,
        agent_config: AgentConfig::read(ag, &AgentConfig::default())?,
        stall: boolean(&t, "stall", false)?,
        steps,
    })
}

// ── processes ───────────────────────────────────────────────────────────────────────────

struct Agent {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    caps: Vec<String>,
    threads: u64,
    fallback_us: u64,
    config: AgentConfig,
    /// UNWIND commands sent whose unwound call has not been seen yet.
    unwinds: u32,
    /// Children started by SPAWN, killed at the end.
    spawned: Vec<u32>,
    model: u64,
    fallbacks: BTreeMap<String, u64>,
    baseline: Option<String>,
    pending: Option<(Call, usize)>,
    /// The scenario killed it, or expects it to die at a crash point.
    doomed: bool,
    closed: bool,
}

#[derive(Clone)]
struct Call {
    count: u64,
    budget_us: u64,
    rows: u64,
    op: u64,
    op_name: String,
    arg: u64,
    seed: u64,
    expect: String,
    reasons: Vec<String>,
    need: Vec<String>,
    /// Check that the ring tails moved by exactly the calls that reached a ring.
    count_publishes: bool,
    /// The ring tails when the step started, for `count_publishes`.
    tails_before: Option<Vec<u64>>,
    /// A hostile worker scribbles while these calls run: their outcomes are unconstrained
    /// (assertions 3 and 4 relaxed); each must still return within its bound.
    hostile: bool,
}

/// An error that skips the scenario instead of failing it.
const SKIP: &str = "\u{1}skip: ";

/// What each kind of outcome publishes on a ring: a call that falls back at admission, or
/// fails to claim a slot or a cell, publishes nothing; one answered, or completed with a
/// status, or abandoned at its deadline, published one descriptor. Reasons that can come from
/// either side of the enqueue (a gate change, a rebind, a codec failure) have no entry.
fn publishes(outcome: &str) -> Option<u64> {
    match outcome {
        "MODEL" | "DEADLINE" | "WORKER_FAILED" | "MODEL_ERROR" | "BAD_REQUEST" | "RESOURCE_EXHAUSTED" => Some(1),
        "NOT_CONNECTED" | "ABI_MISMATCH" | "SCHEMA_MISMATCH" | "STALE_HEARTBEAT" | "CLOCK_FAULT" | "OVERLOADED" | "TOO_LARGE" | "NO_SLOT" | "RING_FULL" => {
            Some(0)
        }
        _ => None,
    }
}

/// The ring tails, from `sczctl dump`.
fn tails(dump: &str) -> Option<Vec<u64>> {
    let list = dump.split("\"tails\":[").nth(1)?.split(']').next()?;
    list.split(',').filter(|x| !x.is_empty()).map(|x| x.trim().parse().ok()).collect()
}

fn signal(pid: u32, sig: &str) -> Result<(), String> {
    let n = match sig {
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "STOP" => libc::SIGSTOP,
        "CONT" => libc::SIGCONT,
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        _ => return Err(format!("unknown signal {sig}")),
    };
    // SAFETY: kill has no memory-safety preconditions.
    if unsafe { libc::kill(pid as i32, n) } != 0 {
        return Err(format!("kill -{sig} {pid}: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// `sczctl status` JSON without its arrays (workers, transitions), whose entries reuse the
/// top-level key names.
fn top_level(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for ch in s.chars() {
        match ch {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// A top-level number from `sczctl status` JSON, by key.
fn json_num(s: &str, key: &str) -> Option<f64> {
    let s = &top_level(s);
    let k = format!("\"{key}\":");
    let i = s.find(&k)?;
    let v = s[i + k.len()..].split([',', '}']).next()?.trim_matches('"');
    v.parse().ok().or(match v {
        "true" => Some(1.0),
        "false" => Some(0.0),
        _ => None,
    })
}

fn json_str(s: &str, key: &str) -> Option<String> {
    top_level(s).split(&format!("\"{key}\":\"")).nth(1)?.split('"').next().map(String::from)
}

struct Run<'a> {
    sc: &'a Scenario,
    bins: &'a Bins,
    dir: PathBuf,
    sock: PathBuf,
    admin: PathBuf,
    sup: Option<Child>,
    agents: BTreeMap<String, Agent>,
    out: Outcome,
    trail: Vec<String>,
}

impl Run<'_> {
    fn bin(&self, name: &str, crash: bool) -> Result<PathBuf, String> {
        let dir = if crash { self.bins.crash_dir.as_ref().ok_or("this scenario needs crash-point binaries")? } else { &self.bins.dir };
        let p = dir.join(name);
        if !p.exists() {
            return Err(format!("{} is not built", p.display()));
        }
        Ok(p)
    }

    fn start_sidecar(&mut self) -> Result<(), String> {
        let crash = self.sc.crash_at.is_some() && self.sc.crash_in == "worker";
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("supervisor.log")).map_err(|e| e.to_string())?;
        let mut c = Command::new(self.bin("scz-supervisor", false)?);
        c.arg(format!("--socket={}", self.sock.display()))
            .arg(format!("--worker={}", self.bin(&self.sc.worker, crash)?.display()))
            .arg(format!("--admin={}", self.admin.display()));
        if !self.sc.args.iter().any(|a| a.starts_with("--hb-stale-ms")) {
            c.arg("--hb-stale-ms=100");
        }
        let dir = self.dir.display().to_string();
        let models = if self.sc.args.iter().any(|a| a.contains("{models}")) { models_dir(&self.bins.dir)? } else { String::new() };
        c.args(self.sc.args.iter().map(|a| a.replace("{dir}", &dir).replace("{models}", &models))).stdout(Stdio::null()).stderr(Stdio::from(log));
        for (k, v) in &self.sc.env {
            c.env(k, v);
        }
        if crash {
            c.env("SCZ_CRASH_AT", self.sc.crash_at.as_ref().unwrap()).env("SCZ_CRASH_GEN", self.sc.crash_gen.to_string());
        }
        self.sup = Some(c.spawn().map_err(|e| format!("starting the supervisor: {e}"))?);
        Ok(())
    }

    fn sup_pid(&self) -> Result<u32, String> {
        self.sup.as_ref().map(Child::id).ok_or("no supervisor".into())
    }

    fn sczctl(&self, cmd: &str) -> Result<String, String> {
        let out = Command::new(self.bin("sczctl", false)?)
            .arg(format!("--admin={}", self.admin.display()))
            .args(cmd.split_whitespace())
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("sczctl {cmd}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn status(&self) -> String {
        self.sczctl("status").unwrap_or_default()
    }

    /// The pid of the worker of generation `gen`, from the supervisor's children.
    fn worker_pid(&self, gen: u64) -> Result<u32, String> {
        let id = self.sup_pid()?;
        let kids = std::fs::read_to_string(format!("/proc/{id}/task/{id}/children")).unwrap_or_default();
        kids.split_whitespace()
            .filter_map(|p| p.parse::<u32>().ok())
            .find(|p| std::fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).split('\0').any(|a| a == format!("--gen={gen}"))))
            .ok_or(format!("no worker of generation {gen}"))
    }

    /// The generation of the first worker in `role` (Loading, Warm, Consumer).
    fn gen_in_role(&self, role: &str) -> Result<u64, String> {
        let s = self.status();
        let w = s.split("\"workers\":[").nth(1).and_then(|r| r.split(']').next()).unwrap_or("");
        w.split("},{")
            .find(|e| e.contains(&format!("\"role\":\"{role}\"")))
            .and_then(|e| json_num(e, "gen"))
            .map(|g| g as u64)
            .ok_or(format!("no worker is {role}: {s}"))
    }

    fn start_agent(&mut self, name: &str, crash: bool) -> Result<(), String> {
        let path = match (&self.bins.agent, &self.bins.crash_agent, crash) {
            (Some(p), _, false) => p.clone(),
            (_, Some(p), true) => p.clone(),
            (Some(_), None, true) => return Err(format!("{SKIP}no crash-point build of the agent under test")),
            (None, _, _) => self.bin("scz-agent-rust", crash)?,
        };
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("agent-{name}.log"))).map_err(|e| e.to_string())?;
        let mut c = Command::new(&path);
        c.arg(format!("--endpoint={}", self.sock.display())).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(log));
        if crash {
            c.env("SCZ_CRASH_AT", self.sc.crash_at.as_ref().ok_or("crash = true needs crash_at")?);
        }
        let mut child = c.spawn().map_err(|e| format!("starting {}: {e}", path.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let hello = rx.recv_timeout(Duration::from_secs(30)).map_err(|_| format!("agent {name} sent no HELLO"))?;
        let caps: Vec<String> = hello.split_whitespace().find_map(|w| w.strip_prefix("caps=")).unwrap_or("").split(',').map(String::from).collect();
        let sdk = hello.split_whitespace().find_map(|w| w.strip_prefix("sdk=")).unwrap_or("").to_string();
        if self.out.sdk.is_empty() && !crash {
            self.out.sdk = sdk;
        }
        let a = Agent {
            child,
            stdin,
            rx,
            caps,
            threads: self.sc.threads,
            fallback_us: self.sc.fallback_us,
            config: self.sc.agent_config.clone(),
            unwinds: 0,
            spawned: vec![],
            model: 0,
            fallbacks: BTreeMap::new(),
            baseline: None,
            pending: None,
            doomed: crash,
            closed: false,
        };
        // The capabilities a scenario needs are checked on the first agent it starts.
        let missing = if self.agents.is_empty() { self.sc.needs.iter().find(|n| !a.caps.contains(n)).cloned() } else { None };
        self.agents.insert(name.to_string(), a);
        if let Some(n) = missing {
            return Err(format!("{SKIP}the agent lacks {n}"));
        }
        self.configure(name)
    }

    /// Send the agent its CONFIG: threads and fallback time always, the v2 keys when set.
    fn configure(&mut self, name: &str) -> Result<(), String> {
        let models = if self.agent(name)?.config.features() { models_dir(&self.bins.dir)? } else { String::new() };
        let a = self.agent(name)?;
        let mut line = format!("CONFIG threads={} fallback_us={}", a.threads, a.fallback_us);
        let c = a.config.clone();
        for (k, v) in [("kind", &c.kind), ("codec", &c.codec), ("fallback", &c.fallback), ("spin", &c.spin)] {
            if let Some(v) = v {
                line += &format!(" {k}={v}");
            }
        }
        if c.features() {
            line += &format!(" inputs={models}/reference-features.f32");
        }
        let r = self.ask(name, &line, "", Duration::from_secs(5))?;
        if r.starts_with("ERR unsupported") {
            return Err(format!("{SKIP}the agent does not support {line}"));
        }
        if r != "OK" {
            return Err(format!("agent {name}: {line} → {r}"));
        }
        Ok(())
    }

    /// The reference scores for codec=features calls (models/gen.py).
    fn scores(&self) -> Result<Vec<f32>, String> {
        let p = Path::new(&models_dir(&self.bins.dir)?).join("reference-scores.f32");
        let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
    }

    fn ring_tails(&self) -> Result<Vec<u64>, String> {
        let d = self.sczctl("dump")?;
        tails(&d).ok_or(format!("no ring tails in sczctl dump: {}", d.trim()))
    }

    fn agent(&mut self, name: &str) -> Result<&mut Agent, String> {
        self.agents.get_mut(name).ok_or(format!("no agent {name}"))
    }

    fn send(&mut self, name: &str, line: &str) -> Result<(), String> {
        let a = self.agent(name)?;
        writeln!(a.stdin, "{line}").and_then(|_| a.stdin.flush()).map_err(|e| format!("agent {name}: {e}"))
    }

    fn recv(&mut self, name: &str, timeout: Duration) -> Result<String, String> {
        let a = self.agent(name)?;
        match a.rx.recv_timeout(timeout) {
            Ok(l) => Ok(l),
            Err(RecvTimeoutError::Timeout) => Err(format!("agent {name} did not answer within {timeout:?}")),
            Err(RecvTimeoutError::Disconnected) => {
                let st = a.child.wait().map(|s| format!("{s}")).unwrap_or_default();
                Err(format!("agent {name} exited ({st})"))
            }
        }
    }

    /// Send a command and return its reply, which must start with `want`.
    fn ask(&mut self, name: &str, line: &str, want: &str, timeout: Duration) -> Result<String, String> {
        self.send(name, line)?;
        let r = self.recv(name, timeout)?;
        if !r.starts_with(want) {
            return Err(format!("agent {name}: {line} → {r}"));
        }
        Ok(r)
    }

    fn proc(&mut self, name: &str) -> Result<String, String> {
        self.ask(name, "PROC", "PROC", Duration::from_secs(5))
    }

    // ── calls

    fn call_spec(&self, t: &toml::Table, i: usize) -> Result<Call, String> {
        let op_name = string(t, "op", "normal")?;
        let op = proto::op::parse(&op_name).ok_or(format!("unknown op {op_name}"))?;
        let arg = match t.get("arg") {
            Some(toml::Value::String(s)) => (0..16).find(|&v| scz_shm::abi::status::name(v) == s).ok_or(format!("unknown status {s}"))? as u64,
            _ => int(t, "arg", 0)?,
        };
        let expect = string(t, "expect", if boolean(t, "background", false)? { "mixed" } else { "model" })?;
        if !["model", "fallback", "mixed"].contains(&expect.as_str()) {
            return Err(format!("expect: {expect} is not model, fallback, or mixed"));
        }
        Ok(Call {
            count: int(t, "count", 1)?,
            budget_us: int(t, "budget_us", self.sc.budget_us)?,
            rows: int(t, "rows", 0)?,
            op,
            op_name,
            arg,
            seed: int(t, "seed", self.sc.seed.wrapping_add(i as u64))?,
            expect,
            reasons: strs(t, "reasons")?,
            need: strs(t, "need")?,
            count_publishes: boolean(t, "count_publishes", false)?,
            tails_before: None,
            hostile: boolean(t, "hostile", false)?,
        })
    }

    fn start_call(&mut self, name: &str, mut c: Call, step: usize) -> Result<(), String> {
        if c.count_publishes {
            c.tails_before = Some(self.ring_tails()?);
        }
        let line = format!("CALL count={} budget_us={} rows={} op={} arg={} seed={}", c.count, c.budget_us, c.rows, c.op, c.arg, c.seed);
        self.send(name, &line)?;
        self.out.ops.insert(c.op_name.clone());
        self.agent(name)?.pending = Some((c, step));
        Ok(())
    }

    fn finish_call(&mut self, name: &str) -> Result<(), String> {
        let Some((c, step)) = self.agent(name)?.pending.take() else { return Ok(()) };
        let (threads, fb_us, config, armed) = {
            let a = self.agent(name)?;
            (a.threads, a.fallback_us, a.config.clone(), a.unwinds)
        };
        let scores = if config.features() { self.scores()? } else { vec![] };
        let limit = Duration::from_micros(c.count * (c.budget_us + fb_us + 1000)) + Duration::from_secs(30);
        let bound_ns = (c.budget_us + fb_us) * 1000 + self.bins.tolerance.as_nanos() as u64;
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let (mut n, mut unwound) = (0u64, 0u32);
        let t = Instant::now();
        loop {
            let left = limit.checked_sub(t.elapsed()).ok_or(format!("step {step}: the calls did not finish within {limit:?}"))?;
            let l = self.recv(name, left)?;
            if let Some(calls) = l.strip_prefix("END calls=") {
                if calls.parse::<u64>().ok() != Some(threads * c.count) || n != threads * c.count {
                    return Err(format!("step {step}: {n} call lines, END says {calls}, expected {}", threads * c.count));
                }
                break;
            }
            let Some(rest) = l.strip_prefix("R ") else { return Err(format!("step {step}: unexpected line {l}")) };
            n += 1;
            let f: BTreeMap<&str, &str> = rest.split_whitespace().filter_map(|w| w.split_once('=')).collect();
            let get = |k: &str| f.get(k).copied().ok_or(format!("step {step}: no {k} in {l}"));
            let num = |k: &str| get(k).and_then(|v| v.parse::<u64>().map_err(|_| format!("step {step}: bad {k} in {l}")));
            let (t0, t1) = (num("t0")?, num("t1")?);
            if t1.saturating_sub(t0) > bound_ns {
                return Err(format!("step {step}: a call took {:.3} ms, over its bound of {:.3} ms: {l}", (t1 - t0) as f64 / 1e6, bound_ns as f64 / 1e6));
            }
            let src = get("src")?;
            let reason = match src {
                // Under a scribbler a model answer may carry any bytes, and any reason may come.
                "model" if c.hostile => "MODEL".to_string(),
                "fallback" if c.hostile => get("reason")?.to_string(),
                "model" => {
                    let (th, s) = (num("t")? as u32, num("s")? as u32);
                    if config.features() {
                        // codec=features: row k of call s is reference row (s + k) mod R, and the
                        // model's answer must be the reference score within float tolerance.
                        let vals: Vec<&str> = get("v")?.split(',').collect();
                        if vals.len() as u64 != c.rows.max(1) {
                            return Err(format!("step {step}: {} values for {} rows: {l}", vals.len(), c.rows.max(1)));
                        }
                        for (k, v) in vals.iter().enumerate() {
                            let bits = u32::from_str_radix(v.trim_start_matches("0x"), 16).map_err(|_| format!("step {step}: bad value {v} in {l}"))?;
                            let want = scores[(s as usize + k) % scores.len()];
                            let got = f32::from_bits(bits);
                            let close = (got - want).abs() <= 1e-5; // false for NaN
                            if !close {
                                return Err(format!("step {step}: row {k} scored {got}, the reference is {want}: {l}"));
                            }
                        }
                    } else {
                        let want = proto::answer(&proto::request(c.op, c.arg, th, s, c.seed, c.rows as usize));
                        let d = u64::from_str_radix(get("d")?.trim_start_matches("0x"), 16).map_err(|_| format!("step {step}: bad digest in {l}"))?;
                        if d != want {
                            return Err(format!("step {step}: a model answer is not keyed to its request (got {d:#x}, want {want:#x}): {l}"));
                        }
                    }
                    if num("gen")? == 0 {
                        return Err(format!("step {step}: a model answer from generation 0: {l}"));
                    }
                    if c.expect == "fallback" {
                        return Err(format!("step {step}: expected only fallbacks, got a model answer: {l}"));
                    }
                    "MODEL".to_string()
                }
                "fallback" | "thrown" => {
                    if (src == "thrown") != config.throws() {
                        return Err(format!("step {step}: src={src} with fallback = {:?}: {l}", config.fallback));
                    }
                    let r = get("reason")?.to_string();
                    let ok = match c.expect.as_str() {
                        "model" => false,
                        "fallback" => c.reasons.contains(&r),
                        _ => c.reasons.contains(&r) || self.sc.allow.contains(&r),
                    };
                    if !ok {
                        return Err(format!(
                            "step {step}: fallback {r} is not allowed here (expect = {}, reasons = {:?}, allow = {:?}): {l}",
                            c.expect, c.reasons, self.sc.allow
                        ));
                    }
                    r
                }
                "unwound" => {
                    // The call an UNWIND armed: it threw out of infer between claiming a ring cell
                    // and publishing it. Counted nowhere; published one (void) cell.
                    unwound += 1;
                    if unwound > armed {
                        return Err(format!("step {step}: a call unwound that no UNWIND armed: {l}"));
                    }
                    continue;
                }
                s => return Err(format!("step {step}: unknown source {s}")),
            };
            *seen.entry(reason).or_default() += 1;
        }
        if armed > 0 && unwound < armed && n > 0 {
            return Err(format!("step {step}: UNWIND was armed but {} of {armed} calls unwound", unwound));
        }
        for r in &c.need {
            if !seen.contains_key(r) {
                return Err(format!("step {step}: needed at least one {r}, saw {seen:?}"));
            }
        }
        if let Some(before) = &c.tails_before {
            // The structure of shedding (testing §3, "Timing rules"): a shed call claims no
            // slot and publishes nothing, so the rings moved by exactly the calls that got
            // through admission. Run only with nothing else calling.
            let after = self.ring_tails()?;
            let moved: u64 = after.iter().zip(before).map(|(a, b)| a.wrapping_sub(*b)).sum();
            let mut want = unwound as u64;
            for (r, k) in &seen {
                want += publishes(r).ok_or(format!("step {step}: count_publishes cannot account for {r}"))? * *k as u64;
            }
            if moved != want {
                return Err(format!("step {step}: the rings moved by {moved}, the outcomes {seen:?} account for {want}"));
            }
        }
        let a = self.agent(name)?;
        a.unwinds -= unwound.min(a.unwinds);
        for (r, k) in &seen {
            if r == "MODEL" {
                a.model += *k as u64;
            } else {
                *a.fallbacks.entry(r.clone()).or_default() += *k as u64;
            }
        }
        self.out.calls += n as usize;
        self.trail.push(format!("step {step}: {seen:?}{}", if unwound > 0 { format!(" and {unwound} unwound") } else { String::new() }));
        for (r, k) in seen {
            *self.out.seen.entry(r).or_default() += k;
        }
        Ok(())
    }

    // ── waiting

    fn wait_until(&self, what: &str, timeout: Duration, f: impl Fn(&Self) -> bool) -> Result<(), String> {
        let t = Instant::now();
        while !f(self) {
            if t.elapsed() > timeout {
                return Err(format!("timed out after {timeout:?} waiting for {what}; status {}", self.status()));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn healthy(&self) -> bool {
        json_str(&self.status(), "state").as_deref() == Some("HEALTHY")
    }

    // ── steps

    fn step(&mut self, i: usize, t: &toml::Table) -> Result<(), String> {
        let what = string(t, "do", "")?;
        let name = string(t, "agent", "a")?;
        let ms = |d: u64| int(t, "ms", d).map(Duration::from_millis);
        let keys: &[&str] = match what.as_str() {
            "call" => &["count", "budget_us", "rows", "op", "arg", "seed", "expect", "reasons", "need", "background", "count_publishes", "hostile"],
            "join" | "close" | "start_sidecar" => &[],
            "bound" => &["ms", "expect"],
            "wait" => &["ms", "healthy", "stat", "min", "max"],
            "signal" => &["target", "sig"],
            "ctl" => &["cmd"],
            "sleep" => &["ms"],
            "codec_fault" => &["mode"],
            "config" => &["threads", "fallback_us", "kind", "codec", "fallback", "spin"],
            "unwind" | "spawn" => &[],
            "clock_skew" => &["ns"],
            "fork" => &["reasons"],
            "start_agent" => &["crash"],
            "agent_died" => &["ms"],
            "restart_sidecar" => &["ms"],
            "agent_stat" => &["stat", "min", "max"],
            "proc" => &["maps"],
            "write" => &["path", "text"],
            "copy" => &["from", "to"],
            _ => return Err(format!("step {i}: unknown do = {what:?}")),
        };
        let mut known = vec!["do", "agent"];
        known.extend_from_slice(keys);
        check_keys(t, &format!("step {i}"), &known)?;
        self.trail.push(format!("step {i}: {what}"));
        match what.as_str() {
            "call" => {
                let c = self.call_spec(t, i)?;
                let bg = boolean(t, "background", false)?;
                self.start_call(&name, c, i)?;
                if !bg {
                    self.finish_call(&name)?;
                }
            }
            "join" => self.finish_call(&name)?,
            "bound" => {
                let want = boolean(t, "expect", true)?;
                let r = self.ask(&name, &format!("BOUND ms={}", ms(10_000)?.as_millis()), "BOUND", ms(10_000)? + Duration::from_secs(5))?;
                if r != format!("BOUND {want}") {
                    return Err(format!("step {i}: {r}, expected BOUND {want}; status {}", self.status()));
                }
                let a = self.agent(&name)?;
                if want && a.baseline.is_none() {
                    let p = self.proc(&name)?;
                    self.agent(&name)?.baseline = Some(p);
                }
            }
            "wait" => {
                let healthy = boolean(t, "healthy", false)?;
                let stat = string(t, "stat", "")?;
                let (min, max) = (int(t, "min", 0)? as f64, int(t, "max", u64::MAX)? as f64);
                self.wait_until(&format!("{t}"), ms(10_000)?, |r| {
                    let s = r.status();
                    (!healthy || json_str(&s, "state").as_deref() == Some("HEALTHY"))
                        && (stat.is_empty() || json_num(&s, &stat).is_some_and(|v| v >= min && v <= max))
                })?;
            }
            "signal" => {
                let sig = string(t, "sig", "KILL")?;
                let pid = match string(t, "target", "active")?.as_str() {
                    "supervisor" => self.sup_pid()?,
                    "active" => self.worker_pid(self.gen_in_role("Consumer")?)?,
                    "standby" => self.worker_pid(self.gen_in_role("Warm")?)?,
                    "agent" => {
                        let a = self.agent(&name)?;
                        a.doomed |= sig == "KILL";
                        a.child.id()
                    }
                    x => return Err(format!("step {i}: unknown target {x}")),
                };
                signal(pid, &sig)?;
            }
            "ctl" => {
                self.sczctl(&string(t, "cmd", "status")?)?;
            }
            "sleep" => std::thread::sleep(ms(0)?),
            "codec_fault" => {
                let m = string(t, "mode", "off")?;
                self.ask(&name, &format!("CODEC_FAULT mode={m}"), "OK", Duration::from_secs(5))?;
            }
            "config" => {
                let a = self.agent(&name)?;
                a.threads = int(t, "threads", a.threads)?;
                a.fallback_us = int(t, "fallback_us", a.fallback_us)?;
                a.config = AgentConfig::read(t, &a.config)?;
                self.configure(&name)?;
            }
            "unwind" => {
                self.ask(&name, "UNWIND", "OK", Duration::from_secs(5))?;
                self.agent(&name)?.unwinds += 1;
            }
            "clock_skew" => {
                let ns = t.get("ns").and_then(toml::Value::as_integer).ok_or(format!("step {i}: clock_skew needs ns"))?;
                self.ask(&name, &format!("CLOCK_SKEW ns={ns}"), "OK", Duration::from_secs(5))?;
            }
            "fork" => {
                // A forked child makes one call and exits; it must fall back (it is not the
                // session's process) and must not disturb the parent's session.
                let mut reasons = strs(t, "reasons")?;
                if reasons.is_empty() {
                    reasons.push("NOT_CONNECTED".into());
                }
                let r = self.ask(&name, "FORK", "FORK ", Duration::from_secs(10))?;
                let f: BTreeMap<&str, &str> = r.split_whitespace().filter_map(|w| w.split_once('=')).collect();
                let ok = f.get("src") == Some(&"fallback") && f.get("reason").is_some_and(|x| reasons.iter().any(|r| r == x)) && f.get("status") == Some(&"0");
                if !ok {
                    return Err(format!("step {i}: {r}, expected src=fallback with one of {reasons:?} and status=0"));
                }
            }
            "spawn" => {
                let r = self.ask(&name, "SPAWN", "SPAWNED pid=", Duration::from_secs(10))?;
                let pid = r["SPAWNED pid=".len()..].trim().parse().map_err(|_| format!("step {i}: {r}"))?;
                self.agent(&name)?.spawned.push(pid);
            }
            "close" => {
                self.ask(&name, "CLOSE", "CLOSED", Duration::from_secs(10))?;
                self.agent(&name)?.closed = true;
            }
            "start_agent" => self.start_agent(&name, boolean(t, "crash", false)?)?,
            "start_sidecar" => self.start_sidecar()?,
            "agent_died" => {
                let limit = ms(10_000)?;
                let a = self.agent(&name)?;
                let t0 = Instant::now();
                let st = loop {
                    if let Some(st) = a.child.try_wait().map_err(|e| e.to_string())? {
                        break st;
                    }
                    if t0.elapsed() > limit {
                        return Err(format!("step {i}: agent {name} is still running"));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                };
                use std::os::unix::process::ExitStatusExt;
                if st.signal() != Some(libc::SIGKILL) {
                    return Err(format!("step {i}: agent {name} ended with {st}, not at a crash point"));
                }
                a.pending = None;
                a.doomed = true;
            }
            "restart_sidecar" => {
                if let Some(mut c) = self.sup.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                std::thread::sleep(ms(0)?);
                self.start_sidecar()?;
            }
            "agent_stat" => {
                let stat = string(t, "stat", "")?;
                let r = self.ask(&name, "STATS", "STATS", Duration::from_secs(5))?;
                let v = r.split_whitespace().find_map(|w| w.strip_prefix(&format!("{stat}="))).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let (min, max) = (int(t, "min", 0)?, int(t, "max", u64::MAX)?);
                if v < min || v > max {
                    return Err(format!("step {i}: agent stat {stat} = {v}, expected {min}..={max} ({r})"));
                }
            }
            "write" => {
                // A file in the scenario's directory, replaced whole: fake cgroup files for the
                // memory monitor (`--cgroup={dir}/cgroup`).
                let rel = string(t, "path", "")?;
                if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == "..") {
                    return Err(format!("step {i}: path must be relative to the scenario directory"));
                }
                let dest = self.dir.join(&rel);
                std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
                let tmp = dest.with_extension("tmp");
                std::fs::write(&tmp, string(t, "text", "")?).and_then(|_| std::fs::rename(&tmp, &dest)).map_err(|e| format!("step {i}: {rel}: {e}"))?;
            }
            "copy" => {
                // Replace a file in the scenario's directory with another, by rename: a model
                // swapped the way an operator swaps one (`{models}` names the test models).
                let rel = string(t, "to", "")?;
                if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == "..") {
                    return Err(format!("step {i}: to must be relative to the scenario directory"));
                }
                let mut from = string(t, "from", "")?;
                if from.contains("{models}") {
                    from = from.replace("{models}", &models_dir(&self.bins.dir)?);
                }
                let from = from.replace("{dir}", &self.dir.display().to_string());
                let dest = self.dir.join(&rel);
                let tmp = dest.with_extension("tmp");
                std::fs::copy(&from, &tmp).and_then(|_| std::fs::rename(&tmp, &dest)).map_err(|e| format!("step {i}: {from} → {rel}: {e}"))?;
            }
            "proc" => {
                let p = self.proc(&name)?;
                let maps = p.split_whitespace().find_map(|w| w.strip_prefix("maps=")).and_then(|v| v.parse::<u64>().ok());
                if let Some(want) = t.get("maps").map(|_| int(t, "maps", 0)).transpose()? {
                    if maps != Some(want) {
                        return Err(format!("step {i}: {p}, expected maps={want}"));
                    }
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    // ── the universal assertions (testing §3)

    fn finish(&mut self) -> Result<(), String> {
        let names: Vec<String> = self.agents.keys().cloned().collect();
        for n in &names {
            if !self.agents[n].doomed {
                self.finish_call(n)?;
            }
        }
        // 6. The supervisor did not exit.
        let sup = self.sup.as_mut().ok_or("no supervisor at the end")?;
        if let Some(st) = sup.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("the supervisor exited ({st})"));
        }
        self.wait_until("the gate to open at the end", Duration::from_secs(10), Self::healthy)?;
        for n in &names {
            let a = self.agents.get_mut(n).unwrap();
            if a.doomed {
                continue;
            }
            // 5. The agent did not crash.
            if let Some(st) = a.child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("agent {n} exited ({st})"));
            }
            if a.closed {
                continue;
            }
            // 7. Its counters agree with the outcomes printed.
            let r = self.ask(n, "STATS", "STATS", Duration::from_secs(5))?;
            let a = &self.agents[n];
            let mut want = format!("model={}", a.model);
            let got: BTreeMap<&str, u64> = r.split_whitespace().filter_map(|w| w.split_once('=')).filter_map(|(k, v)| Some((k, v.parse().ok()?))).collect();
            let mut ok = got.get("model").copied().unwrap_or(0) == a.model;
            for (reason, k) in &a.fallbacks {
                want += &format!(" fb.{reason}={k}");
                ok &= got.get(format!("fb.{reason}").as_str()).copied().unwrap_or(0) == *k;
            }
            let extra = got.iter().filter(|(k, _)| k.starts_with("fb.")).count() != a.fallbacks.len();
            if !ok || extra {
                return Err(format!("agent {n}: counters {r} disagree with the outcomes seen ({want})"));
            }
            // 8. No late wake-ups unless the scenario stalls them. A hard failure only when
            // asked for (dedicated runners); on shared ones it is reported (testing §3).
            let late = got.get("late_wakes").copied().unwrap_or(0);
            self.out.late_wakes += late;
            if late > 0 && !self.sc.stall && std::env::var("SCZ_STRICT_WAKES").is_ok_and(|v| v == "1") {
                return Err(format!("agent {n}: {late} late wake-ups"));
            }
            // 5. Descriptors, mappings, and threads are back where they started. Retiring an
            // old arena waits for the client's background thread, so allow it a moment.
            if let Some(base) = self.agents[n].baseline.clone() {
                let t = Instant::now();
                loop {
                    let p = self.proc(n)?;
                    if p == base {
                        break;
                    }
                    if t.elapsed() > Duration::from_secs(3) {
                        return Err(format!("agent {n}: {p} at the end, {base} at the start"));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        // 4. The arena passes the invariant check. A slot abandoned by its caller is freed by
        // the worker or the sweep, after the call returned: allow that a moment too.
        let t = Instant::now();
        loop {
            let r = self.sczctl("dump")?;
            if r.contains("\"ok\":true") {
                break;
            }
            if t.elapsed() > Duration::from_secs(3) {
                return Err(format!("the arena check failed at the end: {}", r.trim()));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The scenario's coverage claims hold.
        for r in &self.sc.covers {
            if !self.out.seen.contains_key(r) {
                return Err(format!("covers {r}, but no call fell back with it (saw {:?})", self.out.seen));
            }
        }
        Ok(())
    }

    fn cleanup(&mut self) {
        let names: Vec<String> = self.agents.keys().cloned().collect();
        for n in names {
            let a = self.agents.get_mut(&n).unwrap();
            for pid in a.spawned.drain(..) {
                let _ = signal(pid, "KILL");
            }
            if a.child.try_wait().ok().flatten().is_none() {
                let _ = writeln!(a.stdin, "EXIT");
                let t = Instant::now();
                while a.child.try_wait().ok().flatten().is_none() && t.elapsed() < Duration::from_secs(2) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = a.child.kill();
                let _ = a.child.wait();
            }
        }
        if let Some(mut c) = self.sup.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The test models (models/gen.py), generated next to the binaries' target directory on first
/// use: what `{models}` in a sidecar argument names.
fn models_dir(bins: &Path) -> Result<String, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = bins.parent().map(|t| t.join("models")).ok_or("no target directory")?;
    let ok = Command::new("python3").arg(root.join("models/gen.py")).arg(&dir).status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        return Err("models/gen.py failed".into());
    }
    Ok(dir.display().to_string())
}

/// Run one scenario file.
pub fn run_file(path: &Path, bins: &Bins) -> Result<Outcome, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sc = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    run(&sc, bins, path)
}

fn run(sc: &Scenario, bins: &Bins, path: &Path) -> Result<Outcome, String> {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!("scz-conf-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = if sc.name.is_empty() { path.file_stem().unwrap_or_default().to_string_lossy().to_string() } else { sc.name.clone() };
    let mut r = Run {
        sc,
        bins,
        sock: dir.join("scz.sock"),
        admin: dir.join("admin.sock"),
        dir: dir.clone(),
        sup: None,
        agents: BTreeMap::new(),
        out: Outcome { name, crash_at: sc.crash_at.clone(), ..Default::default() },
        trail: vec![],
    };
    let t = Instant::now();
    let res = (|| {
        if sc.start_sidecar {
            r.start_sidecar()?;
        }
        if sc.start_agent {
            r.start_agent("a", false)?;
        }
        for (i, s) in sc.steps.iter().enumerate() {
            r.step(i, s)?;
        }
        r.finish()
    })();
    r.cleanup();
    r.out.elapsed = t.elapsed();
    let res = match res {
        Err(e) if e.starts_with(SKIP) => {
            r.out.skipped = Some(e[SKIP.len()..].to_string());
            Ok(())
        }
        x => x,
    };
    match res {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&dir);
            r.out.trail = std::mem::take(&mut r.trail);
            Ok(std::mem::take(&mut r.out))
        }
        Err(e) => {
            let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
            let tail = |s: String| s.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            let agents: String = r.agents.keys().map(|n| format!("\n--- agent {n} stderr ---\n{}", tail(read(&format!("agent-{n}.log"))))).collect();
            let _ = std::fs::remove_dir_all(&dir);
            Err(format!("{}: {e}\n--- trail ---\n{}\n--- supervisor log ---\n{}{agents}", r.out.name, r.trail.join("\n"), tail(read("supervisor.log"))))
        }
    }
}

/// The scenario files under `dir`, sorted.
pub fn scenarios(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// The `crash_at` of a scenario file, without running it.
pub fn crash_at(path: &Path) -> Option<String> {
    parse(&std::fs::read_to_string(path).ok()?).ok()?.crash_at
}

/// Every fallback reason the Rust SDK defines: what coverage is measured against.
pub fn all_reasons() -> Vec<String> {
    sidecarzero::FallbackReason::ALL.iter().map(|r| r.as_str().to_string()).collect()
}

/// A scenario's name in a skips file: its file stem, or `crash/<stem>` for a crash-point one.
pub fn scenario_key(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    match path.parent().and_then(|p| p.file_name()) {
        Some(d) if d == "crash" => format!("crash/{stem}"),
        _ => stem,
    }
}

fn conformance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn toml_file(p: &Path) -> Result<toml::Table, String> {
    match std::fs::read_to_string(p) {
        Ok(t) => t.parse().map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

fn keys_of(t: &toml::Table, k: &str) -> BTreeSet<String> {
    t.get(k).and_then(toml::Value::as_table).map(|t| t.keys().cloned().collect()).unwrap_or_default()
}

/// The suite-level checks of testing §3 over one agent's results, keyed by [`scenario_key`]:
///
/// - every skipped scenario is listed, with its reason, in `conformance/skips/<sdk>.toml`, and
///   every scenario listed there that ran was skipped (so a scenario is never skipped silently);
/// - with `full` (every scenario ran): every fallback reason the SDKs define is produced by some
///   scenario, or exempted in `conformance/coverage.toml` (`[exempt]` for every SDK,
///   `[sdk.<sdk>]` for one); an exempted reason that is produced is an error; every chaos
///   opcode is used; and every skips entry names a scenario that ran.
pub fn check_suite(results: &[(String, Outcome)], sdk: &str, full: bool) -> Vec<String> {
    let mut problems = vec![];
    let skips_path = conformance_dir().join("skips").join(format!("{sdk}.toml"));
    let skips = match toml_file(&skips_path) {
        Ok(t) => keys_of(&t, "skip"),
        Err(e) => return vec![e],
    };
    for (key, o) in results {
        match (&o.skipped, skips.contains(key)) {
            (Some(why), false) => problems.push(format!("{key}: skipped ({why}), and not listed in conformance/skips/{sdk}.toml")),
            (None, true) => problems.push(format!("{key}: listed in conformance/skips/{sdk}.toml, but it ran")),
            _ => {}
        }
    }
    if !full {
        return problems;
    }
    let crash_ran = results.iter().any(|(k, _)| k.starts_with("crash/"));
    for k in &skips {
        if (crash_ran || !k.starts_with("crash/")) && !results.iter().any(|(r, _)| r == k) {
            problems.push(format!("conformance/skips/{sdk}.toml lists {k}, which is not a scenario"));
        }
    }
    let cov = match toml_file(&conformance_dir().join("coverage.toml")) {
        Ok(t) => t,
        Err(e) => return vec![e],
    };
    let mut exempt = keys_of(&cov, "exempt");
    exempt.extend(cov.get("sdk").and_then(toml::Value::as_table).map(|t| keys_of(t, sdk)).unwrap_or_default());
    let seen: BTreeSet<String> = results.iter().flat_map(|(_, o)| o.seen.keys().cloned()).collect();
    let all: BTreeSet<String> = all_reasons().into_iter().collect();
    let missing: Vec<&String> = all.iter().filter(|r| !seen.contains(*r) && !exempt.contains(*r)).collect();
    if !missing.is_empty() {
        problems.push(format!("fallback reasons no scenario produced for {sdk} and coverage.toml does not exempt: {missing:?}"));
    }
    let stale: Vec<&String> = exempt.iter().filter(|r| seen.contains(*r) || !all.contains(*r)).collect();
    if !stale.is_empty() {
        problems.push(format!("coverage.toml exempts, for {sdk}, reasons that were produced or do not exist: {stale:?}"));
    }
    let ops: BTreeSet<&String> = results.iter().flat_map(|(_, o)| o.ops.iter()).collect();
    let unused: Vec<&str> = proto::op::ALL.iter().map(|x| x.0).filter(|o| !ops.iter().any(|u| u.as_str() == *o)).collect();
    if !unused.is_empty() {
        problems.push(format!("chaos opcodes no scenario used: {unused:?}"));
    }
    problems
}
