//! The admin socket (supervisor §7): fixed-size commands from `sczctl`, from the sidecar's
//! own container only (the socket lives in a directory only it mounts), with the peer's user
//! checked by SO_PEERCRED.
//!
//! A command is 16 bytes, little-endian: magic "SCZA", command, flags, zero. Replies:
//!   status   the fixed-size struct of `machine::Core::status_struct` (STATUS_LEN bytes)
//!   dump     a 64-byte header and, by SCM_RIGHTS, read-only descriptors for both segments:
//!            the control segment's own (its seals forbid a writable mapping) and the data
//!            segment's reopened read-only, so the tool cannot write to the arena
//!   reload, reset   "accepted"

use crate::machine::{Core, Event};
use crate::{Epoll, K_ADMIN, T_ADMIN};
use scz_shm::arena::OwnedArena;
use scz_shm::handshake::{self, Endpoint};
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;

pub const MAGIC: u32 = 0x415A_4353; // "SCZA"
pub const STATUS: u32 = 1;
pub const RELOAD: u32 = 2;
pub const RESET: u32 = 3;
pub const DUMP: u32 = 4;
/// RELOAD's flag: load the model even if it was rolled back from (§5.6).
pub const FORCE: u32 = 1;

pub struct Admin {
    listener: OwnedFd,
    conns: HashMap<u64, OwnedFd>,
    next: u64,
}

impl Admin {
    pub fn bind(path: &Path, ep: &Epoll) -> io::Result<Admin> {
        let listener = handshake::listen(&Endpoint::Path(path.into()))?;
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        scz_shm::sys::set_nonblocking(listener.as_fd())?;
        ep.add(listener.as_fd(), T_ADMIN)?;
        Ok(Admin { listener, conns: HashMap::new(), next: 0 })
    }

    pub fn accept(&mut self, ep: &Epoll) {
        while let Ok(fd) = handshake::accept(self.listener.as_fd()) {
            // SAFETY: geteuid has no preconditions.
            let me = unsafe { libc::geteuid() };
            let ok = handshake::peer_cred(fd.as_fd()).is_ok_and(|c| c.uid == me || c.uid == 0);
            if !ok || self.conns.len() >= 8 || scz_shm::sys::set_nonblocking(fd.as_fd()).is_err() {
                continue;
            }
            let id = self.next;
            self.next += 1;
            if ep.add(fd.as_fd(), K_ADMIN | id).is_ok() {
                self.conns.insert(id, fd);
            }
        }
    }

    pub fn serve(&mut self, id: u64, ep: &Epoll, core: &Core<'_>, arena: &OwnedArena, bound: bool) -> Option<Event> {
        let fd = self.conns.remove(&id)?;
        ep.del(fd.as_fd());
        let mut b = [0u8; 16];
        // SAFETY: a valid buffer of its own length.
        let n = unsafe { libc::recv(fd.as_raw_fd(), b.as_mut_ptr() as *mut libc::c_void, b.len(), libc::MSG_DONTWAIT) };
        let (cmd, flags) = command(&b[..n.clamp(0, 16) as usize])?;
        let send = |bytes: &[u8]| {
            // SAFETY: a valid buffer of its own length.
            unsafe { libc::send(fd.as_raw_fd(), bytes.as_ptr() as *const libc::c_void, bytes.len(), libc::MSG_NOSIGNAL) };
        };
        match cmd {
            STATUS => send(&core.status_struct(bound)),
            RELOAD => {
                send(b"accepted");
                return Some(if flags & FORCE != 0 { Event::ForceReload } else { Event::Reload });
            }
            RESET => {
                send(b"accepted");
                return Some(Event::RequestReset);
            }
            DUMP => {
                let path = std::ffi::CString::new(format!("/proc/self/fd/{}", arena.data_fd.as_raw_fd())).unwrap();
                // SAFETY: a valid path; the new descriptor is closed below.
                let ro = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
                if ro < 0 {
                    send(b"dump failed");
                    return None;
                }
                let mut hdr = [0u8; 64];
                hdr[..4].copy_from_slice(&MAGIC.to_le_bytes());
                hdr[4..8].copy_from_slice(&DUMP.to_le_bytes());
                hdr[8..16].copy_from_slice(&(arena.ctl.len() as u64).to_le_bytes());
                hdr[16..24].copy_from_slice(&arena.dir.data_len.to_le_bytes());
                let _ = handshake::send(fd.as_fd(), &hdr, &[arena.ctl_fd.as_raw_fd(), ro]);
                // SAFETY: closing the descriptor opened above.
                unsafe { libc::close(ro) };
            }
            _ => send(b"unknown command"),
        }
        None
    }
}

/// The command in a request: exactly 16 bytes, starting with the magic. Anything else is
/// dropped without a reply.
pub fn command(b: &[u8]) -> Option<(u32, u32)> {
    let word = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
    (b.len() == 16 && word(0) == MAGIC).then(|| (word(1), word(2)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_whole_request_with_the_magic_is_a_command() {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&DUMP.to_le_bytes());
        assert_eq!(command(&b), Some((DUMP, 0)));
        assert_eq!(command(&b[..15]), None);
        assert_eq!(command(&[]), None);
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 17) as usize;
            let r: Vec<u8> = (0..n).map(|i| (x >> (i * 3 % 60)) as u8).collect();
            assert!(command(&r).is_none() || n == 16);
        }
    }
}
