"""The Python worker runner (sdk-python §5): a Python model behind the arena.

    python -m sidecarzero.worker my_model        # or a path: .../my_model.py

is started by the supervisor like any worker (supervisor flags in the README). The consume
loop, slot protocol, heartbeat, deadlines and stats run in Rust (scz-worker-core); Python is
entered only for the user module's three functions:

    def load(model_path: str): ...                 # /proc/self/fd/N: the file the supervisor hashed
    def signature(model) -> Signature: ...
    def predict(model, batch: dict[str, numpy.ndarray]) -> numpy.ndarray | dict[str, numpy.ndarray]

``batch`` maps each input column to a ``(rows, *shape)`` NumPy array. Floating-point columns
are read-only views of slot memory, valid only until ``predict`` returns: copy anything that
must be kept, because the slot is reused for another request. Integer (index-like) columns are
copied into private memory first. ``predict`` returns one array per output column (a dict, or a
bare array for a single output), each of the signature's dtype and ``rows`` rows; anything else
completes the request with MODEL_ERROR, as does an exception. A ``MemoryError`` completes it
with RESOURCE_EXHAUSTED; three in a row make the runner exit so the supervisor replaces it.

The RecordBatch form of the design (pyarrow) is not built: pyarrow is not in the image.
"""

from dataclasses import dataclass, field
from typing import Mapping, Optional, Tuple

__all__ = ["Signature", "Tensor", "tensor", "model_path", "model_bytes"]


@dataclass(frozen=True)
class Tensor:
    """A fixed-width column: a NumPy dtype name and the shape of one row (() for a scalar)."""

    dtype: str
    shape: Tuple[int, ...] = ()


def tensor(dtype: str, shape=()) -> Tensor:
    """``tensor("float32", [256])``: one row is 256 float32 values; ``tensor("float32", [])``: a scalar."""
    return Tensor(str(dtype), tuple(int(d) for d in shape))


@dataclass(frozen=True)
class Signature:
    """A model's columns. ``fixed_batch``: the model takes exactly that many rows (worker §4)."""

    inputs: Mapping[str, Tensor]
    outputs: Mapping[str, Tensor]
    fixed_batch: Optional[int] = None


def model_path() -> Optional[str]:
    """The path of the model the supervisor hashed (``/proc/self/fd/N``), inside a runner."""
    from .._core import _worker_model_path

    return _worker_model_path()


def model_bytes() -> Optional[memoryview]:
    """The model file, mapped read-only, inside a runner: no path or filesystem access needed."""
    from .._core import _worker_model_bytes

    return _worker_model_bytes()
