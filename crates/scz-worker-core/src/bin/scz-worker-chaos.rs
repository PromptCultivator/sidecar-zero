//! scz-worker-chaos: the echo worker with faults on request, for the fault tests (testing §4).
//!
//! Every fault is set by an environment variable, which the supervisor passes through, and
//! applies only to the generation named by SCZ_CHAOS_GEN when that is set, so that the
//! replacement worker is healthy:
//!
//!   SCZ_CHAOS_CRASH_AFTER=N   SIGKILL itself on its Nth request, after claiming the slot
//!   SCZ_CHAOS_HANG_AFTER=N    never finish its Nth request (the inference hangs; the heartbeat goes on)
//!   SCZ_CHAOS_DELAY_US=N      every request takes at least N microseconds
//!   SCZ_CHAOS_LOAD_MS=N       loading takes N milliseconds before WARM
//!   SCZ_CHAOS_FAIL_LOAD=1     exit during loading, before WARM
//!   SCZ_CHAOS_GEN=G           apply the faults above only in generation G
//!
//! A request in the conformance format (`scz_chaos_proto`) carries its own fault: an opcode
//! and argument, applied in every generation, and is answered with its keyed value. Anything
//! else is echoed.
//!
//! The model file can be a script (worker §7.4, "per process"): a first line
//! `SCZCHAOS-SCRIPT`, then `key=value` lines with the keys above in lower case and without the
//! prefix (`crash_after=1`, `load_ms=500`, `fail_load=1`, `hang_after=3`, `delay_us=100`).
//! A script applies to every generation that loads that file, which is how a test publishes a
//! model that crashes on real requests and has the supervisor roll it back.
//!
//! One key exists only in scripts: `scribble=N`, the hostile worker of testing §3. Once active,
//! a thread writes N random 64-bit words a millisecond over the data segment's ring headers and
//! cells (one in eight), slot words and records (half), and response regions, while the worker
//! goes on serving. Memory faults for the deployment tests (kubernetes-deploy §10): `leak_mb=N`
//! allocates and touches N MiB a second once active, and `spike_after_ms=N` maps and populates
//! 1 GiB at once, N milliseconds after activation, faster than the memory monitor can act; `gen=G` applies a
//! script's faults only in generation G, so that the replacement is healthy. Two targeted patterns: `scribble_seq=N` sets the sequence number of the cell
//! at each ring's `tail` ahead of it N times a millisecond, and `flip_waiter=1` flips the
//! `WAITER` bit of every slot word in class 0 continuously.

use scz_chaos_proto::{self as proto, op};
use scz_shm::abi::{self, status};
use scz_shm::layout::Directory;
use scz_shm::seg::Seg;
use scz_shm::view::{SlotReader, SlotWriter};
use scz_worker_core::Backend;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

fn env(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

struct Chaos {
    armed: AtomicBool,
    served: AtomicU64,
    crash_after: Option<u64>,
    hang_after: Option<u64>,
    delay: Option<Duration>,
    load: Option<Duration>,
    fail_load: bool,
    /// Random words written a millisecond, once active (scripts only).
    scribble: Option<u64>,
    /// Ring tails' cells given a sequence number ahead of `tail`, a millisecond.
    scribble_seq: Option<u64>,
    /// Flip the WAITER bits of class 0's slot words, continuously.
    flip_waiter: bool,
    /// MiB allocated and touched a second once active (scripts only).
    leak_mb: Option<u64>,
    /// Milliseconds after activation to start allocating without limit (scripts only).
    spike_after_ms: Option<u64>,
    only_gen: Option<u64>,
    /// Set from the model file's script, if it is one; it takes precedence.
    script: std::sync::OnceLock<Box<Chaos>>,
}

impl Chaos {
    fn from_script(text: &str) -> Chaos {
        let get = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k)?.strip_prefix('=')?.parse::<u64>().ok());
        Chaos {
            armed: AtomicBool::new(true),
            served: AtomicU64::new(0),
            crash_after: get("crash_after"),
            hang_after: get("hang_after"),
            delay: get("delay_us").map(Duration::from_micros),
            load: get("load_ms").map(Duration::from_millis),
            fail_load: get("fail_load") == Some(1),
            scribble: get("scribble"),
            scribble_seq: get("scribble_seq"),
            flip_waiter: get("flip_waiter") == Some(1),
            leak_mb: get("leak_mb"),
            spike_after_ms: get("spike_after_ms"),
            only_gen: get("gen"),
            script: std::sync::OnceLock::new(),
        }
    }
    /// The faults in force: the script's, or the environment's.
    fn rules(&self) -> &Chaos {
        self.script.get().map(|b| &**b).unwrap_or(self)
    }
}

impl Backend for Chaos {
    fn load(&self, gen: u32, model: &'static [u8]) {
        if let Some(text) = std::str::from_utf8(model).ok().and_then(|t| t.strip_prefix("SCZCHAOS-SCRIPT\n")) {
            let _ = self.script.set(Box::new(Chaos::from_script(text)));
        }
        let r = self.rules();
        let armed = r.only_gen.is_none_or(|g| g == gen as u64);
        self.armed.store(armed, Relaxed);
        if armed {
            if let Some(d) = r.load {
                std::thread::sleep(d);
            }
            if r.fail_load {
                eprintln!("scz-worker-chaos: failing the load, as asked");
                std::process::exit(4);
            }
        }
    }

    fn infer(&self, req: &SlotReader<'_>, resp: &mut SlotWriter<'_>) -> u32 {
        let n = self.served.fetch_add(1, Relaxed) + 1;
        let r = self.rules();
        if self.armed.load(Relaxed) {
            if r.crash_after == Some(n) {
                // SAFETY: raising a signal has no memory-safety preconditions.
                unsafe { libc::raise(libc::SIGKILL) };
            }
            if r.hang_after == Some(n) {
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            if let Some(d) = r.delay {
                std::thread::sleep(d);
            }
        }
        // Requests are small; a keyed one is read whole to be hashed.
        let mut buf = [0u8; 4096];
        let n = req.len().min(buf.len());
        if req.copy_to(0, &mut buf[..n]).is_ok() {
            if let Some((code, arg)) = proto::parse(&buf[..n]) {
                return keyed(code, arg, &buf[..n], resp);
            }
        }
        match resp.copy_from(req) {
            Ok(()) => status::OK,
            Err(_) => status::RESOURCE_EXHAUSTED,
        }
    }

    fn attach(&'static self, data: Seg<'static>, dir: &'static Directory) {
        if !self.armed.load(Relaxed) {
            return;
        }
        if let Some(mb) = self.rules().leak_mb.filter(|&n| n > 0) {
            eprintln!("scz-worker-chaos: leaking {mb} MiB a second, as asked");
            std::thread::spawn(move || loop {
                hold(mb << 20);
                std::thread::sleep(Duration::from_secs(1));
            });
        }
        if let Some(ms) = self.rules().spike_after_ms {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                eprintln!("scz-worker-chaos: allocating without limit, as asked");
                spike();
            });
        }
        if let Some(rate) = self.rules().scribble.filter(|&n| n > 0) {
            eprintln!("scz-worker-chaos: scribbling {rate} words a millisecond over the data segment, as asked");
            std::thread::spawn(move || scribble(data, dir, rate));
        }
        if let Some(rate) = self.rules().scribble_seq.filter(|&n| n > 0) {
            eprintln!("scz-worker-chaos: setting ring cells' sequence numbers ahead of tail, as asked");
            std::thread::spawn(move || seq_ahead(data, dir, rate));
        }
        if self.rules().flip_waiter {
            eprintln!("scz-worker-chaos: flipping slot WAITER bits, as asked");
            std::thread::spawn(move || flip_waiter(data, dir));
        }
    }

    fn before_wake(&self) {
        let ms = STALL.take();
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }

    fn finish(&self, status: &mut u32, resp_len: &mut usize) {
        match FINISH.take() {
            Finish::Nothing => {}
            Finish::Crash => die(),
            Finish::Length(n) => {
                *status = status::OK;
                *resp_len = n;
            }
        }
    }
}

/// What `finish` does for the request this thread just served.
#[derive(Clone, Copy, Default)]
enum Finish {
    #[default]
    Nothing,
    Crash,
    Length(usize),
}

thread_local! {
    static FINISH: Cell<Finish> = const { Cell::new(Finish::Nothing) };
    /// Milliseconds to wait before waking the client of the request just served.
    static STALL: Cell<u64> = const { Cell::new(0) };
}

/// Allocate `bytes`, touch every page so it is charged, and never free it. The writes are
/// volatile and the buffer escapes: an allocation nothing reads is otherwise removed by the
/// optimizer, and the leak with it.
fn hold(bytes: u64) {
    let v: &'static mut [u8] = vec![0u8; bytes as usize].leak();
    for i in (0..v.len()).step_by(4096) {
        // SAFETY: an in-bounds byte of a buffer this function owns.
        unsafe { std::ptr::write_volatile(v.as_mut_ptr().add(i), 1) };
    }
    std::hint::black_box(v);
}

/// A spike that outruns the supervisor's 10 ms memory monitor: one mapping of 1 GiB that the
/// kernel populates in a single call, far faster than page faults taken one at a time. In a
/// container with a smaller limit the kernel's OOM killer runs inside the call
/// (kubernetes-deploy §5, Layer 1).
fn spike() -> ! {
    loop {
        // SAFETY: a fresh anonymous mapping, never used.
        unsafe {
            libc::mmap(core::ptr::null_mut(), 1 << 30, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE, -1, 0);
        }
    }
}

fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The hostile worker: random words over the parts of the segment a client reads and writes.
/// Half of them are small numbers, which reach the code paths a random 64-bit value would not
/// (a state, an epoch, a length just out of range).
fn scribble(data: Seg<'static>, dir: &'static Directory, rate: u64) -> ! {
    let mut x = std::process::id() as u64 ^ scz_shm::wait::monotonic_ns();
    let rings = &dir.layout.rings;
    let slabs = &dir.layout.slabs;
    loop {
        for _ in 0..rate {
            let r = splitmix(&mut x);
            // One word in eight goes to a ring: a corrupt ring header stalls every caller until
            // the session is reset, and the rest are spread over the slots.
            let off = match r % 8 {
                0 if !rings.is_empty() => {
                    let (ring_off, cap) = rings[(r >> 8) as usize % rings.len()];
                    let span = abi::RING_HEADER_LEN + cap * abi::CELL_LEN;
                    ring_off + (splitmix(&mut x) % span) / 8 * 8
                }
                1..=4 if !slabs.is_empty() => {
                    let (table, _, c) = slabs[(r >> 8) as usize % slabs.len()];
                    let i = splitmix(&mut x) % c.slot_count.max(1) as u64;
                    table + i * abi::SLOT_RECORD_LEN + (splitmix(&mut x) % abi::SLOT_RECORD_LEN) / 8 * 8
                }
                _ if !slabs.is_empty() => {
                    let (_, base, c) = slabs[(r >> 8) as usize % slabs.len()];
                    let i = splitmix(&mut x) % c.slot_count.max(1) as u64;
                    let region = base + i * c.slot_size as u64 + c.req_cap as u64;
                    region + (splitmix(&mut x) % c.resp_cap.max(8) as u64) / 8 * 8
                }
                _ => continue,
            };
            let v = splitmix(&mut x);
            let v = if v & 1 == 0 { v >> 1 } else { v % 16 };
            if (off as usize) + 8 <= data.len() {
                data.u64_at(off as usize).store(v, Relaxed);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A cell sequence number left ahead of `tail` (testing §3, "Hostile worker"): a producer sees
/// the cell as taken by another and goes round again, until its attempts run out.
fn seq_ahead(data: Seg<'static>, dir: &'static Directory, rate: u64) -> ! {
    let mut x = std::process::id() as u64;
    loop {
        for _ in 0..rate {
            for &(ring_off, cap) in &dir.layout.rings {
                let tail = data.u64_at(ring_off as usize + abi::ring_header::TAIL).load(Relaxed);
                let cell = ring_off + abi::RING_HEADER_LEN + (tail & (cap - 1)) * abi::CELL_LEN;
                data.u64_at(cell as usize).store(tail.wrapping_add(1 + splitmix(&mut x) % cap), Relaxed);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A slot's WAITER bit flipped continuously: every CAS on that word may fail, and each loop
/// over it must stay bounded.
fn flip_waiter(data: Seg<'static>, dir: &'static Directory) -> ! {
    let Some(&(table, _, c)) = dir.layout.slabs.first() else {
        loop {
            std::thread::sleep(Duration::from_secs(1))
        }
    };
    loop {
        for i in 0..c.slot_count as u64 {
            data.u64_at((table + i * abi::SLOT_RECORD_LEN) as usize).fetch_xor(1 << 8, Relaxed);
        }
        std::hint::spin_loop();
    }
}

fn die() -> ! {
    // SAFETY: raising a signal has no memory-safety preconditions.
    unsafe { libc::raise(libc::SIGKILL) };
    unreachable!()
}

/// A conformance request: apply its opcode, then answer with its keyed value.
fn keyed(code: u64, arg: u64, req: &[u8], resp: &mut SlotWriter<'_>) -> u32 {
    match code {
        // A sleep shorter than a couple of milliseconds lasts as long as the host's timer makes
        // it (a millisecond in some VMs), so short delays spin on the clock instead.
        op::DELAY if arg <= 2000 => {
            let until = std::time::Instant::now() + Duration::from_micros(arg);
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
        }
        op::DELAY => std::thread::sleep(Duration::from_micros(arg)),
        op::CRASH if arg == 2 => FINISH.set(Finish::Crash),
        op::CRASH => die(),
        op::HANG if arg == 0 => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        op::HANG => std::thread::sleep(Duration::from_millis(arg)),
        op::STATUS => return arg as u32,
        op::BAD_LEN => FINISH.set(Finish::Length(arg as usize)),
        // Work for 3 ms before completing, so the caller has parked by then: a slot completed
        // before its caller parks needs no wake, and no wake-up can be late. Then stall the
        // wake by `arg` ms (Backend::before_wake).
        op::STALL_WAKE => {
            std::thread::sleep(Duration::from_millis(3));
            STALL.set(arg)
        }
        _ => {}
    }
    match resp.write(&proto::answer(req).to_le_bytes()) {
        Ok(()) => status::OK,
        Err(_) => status::RESOURCE_EXHAUSTED,
    }
}

fn main() {
    scz_worker_core::run(Chaos {
        armed: AtomicBool::new(false),
        served: AtomicU64::new(0),
        crash_after: env("SCZ_CHAOS_CRASH_AFTER"),
        hang_after: env("SCZ_CHAOS_HANG_AFTER"),
        delay: env("SCZ_CHAOS_DELAY_US").map(Duration::from_micros),
        load: env("SCZ_CHAOS_LOAD_MS").map(Duration::from_millis),
        fail_load: env("SCZ_CHAOS_FAIL_LOAD") == Some(1),
        scribble: None,
        scribble_seq: None,
        flip_waiter: false,
        leak_mb: None,
        spike_after_ms: None,
        only_gen: env("SCZ_CHAOS_GEN"),
        script: std::sync::OnceLock::new(),
    })
}
