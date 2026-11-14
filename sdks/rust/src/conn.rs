//! The background thread (sdk-contract §6): connect, bind to the registry, hold the session,
//! rebind when the registry changes, retire, reconnect.

use crate::client::{ClientState, Shared};
use scz_shm::abi::welcome_status as ws;
use scz_shm::arena::{Expect, MappedArena};
use scz_shm::handshake::{self, Hello, Welcome};
use scz_shm::registry::{self, Registry};
use scz_shm::snapshot::Retiring;
use scz_shm::sys;
use scz_shm::wait::monotonic_ns;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering::*};
use std::sync::Arc;
use std::time::Duration;

/// What request threads see: one mapped arena, and the binding to its registry. A rebind
/// publishes a new snapshot of the same arena.
pub(crate) struct ArenaSnap {
    pub arena: Arc<MappedArena>,
    #[allow(dead_code)]
    pub session_id: u64,
    pub binding: Binding,
}

/// How this client's codec meets the published registry (arrow-data-plane §9).
pub(crate) enum Mode {
    /// An empty registry and a codec without columns: opaque bytes.
    Raw,
    /// The registry's schema is the codec's.
    Frames(Registry),
    /// They differ: every call falls back with SCHEMA_MISMATCH.
    Mismatch,
    /// The registry could not be read or parsed; the next admission asks again.
    Unbound,
}

pub(crate) struct Binding {
    pub mode: Mode,
    pub epoch: u32,
    /// The gate generation this binding was last confirmed under. Updated by request threads
    /// when the gate shows a new generation and the registry epoch has not moved.
    pub confirmed: AtomicU32,
}

/// Read the registry under the model seqlock and bind `want` (the codec's fingerprint, None
/// for opaque bytes) to it. The gate generation is read first: the supervisor writes the model
/// block before the gate shows a new generation, so the block read after it is at least as new.
pub(crate) fn bind(arena: &MappedArena, want: Option<u64>) -> Binding {
    let ctl = arena.ctl.seg();
    let gen = scz_shm::word::gate_gen(scz_shm::abi::acc::control_header::gate(&ctl, 0));
    let Some((block, bytes)) = scz_shm::model::read(&ctl, 1000) else {
        return Binding { mode: Mode::Unbound, epoch: 0, confirmed: AtomicU32::new(0) };
    };
    let caps: Vec<(u32, u32)> = arena.dir.layout.slabs.iter().map(|s| (s.2.req_cap, s.2.resp_cap)).collect();
    let mode = match (want, registry::parse(&bytes, &caps)) {
        (None, Ok(None)) => Mode::Raw,
        (Some(fp), Ok(Some(reg))) if reg.fingerprint() == fp && block.schema_fingerprint == fp => Mode::Frames(reg),
        (_, Err(_)) => Mode::Unbound,
        _ if block.registry_epoch == 0 => Mode::Unbound, // nothing published yet
        _ => Mode::Mismatch,
    };
    Binding { mode, epoch: block.registry_epoch, confirmed: AtomicU32::new(gen) }
}

pub(crate) const SDK_VERSION: u32 = 0x00_01_00;
const BACKOFF_MIN: Duration = Duration::from_millis(10);
const BACKOFF_MAX: Duration = Duration::from_secs(1);

enum ConnErr {
    Busy,
    Abi,
    Other,
}

fn set_state(s: &Shared, st: ClientState) {
    let v = match st {
        ClientState::Disconnected => 0,
        ClientState::Connecting => 1,
        ClientState::Bound => 2,
        ClientState::Rejected => 3,
        ClientState::SchemaRejected => 4,
    };
    s.state.store(v, Release);
}

/// Sleep, but wake at once if the client is closing.
fn nap(s: &Shared, d: Duration) {
    let end = std::time::Instant::now() + d;
    while !s.stop.load(Acquire) {
        let now = std::time::Instant::now();
        if now >= end {
            return;
        }
        std::thread::park_timeout(end - now);
    }
}

fn jitter(d: Duration) -> Duration {
    let half = d.as_nanos() as u64 / 2;
    Duration::from_nanos(half + monotonic_ns() % half.max(1))
}

fn connect_once(s: &Shared) -> Result<(OwnedFd, ArenaSnap), ConnErr> {
    let (ep, timeout, want) = (&s.endpoint, s.connect_timeout, s.want);
    let sock = handshake::connect(ep, timeout).map_err(|_| ConnErr::Other)?;
    handshake::send(sock.as_fd(), &Hello::this_crate(s.sdk_lang, s.sdk_version).encode(), &[]).map_err(|_| ConnErr::Other)?;
    let (msg, fds) = handshake::recv(sock.as_fd()).ok().flatten().ok_or(ConnErr::Other)?;
    let w = Welcome::decode(&msg).ok_or(ConnErr::Other)?;
    match w.status {
        ws::OK => {}
        ws::BUSY => return Err(ConnErr::Busy),
        ws::ABI_MISMATCH => return Err(ConnErr::Abi),
        _ => return Err(ConnErr::Other),
    }
    let [ctl, data] = <[OwnedFd; 2]>::try_from(fds).map_err(|_| ConnErr::Other)?;
    let arena = MappedArena::open(ctl.as_fd(), data.as_fd(), Some(Expect { ctl_len: w.ctl_len as u64, data_len: w.data_len, arena_id: w.arena_id }))
        .map_err(|_| ConnErr::Other)?;
    drop((ctl, data)); // the mappings stay
    handshake::send(sock.as_fd(), &handshake::encode_ready(w.arena_id), &[]).map_err(|_| ConnErr::Other)?;
    let binding = bind(&arena, want);
    Ok((sock, ArenaSnap { arena: Arc::new(arena), session_id: w.session_id, binding }))
}

/// Keep the session until the supervisor's end closes or the client stops, rebinding when a
/// request thread asks. The socket is idle in normal operation; end of stream is the only
/// thing it carries.
fn hold(s: &Shared, sock: &OwnedFd, arena: &Arc<MappedArena>, session_id: u64) {
    while !s.stop.load(Acquire) {
        if s.rebind.swap(false, AcqRel) {
            let binding = bind(arena, s.want);
            set_state(s, if matches!(binding.mode, Mode::Mismatch) { ClientState::SchemaRejected } else { ClientState::Bound });
            if let Ok(Some(old)) = s.published.replace(Some(Box::new(ArenaSnap { arena: arena.clone(), session_id, binding }))) {
                retire(s, old);
            }
        }
        match sys::wait_readable(sock.as_fd(), 5) {
            Ok(false) => {}
            Ok(true) => match sys::peer_closed(sock.as_fd()) {
                Ok(false) => {
                    let _ = handshake::recv(sock.as_fd()); // nothing is defined yet; drop it
                }
                _ => return,
            },
            Err(_) => return,
        }
    }
}

/// Unmap a replaced arena once no call is inside it. Request threads never do this.
fn retire(s: &Shared, mut r: Retiring) {
    let t0 = std::time::Instant::now();
    loop {
        match s.published.reclaim(r, &s.callers, sys::thread_exists) {
            Ok(snap) => {
                // The last snapshot of an arena unmaps it; a rebind's old snapshot does not.
                if Arc::strong_count(&snap.arena) == 1 {
                    s.retired.fetch_add(1, Relaxed);
                }
                drop(snap);
                return;
            }
            Err(back) => r = back,
        }
        if s.stop.load(Acquire) && t0.elapsed() > Duration::from_secs(5) {
            // A call has been inside for longer than any deadline allows: leave the mapping
            // rather than unmap under it (sdk-contract §6, close()).
            eprintln!("sidecarzero: an arena is still in use after 5 s; leaving it mapped");
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(crate) fn background(s: Arc<Shared>) {
    let mut backoff = BACKOFF_MIN;
    let mut busy_since: Option<std::time::Instant> = None;
    let mut warned_abi = false;
    while !s.stop.load(Acquire) {
        set_state(&s, ClientState::Connecting);
        match connect_once(&s) {
            Ok((sock, snap)) => {
                // A forked child closes its copy of the session socket (sdk-contract §7).
                let fork_reg = scz_shm::fork::close_in_child(sock.as_raw_fd());
                busy_since = None;
                backoff = BACKOFF_MIN;
                warned_abi = false;
                let (arena, session_id) = (snap.arena.clone(), snap.session_id);
                let rejected = matches!(snap.binding.mode, Mode::Mismatch);
                match s.published.replace(Some(Box::new(snap))) {
                    Ok(old) => debug_assert!(old.is_none()),
                    Err(_) => continue, // too many arenas awaiting retirement; cannot happen here
                }
                set_state(&s, if rejected { ClientState::SchemaRejected } else { ClientState::Bound });
                s.sessions.fetch_add(1, Relaxed);
                hold(&s, &sock, &arena, session_id);
                drop(arena);
                // Retirement starts now, before any reconnect (sdk-contract §6).
                set_state(&s, ClientState::Disconnected);
                if let Ok(Some(r)) = s.published.replace(None) {
                    retire(&s, r);
                }
                drop(fork_reg);
                drop(sock); // last: its closing tells the supervisor nothing maps the arena
            }
            Err(ConnErr::Busy) => {
                let since = *busy_since.get_or_insert_with(std::time::Instant::now);
                set_state(&s, ClientState::Disconnected);
                if since.elapsed() < Duration::from_secs(1) {
                    nap(&s, Duration::from_millis(20));
                } else {
                    nap(&s, jitter(backoff));
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
            Err(ConnErr::Abi) => {
                set_state(&s, ClientState::Rejected);
                if !warned_abi {
                    eprintln!("sidecarzero: the sidecar at {} does not speak this SDK's ABI; serving fallbacks, retrying every 30 s", s.endpoint);
                    warned_abi = true;
                }
                nap(&s, Duration::from_secs(30));
            }
            Err(ConnErr::Other) => {
                set_state(&s, ClientState::Disconnected);
                nap(&s, jitter(backoff));
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
    set_state(&s, ClientState::Disconnected);
}
