"""SidecarZero for Python: model inference through shared memory, with a fallback that always
answers (sdk-python §3).

    from sidecarzero import Client, Source
    from sidecarzero.codecs import DenseF32ToScore

    def heuristic(features, reason):
        return 0.1

    with Client("/run/sidecarzero/scz.sock", codec=DenseF32ToScore(256), fallback=heuristic) as client:
        out = client.infer(features, budget_ms=2.0)
        out.value, out.source, out.reason, out.model_gen

``fallback`` is a required keyword-only argument. ``infer`` raises nothing of its own: only an
exception from the fallback can come out of it.

The client is the Rust client (``sdks/rust``) underneath, through the ``_core`` extension.
It connects only on Linux; elsewhere every call falls back with ``NOT_CONNECTED``.
"""

from typing import Protocol, TypeVar

from ._enums import FallbackReason, Source
from ._core import LINUX, VERSION, Client, Outcome, Unwound
from . import codecs, fallbacks

__version__ = VERSION

_In = TypeVar("_In", contravariant=True)
_Out = TypeVar("_Out", covariant=True)


class FallbackPolicy(Protocol[_In, _Out]):
    """The caller's own answer: pure, fast, and able to take any input the caller can pass,
    including one the codec refused (sdk-contract §3.2)."""

    def __call__(self, input: _In, reason: FallbackReason, /) -> _Out: ...


__all__ = [
    "Client",
    "Outcome",
    "Source",
    "FallbackReason",
    "FallbackPolicy",
    "Unwound",
    "codecs",
    "fallbacks",
    "LINUX",
    "__version__",
]
