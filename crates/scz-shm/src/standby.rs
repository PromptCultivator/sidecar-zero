//! Activation (supervisor §5.3; worker §3 step 8): a warm worker sleeps on `standby_wait`
//! until the gate carries its generation.
//!
//! The supervisor stores the gate, then bumps `standby_wait` and wakes every sleeper. A
//! waiter loads `standby_wait` before the gate, and sleeps only while `standby_wait` still
//! holds what it loaded. So either it reads the bump (and with it the gate store that came
//! before), or its futex wait sees the word changed, or it is asleep when the wake comes.

use crate::sync::{AtomicU32, AtomicU64, Ordering::*};
use crate::word::gate_gen;

pub trait StandbyMem {
    fn gate(&self) -> &AtomicU64;
    fn standby_wait(&self) -> &AtomicU32;
    /// Sleep while `standby_wait` holds `expected`. May return early.
    fn sleep(&self, expected: u32);
    fn wake_all(&self);
}

/// Worker side: return once the gate names generation `gen`.
pub fn wait_for_gen(m: &impl StandbyMem, gen: u32) {
    loop {
        let v = m.standby_wait().load(Acquire); // before the gate
        if gate_gen(m.gate().load(Acquire)) == gen {
            return;
        }
        m.sleep(v);
    }
}

/// Supervisor side, after the gate has been stored: bump the word and wake every waiter.
pub fn bump_and_wake(m: &impl StandbyMem) {
    m.standby_wait().fetch_add(1, AcqRel);
    m.wake_all();
}

/// The words in ordinary memory, for tests and models.
pub struct LocalStandby {
    pub gate: AtomicU64,
    pub standby_wait: AtomicU32,
    #[cfg(loom)]
    pub futex: crate::wait::MockFutex,
}

impl LocalStandby {
    pub fn new(gate: u64) -> Self {
        LocalStandby {
            gate: AtomicU64::new(gate),
            standby_wait: AtomicU32::new(0),
            #[cfg(loom)]
            futex: crate::wait::MockFutex::new(),
        }
    }
}

impl StandbyMem for LocalStandby {
    fn gate(&self) -> &AtomicU64 {
        &self.gate
    }
    fn standby_wait(&self) -> &AtomicU32 {
        &self.standby_wait
    }
    fn sleep(&self, expected: u32) {
        #[cfg(loom)]
        self.futex.wait32(&self.standby_wait, expected);
        #[cfg(not(loom))]
        crate::wait::futex_wait(&self.standby_wait, expected, None);
    }
    fn wake_all(&self) {
        #[cfg(loom)]
        self.futex.wake();
        #[cfg(not(loom))]
        crate::wait::futex_wake(&self.standby_wait, i32::MAX as u32);
    }
}

/// The words in a mapped control segment.
#[cfg(not(loom))]
pub struct ShmStandby<'a>(pub crate::seg::Seg<'a>);

#[cfg(not(loom))]
impl StandbyMem for ShmStandby<'_> {
    fn gate(&self) -> &AtomicU64 {
        self.0.u64_at(crate::abi::control_header::GATE)
    }
    fn standby_wait(&self) -> &AtomicU32 {
        self.0.u32_at(crate::abi::control_header::STANDBY_WAIT)
    }
    fn sleep(&self, expected: u32) {
        crate::wait::futex_wait(self.standby_wait(), expected, None);
    }
    fn wake_all(&self) {
        crate::wait::futex_wake(self.standby_wait(), i32::MAX as u32);
    }
}
