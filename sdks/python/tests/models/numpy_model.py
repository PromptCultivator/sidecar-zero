"""A NumPy model for the runner's tests (sdk-python §5).

`features: float32[256]` in, `score: float32` out, the fraud model's schema, so the dense codec
of every SDK can call it. Each row's score is keyed to the row: (x[1] + 1000·x[2])·w[0] + w[1],
with w read from the model file (a .npy array). x[0] of the first row is an opcode, like the
chaos worker's: it makes predict misbehave on exactly the call that carries it.
"""

import os
import time

import numpy as np

from sidecarzero.worker import Signature, model_bytes, tensor

NORMAL, RAISE, ROWS, DTYPE, HANG, LEAK, SLOW, MISSING = range(8)

_kept = []  # what LEAK keeps
# The socket-pair probe: 1 all as the runner's profile says (worker §7.3), 0.5 socketpair but
# not send(2), 0.25 a sendto naming an address got through, -1 socketpair refused.
_probe = None


def load(model_path):
    global _probe
    w = np.load(model_path)
    assert bytes(model_bytes()[:6]) == b"\x93NUMPY", "the mapped model is the file the supervisor hashed"
    if os.environ.get("SCZ_TEST_PROBE") == "socketpair":
        # asyncio and multiprocessing make socket pairs for their own plumbing.
        try:
            import asyncio
            import multiprocessing

            loop = asyncio.new_event_loop()
            loop.run_until_complete(asyncio.sleep(0))
            loop.close()
            a, b = multiprocessing.Pipe()
            a.send(42)
            assert b.recv() == 42
            _probe = 1.0
        except PermissionError:
            _probe = -1.0
        if _probe == 1.0:
            # send(2) on a socket pair, which asyncio's self-pipe wake-up uses: allowed. A
            # sendto naming an address, the one way to a named socket without connect: refused.
            import socket

            a, b = socket.socketpair()
            try:
                a.send(b"x")
                assert b.recv(1) == b"x"
            except PermissionError:
                _probe = 0.5
            a.close()
            b.close()
            a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
            try:
                a.sendto(b"x", "/tmp/scz-probe.sock")
                _probe = 0.25
            except PermissionError:
                pass
            except OSError:
                _probe = 0.25  # it got past the filter, to the kernel's own refusal
            a.close()
            b.close()
    return w


def signature(model):
    return Signature(inputs={"features": tensor("float32", [256])}, outputs={"score": tensor("float32", [])})


def predict(w, batch):
    x = batch["features"]
    op = int(x[0, 0])
    y = ((x[:, 1] + 1000 * x[:, 2]) * w[0] + w[1]).astype(np.float32)
    if _probe is not None:
        y[:] = _probe
    if op == RAISE:
        raise ValueError("asked to fail")
    if op == ROWS:
        return np.concatenate([y, y[:1]])
    if op == DTYPE:
        return y.astype(np.float64)
    if op == MISSING:
        return {"not_score": y}
    if op == HANG:
        time.sleep(3600)
    if op == LEAK:
        _kept.append(bytearray(int(x[0, 3]) << 20))
    if op == SLOW:
        time.sleep(float(x[0, 3]) / 1000)
    return y
