"""The Python half of the runner: importing the user module, checking its signature, and
building and checking each request's arrays. The native core calls these with the GIL held."""

import gc
import importlib
import importlib.util
import math
import os
import sys
from collections.abc import Mapping

import numpy as np

from . import Signature, Tensor

# abi/scz_abi.toml [enums.elem_type]
ELEM = {"int8": 1, "int16": 2, "int32": 3, "int64": 4, "uint8": 5, "uint16": 6, "uint32": 7, "uint64": 8, "float16": 9, "float32": 10, "float64": 11}


class Mismatch(Exception):
    """predict's answer does not match the response template."""


def import_module(spec):
    if spec.endswith(".py") or os.sep in spec:
        path = os.path.abspath(spec)
        name = os.path.splitext(os.path.basename(path))[0]
        s = importlib.util.spec_from_file_location(name, path)
        if s is None or s.loader is None:
            raise ImportError("cannot load %s" % path)
        m = importlib.util.module_from_spec(s)
        sys.modules[name] = m
        s.loader.exec_module(m)
        return m
    return importlib.import_module(spec)


def columns(spec, what):
    if not isinstance(spec, Mapping) or not spec:
        raise TypeError("Signature.%s must be a non-empty dict of name -> tensor(...)" % what)
    out = []
    for name, t in spec.items():
        if not isinstance(t, Tensor):
            raise TypeError("%s column %r: not a sidecarzero.worker.tensor(...)" % (what, name))
        dt = np.dtype(t.dtype)
        if dt.name not in ELEM or dt.byteorder == ">":
            raise TypeError("%s column %r: dtype %s is not a little-endian fixed-width number" % (what, name, dt))
        n = math.prod(t.shape)
        if n < 1:
            raise TypeError("%s column %r: shape %s holds no values" % (what, name, t.shape))
        # (name, elem_type, list_size, dtype, row shape, copied into private memory)
        out.append((str(name), ELEM[dt.name], n, dt.newbyteorder("<").str, tuple(t.shape), dt.kind in "iu"))
    return out


def load(spec, model_path):
    m = import_module(spec)
    for f in ("load", "signature", "predict"):
        if not callable(getattr(m, f, None)):
            raise TypeError("model module %s has no %s()" % (spec, f))
    model = m.load(model_path)
    sig = m.signature(model)
    if not isinstance(sig, Signature):
        raise TypeError("%s.signature() returned %s, not a sidecarzero.worker.Signature" % (spec, type(sig).__name__))
    fixed = sig.fixed_batch
    if fixed is not None and (not isinstance(fixed, int) or fixed < 1):
        raise TypeError("Signature.fixed_batch must be a positive int or None")
    inputs, outputs = columns(sig.inputs, "inputs"), columns(sig.outputs, "outputs")
    return (m.predict, model, inputs, outputs), fixed


def run(plan, buffers, rows):
    """One request: arrays over `buffers` (slot views, or private copies), predict, check."""
    predict, model, inputs, outputs = plan
    batch = {}
    for (name, _, _, dt, shape, _), buf in zip(inputs, buffers):
        batch[name] = np.frombuffer(buf, dtype=dt).reshape((rows,) + shape)
    return check(predict(model, batch), rows, outputs)


def check(y, rows, outputs):
    if not isinstance(y, Mapping):
        if len(outputs) != 1:
            raise Mismatch("predict returned a %s; this model has %d outputs, so return a dict" % (type(y).__name__, len(outputs)))
        y = {outputs[0][0]: y}
    names = [o[0] for o in outputs]
    extra = [k for k in y if k not in names]
    if extra:
        raise Mismatch("predict returned columns the signature does not have: %s" % extra)
    out = []
    for name, _, n, dt, shape, _ in outputs:
        if name not in y:
            raise Mismatch("predict returned no %r column" % name)
        a = y[name]
        if not isinstance(a, np.ndarray):
            raise Mismatch("column %r is a %s, not a NumPy array" % (name, type(a).__name__))
        if a.dtype != np.dtype(dt):
            raise Mismatch("column %r is %s; the signature says %s" % (name, a.dtype, np.dtype(dt)))
        if a.ndim < 1 or a.shape[0] != rows or a.size != rows * n:
            raise Mismatch("column %r has shape %s; the request has %d rows of %s" % (name, a.shape, rows, shape or "one value"))
        out.append(np.ascontiguousarray(a))
    return out


def warm(plan, rows):
    """worker §3 step 7: one run on private zeros at `rows` rows."""
    inputs = plan[2]
    bufs = [bytearray(rows * n * np.dtype(dt).itemsize) for (_, _, n, dt, _, _) in inputs]
    run(plan, bufs, rows)


def trim():
    gc.collect()
