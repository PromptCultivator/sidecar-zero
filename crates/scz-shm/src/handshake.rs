//! The session socket (supervisor §4; sdk-contract §6): a `SOCK_SEQPACKET` Unix socket that
//! carries three fixed 64-byte messages and then stays open, idle, as the liveness channel.

use crate::abi::{self, msg_hello as mh, msg_kind, msg_ready as mr, msg_welcome as mw};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

pub type Msg = [u8; abi::MSG_LEN as usize];

fn put16(b: &mut Msg, off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut Msg, off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut Msg, off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get16(b: &Msg, off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
fn get32(b: &Msg, off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn get64(b: &Msg, off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn header(b: &mut Msg, kind: u32) {
    put32(b, mh::MAGIC, abi::MSG_MAGIC as u32);
    put32(b, mh::KIND, kind);
}

fn kind_of(b: &Msg) -> Option<u32> {
    (get32(b, mh::MAGIC) == abi::MSG_MAGIC as u32).then(|| get32(b, mh::KIND))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub abi_major: u16,
    pub abi_minor_min: u16,
    pub abi_minor_max: u16,
    pub sdk_lang: u16,
    pub sdk_version: u32,
    pub pid: u32,
    pub flags: u64,
}

impl Hello {
    /// What a client built from this crate sends: it speaks exactly this crate's ABI.
    pub fn this_crate(sdk_lang: u32, sdk_version: u32) -> Hello {
        Hello {
            abi_major: abi::ABI_MAJOR,
            abi_minor_min: abi::ABI_MINOR_MIN,
            abi_minor_max: abi::ABI_MINOR,
            sdk_lang: sdk_lang as u16,
            sdk_version,
            pid: std::process::id(),
            flags: crate::next::HELLO_FLAGS,
        }
    }
    pub fn encode(&self) -> Msg {
        let mut b = [0; 64];
        header(&mut b, msg_kind::HELLO);
        put16(&mut b, mh::ABI_MAJOR, self.abi_major);
        put16(&mut b, mh::ABI_MINOR_MIN, self.abi_minor_min);
        put16(&mut b, mh::ABI_MINOR_MAX, self.abi_minor_max);
        put16(&mut b, mh::SDK_LANG, self.sdk_lang);
        put32(&mut b, mh::SDK_VERSION, self.sdk_version);
        put32(&mut b, mh::PID, self.pid);
        put64(&mut b, mh::FLAGS, self.flags);
        b
    }
    pub fn decode(b: &Msg) -> Option<Hello> {
        (kind_of(b)? == msg_kind::HELLO).then(|| Hello {
            abi_major: get16(b, mh::ABI_MAJOR),
            abi_minor_min: get16(b, mh::ABI_MINOR_MIN),
            abi_minor_max: get16(b, mh::ABI_MINOR_MAX),
            sdk_lang: get16(b, mh::SDK_LANG),
            sdk_version: get32(b, mh::SDK_VERSION),
            pid: get32(b, mh::PID),
            flags: get64(b, mh::FLAGS),
        })
    }
    /// The minor version the supervisor will speak with this client, or None if it cannot.
    // At 1.0 (ABI_MINOR = 0) some of these comparisons are constant; they are the rule for
    // every later minor (shm-abi §12.4 rule 4).
    #[allow(clippy::absurd_extreme_comparisons, clippy::unnecessary_min_or_max)]
    pub fn negotiate(&self) -> Option<u16> {
        (self.abi_major == abi::ABI_MAJOR && self.abi_minor_min <= abi::ABI_MINOR && self.abi_minor_min <= self.abi_minor_max)
            .then(|| self.abi_minor_max.min(abi::ABI_MINOR))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub status: u32,
    pub abi_major: u16,
    pub abi_minor: u16,
    pub ctl_len: u32,
    pub data_len: u64,
    pub arena_id: u64,
    pub session_id: u64,
}

impl Welcome {
    pub fn refusal(status: u32) -> Welcome {
        Welcome { status, abi_major: abi::ABI_MAJOR, abi_minor: abi::ABI_MINOR, ctl_len: 0, data_len: 0, arena_id: 0, session_id: 0 }
    }
    pub fn encode(&self) -> Msg {
        let mut b = [0; 64];
        header(&mut b, msg_kind::WELCOME);
        put32(&mut b, mw::STATUS, self.status);
        put16(&mut b, mw::ABI_MAJOR, self.abi_major);
        put16(&mut b, mw::ABI_MINOR, self.abi_minor);
        put32(&mut b, mw::CTL_LEN, self.ctl_len);
        put64(&mut b, mw::DATA_LEN, self.data_len);
        put64(&mut b, mw::ARENA_ID, self.arena_id);
        put64(&mut b, mw::SESSION_ID, self.session_id);
        b
    }
    pub fn decode(b: &Msg) -> Option<Welcome> {
        (kind_of(b)? == msg_kind::WELCOME).then(|| Welcome {
            status: get32(b, mw::STATUS),
            abi_major: get16(b, mw::ABI_MAJOR),
            abi_minor: get16(b, mw::ABI_MINOR),
            ctl_len: get32(b, mw::CTL_LEN),
            data_len: get64(b, mw::DATA_LEN),
            arena_id: get64(b, mw::ARENA_ID),
            session_id: get64(b, mw::SESSION_ID),
        })
    }
}

pub fn encode_ready(arena_id: u64) -> Msg {
    let mut b = [0; 64];
    header(&mut b, msg_kind::READY);
    put64(&mut b, mr::ARENA_ID, arena_id);
    b
}

pub fn decode_ready(b: &Msg) -> Option<u64> {
    (kind_of(b)? == msg_kind::READY).then(|| get64(b, mr::ARENA_ID))
}

// ── sockets ─────────────────────────────────────────────────────────────────────────────

/// Where the session socket lives: a path (in a shared `emptyDir`), or a name in the
/// abstract namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Path(std::path::PathBuf),
    Abstract(String),
}

impl Endpoint {
    /// `@name` is the abstract name `name`; anything else is a path.
    pub fn parse(s: &str) -> Endpoint {
        match s.strip_prefix('@') {
            Some(n) => Endpoint::Abstract(n.to_string()),
            None => Endpoint::Path(s.into()),
        }
    }
    fn sockaddr(&self) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: sockaddr_un is plain data; all-zero is a valid value.
        let mut a: libc::sockaddr_un = unsafe { core::mem::zeroed() };
        a.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let (bytes, start): (&[u8], usize) = match self {
            Endpoint::Path(p) => (p.as_os_str().as_bytes(), 0),
            Endpoint::Abstract(n) => (n.as_bytes(), 1),
        };
        if start + bytes.len() >= a.sun_path.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket name too long"));
        }
        for (i, &c) in bytes.iter().enumerate() {
            a.sun_path[start + i] = c as libc::c_char;
        }
        let base = core::mem::size_of::<libc::sa_family_t>();
        let len = match self {
            Endpoint::Path(_) => base + bytes.len() + 1,
            Endpoint::Abstract(_) => base + 1 + bytes.len(),
        };
        Ok((a, len as libc::socklen_t))
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Path(p) => write!(f, "{}", p.display()),
            Endpoint::Abstract(n) => write!(f, "@{n}"),
        }
    }
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn seqpacket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation.
    let fd = cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Bind and listen. A stale socket file left by a previous run is removed first. A path
/// socket is made mode 0660: the host usually runs as another user and reaches it through a
/// group the two containers share, the pod's `fsGroup` (kubernetes-deploy §7). Connecting to a
/// Unix socket needs write permission on its file, which the umask would otherwise withhold
/// from the group. The supervisor still authorizes each peer by `SO_PEERCRED`.
pub fn listen(ep: &Endpoint) -> io::Result<OwnedFd> {
    if let Endpoint::Path(p) = ep {
        let _ = std::fs::remove_file(p);
    }
    let fd = seqpacket()?;
    let (a, len) = ep.sockaddr()?;
    // SAFETY: a valid address of the given length.
    cvt(unsafe { libc::bind(fd.as_raw_fd(), &a as *const _ as *const libc::sockaddr, len) })?;
    if let Endpoint::Path(p) = ep {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660))?;
    }
    cvt(unsafe { libc::listen(fd.as_raw_fd(), 16) })?;
    Ok(fd)
}

/// Accept one connection, close-on-exec. WouldBlock if the listener is non-blocking and idle.
pub fn accept(listener: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    // SAFETY: the peer address is not wanted.
    let fd = cvt(unsafe { libc::accept4(listener.as_raw_fd(), core::ptr::null_mut(), core::ptr::null_mut(), libc::SOCK_CLOEXEC) })?;
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Connect, with `timeout` applied to the connect and to every later send and receive.
pub fn connect(ep: &Endpoint, timeout: Duration) -> io::Result<OwnedFd> {
    let fd = seqpacket()?;
    set_timeouts(fd.as_fd(), timeout)?;
    let (a, len) = ep.sockaddr()?;
    // SAFETY: a valid address of the given length. A Unix-socket connect honors SO_SNDTIMEO.
    cvt(unsafe { libc::connect(fd.as_raw_fd(), &a as *const _ as *const libc::sockaddr, len) })?;
    Ok(fd)
}

pub fn set_timeouts(fd: BorrowedFd<'_>, t: Duration) -> io::Result<()> {
    let tv = libc::timeval { tv_sec: t.as_secs() as _, tv_usec: t.subsec_micros() as _ };
    for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: a valid timeval of the stated size.
        cvt(unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                &tv as *const _ as *const libc::c_void,
                core::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        })?;
    }
    Ok(())
}

/// The peer's credentials. Across containers the pid is 0 (spike S1); uid and gid are real.
pub fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<libc::ucred> {
    let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = core::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid out-buffer of the stated size.
    cvt(unsafe { libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut c as *mut _ as *mut libc::c_void, &mut len) })?;
    Ok(c)
}

const MAX_FDS: usize = 4;

/// Send one message, with descriptors attached by SCM_RIGHTS if any.
pub fn send(fd: BorrowedFd<'_>, msg: &Msg, fds: &[RawFd]) -> io::Result<()> {
    assert!(fds.len() <= MAX_FDS);
    let mut iov = libc::iovec { iov_base: msg.as_ptr() as *mut libc::c_void, iov_len: msg.len() };
    // u64 storage keeps the control buffer aligned for cmsghdr.
    let mut cbuf = [0u64; 8];
    // SAFETY: msghdr is plain data.
    let mut mh: libc::msghdr = unsafe { core::mem::zeroed() };
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    if !fds.is_empty() {
        let payload = std::mem::size_of_val(fds) as u32;
        // SAFETY: CMSG_SPACE/CMSG_LEN are pure arithmetic.
        let space = unsafe { libc::CMSG_SPACE(payload) } as usize;
        assert!(space <= core::mem::size_of_val(&cbuf));
        mh.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        mh.msg_controllen = space as _;
        // SAFETY: the control buffer is large enough (asserted) and aligned; CMSG_FIRSTHDR
        // points into it, and CMSG_DATA has room for `payload` bytes.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&mh);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(payload) as _;
            core::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), payload as usize);
        }
    }
    // SAFETY: every pointer in `mh` refers to live local buffers.
    let n = unsafe { libc::sendmsg(fd.as_raw_fd(), &mh, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != msg.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short send"));
    }
    Ok(())
}

/// Receive one message and any descriptors with it (close-on-exec). Ok(None) is end of stream.
pub fn recv(fd: BorrowedFd<'_>) -> io::Result<Option<(Msg, Vec<OwnedFd>)>> {
    let mut msg: Msg = [0; 64];
    let mut iov = libc::iovec { iov_base: msg.as_mut_ptr() as *mut libc::c_void, iov_len: msg.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: msghdr is plain data.
    let mut mh: libc::msghdr = unsafe { core::mem::zeroed() };
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    mh.msg_controllen = core::mem::size_of_val(&cbuf) as _;
    // SAFETY: every pointer in `mh` refers to live local buffers.
    let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &mut mh, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // Take ownership of every descriptor first, so that none leaks whatever happens next.
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel wrote into our buffer.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&mh);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let bytes = (*c).cmsg_len as usize - (data as usize - c as usize);
                for i in 0..bytes / core::mem::size_of::<RawFd>() {
                    let raw = core::ptr::read_unaligned((data as *const RawFd).add(i));
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            }
            c = libc::CMSG_NXTHDR(&mh, c);
        }
    }
    if n == 0 && fds.is_empty() {
        return Ok(None);
    }
    if mh.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || n as usize != msg.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "malformed handshake message"));
    }
    Ok(Some((msg, fds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let h = Hello::this_crate(abi::sdk_lang::RUST, 7);
        assert_eq!(Hello::decode(&h.encode()), Some(h));
        assert_eq!(h.negotiate(), Some(abi::ABI_MINOR));
        let w = Welcome { status: 0, abi_major: 0, abi_minor: 1, ctl_len: 4096, data_len: 1 << 23, arena_id: 9, session_id: 3 };
        assert_eq!(Welcome::decode(&w.encode()), Some(w));
        assert_eq!(decode_ready(&encode_ready(42)), Some(42));
        assert_eq!(Welcome::decode(&h.encode()), None, "the kind is checked");
    }

    #[test]
    fn a_newer_major_or_minimum_is_refused() {
        let mut h = Hello::this_crate(1, 1);
        h.abi_major += 1;
        assert_eq!(h.negotiate(), None);
        let mut h = Hello::this_crate(1, 1);
        h.abi_minor_min = abi::ABI_MINOR + 1;
        h.abi_minor_max = abi::ABI_MINOR + 1;
        assert_eq!(h.negotiate(), None);
    }

    #[test]
    fn descriptors_cross_the_socket() {
        let dir = std::env::temp_dir().join(format!("scz-hs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ep = Endpoint::Path(dir.join("s.sock"));
        let l = listen(&ep).unwrap();
        let c = connect(&ep, Duration::from_secs(1)).unwrap();
        let s = accept(l.as_fd()).unwrap();
        let m = crate::arena::memfd(c"t", 4096).unwrap();
        send(s.as_fd(), &Welcome::refusal(0).encode(), &[m.as_raw_fd()]).unwrap();
        let (msg, fds) = recv(c.as_fd()).unwrap().unwrap();
        assert!(Welcome::decode(&msg).is_some());
        assert_eq!(fds.len(), 1);
        assert_eq!(crate::arena::file_len(fds[0].as_fd()).unwrap(), 4096);
        // SAFETY: F_GETFD only reads flags.
        assert_eq!(unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        drop(s);
        assert!(recv(c.as_fd()).unwrap().is_none(), "end of stream");
        let _ = std::fs::remove_dir_all(dir);
    }
}
