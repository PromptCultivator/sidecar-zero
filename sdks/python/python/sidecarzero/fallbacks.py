"""Native fallbacks (sdk-python §4): descriptions the core evaluates in Rust, without calling
into Python. Checked against the codec when the client is built.

    fallbacks.constant(0.1)                                   any codec
    fallbacks.threshold(feature=17, gt=0.8, then=0.9, otherwise=0.1)
    fallbacks.linear(weights=w, bias=b, link="sigmoid")       len(w) == the codec's n
    fallbacks.lookup(feature=3, table=[...], otherwise=0.0)   table[floor(x[3])]

A dense input the codec refused gets ``otherwise`` (threshold, lookup) or ``link(bias)``
(linear).
"""

from ._core import NativeFallback, constant, linear, lookup, threshold

__all__ = ["NativeFallback", "constant", "threshold", "linear", "lookup"]
