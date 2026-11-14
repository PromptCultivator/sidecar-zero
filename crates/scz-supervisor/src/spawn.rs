//! Starting a worker or a helper (supervisor §5.1, worker §6).
//!
//! Everything the child needs (argument strings, the empty signal mask, the descriptor
//! numbers, the seccomp program, the Landlock ruleset) is prepared before the clone, and the
//! child runs only async-signal-safe system calls on it, then `execve`. The clone is `clone3`
//! with `CLONE_PIDFD`, or `clone` with it where a seccomp profile answers `clone3` with
//! `ENOSYS` (spike S7), so the pidfd exists before the child can run at all.

use crate::sandbox;
use scz_shm::abi;
use scz_shm::arena::{add_seals, memfd, Mapping, OwnedArena, DATA_SEALS};
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The sandbox, prepared once and reused for every spawn.
pub struct Stub {
    filter: Option<Vec<libc::sock_filter>>,
    ruleset: Option<OwnedFd>,
    rlimit_data: Option<u64>,
    nofile: u64,
    core_dumps: bool,
    /// What `describe` reports: which layers are in force.
    pub layers: String,
}

impl Stub {
    pub fn new(cfg: &sandbox::Config) -> Stub {
        let filter = cfg.seccomp.then(|| sandbox::filter(cfg.allow_socketpair));
        let (ruleset, landlock) = match (cfg.landlock, sandbox::landlock_abi()) {
            (false, _) => (None, "off".to_string()),
            (true, Err(e)) => (None, format!("unavailable ({e})")),
            (true, Ok(abi)) => match sandbox::ruleset(&cfg.read, abi) {
                Ok(fd) => (Some(fd), format!("ABI {abi}")),
                Err(e) => (None, format!("failed ({e})")),
            },
        };
        let layers = format!(
            "seccomp {}, landlock {landlock}, RLIMIT_DATA {}, RLIMIT_NOFILE {}, core dumps {}",
            if filter.is_some() { "on" } else { "off" },
            cfg.rlimit_data.map(|b| format!("{} MiB", b >> 20)).unwrap_or_else(|| "unset".into()),
            cfg.nofile,
            if cfg.core_dumps { "on" } else { "off" },
        );
        Stub { filter, ruleset, rlimit_data: cfg.rlimit_data, nofile: cfg.nofile, core_dumps: cfg.core_dumps, layers }
    }
}

pub struct Spawned {
    pub pid: libc::pid_t,
    pub pidfd: OwnedFd,
    /// Read end of the status pipe, non-blocking.
    pub status: OwnedFd,
    /// The worker segment (shm-abi §2), where its consume threads' death locks live. Shared
    /// with the watcher threads, so it stays mapped until the last of them is done.
    pub wrk: std::sync::Arc<Mapping>,
    pub beat: u32,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn cstr(s: impl AsRef<OsStr>) -> CString {
    CString::new(s.as_ref().as_bytes()).expect("no NUL in arguments")
}

/// A copy of `fd` numbered 64 or above, so that the child's `dup2` onto 3, 4, … cannot
/// overwrite a source it has yet to copy.
fn high_dup(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor nothing else owns.
    let n = cvt(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 64) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0; 2];
    // SAFETY: a valid two-element out-array.
    cvt(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: pipe2 returned two new descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

#[repr(C)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
}

const CLONE_PIDFD: u64 = 0x1000;

/// Start `path` with `args`, the descriptors `srcs` as 3, 4, … and nothing else, inside the
/// sandbox. With `cgroup`, an open `cgroup.procs`, the child moves itself into that cgroup
/// before anything else (the `nestedCgroup` shield, kubernetes-deploy §5). Returns the pid and
/// its pidfd.
pub fn exec(stub: &Stub, path: &Path, args: &[String], srcs: &[RawFd], cgroup: Option<RawFd>) -> io::Result<(libc::pid_t, OwnedFd)> {
    let highs: Vec<OwnedFd> = srcs.iter().map(|&f| high_dup(f)).collect::<io::Result<_>>()?;
    let cg = cgroup.map(high_dup).transpose()?;
    let cg_fd = cg.as_ref().map_or(-1, |f| f.as_raw_fd());
    let src_nums: Vec<RawFd> = highs.iter().map(|f| f.as_raw_fd()).collect();
    let ruleset = stub.ruleset.as_ref().map(|r| high_dup(r.as_raw_fd())).transpose()?;
    let ruleset_fd = ruleset.as_ref().map_or(-1, |r| r.as_raw_fd());
    let cpath = cstr(path);
    let argv_s: Vec<CString> = std::iter::once(path.file_name().map(cstr).unwrap_or_else(|| cstr("scz-child"))).chain(args.iter().map(cstr)).collect();
    let env: Vec<CString> = std::env::vars_os()
        .map(|(k, v)| {
            let mut kv = k.as_bytes().to_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            CString::new(kv).expect("no NUL in the environment")
        })
        .collect();
    let mut argv: Vec<*const libc::c_char> = argv_s.iter().map(|a| a.as_ptr()).collect();
    argv.push(core::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|a| a.as_ptr()).collect();
    envp.push(core::ptr::null());
    // SAFETY: sigset_t is plain data; sigemptyset initializes it.
    let mut empty: libc::sigset_t = unsafe { core::mem::zeroed() };
    unsafe { libc::sigemptyset(&mut empty) };
    let parent = std::process::id() as libc::pid_t;
    let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let nofile = libc::rlimit { rlim_cur: stub.nofile, rlim_max: stub.nofile };
    let data = stub.rlimit_data.map(|b| libc::rlimit { rlim_cur: b, rlim_max: b });
    let prog = stub.filter.as_ref().map(|f| libc::sock_fprog { len: f.len() as u16, filter: f.as_ptr() as *mut libc::sock_filter });
    let first_free = 3 + src_nums.len() as u32;

    let mut pidfd: libc::c_int = -1;
    let ca = CloneArgs {
        flags: CLONE_PIDFD,
        pidfd: &mut pidfd as *mut libc::c_int as u64,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: libc::SIGCHLD as u64,
        stack: 0,
        stack_size: 0,
        tls: 0,
    };
    // SAFETY: clone3 and clone without a new stack behave as fork: the child continues on a
    // copy of this stack, runs only async-signal-safe calls on memory prepared above, then
    // execs or exits. The parent continues normally.
    let mut pid = unsafe { libc::syscall(libc::SYS_clone3, &ca, core::mem::size_of::<CloneArgs>()) } as libc::pid_t;
    if pid < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOSYS) {
        // clone(flags, stack, parent_tid, …): with CLONE_PIDFD the pidfd is stored through
        // parent_tid, the third argument on both architectures.
        pid = unsafe { libc::syscall(libc::SYS_clone, CLONE_PIDFD | libc::SIGCHLD as u64, 0usize, &mut pidfd as *mut libc::c_int, 0usize, 0usize) }
            as libc::pid_t;
    }
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            for (i, &s) in src_nums.iter().enumerate() {
                // dup2 clears close-on-exec on the new number, so exactly these survive.
                if libc::dup2(s, 3 + i as libc::c_int) < 0 {
                    libc::_exit(126);
                }
            }
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if libc::getppid() != parent {
                libc::_exit(125); // the supervisor died before the death signal was armed
            }
            // Writing 0 to cgroup.procs moves the writer: this worker, into its own cgroup.
            if cg_fd >= 0 && libc::write(cg_fd, b"0".as_ptr() as *const libc::c_void, 1) != 1 {
                libc::_exit(121);
            }
            // The supervisor is non-dumpable and this child inherited that; set it back so the
            // child may write its own /proc entry (supervisor §5.1).
            libc::prctl(libc::PR_SET_DUMPABLE, 1);
            let adj = libc::open(c"/proc/self/oom_score_adj".as_ptr(), libc::O_WRONLY);
            if adj >= 0 {
                libc::write(adj, b"1000".as_ptr() as *const libc::c_void, 4);
                libc::close(adj);
            }
            if !stub.core_dumps {
                // A core dump holds the death locks until it is written (spike S6b).
                libc::setrlimit(libc::RLIMIT_CORE, &no_core);
            }
            libc::setrlimit(libc::RLIMIT_NOFILE, &nofile);
            if let Some(d) = data.as_ref() {
                libc::setrlimit(libc::RLIMIT_DATA, d);
            }
            libc::sigprocmask(libc::SIG_SETMASK, &empty, core::ptr::null_mut());
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                libc::_exit(124);
            }
            if ruleset_fd >= 0 && libc::syscall(sandbox::SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) != 0 {
                libc::_exit(123);
            }
            libc::syscall(libc::SYS_close_range, first_free, u32::MAX, 0u32);
            if let Some(p) = prog.as_ref() {
                if libc::prctl(libc::PR_SET_SECCOMP, 2 /* SECCOMP_MODE_FILTER */, p as *const libc::sock_fprog, 0, 0) != 0 {
                    libc::_exit(122);
                }
            }
            libc::execve(cpath.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    drop((highs, ruleset, cg));
    // SAFETY: the clone stored a new pidfd that nothing else owns.
    Ok((pid, unsafe { OwnedFd::from_raw_fd(pidfd) }))
}

/// Start worker `gen` in beat slot `beat`, loading the model open at `model`, in the cgroup
/// whose `cgroup.procs` is `cgroup` if there is one. `extra` go before the descriptor
/// arguments: a runner's own arguments (`-m sidecarzero.worker my_model`).
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    stub: &Stub,
    worker: &Path,
    extra: &[String],
    arena: &OwnedArena,
    gen: u32,
    beat: u32,
    model: BorrowedFd<'_>,
    cgroup: Option<BorrowedFd<'_>>,
) -> io::Result<Spawned> {
    let wrk_fd = memfd(c"scz-worker", abi::WORKER_SEGMENT_LEN)?;
    let wrk = Mapping::map(wrk_fd.as_fd(), abi::WORKER_SEGMENT_LEN as usize, true)?;
    add_seals(wrk_fd.as_fd(), DATA_SEALS)?;
    let (status_rd, status_wr) = pipe()?;
    scz_shm::sys::set_nonblocking(status_rd.as_fd())?;
    // fds 3..=7, in the order of supervisor §5.1.
    let srcs = [arena.ctl_fd.as_raw_fd(), arena.data_fd.as_raw_fd(), model.as_raw_fd(), status_wr.as_raw_fd(), wrk_fd.as_raw_fd()];
    let args = extra
        .iter()
        .cloned()
        .chain(["--ctl-fd=3", "--data-fd=4", "--model-fd=5", "--status-fd=6", "--wrk-fd=7"].iter().map(|s| s.to_string()))
        .chain([format!("--gen={gen}"), format!("--beat-slot={beat}")])
        .collect::<Vec<_>>();
    let (pid, pidfd) = exec(stub, worker, &args, &srcs, cgroup.map(|c| c.as_raw_fd()))?;
    drop(status_wr); // the parent must not hold the write end, or it would never see EOF
    Ok(Spawned { pid, pidfd, status: status_rd, wrk: std::sync::Arc::new(wrk), beat })
}

/// A helper process (supervisor §2): this binary in helper mode, sandboxed as a worker is,
/// reading `input` as fd 3 and writing its answer to a pipe as fd 4.
pub struct Helper {
    pub pid: libc::pid_t,
    /// The read end of the answer pipe, non-blocking.
    pub out: OwnedFd,
}

pub fn helper(stub: &Stub, args: &[String], input: BorrowedFd<'_>) -> io::Result<Helper> {
    let exe = std::env::current_exe()?;
    let (rd, wr) = pipe()?;
    scz_shm::sys::set_nonblocking(rd.as_fd())?;
    let (pid, _pidfd) = exec(stub, &exe, args, &[input.as_raw_fd(), wr.as_raw_fd()], None)?;
    drop(wr);
    Ok(Helper { pid, out: rd })
}

/// Format an already-collected wait status (e.g. from SIGCHLD) or reap the child non-blockingly.
pub fn reap_status(pid: libc::pid_t, cached_status: Option<i32>) -> String {
    let st = match cached_status {
        Some(st) => st,
        None => {
            let mut st = 0;
            // SAFETY: waiting for our own child, whose pidfd reported it exited.
            let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
            if r != pid {
                return "exited (status unknown)".into();
            }
            st
        }
    };
    if libc::WIFEXITED(st) {
        format!("exited with status {}", libc::WEXITSTATUS(st))
    } else if libc::WIFSIGNALED(st) {
        format!("killed by signal {}", libc::WTERMSIG(st))
    } else {
        format!("ended ({st:#x})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read;

    fn probe_path() -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap();
        let probe = exe.parent().unwrap().parent().unwrap().join("scz-sandbox-probe");
        assert!(probe.exists(), "{} not built: build the workspace's binaries first", probe.display());
        probe
    }

    /// Run the probe under `cfg`; what it found.
    fn run_probe(cfg: &sandbox::Config) -> HashMap<String, String> {
        let probe = probe_path();
        let stub = Stub::new(cfg);
        let (res_rd, res_wr) = pipe().unwrap();
        let (gate_rd, gate_wr) = pipe().unwrap();
        let (pid, _pidfd) = exec(&stub, &probe, &[probe.display().to_string()], &[res_wr.as_raw_fd(), gate_rd.as_raw_fd()], None).unwrap();
        drop((res_wr, gate_rd));
        let mut text = String::new();
        std::fs::File::from(res_rd).read_to_string(&mut text).unwrap();
        drop(gate_wr);
        // SAFETY: reaping our own child.
        unsafe { libc::waitpid(pid, core::ptr::null_mut(), 0) };
        text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_runner_profile_allows_socket_pairs_and_nothing_more() {
        let probe = probe_path();
        let mut cfg = sandbox::Config::new(&[probe.as_path()]);
        cfg.allow_socketpair = true;
        let r = run_probe(&cfg);
        let eperm = libc::EPERM.to_string();
        assert_eq!((r["socketpair"].as_str(), r["pair_send"].as_str()), ("0", "0"), "a socket pair, and send on it: {r:?}");
        for k in ["pair_sendto_addr", "socket_inet", "socket_unix"] {
            assert_eq!(r[k], eperm, "{k} must still be refused");
        }
        let plain = run_probe(&sandbox::Config::new(&[probe.as_path()]));
        assert_eq!(plain["socketpair"], eperm, "without the profile, no socket pair");
    }

    #[test]
    fn the_sandbox_is_in_force_in_the_child() {
        let probe = probe_path();
        let mut cfg = sandbox::Config::new(&[probe.as_path()]);
        cfg.rlimit_data = Some(1 << 30);
        let stub = Stub::new(&cfg);
        let landlock = stub.layers.contains("landlock ABI");
        let (res_rd, res_wr) = pipe().unwrap();
        let (gate_rd, gate_wr) = pipe().unwrap();
        let (pid, _pidfd) = exec(&stub, &probe, &[probe.display().to_string()], &[res_wr.as_raw_fd(), gate_rd.as_raw_fd()], None).unwrap();
        drop((res_wr, gate_rd));
        let mut text = String::new();
        std::fs::File::from(res_rd).read_to_string(&mut text).unwrap();
        // While it lives: what /proc says about it from outside.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let oom = std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj")).unwrap();
        drop(gate_wr);
        // SAFETY: reaping our own child.
        unsafe { libc::waitpid(pid, core::ptr::null_mut(), 0) };
        let r: HashMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
        eprintln!("sandbox: {}\nprobe: {r:?}", stub.layers);
        let eperm = libc::EPERM.to_string();
        for k in ["socket_inet", "socket_unix", "ptrace", "process_vm_readv", "io_uring_setup"] {
            assert_eq!(r[k], eperm, "{k} must be refused");
        }
        assert_eq!((r["no_new_privs"], r["seccomp"]), ("1", "2"));
        assert_eq!((r["rlimit_core"], r["rlimit_nofile"], r["rlimit_data"]), ("0", "256", &*(1u64 << 30).to_string()));
        assert_eq!(r["fds"], "0,1,2,3,4", "exactly the descriptors passed, and the standard three");
        assert_eq!(r["open_self"], "0");
        if landlock {
            let eacces = libc::EACCES.to_string();
            for k in ["open_proc", "open_etc", "open_write"] {
                assert_eq!(r[k], eacces, "{k}: outside the Landlock view");
            }
            assert_eq!(r["open_cpuinfo"], "0", "the one file of /proc in the view");
        }
        assert_eq!(oom.trim(), "1000");
        assert!(status.lines().any(|l| l.starts_with("NoNewPrivs:") && l.ends_with('1')));
    }
}
