//! The OOM shield (kubernetes-deploy §5; supervisor §3 step 3): what keeps PID 1, and if
//! possible the standby, alive when the kernel's OOM killer runs in this container.
//!
//! | Mode | How | Settled when |
//! |---|---|---|
//! | `singleProcess` | The kubelet's `singleProcessOOMKill`: the kernel kills one process, the worker | The container's `memory.oom.group` reads `0` |
//! | `nestedCgroup` | The supervisor moves itself into a child cgroup and gives each beat slot's worker a cgroup of its own with its own `memory.max`, so a runaway worker hits its own limit and only it dies | The container's cgroup is writable cgroup v2 (a delegated subtree, or a privileged container) |
//! | `oomScoreAdj` | `oom_score_adj = -1000` on the supervisor, which a group kill spares; the standby dies with the worker | Asked for explicitly, and the write succeeds (it needs `CAP_SYS_RESOURCE` in effect) |
//! | `none` | Layer 0 only: the memory monitor's relief ladder | Otherwise |
//!
//! `auto` (the default) takes `singleProcess` if it is in effect, then tries `nestedCgroup`,
//! then settles for `none`. It never takes `oomScoreAdj`. The effective mode is logged and
//! exported as `scz_oom_shield_mode`.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asked {
    Auto,
    SingleProcess,
    NestedCgroup,
    OomScoreAdj,
    None,
}

impl Asked {
    pub fn parse(s: &str) -> Option<Asked> {
        Some(match s {
            "auto" => Asked::Auto,
            "singleProcess" => Asked::SingleProcess,
            "nestedCgroup" => Asked::NestedCgroup,
            "oomScoreAdj" => Asked::OomScoreAdj,
            "none" => Asked::None,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    None,
    SingleProcess,
    NestedCgroup,
    OomScoreAdj,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::SingleProcess => "singleProcess",
            Mode::NestedCgroup => "nestedCgroup",
            Mode::OomScoreAdj => "oomScoreAdj",
        }
    }
}

/// The settled shield: the mode, why, and in `nestedCgroup` the cgroup of each beat slot.
#[derive(Debug)]
pub struct Shield {
    pub mode: Mode,
    pub note: String,
    pub workers: Option<[PathBuf; 2]>,
}

/// What the metrics need, settled once at startup: the mode and the container's cgroup.
pub static SETTLED: OnceLock<(Mode, Option<PathBuf>)> = OnceLock::new();

/// This container's own cgroup directory: from `/proc/self/cgroup`, never `/sys/fs/cgroup`
/// itself, which in a privileged container is the node's whole tree (spike S2).
pub fn own_cgroup() -> Result<PathBuf, String> {
    let own = std::fs::read_to_string("/proc/self/cgroup").map_err(|e| format!("/proc/self/cgroup: {e}"))?;
    let rel = own.lines().find_map(|l| l.strip_prefix("0::")).ok_or("not on cgroup v2")?;
    Ok(Path::new("/sys/fs/cgroup").join(rel.trim().trim_start_matches('/')))
}

const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;

/// Whether `dir` is on a cgroup v2 filesystem (a test's fake files are not).
fn is_cgroup2(dir: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else { return false };
    // SAFETY: statfs fills a plain struct we own.
    let mut s: libc::statfs = unsafe { core::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut s) }) == 0 && s.f_type as libc::c_long == CGROUP2_SUPER_MAGIC
}

fn write(p: &Path, v: &str) -> Result<(), String> {
    std::fs::write(p, v).map_err(|e| format!("{}: {e}", p.display()))
}

/// Settle the shield. `dir` is the container's cgroup (None without cgroup v2 files);
/// `worker_max` the `memory.max` each worker's cgroup gets in `nestedCgroup` (None: no limit).
pub fn settle(asked: Asked, dir: Option<&Path>, worker_max: Option<u64>) -> Shield {
    let none = |note: String| Shield { mode: Mode::None, note, workers: None };
    let group = dir.and_then(|d| std::fs::read_to_string(d.join("memory.oom.group")).ok()).map(|s| s.trim().to_string());
    let single = group.as_deref() == Some("0");
    match asked {
        Asked::None => none("as configured".into()),
        Asked::OomScoreAdj => match write(Path::new("/proc/self/oom_score_adj"), "-1000") {
            Ok(()) => Shield { mode: Mode::OomScoreAdj, note: "oom_score_adj -1000 on the supervisor; the standby is not protected".into(), workers: None },
            Err(e) => none(format!("oomScoreAdj was asked for and cannot be set ({e}): it needs CAP_SYS_RESOURCE in effect")),
        },
        Asked::SingleProcess if single => Shield { mode: Mode::SingleProcess, note: "memory.oom.group is 0".into(), workers: None },
        Asked::SingleProcess => none(format!("singleProcess was asked for, but memory.oom.group is {}", group.as_deref().unwrap_or("unreadable"))),
        Asked::Auto if single => Shield { mode: Mode::SingleProcess, note: "memory.oom.group is 0: the kubelet's singleProcessOOMKill".into(), workers: None },
        Asked::Auto | Asked::NestedCgroup => {
            let Some(dir) = dir else { return none("no cgroup v2 files".into()) };
            match nest(dir, worker_max) {
                Ok(w) => Shield {
                    mode: Mode::NestedCgroup,
                    note: format!(
                        "each worker in its own cgroup under {}, memory.max {}",
                        dir.display(),
                        worker_max.map_or("max".into(), |m| format!("{} MiB", m >> 20))
                    ),
                    workers: Some(w),
                },
                Err(e) => none(format!(
                    "{}nestedCgroup is not available: {e}",
                    if group.is_some() { format!("memory.oom.group is {}; ", group.as_deref().unwrap_or("")) } else { String::new() }
                )),
            }
        }
    }
}

/// `nestedCgroup`: in cgroup v2 a cgroup that hands the memory controller to its children may
/// hold no processes itself, so the supervisor first moves into a child of its own, then
/// enables the controller, then makes one cgroup per beat slot (kubernetes-deploy §5).
fn nest(dir: &Path, worker_max: Option<u64>) -> Result<[PathBuf; 2], String> {
    if !is_cgroup2(dir) {
        return Err(format!("{} is not a cgroup v2 directory", dir.display()));
    }
    let sup = dir.join("scz-supervisor");
    std::fs::create_dir(&sup)
        .or_else(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { Ok(()) } else { Err(e) })
        .map_err(|e| format!("{}: {e}", sup.display()))?;
    let undo = |why: String| {
        let _ = write(&dir.join("cgroup.procs"), &std::process::id().to_string());
        let _ = std::fs::remove_dir(&sup);
        why
    };
    write(&sup.join("cgroup.procs"), &std::process::id().to_string()).map_err(undo)?;
    write(&dir.join("cgroup.subtree_control"), "+memory").map_err(undo)?;
    let workers = [dir.join("scz-worker-0"), dir.join("scz-worker-1")];
    for w in &workers {
        std::fs::create_dir(w)
            .or_else(|e| if e.kind() == std::io::ErrorKind::AlreadyExists { Ok(()) } else { Err(e) })
            .map_err(|e| format!("{}: {e}", w.display()))?;
        write(&w.join("memory.max"), &worker_max.map_or("max".into(), |m| m.to_string()))?;
    }
    Ok(workers)
}

/// The `cgroup.procs` of beat slot `beat`'s cgroup, opened for the spawn stub, which writes
/// `0` into it to move the new worker there before `exec`.
pub fn worker_procs(sh: &Shield, beat: u32) -> std::io::Result<Option<OwnedFd>> {
    let Some(w) = &sh.workers else { return Ok(None) };
    let f = std::fs::OpenOptions::new().write(true).open(w[beat as usize % 2].join("cgroup.procs"))?;
    Ok(Some(f.into()))
}

/// `memory.events`' `oom_kill` and `oom_group_kill` in the container's cgroup, which count
/// what happened anywhere beneath it.
pub fn kernel_oom_events() -> Option<(u64, u64)> {
    let dir = SETTLED.get()?.1.as_ref()?;
    let s = std::fs::read_to_string(dir.join("memory.events")).ok()?;
    let get = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix(' ')?.trim().parse().ok()).unwrap_or(0);
    Some((get("oom_kill"), get("oom_group_kill")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_fake_cgroup_files_are_never_nested() {
        assert_eq!(Asked::parse("nestedCgroup"), Some(Asked::NestedCgroup));
        assert_eq!(Asked::parse("nested"), None);
        let d = std::env::temp_dir().join(format!("scz-shield-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("memory.oom.group"), "1\n").unwrap();
        // A plain directory is not cgroup v2: auto settles for none, and creates nothing.
        let s = settle(Asked::Auto, Some(&d), None);
        assert_eq!(s.mode, Mode::None, "{s:?}");
        assert!(!d.join("scz-supervisor").exists());
        std::fs::write(d.join("memory.oom.group"), "0\n").unwrap();
        assert_eq!(settle(Asked::Auto, Some(&d), None).mode, Mode::SingleProcess);
        assert_eq!(settle(Asked::None, Some(&d), None).mode, Mode::None);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
