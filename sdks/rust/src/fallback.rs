//! Fallbacks and their reasons (sdk-contract §3).

/// Why a call returned the fallback's answer. A minor version may add reasons.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FallbackReason {
    NotConnected,
    AbiMismatch,
    SchemaMismatch,
    Rebinding,
    GateInit,
    GateShedding,
    GateReloading,
    GateDead,
    GateDraining,
    StaleHeartbeat,
    ClockFault,
    Overloaded,
    TooLarge,
    NoSlot,
    CodecError,
    RingFull,
    Deadline,
    WorkerFailed,
    ModelError,
    BadRequest,
    ResourceExhausted,
}

impl FallbackReason {
    pub const ALL: [FallbackReason; 21] = [
        Self::NotConnected,
        Self::AbiMismatch,
        Self::SchemaMismatch,
        Self::Rebinding,
        Self::GateInit,
        Self::GateShedding,
        Self::GateReloading,
        Self::GateDead,
        Self::GateDraining,
        Self::StaleHeartbeat,
        Self::ClockFault,
        Self::Overloaded,
        Self::TooLarge,
        Self::NoSlot,
        Self::CodecError,
        Self::RingFull,
        Self::Deadline,
        Self::WorkerFailed,
        Self::ModelError,
        Self::BadRequest,
        Self::ResourceExhausted,
    ];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotConnected => "NOT_CONNECTED",
            Self::AbiMismatch => "ABI_MISMATCH",
            Self::SchemaMismatch => "SCHEMA_MISMATCH",
            Self::Rebinding => "REBINDING",
            Self::GateInit => "GATE_INIT",
            Self::GateShedding => "GATE_SHEDDING",
            Self::GateReloading => "GATE_RELOADING",
            Self::GateDead => "GATE_DEAD",
            Self::GateDraining => "GATE_DRAINING",
            Self::StaleHeartbeat => "STALE_HEARTBEAT",
            Self::ClockFault => "CLOCK_FAULT",
            Self::Overloaded => "OVERLOADED",
            Self::TooLarge => "TOO_LARGE",
            Self::NoSlot => "NO_SLOT",
            Self::CodecError => "CODEC_ERROR",
            Self::RingFull => "RING_FULL",
            Self::Deadline => "DEADLINE",
            Self::WorkerFailed => "WORKER_FAILED",
            Self::ModelError => "MODEL_ERROR",
            Self::BadRequest => "BAD_REQUEST",
            Self::ResourceExhausted => "RESOURCE_EXHAUSTED",
        }
    }
}

impl std::fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The caller's own answer, used whenever the model's is not available in time. It must be
/// pure, fast, and accept any input (sdk-contract §3.2).
pub trait Fallback<In: ?Sized, Out> {
    fn apply(&self, input: &In, reason: FallbackReason) -> Out;
}

impl<In: ?Sized, Out, F: Fn(&In, FallbackReason) -> Out> Fallback<In, Out> for F {
    #[inline]
    fn apply(&self, input: &In, reason: FallbackReason) -> Out {
        self(input, reason)
    }
}

/// Where an answer came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Model { model_gen: u32 },
    Fallback { reason: FallbackReason },
}

/// What `infer` returns: always a value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome<Out> {
    pub value: Out,
    pub source: Source,
}

impl<Out> Outcome<Out> {
    pub fn is_model(&self) -> bool {
        matches!(self.source, Source::Model { .. })
    }
    pub fn reason(&self) -> Option<FallbackReason> {
        match self.source {
            Source::Fallback { reason } => Some(reason),
            Source::Model { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_are_dense() {
        for (i, r) in FallbackReason::ALL.iter().enumerate() {
            assert_eq!(r.index(), i);
        }
    }
}
