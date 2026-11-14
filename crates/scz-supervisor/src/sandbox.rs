//! The worker and helper sandbox (worker §6), prepared in the supervisor before the clone so
//! the child stub only makes system calls on memory that already exists.
//!
//! | Layer | Here |
//! |---|---|
//! | Native system calls only | The seccomp filter kills the process on a call made with any other architecture's convention, and on x86-64 the x32 one |
//! | No sockets, no reaching into other processes | The filter answers `EPERM` to the socket calls, io_uring, ptrace and cross-process memory access, `pidfd_getfd`, mounts, bpf, modules, and keyrings |
//! | Read-only filesystem view | Landlock, where the kernel has it: read and execute beneath the runtime's directories, nothing else; `/proc` is outside the view |
//! | Resource limits | `RLIMIT_DATA` if configured, `RLIMIT_NOFILE`, `RLIMIT_CORE = 0` unless core dumps are on |
//!
//! No new privileges, the parent-death signal, `oom_score_adj`, and the descriptor list are set
//! by the stub in `spawn.rs`.

use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    pub seccomp: bool,
    pub landlock: bool,
    /// Directories (or files) a sandboxed process may read and execute beneath.
    pub read: Vec<PathBuf>,
    pub rlimit_data: Option<u64>,
    pub nofile: u64,
    pub core_dumps: bool,
    /// Let workers make local socket pairs: the Python runner's interpreter uses them for its
    /// own plumbing (asyncio, multiprocessing). Network sockets stay refused (sdk-python §5).
    pub allow_socketpair: bool,
}

impl Config {
    /// The default view: the system's libraries, the dynamic loader's cache, ONNX Runtime
    /// itself, and the directories of the binaries started; and the two places the runtime
    /// reads to learn the CPU's features, without which it falls back to slower kernels
    /// (`/proc/cpuinfo` is the one file of `/proc` in the view).
    pub fn new(binaries: &[&Path]) -> Config {
        let mut read: Vec<PathBuf> = [
            "/usr",
            "/lib",
            "/lib64",
            "/etc/ld.so.cache",
            "/etc/ld.so.conf",
            "/etc/ld.so.conf.d",
            "/sys/devices/system/cpu",
            "/proc/cpuinfo",
            "/dev/null",
            "/dev/urandom",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        read.push(std::env::var_os("ORT_DIR").map(PathBuf::from).unwrap_or_else(|| "/opt/ort".into()));
        for b in binaries {
            if let Some(d) = b.canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
                read.push(d);
            }
        }
        Config { seccomp: true, landlock: true, read, rlimit_data: None, nofile: 256, core_dumps: false, allow_socketpair: false }
    }
}

// ── seccomp ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7FFF_0000;

/// The calls a worker or helper has no use for (worker §6).
fn denied() -> Vec<libc::c_long> {
    use libc::*;
    let mut v = vec![
        SYS_socket,
        SYS_socketpair,
        SYS_connect,
        SYS_bind,
        SYS_listen,
        SYS_accept,
        SYS_accept4,
        SYS_sendto,
        SYS_sendmsg,
        SYS_sendmmsg,
        SYS_io_uring_setup,
        SYS_io_uring_enter,
        SYS_io_uring_register,
        SYS_ptrace,
        SYS_process_vm_readv,
        SYS_process_vm_writev,
        SYS_pidfd_getfd,
        SYS_mount,
        SYS_umount2,
        SYS_pivot_root,
        SYS_bpf,
        SYS_init_module,
        SYS_finit_module,
        SYS_delete_module,
        SYS_add_key,
        SYS_request_key,
        SYS_keyctl,
        SYS_kexec_load,
        SYS_perf_event_open,
        SYS_setns,
        SYS_unshare,
        SYS_open_by_handle_at,
        SYS_userfaultfd,
    ];
    v.sort();
    v.dedup();
    v
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// The BPF program: check the architecture, then refuse the denied calls with EPERM.
///
/// `allow_socketpair` (a runner's profile, worker §7.3) lets `socketpair` through, and
/// `sendto` without a destination address, which is what `send` on a socket pair is (glibc
/// has no `send` system call; asyncio's self-pipe wakes its loop with it). `socket` and
/// `connect` stay refused, so the only sockets such a worker can hold are pairs it made, and a
/// `sendto` that names an address, the one way to reach a named socket without `connect`, is
/// still refused.
pub fn filter(allow_socketpair: bool) -> Vec<libc::sock_filter> {
    const LD_ABS_W: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K
    let mut p = vec![
        stmt(LD_ABS_W, 4), // seccomp_data.arch
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_KILL_PROCESS),
        stmt(LD_ABS_W, 0), // seccomp_data.nr
    ];
    #[cfg(target_arch = "x86_64")]
    {
        const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
        p.push(jump(JGE_K, 0x4000_0000, 0, 1)); // the x32 calling convention
        p.push(stmt(RET_K, RET_KILL_PROCESS));
    }
    if allow_socketpair {
        // seccomp_data.args[4], sendto's dest_addr, at byte 16 + 4 × 8: allowed only if NULL.
        p.push(jump(JEQ_K, libc::SYS_sendto as u32, 0, 6));
        p.push(stmt(LD_ABS_W, 48));
        p.push(jump(JEQ_K, 0, 0, 2));
        p.push(stmt(LD_ABS_W, 52));
        p.push(jump(JEQ_K, 0, 1, 0));
        p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
        p.push(stmt(RET_K, RET_ALLOW));
    }
    let relaxed = |n: libc::c_long| allow_socketpair && (n == libc::SYS_socketpair || n == libc::SYS_sendto);
    for nr in denied().into_iter().filter(|&n| !relaxed(n)) {
        p.push(jump(JEQ_K, nr as u32, 0, 1));
        p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
    }
    p.push(stmt(RET_K, RET_ALLOW));
    p
}

// ── Landlock ────────────────────────────────────────────────────────────────────────────

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
pub const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const ACCESS_EXECUTE: u64 = 1 << 0;
const ACCESS_READ_FILE: u64 = 1 << 2;
const ACCESS_READ_DIR: u64 = 1 << 3;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneath {
    allowed_access: u64,
    parent_fd: i32,
}

/// The kernel's Landlock ABI version, or why there is none.
pub fn landlock_abi() -> Result<i32, String> {
    // SAFETY: the version query takes no attribute.
    let v = unsafe { libc::syscall(SYS_LANDLOCK_CREATE_RULESET, core::ptr::null::<RulesetAttr>(), 0usize, 1u32) };
    if v < 0 {
        Err(io::Error::last_os_error().to_string())
    } else {
        Ok(v as i32)
    }
}

/// A ruleset that allows reading and executing beneath `paths` and nothing else that Landlock
/// handles. Paths that do not exist are skipped.
pub fn ruleset(paths: &[PathBuf], abi: i32) -> io::Result<OwnedFd> {
    // Everything this ABI version can handle is handled, so it is denied unless allowed.
    let handled = match abi {
        1 => (1 << 13) - 1,
        2 => (1 << 14) - 1,
        3 | 4 => (1 << 15) - 1,
        _ => (1 << 16) - 1,
    };
    let attr = RulesetAttr { handled_access_fs: handled };
    // SAFETY: a valid attribute of the size given.
    let fd = unsafe { libc::syscall(SYS_LANDLOCK_CREATE_RULESET, &attr, core::mem::size_of::<RulesetAttr>(), 0u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nothing else owns.
    let rs = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    for p in paths {
        let Ok(c) = CString::new(p.as_os_str().as_bytes()) else { continue };
        // SAFETY: a valid C string; O_PATH opens nothing for reading.
        let pfd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if pfd < 0 {
            continue;
        }
        // SAFETY: a new descriptor nothing else owns.
        let pfd = unsafe { OwnedFd::from_raw_fd(pfd) };
        let dir = p.is_dir();
        let rule = PathBeneath {
            allowed_access: ACCESS_EXECUTE | ACCESS_READ_FILE | if dir { ACCESS_READ_DIR } else { 0 },
            parent_fd: std::os::fd::AsRawFd::as_raw_fd(&pfd),
        };
        // SAFETY: a valid rule for a live ruleset.
        if unsafe { libc::syscall(SYS_LANDLOCK_ADD_RULE, std::os::fd::AsRawFd::as_raw_fd(&rs), 1u32, &rule, 0u32) } < 0 {
            return Err(io::Error::other(format!("landlock rule for {}: {}", p.display(), io::Error::last_os_error())));
        }
    }
    Ok(rs)
}
