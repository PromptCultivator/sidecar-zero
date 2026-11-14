//! The arena's two memfds: creating and sealing them (supervisor), and mapping and checking
//! them (client and worker). shm-abi §2; supervisor §3 step 4; sdk-contract §6, handshake.

use crate::abi::{self, acc};
use crate::layout::{self, Config, Directory, Geometry, Layout, LayoutError};
use crate::seg::Seg;
use std::ffi::CStr;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// Seals on the control segment: no resize, no new writable mapping, no further seals.
pub const CTL_SEALS: i32 = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_FUTURE_WRITE | libc::F_SEAL_SEAL;
/// Seals on the data segment, and on a worker segment: no resize, no further seals.
pub const DATA_SEALS: i32 = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

const MADV_POPULATE_READ: i32 = 22;
const MADV_POPULATE_WRITE: i32 = 23;
const MFD_HUGETLB: libc::c_uint = 0x0004;

#[derive(Debug)]
pub enum ArenaError {
    Io(&'static str, io::Error),
    /// A segment arrived without the seals the protocol requires.
    Seals {
        segment: &'static str,
        have: i32,
        need: i32,
    },
    Layout(LayoutError),
    /// The two segments, or the segments and the WELCOME message, disagree.
    Mismatch(&'static str),
}

impl fmt::Display for ArenaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArenaError::Io(what, e) => write!(f, "{what}: {e}"),
            ArenaError::Seals { segment, have, need } => write!(f, "{segment} segment has seals {have:#x}, needs {need:#x}"),
            ArenaError::Layout(e) => write!(f, "layout: {e:?}"),
            ArenaError::Mismatch(what) => write!(f, "mismatch: {what}"),
        }
    }
}

impl std::error::Error for ArenaError {}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> ArenaError {
    move |e| ArenaError::Io(what, e)
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// A fresh, sealable memfd of `len` bytes, close-on-exec.
pub fn memfd(name: &CStr, len: u64) -> io::Result<OwnedFd> {
    memfd_with_flags(name, len, libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
}

fn memfd_with_flags(name: &CStr, len: u64, flags: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: `name` is a valid C string.
    let fd = cvt(unsafe { libc::memfd_create(name.as_ptr(), flags) })?;
    // SAFETY: memfd_create returned a new descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) })?;
    Ok(fd)
}

/// Like [`memfd`], optionally attempting `MFD_HUGETLB` and falling back to a standard memfd if
/// hugepages are unavailable or `ftruncate` / sealing flags are rejected by the kernel.
pub fn memfd_ext(name: &CStr, len: u64, hugepages: bool) -> io::Result<OwnedFd> {
    if hugepages {
        if let Ok(fd) = memfd_with_flags(name, len, libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | MFD_HUGETLB) {
            return Ok(fd);
        }
    }
    memfd(name, len)
}

pub fn add_seals(fd: BorrowedFd<'_>, seals: i32) -> io::Result<()> {
    // SAFETY: F_ADD_SEALS on an open descriptor.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) }).map(|_| ())
}

pub fn seals(fd: BorrowedFd<'_>) -> io::Result<i32> {
    // SAFETY: F_GET_SEALS on an open descriptor.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) })
}

pub fn file_len(fd: BorrowedFd<'_>) -> io::Result<u64> {
    // SAFETY: lseek on an open descriptor. Used instead of fstat, as sdk-contract §6 suggests.
    let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    if end < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(end as u64)
}

/// One shared mapping, unmapped on drop.
pub struct Mapping {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is reached only through `Seg`, which uses atomics or explicit copies.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn map(fd: BorrowedFd<'_>, len: usize, writable: bool) -> io::Result<Mapping> {
        let prot = if writable { libc::PROT_READ | libc::PROT_WRITE } else { libc::PROT_READ };
        // SAFETY: a fresh shared mapping of an open descriptor; the kernel picks the address.
        let p = unsafe { libc::mmap(core::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr: p as *mut u8, len })
    }
    pub fn seg(&self) -> Seg<'_> {
        // SAFETY: mapped for the lifetime of self, page-aligned by mmap.
        unsafe { Seg::new(self.ptr, self.len) }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Populate page tables now, so no request takes a first-touch fault (worker §3 step 3).
    /// Kernels before 5.14 lack the advice; the pages then fault in on first use instead.
    pub fn populate(&self, write: bool) {
        let advice = if write { MADV_POPULATE_WRITE } else { MADV_POPULATE_READ };
        // SAFETY: advice over our own mapping.
        unsafe { libc::madvise(self.ptr as *mut libc::c_void, self.len, advice) };
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped in `map`, and every Seg borrowed from it has ended.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

/// The supervisor's arena: both descriptors and writable mappings of both.
pub struct OwnedArena {
    pub ctl_fd: OwnedFd,
    pub data_fd: OwnedFd,
    pub ctl: Mapping,
    pub data: Mapping,
    pub dir: Directory,
}

impl OwnedArena {
    /// Supervisor §3 step 4: data, then control: memfd → ftruncate → mmap → populate → init → seal.
    pub fn create(g: &Geometry, cfg: &Config) -> Result<OwnedArena, ArenaError> {
        Self::create_with_opts(g, cfg, false)
    }

    /// Like [`OwnedArena::create`], attempting `MFD_HUGETLB` for the data segment when
    /// `hugepages` is true and falling back to a standard memfd if hugepages are unavailable.
    pub fn create_with_opts(g: &Geometry, cfg: &Config, hugepages: bool) -> Result<OwnedArena, ArenaError> {
        let layout = Layout::compute(g).map_err(ArenaError::Layout)?;
        let data_fd = memfd_ext(c"scz-data", layout.data_len, hugepages).map_err(io_err("data memfd"))?;
        let data = Mapping::map(data_fd.as_fd(), layout.data_len as usize, true).map_err(io_err("map data"))?;
        data.populate(true);
        let ctl_fd = memfd(c"scz-control", layout::CTL_LEN).map_err(io_err("control memfd"))?;
        let ctl = Mapping::map(ctl_fd.as_fd(), layout::CTL_LEN as usize, true).map_err(io_err("map control"))?;
        ctl.populate(true);
        layout::write_data(&data.seg(), &layout, cfg);
        layout::write_control(&ctl.seg(), &layout, cfg);
        add_seals(data_fd.as_fd(), DATA_SEALS).map_err(io_err("seal data"))?;
        // FUTURE_WRITE refuses new writable mappings and write(2); this one stays writable,
        // and it is the only one there will ever be (shm-abi §2).
        add_seals(ctl_fd.as_fd(), CTL_SEALS).map_err(io_err("seal control"))?;
        let dir = Directory::read(&ctl.seg(), layout.data_len).map_err(ArenaError::Layout)?;
        Ok(OwnedArena { ctl_fd, data_fd, ctl, data, dir })
    }
}

/// A client's or a worker's arena: the control segment read-only, the data segment read-write.
pub struct MappedArena {
    pub ctl: Mapping,
    pub data: Mapping,
    pub dir: Directory,
}

/// What the WELCOME message promised, checked against the segments themselves.
#[derive(Clone, Copy, Debug)]
pub struct Expect {
    pub ctl_len: u64,
    pub data_len: u64,
    pub arena_id: u64,
}

impl MappedArena {
    /// Map both segments read-only, for inspection (`sczctl dump`, the conformance runner).
    pub fn open_readonly(ctl_fd: BorrowedFd<'_>, data_fd: BorrowedFd<'_>) -> Result<MappedArena, ArenaError> {
        let ctl_len = file_len(ctl_fd).map_err(io_err("control size"))?;
        let data_len = file_len(data_fd).map_err(io_err("data size"))?;
        let ctl = Mapping::map(ctl_fd, ctl_len as usize, false).map_err(io_err("map control"))?;
        let dir = Directory::read(&ctl.seg(), data_len).map_err(ArenaError::Layout)?;
        let data = Mapping::map(data_fd, data_len as usize, false).map_err(io_err("map data"))?;
        Ok(MappedArena { ctl, data, dir })
    }

    /// Map and check both segments (sdk-contract §6, handshake; worker §3 step 2).
    pub fn open(ctl_fd: BorrowedFd<'_>, data_fd: BorrowedFd<'_>, expect: Option<Expect>) -> Result<MappedArena, ArenaError> {
        let have = seals(ctl_fd).map_err(io_err("control seals"))?;
        if have & CTL_SEALS != CTL_SEALS {
            return Err(ArenaError::Seals { segment: "control", have, need: CTL_SEALS });
        }
        let have = seals(data_fd).map_err(io_err("data seals"))?;
        if have & DATA_SEALS != DATA_SEALS {
            return Err(ArenaError::Seals { segment: "data", have, need: DATA_SEALS });
        }
        let ctl_len = file_len(ctl_fd).map_err(io_err("control size"))?;
        let data_len = file_len(data_fd).map_err(io_err("data size"))?;
        if ctl_len < abi::CTL_HEADER_LEN {
            return Err(ArenaError::Mismatch("control segment shorter than its header"));
        }
        if let Some(x) = expect {
            if x.ctl_len != ctl_len || x.data_len != data_len {
                return Err(ArenaError::Mismatch("segment sizes differ from WELCOME"));
            }
        }
        let ctl = Mapping::map(ctl_fd, ctl_len as usize, false).map_err(io_err("map control"))?;
        let dir = Directory::read(&ctl.seg(), data_len).map_err(ArenaError::Layout)?;
        if let Some(x) = expect {
            if x.arena_id != dir.arena_id {
                return Err(ArenaError::Mismatch("arena_id differs from WELCOME"));
            }
        }
        let data = Mapping::map(data_fd, data_len as usize, true).map_err(io_err("map data"))?;
        {
            let d = data.seg();
            let s = dir.status_off as usize;
            if acc::status_page::magic(&d, s) != abi::DATA_MAGIC || acc::status_page::arena_id(&d, s) != dir.arena_id {
                return Err(ArenaError::Mismatch("the data segment's status page names another arena"));
            }
        }
        ctl.populate(false);
        data.populate(true);
        Ok(MappedArena { ctl, data, dir })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Config;

    #[test]
    fn create_seal_and_reopen() {
        let a = OwnedArena::create(&Geometry::default(), &Config::with_defaults(0xABCD, 1)).unwrap();
        assert_eq!(seals(a.ctl_fd.as_fd()).unwrap() & CTL_SEALS, CTL_SEALS);
        // FUTURE_WRITE: a second writable mapping of the control segment is refused.
        assert!(Mapping::map(a.ctl_fd.as_fd(), layout::CTL_LEN as usize, true).is_err());
        let m = MappedArena::open(a.ctl_fd.as_fd(), a.data_fd.as_fd(), Some(Expect { ctl_len: layout::CTL_LEN, data_len: a.dir.data_len, arena_id: 0xABCD }))
            .unwrap();
        assert_eq!(m.dir.layout, a.dir.layout);
        // The supervisor's writes reach the read-only mapping.
        acc::control_header::set_gate(&a.ctl.seg(), 0, 0x55);
        assert_eq!(acc::control_header::gate(&m.ctl.seg(), 0), 0x55);
    }

    #[test]
    fn an_unsealed_segment_is_refused() {
        let ctl = memfd(c"t", 4096).unwrap();
        let data = memfd(c"t", 4096).unwrap();
        match MappedArena::open(ctl.as_fd(), data.as_fd(), None) {
            Err(ArenaError::Seals { segment: "control", .. }) => {}
            other => panic!("{:?}", other.err()),
        }
    }

    #[test]
    fn a_wrong_welcome_is_refused() {
        let a = OwnedArena::create(&Geometry::default(), &Config::with_defaults(7, 1)).unwrap();
        let r = MappedArena::open(a.ctl_fd.as_fd(), a.data_fd.as_fd(), Some(Expect { ctl_len: layout::CTL_LEN, data_len: a.dir.data_len, arena_id: 8 }));
        assert!(matches!(r, Err(ArenaError::Mismatch(_))));
    }
}
