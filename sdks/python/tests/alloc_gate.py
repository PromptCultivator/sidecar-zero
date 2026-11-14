"""The Python SDK's allocation gate (testing §7): the interpreter's allocated-block count
(`sys.getallocatedblocks()`) must be flat across calls, on every path of the gate the SDK has.

Python cannot be allocation-free (every call returns a new `Outcome`, and a model answer a
new `bytes`), but nothing may be kept. The model path and the fallback path run a million
calls each; the slower paths (parked, shed, deadline, a new thread per call) run fewer, since
each costs a wait or a thread. Each path is warmed up first, and garbage is collected before
each reading. Exit status 1 if any path grew by more than SLACK blocks.

    PYTHONPATH=target/sdk/python python3 sdks/python/tests/alloc_gate.py [--scale=0.1]
"""

import gc
import signal
import sys
import threading
import time

from sidecarzero import Client, FallbackReason, fallbacks
from sidecarzero.codecs import Bytes

from harness import DELAY, NORMAL, Sidecar, request

SLACK = 64
SCALE = next((float(a.split("=", 1)[1]) for a in sys.argv[1:] if a.startswith("--scale=")), 1.0)


def fb(x, reason):
    return b""


def measure(name, call, n, warm=10_000):
    n = max(int(n * SCALE), 1000)
    for i in range(warm):
        call(i)
    gc.collect()
    b0 = sys.getallocatedblocks()
    t = time.monotonic()
    reasons = {}
    for i in range(n):
        o = call(i)
        r = o.reason
        reasons[r] = reasons.get(r, 0) + 1
    took = time.monotonic() - t
    del o, r
    gc.collect()
    b1 = sys.getallocatedblocks()
    seen = ", ".join("%s %d" % ("MODEL" if r is None else r.value, k) for r, k in reasons.items())
    ok = b1 - b0 <= SLACK
    print("%-34s %8d calls %6.2f us/call  blocks %d -> %d (%+d)  %s  [%s]" % (name, n, took / n * 1e6, b0, b1, b1 - b0, "ok" if ok else "GREW", seen), flush=True)
    return ok


def main():
    ok = True
    # ── no arena: a fallback at admission, NOT_CONNECTED
    with Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=fb) as c:
        x = b"x" * 48
        ok &= measure("fallback (Python), NOT_CONNECTED", lambda i: c.infer(x, 1.0), 1_000_000)
    with Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=fallbacks.constant(b"")) as c:
        ok &= measure("fallback (native), NOT_CONNECTED", lambda i: c.infer(x, 1.0), 1_000_000)

    with Sidecar() as sc:
        sc.wait_healthy()
        c = Client(sc.sock, codec=Bytes(response_len=8), fallback=fb)
        assert c.await_bound(10.0), sc.status()
        req = request(NORMAL, 0, 0, 0, rows=8)
        ok &= measure("model, spinning", lambda i: c.infer(req, 20.0), 1_000_000)

        # A fallback at admission with an arena mapped: the supervisor stopped, its heartbeat stale.
        sc.signal(signal.SIGSTOP)
        try:
            time.sleep(0.3)
            ok &= measure("fallback at admission, STALE_HB", lambda i: c.infer(req, 20.0), 1_000_000)
        finally:
            sc.signal(signal.SIGCONT)
        t = time.monotonic()
        while c.infer(req, 20.0).reason is not None:
            assert time.monotonic() - t < 10, sc.status()
            time.sleep(0.01)

        # Spinning off and a worker that takes 20 us: every call parks in futex and is woken.
        c.set_spin_ceiling(0.0)
        slowish = request(DELAY, 20, 0, 3)
        ok &= measure("model, after parking", lambda i: c.infer(slowish, 20.0), 100_000, warm=2000)
        c.set_spin_ceiling(None)

        # Misses in a row open the breaker: shed calls, with a probe now and then.
        slow = request(DELAY, 3000, 0, 1)
        ok &= measure("shed (OVERLOADED)", lambda i: c.infer(slow, 1.0), 200_000, warm=2000)
        # Past the breaker's longest interval, the next model answer closes it.
        t = time.monotonic()
        while c.infer(req, 20.0).reason is not None:
            assert time.monotonic() - t < 10, sc.status()
            time.sleep(0.01)

        # A deadline on every other call; the model answer between them keeps the breaker shut.
        # The worker takes 2.5 ms against a 0.2 ms budget: the caller abandons the slot, or the
        # worker drops the request when it dequeues it (both DEADLINE). The container's timer
        # wakes parked threads up to ~1 ms late, so now and then the answer wins the race.
        late = request(DELAY, 2500, 0, 2)
        ok &= measure("deadline / abandon", lambda i: c.infer(late, 0.2) if i % 2 == 0 else c.infer(req, 20.0), 10_000, warm=500)

        def on_new_thread(i):
            box = []
            th = threading.Thread(target=lambda: box.append(c.infer(req, 20.0)))
            th.start()
            th.join()
            return box[0]

        ok &= measure("first call from a new thread", on_new_thread, 20_000, warm=1000)
        c.close()
    print("allocation gate: %s" % ("flat" if ok else "GREW"), flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
