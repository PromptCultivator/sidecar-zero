//! The cgroup v2 readings behind the memory monitor (supervisor §6), taken every 10 ms.
//!
//! The container's own cgroup is found from /proc/self/cgroup: a privileged container sees the
//! node's whole tree at /sys/fs/cgroup (spike S2). `--cgroup=DIR` points the reader at another
//! directory, which is how tests drive the monitor deterministically (supervisor §12).
//!
//! The poll is most of what an idle supervisor costs, so the files it reads every 10 ms are
//! kept open and read again from offset zero, one `pread` each, where that sees new contents:
//! on the cgroup filesystem and on procfs every read regenerates the file. A directory
//! elsewhere (the tests' fake files, replaced by rename) is opened afresh each time.

use crate::machine::Sample;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub struct Monitor {
    dir: PathBuf,
    psi: bool,
    max: Option<u64>,
    max_read_ns: u64,
    throttled: Option<u64>,
    page: u64,
    /// Files read every poll, kept open when `held` (the cgroup filesystem).
    held: bool,
    files: HashMap<&'static str, File>,
    /// Each worker's /proc/PID/statm, by generation (a pid can be reused, a generation is
    /// not): procfs regenerates it on every read.
    statm: HashMap<u32, (i32, File)>,
    buf: Vec<u8>,
}

fn read_u64(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn parse_u64(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

/// A "key value" line from a flat-keyed file such as memory.stat or cpu.stat.
fn keyed(s: &str, key: &str) -> Option<u64> {
    s.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
}

/// The `total=` of the `some` and `full` lines of memory.pressure.
fn psi(s: &str) -> Option<(u64, u64)> {
    let total = |kind: &str| s.lines().find(|l| l.starts_with(kind))?.split_whitespace().find_map(|f| f.strip_prefix("total="))?.parse().ok();
    Some((total("some")?, total("full")?))
}

/// Whether `dir` is on the cgroup v2 filesystem.
fn on_cgroupfs(dir: &Path) -> bool {
    const CGROUP2_SUPER_MAGIC: i64 = 0x6367_7270;
    let Ok(c) = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(dir.as_os_str())) else { return false };
    // SAFETY: a valid C string and a statfs the kernel fills in.
    let mut st: libc::statfs = unsafe { core::mem::zeroed() };
    let ok = unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0;
    ok && st.f_type as i64 == CGROUP2_SUPER_MAGIC
}

/// The whole of a kept-open file, read again from its start into `buf`.
fn pread_all<'b>(f: &File, buf: &'b mut Vec<u8>) -> Option<&'b str> {
    buf.clear();
    let mut off = 0usize;
    loop {
        if buf.len() < off + 4096 {
            buf.resize(off + 4096, 0);
        }
        let n = f.read_at(&mut buf[off..], off as u64).ok()?;
        off += n;
        if n == 0 || off < buf.len() {
            break;
        }
    }
    buf.truncate(off);
    std::str::from_utf8(buf).ok()
}

impl Monitor {
    /// The container's cgroup, or `dir` if given. None, with the reason, when there is nothing
    /// to read: the supervisor then serves with the monitor off (a degraded start, §3).
    pub fn open(dir: Option<PathBuf>) -> Result<Monitor, String> {
        let dir = match dir {
            Some(d) => d,
            None => {
                let own = std::fs::read_to_string("/proc/self/cgroup").map_err(|e| format!("/proc/self/cgroup: {e}"))?;
                let rel = own.lines().find_map(|l| l.strip_prefix("0::")).ok_or("not on cgroup v2")?;
                Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'))
            }
        };
        if read_u64(&dir.join("memory.current")).is_none() {
            return Err(format!("{}: no memory.current", dir.display()));
        }
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        let held = on_cgroupfs(&dir);
        let mut m =
            Monitor { dir, psi: false, max: None, max_read_ns: 0, throttled: None, page, held, files: HashMap::new(), statm: HashMap::new(), buf: Vec::new() };
        m.psi = m.read("memory.pressure").and_then(psi).is_some();
        m.max = m.read_max();
        Ok(m)
    }
    /// A file of the cgroup directory, from a kept descriptor where that is safe.
    fn read(&mut self, name: &'static str) -> Option<&str> {
        if !self.held {
            let s = std::fs::read(self.dir.join(name)).ok()?;
            self.buf = s;
            return std::str::from_utf8(&self.buf).ok();
        }
        if !self.files.contains_key(name) {
            let f = File::open(self.dir.join(name)).ok()?;
            self.files.insert(name, f);
        }
        let f = self.files.get(name)?;
        pread_all(f, &mut self.buf)
    }
    pub fn describe(&self) -> String {
        format!(
            "{}: limit {}, PSI {}",
            self.dir.display(),
            self.max.map(|m| format!("{} MiB", m >> 20)).unwrap_or_else(|| "none (utilization rules off)".into()),
            if self.psi { "on" } else { "off" }
        )
    }
    pub fn limit(&self) -> Option<u64> {
        self.max
    }
    fn read_max(&mut self) -> Option<u64> {
        self.read("memory.max").and_then(parse_u64) // "max" parses as None
    }
    pub fn sample(&mut self, now: u64, workers: &[(u32, i32)]) -> Option<Sample> {
        let current = self.read("memory.current").and_then(parse_u64)?;
        if now.saturating_sub(self.max_read_ns) >= 1_000_000_000 {
            self.max = self.read_max(); // follows an in-place pod resize
            self.max_read_ns = now;
        }
        // inactive_file only once utilization passes 70%: memory.stat is a larger file.
        let inactive_file = match self.max {
            Some(max) if current as f64 > 0.7 * max as f64 => self.read("memory.stat").and_then(|s| keyed(s, "inactive_file")).unwrap_or(0),
            _ => 0,
        };
        let (psi_some_us, psi_full_us) = match if self.psi { self.read("memory.pressure").and_then(psi) } else { None } {
            Some((s, f)) => (Some(s), Some(f)),
            None => (None, None),
        };
        // The workers' statm files, opened once per worker; one that has gone is dropped.
        self.statm.retain(|g, (p, _)| workers.contains(&(*g, *p)));
        let mut private = Vec::with_capacity(workers.len());
        for &(g, pid) in workers {
            if let std::collections::hash_map::Entry::Vacant(e) = self.statm.entry(g) {
                let Ok(f) = File::open(format!("/proc/{pid}/statm")) else { continue };
                e.insert((pid, f));
            }
            let Some(s) = self.statm.get(&g).and_then(|(_, f)| pread_all(f, &mut self.buf)) else { continue };
            let mut it = s.split_whitespace().skip(1).filter_map(|x| x.parse::<u64>().ok());
            if let (Some(resident), Some(shared)) = (it.next(), it.next()) {
                private.push((g, resident.saturating_sub(shared) * self.page));
            }
        }
        Some(Sample { current, max: self.max, inactive_file, psi_some_us, psi_full_us, private })
    }
    /// Whether `cpu.stat`'s `nr_throttled` advanced since the last call.
    pub fn throttled(&mut self) -> bool {
        let Some(n) = self.read("cpu.stat").and_then(|s| keyed(s, "nr_throttled")) else { return false };
        let before = self.throttled.replace(n);
        before.is_some_and(|b| n > b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kept descriptors see new contents: this process's private memory, read through a
    /// held /proc/PID/statm, grows when it touches 64 MiB; the cgroup files read back sensibly.
    #[test]
    fn held_files_are_read_afresh() {
        let Ok(mut m) = Monitor::open(None) else { return }; // no cgroup v2 here
        let me = (7, std::process::id() as i32);
        let a = m.sample(0, &[me]).expect("a first sample");
        let mut v = vec![0u8; 64 << 20];
        for i in (0..v.len()).step_by(4096) {
            v[i] = 1;
        }
        let b = m.sample(1, &[me]).expect("a second sample");
        assert!(a.current > 0 && b.current > 0);
        let (pa, pb) = (a.private[0].1, b.private[0].1);
        assert!(pb >= pa + (48 << 20), "private memory read again: {pa} then {pb}");
        assert!(m.held == on_cgroupfs(&m.dir) && m.statm.contains_key(&7));
        let _ = m.throttled();
        std::hint::black_box(&v);
        // A worker that has gone is dropped.
        m.sample(2, &[]);
        assert!(m.statm.is_empty());
    }

    #[test]
    fn a_directory_off_the_cgroup_filesystem_is_read_by_path() {
        let d = std::env::temp_dir().join(format!("scz-monitor-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let put = |name: &str, text: &str| {
            let tmp = d.join(format!("{name}.tmp"));
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, d.join(name)).unwrap(); // replaced whole, as the tests do
        };
        put("memory.current", "100");
        put("memory.max", "1000");
        let mut m = Monitor::open(Some(d.clone())).unwrap();
        assert!(!m.held);
        assert_eq!(m.sample(0, &[]).unwrap().current, 100);
        put("memory.current", "200");
        assert_eq!(m.sample(1, &[]).unwrap().current, 200, "a replaced file is seen");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
