//! The harness shared by the end-to-end tests: a real supervisor and worker processes, found
//! next to the test executable (build the workspace first; `cargo test --workspace` does).
#![allow(dead_code)]

use sidecarzero::Endpoint;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub fn bin(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let p = exe.parent().unwrap().parent().unwrap().join(name);
    assert!(p.exists(), "{} not built: run `cargo build --workspace` first", p.display());
    p
}

static N: AtomicU32 = AtomicU32::new(0);

pub struct Sidecar {
    pub child: Child,
    pub dir: PathBuf,
    pub sock: PathBuf,
    pub admin: PathBuf,
}

impl Sidecar {
    /// Start a supervisor running `worker` (a binary name), with extra arguments and
    /// environment. The worker heartbeat's stale limit is 100 ms unless an argument sets it:
    /// a loaded test machine stalls threads for tens of milliseconds.
    pub fn start(worker: &str, args: &[&str], env: &[(&str, &str)]) -> Sidecar {
        let dir = std::env::temp_dir().join(format!("scz-e2e-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        Self::start_in(dir, worker, args, env)
    }
    pub fn start_in(dir: PathBuf, worker: &str, args: &[&str], env: &[(&str, &str)]) -> Sidecar {
        let sock = dir.join("scz.sock");
        let admin = dir.join("admin.sock");
        let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("supervisor.log")).unwrap();
        let mut c = Command::new(bin("scz-supervisor"));
        c.arg(format!("--socket={}", sock.display())).arg(format!("--worker={}", bin(worker).display())).arg(format!("--admin={}", admin.display()));
        if !args.iter().any(|a| a.starts_with("--hb-stale-ms")) {
            c.arg("--hb-stale-ms=100");
        }
        c.args(args).stderr(Stdio::from(log)).stdout(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        Sidecar { child: c.spawn().unwrap(), dir, sock, admin }
    }
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Path(self.sock.clone())
    }
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("supervisor.log")).unwrap_or_default()
    }
    /// `sczctl status`, as raw JSON text.
    pub fn status(&self) -> String {
        let out = Command::new(bin("sczctl")).arg(format!("--admin={}", self.admin.display())).arg("status").output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }
    pub fn ctl(&self, cmd: &str) {
        let out = Command::new(bin("sczctl")).arg(format!("--admin={}", self.admin.display())).arg(cmd).output().unwrap();
        assert!(out.status.success(), "sczctl {cmd} failed");
    }
    /// A top-level number from the status JSON, by key. The arrays (workers, transitions) are
    /// left out first: their entries reuse key names such as `condemned` and `gen`.
    pub fn stat(&self, key: &str) -> f64 {
        let mut depth = 0;
        let s: String = self
            .status()
            .chars()
            .filter(|&c| {
                depth += i32::from(c == '[') - i32::from(c == ']');
                depth == 0 && c != ']'
            })
            .collect();
        let k = format!("\"{key}\":");
        let Some(i) = s.find(&k) else { return f64::NAN };
        s[i + k.len()..].split([',', '}']).next().unwrap().trim_matches('"').parse().unwrap_or(f64::NAN)
    }
    pub fn gate(&self) -> String {
        let s = self.status();
        s.split("\"state\":\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("").to_string()
    }
    /// The pid of the worker of generation `gen`, from the children's command lines.
    pub fn worker_pid(&self, gen: u32) -> Option<i32> {
        let id = self.child.id();
        let kids = std::fs::read_to_string(format!("/proc/{id}/task/{id}/children")).unwrap_or_default();
        kids.split_whitespace()
            .filter_map(|p| p.parse::<i32>().ok())
            .find(|p| std::fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).split('\0').any(|a| a == format!("--gen={gen}"))))
    }
    /// The generation the gate names: the active worker.
    pub fn active_gen(&self) -> u32 {
        self.stat("gen") as u32
    }
    pub fn wait_for(&self, what: &str, timeout: Duration, f: impl Fn(&Sidecar) -> bool) {
        let t = Instant::now();
        while !f(self) {
            assert!(t.elapsed() < timeout, "timed out waiting for {what}\n{}\n{}", self.status(), self.log());
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    pub fn signal(&self, sig: &str) {
        assert!(Command::new("kill").arg(format!("-{sig}")).arg(self.child.id().to_string()).status().unwrap().success());
    }
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.kill();
        if std::thread::panicking() {
            eprintln!("--- supervisor log ---\n{}", self.log());
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The generated test models (models/gen.py), made on first use.
pub fn models() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("target/models");
    let ok = Command::new("python3").arg(root.join("models/gen.py")).arg(&dir).status().map(|s| s.success()).unwrap_or(false);
    assert!(ok, "models/gen.py failed: it needs the dev image's Python packages (scripts/dev)");
    dir
}

/// A fresh directory for one sidecar.
pub fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scz-e2e-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Put `src`'s bytes at `dst` as a new file, renamed into place: how a model is published.
pub fn publish(src: &std::path::Path, dst: &std::path::Path) {
    let tmp = dst.with_extension("tmp");
    std::fs::copy(src, &tmp).unwrap();
    std::fs::rename(&tmp, dst).unwrap();
}

pub fn publish_bytes(bytes: &[u8], dst: &std::path::Path) {
    let tmp = dst.with_extension("tmp");
    std::fs::write(&tmp, bytes).unwrap();
    std::fs::rename(&tmp, dst).unwrap();
}

impl Sidecar {
    /// `sczctl dump`: the arena check's report, then the workers' counters.
    pub fn dump(&self) -> String {
        let out = Command::new(bin("sczctl")).arg(format!("--admin={}", self.admin.display())).arg("dump").output().unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }
    /// A worker counter from the dump, summed over both beat slots.
    pub fn worker_stat(&self, key: &str) -> u64 {
        let d = self.dump();
        let line = d.lines().find(|l| l.contains("worker_stats")).unwrap_or("");
        line.split(&format!("\"{key}\":")).skip(1).filter_map(|r| r.split([',', '}']).next()?.parse::<u64>().ok()).sum()
    }
    pub fn status_str(&self, key: &str) -> String {
        let s = self.status();
        s.split(&format!("\"{key}\":\"")).nth(1).and_then(|r| r.split('"').next()).unwrap_or("").to_string()
    }
}

pub fn signal_pid(pid: i32, sig: &str) {
    assert!(Command::new("kill").arg(format!("-{sig}")).arg(pid.to_string()).status().unwrap().success());
}
