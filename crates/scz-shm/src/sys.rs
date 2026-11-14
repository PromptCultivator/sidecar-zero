//! The few system calls the SDK and the worker need beyond the arena and the handshake, kept
//! here so that neither of them has to contain `unsafe` code.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// The calling thread's kernel thread id.
pub fn tid() -> u32 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

/// Whether a thread of this process with that id still exists.
pub fn thread_exists(tid: u32) -> bool {
    std::path::Path::new(&format!("/proc/self/task/{tid}")).exists()
}

/// Take ownership of an inherited descriptor, checking that it is open, and mark it
/// close-on-exec (worker §3 step 3).
pub fn adopt_fd(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: F_GETFD/F_SETFD only inspect and set the descriptor's flags.
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) })?;
    // SAFETY: the descriptor is open (checked above) and nothing else in this process owns it:
    // it was inherited, and the caller adopts each inherited number once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Write one byte, retrying on EINTR. The status pipe's only message.
pub fn write_byte(fd: BorrowedFd<'_>, b: u8) -> io::Result<()> {
    loop {
        // SAFETY: a one-byte buffer that lives across the call.
        let r = unsafe { libc::write(fd.as_raw_fd(), &b as *const u8 as *const libc::c_void, 1) };
        if r == 1 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Read what is available into `buf`. Ok(0) is end of file; WouldBlock if nothing is there.
pub fn read(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the buffer is valid for its length.
    let r = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

/// Wait up to `timeout_ms` for the descriptor to become readable or hung up.
pub fn wait_readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN | libc::POLLRDHUP, revents: 0 };
    // SAFETY: one valid pollfd.
    let r = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
    }
    Ok(r > 0)
}

/// Whether the peer has closed a connected socket: a non-blocking peek that returns 0 bytes.
/// A pending message is not end of stream.
pub fn peer_closed(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut b = [0u8; 64];
    // SAFETY: the buffer is valid for its length.
    let r = unsafe { libc::recv(fd.as_raw_fd(), b.as_mut_ptr() as *mut libc::c_void, b.len(), libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    if r == 0 {
        return Ok(true);
    }
    if r > 0 {
        return Ok(false);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(false),
        Some(libc::ECONNRESET) => Ok(true),
        _ => Err(e),
    }
}

/// Make a descriptor non-blocking.
pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: flag manipulation only.
    let fl = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK) })?;
    Ok(())
}
