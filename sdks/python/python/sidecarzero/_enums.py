"""Where an answer came from, and why the fallback gave it (sdk-contract §2, §3.3).

The members are the metric labels. The native core hands out these exact objects, so
``outcome.source is Source.MODEL`` holds.
"""

import enum


class Source(enum.Enum):
    """Who answered a call."""

    MODEL = "MODEL"
    FALLBACK = "FALLBACK"

    def __str__(self) -> str:
        return self.value


class FallbackReason(enum.Enum):
    """Why a call returned the fallback's answer (sdk-contract §3.3). A minor version may add
    reasons: do not match on them exhaustively."""

    NOT_CONNECTED = "NOT_CONNECTED"
    ABI_MISMATCH = "ABI_MISMATCH"
    SCHEMA_MISMATCH = "SCHEMA_MISMATCH"
    REBINDING = "REBINDING"
    GATE_INIT = "GATE_INIT"
    GATE_SHEDDING = "GATE_SHEDDING"
    GATE_RELOADING = "GATE_RELOADING"
    GATE_DEAD = "GATE_DEAD"
    GATE_DRAINING = "GATE_DRAINING"
    STALE_HEARTBEAT = "STALE_HEARTBEAT"
    CLOCK_FAULT = "CLOCK_FAULT"
    OVERLOADED = "OVERLOADED"
    TOO_LARGE = "TOO_LARGE"
    NO_SLOT = "NO_SLOT"
    CODEC_ERROR = "CODEC_ERROR"
    RING_FULL = "RING_FULL"
    DEADLINE = "DEADLINE"
    WORKER_FAILED = "WORKER_FAILED"
    MODEL_ERROR = "MODEL_ERROR"
    BAD_REQUEST = "BAD_REQUEST"
    RESOURCE_EXHAUSTED = "RESOURCE_EXHAUSTED"

    def __str__(self) -> str:
        return self.value
