"""Test support (sdk-python §6, sdk-contract §9).

``InProcessClient`` runs a model function in-process, with no sidecar, on every platform. It
uses the real codecs' input checks and conversions (the same Rust code the client runs), so a
wrong shape falls back with ``CODEC_ERROR`` here exactly as it would against a sidecar.

    client = InProcessClient(DenseF32ToScore(256), model=lambda x: x.sum(axis=1), fallback=heuristic)
    client.inject(Fault.GATE_DEAD)
    assert client.infer(x, budget_ms=1).source is Source.FALLBACK

The model function receives the request as the codec encodes it: ``bytes`` for ``Bytes``; for
``DenseF32ToScore`` a ``(rows, n)`` float32 array (NumPy's when installed, else a memoryview).
It returns the response: bytes-like, or one score per row (a number for a single row). An
exception from the model is answered as a worker would answer it: ``MODEL_ERROR``.

``arm_unwind()`` arms the core's unwind test hook (the conformance agent's ``UNWIND``).
"""

import enum
import threading

from ._core import Bytes, DenseF32ToScore, NativeFallback, Outcome, _arm_unwind
from ._enums import FallbackReason, Source

__all__ = ["InProcessClient", "Fault", "arm_unwind"]

try:  # optional: the model sees an ndarray when NumPy is installed
    import numpy as _np
except ImportError:  # pragma: no cover
    _np = None


class Fault(enum.Enum):
    """A failure to inject: every call falls back with this reason until cleared."""

    NOT_CONNECTED = FallbackReason.NOT_CONNECTED
    GATE_INIT = FallbackReason.GATE_INIT
    GATE_SHEDDING = FallbackReason.GATE_SHEDDING
    GATE_RELOADING = FallbackReason.GATE_RELOADING
    GATE_DEAD = FallbackReason.GATE_DEAD
    GATE_DRAINING = FallbackReason.GATE_DRAINING
    STALE_HEARTBEAT = FallbackReason.STALE_HEARTBEAT
    SCHEMA_MISMATCH = FallbackReason.SCHEMA_MISMATCH
    OVERLOADED = FallbackReason.OVERLOADED
    NO_SLOT = FallbackReason.NO_SLOT
    RING_FULL = FallbackReason.RING_FULL
    DEADLINE = FallbackReason.DEADLINE
    WORKER_FAILED = FallbackReason.WORKER_FAILED
    MODEL_ERROR = FallbackReason.MODEL_ERROR
    BAD_REQUEST = FallbackReason.BAD_REQUEST
    RESOURCE_EXHAUSTED = FallbackReason.RESOURCE_EXHAUSTED


def arm_unwind() -> bool:
    """Make the next enqueue in this process unwind between claiming a ring cell and
    publishing it. False where the core has no rings (off Linux)."""
    return _arm_unwind()


class InProcessClient:
    """A client whose "sidecar" is a function in this process."""

    def __init__(self, codec, *, model, fallback, model_gen: int = 1):
        if fallback is None:
            raise ValueError("fallback must not be None: every call needs an answer when the model has none")
        if not (callable(fallback) or isinstance(fallback, NativeFallback)):
            raise TypeError("fallback must be callable as fallback(input, reason), or a sidecarzero.fallbacks description")
        if not isinstance(codec, (Bytes, DenseF32ToScore)):
            raise TypeError("codec must be a sidecarzero.codecs codec (Bytes or DenseF32ToScore)")
        if isinstance(fallback, NativeFallback):
            fallback._check(codec)
        self._codec, self._model, self._fallback, self._gen = codec, model, fallback, model_gen
        self._fault = None
        self._lock = threading.Lock()
        self._model_calls = 0
        self._fallbacks = {}
        self._closed = False

    def inject(self, fault):
        """Fall back with ``fault`` (a ``Fault`` or ``FallbackReason``) on every call; None clears it."""
        if isinstance(fault, Fault):
            fault = fault.value
        if fault is not None and not isinstance(fault, FallbackReason):
            raise TypeError("fault must be a Fault, a FallbackReason, or None")
        self._fault = fault

    def _fall_back(self, input, reason):
        with self._lock:
            self._fallbacks[reason] = self._fallbacks.get(reason, 0) + 1
        if isinstance(self._fallback, NativeFallback):
            value = self._fallback._apply(self._codec, input)
        else:
            value = self._fallback(input, reason)
        return Outcome(value, Source.FALLBACK, reason)

    def infer(self, input, budget_ms):
        if self._closed:
            return self._fall_back(input, FallbackReason.NOT_CONNECTED)
        if self._fault is not None:
            return self._fall_back(input, self._fault)
        codec = self._codec
        if isinstance(codec, Bytes):
            req = codec._prepare(input)
            if req is None:
                return self._fall_back(input, FallbackReason.CODEC_ERROR)
            try:
                resp = self._model(req)
            except Exception:
                return self._fall_back(input, FallbackReason.MODEL_ERROR)
            value = codec._decode(resp)
        else:
            prepared = codec._prepare(input)
            if prepared is None:
                return self._fall_back(input, FallbackReason.CODEC_ERROR)
            raw, rows, scalar = prepared
            view = memoryview(raw).cast("f", (rows, codec.n))
            x = _np.frombuffer(raw, dtype=_np.float32).reshape(rows, codec.n) if _np is not None else view
            try:
                y = self._model(x)
                scores = [float(y)] if rows == 1 and not hasattr(y, "__len__") else [float(v) for v in y]
            except Exception:
                return self._fall_back(input, FallbackReason.MODEL_ERROR)
            value = codec._decode(scores, rows, scalar)
        if value is None:
            return self._fall_back(input, FallbackReason.CODEC_ERROR)
        with self._lock:
            self._model_calls += 1
        return Outcome(value, Source.MODEL, model_gen=self._gen)

    def await_bound(self, timeout: float = 5.0) -> bool:
        return not self._closed and self._fault is None

    def stats(self):
        with self._lock:
            return {"model": self._model_calls, "fallbacks": dict(self._fallbacks), "sessions": 1, "retired": 0, "late_wakes": 0}

    @property
    def state(self) -> str:
        return "CLOSED" if self._closed else "BOUND"

    def close(self) -> bool:
        self._closed = True
        return True

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False
