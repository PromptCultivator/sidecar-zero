//! scz-sandbox-probe: started through the worker stub by the sandbox test (spawn.rs), it tries
//! what a worker must not be able to do and reports, one `key=value` line each, to fd 3. Then
//! it waits for fd 4 to close, so the test can look at it from outside while it lives.
//!
//! Its one argument is its own path, which it must still be able to read.

use std::io::Write;
use std::os::fd::FromRawFd;

fn errno<T: PartialOrd + Default>(r: T) -> i32 {
    if r < T::default() {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    } else {
        0
    }
}

fn open(path: &str, flags: i32) -> i32 {
    let c = std::ffi::CString::new(path).unwrap();
    // SAFETY: a valid C string; the descriptor, if any, is closed at once.
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, 0o600) };
    let e = errno(fd);
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
    e
}

fn main() {
    let me = std::env::args().nth(1).unwrap_or_default();
    // SAFETY: fd 3 is the result pipe the stub placed there.
    let mut out = unsafe { std::fs::File::from_raw_fd(3) };
    let mut say = |k: &str, v: String| {
        let _ = writeln!(out, "{k}={v}");
    };
    unsafe {
        say("socket_inet", errno(libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0)).to_string());
        say("socket_unix", errno(libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0)).to_string());
        // A runner's profile (worker §7.3): a socket pair, `send` on it, and a `sendto` that
        // names an address, from a datagram pair.
        let mut sv = [0i32; 2];
        let r = errno(libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()));
        say("socketpair", r.to_string());
        if r == 0 {
            say("pair_send", errno(libc::send(sv[0], b"x".as_ptr() as *const libc::c_void, 1, 0) as i32).to_string());
            libc::close(sv[0]);
            libc::close(sv[1]);
        }
        if libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) == 0 {
            let mut a: libc::sockaddr_un = core::mem::zeroed();
            a.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (d, s) in a.sun_path.iter_mut().zip(b"/tmp/scz-probe.sock") {
                *d = *s as libc::c_char;
            }
            let len = core::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            let r = libc::sendto(sv[0], b"x".as_ptr() as *const libc::c_void, 1, 0, &a as *const _ as *const libc::sockaddr, len);
            say("pair_sendto_addr", errno(r as i32).to_string());
            libc::close(sv[0]);
            libc::close(sv[1]);
        }
        say("ptrace", errno(libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0)).to_string());
        let mut buf = [0u8; 8];
        let local = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: 8 };
        let remote = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: 8 };
        say("process_vm_readv", errno(libc::process_vm_readv(libc::getppid(), &local, 1, &remote, 1, 0)).to_string());
        say("io_uring_setup", errno(libc::syscall(libc::SYS_io_uring_setup, 1u32, core::ptr::null_mut::<u8>())).to_string());
        say("no_new_privs", libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0).to_string());
        say("seccomp", libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0).to_string());
        let lim = |r| {
            let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            libc::getrlimit(r, &mut l);
            l.rlim_cur.to_string()
        };
        say("rlimit_core", lim(libc::RLIMIT_CORE));
        say("rlimit_nofile", lim(libc::RLIMIT_NOFILE));
        say("rlimit_data", lim(libc::RLIMIT_DATA));
        let fds: Vec<String> = (0..256).filter(|&f| libc::fcntl(f, libc::F_GETFD) >= 0).map(|f: i32| f.to_string()).collect();
        say("fds", fds.join(","));
    }
    say("open_proc", open("/proc/self/status", libc::O_RDONLY).to_string());
    say("open_etc", open("/etc/passwd", libc::O_RDONLY).to_string());
    say("open_write", open("/tmp/scz-sandbox-probe-write", libc::O_WRONLY | libc::O_CREAT).to_string());
    say("open_self", open(&me, libc::O_RDONLY).to_string());
    say("open_cpuinfo", open("/proc/cpuinfo", libc::O_RDONLY).to_string());
    drop(say);
    drop(out);
    let mut b = [0u8; 1];
    // SAFETY: fd 4 is the gate pipe; a read returns when the test closes its end.
    unsafe { libc::read(4, b.as_mut_ptr() as *mut libc::c_void, 1) };
}
