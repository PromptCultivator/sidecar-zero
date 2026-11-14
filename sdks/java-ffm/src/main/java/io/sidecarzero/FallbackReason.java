package io.sidecarzero;

/**
 * Why a call returned the fallback's answer (sdk-contract §3.3). The label set of the fallback
 * metric. A minor version may add reasons: treat the enum as open.
 */
public enum FallbackReason {
    NOT_CONNECTED,
    ABI_MISMATCH,
    SCHEMA_MISMATCH,
    REBINDING,
    GATE_INIT,
    GATE_SHEDDING,
    GATE_RELOADING,
    GATE_DEAD,
    GATE_DRAINING,
    STALE_HEARTBEAT,
    CLOCK_FAULT,
    OVERLOADED,
    TOO_LARGE,
    NO_SLOT,
    CODEC_ERROR,
    RING_FULL,
    DEADLINE,
    WORKER_FAILED,
    MODEL_ERROR,
    BAD_REQUEST,
    RESOURCE_EXHAUSTED;

    private static final FallbackReason[] ALL = values();

    /** The metric label: the constant's name. */
    public String label() {
        return name();
    }

    /** By ordinal, without allocating (values() copies its array). */
    public static FallbackReason of(int ordinal) {
        return ALL[ordinal];
    }

    public static int count() {
        return ALL.length;
    }
}
