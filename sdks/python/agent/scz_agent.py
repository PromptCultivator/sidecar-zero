"""scz-agent-python: the Python SDK's conformance agent (testing §3, "Agent protocol").

It speaks protocol v2 of the Rust agent (sdks/rust/conformance-agent/src/main.rs, whose header
is the reference): it builds one ``sidecarzero.Client`` for ``--endpoint=PATH`` and obeys one
command per line on stdin, answering one line on stdout.

| Command | Reply |
|---|---|
| (on start) | ``HELLO sdk=python version=V caps=C,...`` |
| ``CONFIG threads=N fallback_us=N kind=K fallback=F spin=S codec=C inputs=PATH`` | ``OK``, or ``ERR unsupported`` |
| ``BOUND ms=N`` | ``BOUND true|false`` |
| ``CALL count=N budget_us=N rows=N op=O arg=N seed=N`` | one ``R`` line per call, then ``END calls=N`` |
| ``CODEC_FAULT mode=off|encode|decode`` | ``OK`` |
| ``UNWIND`` | ``OK``: the next call unwinds between claiming a ring cell and publishing it |
| ``CLOCK_SKEW ns=N`` | ``ERR unsupported``: the SDK reads CLOCK_MONOTONIC directly, no cheap clock |
| ``FORK`` | ``FORK src=S reason=R status=N`` |
| ``SPAWN`` | ``SPAWNED pid=N`` |
| ``STATS`` | ``STATS model=N sessions=N retired=N late_wakes=N fb.REASON=N ...`` |
| ``PROC`` | ``PROC fds=N maps=N threads=N`` |
| ``CLOSE`` | ``CLOSED``; later client commands answer ``ERR closed`` |
| ``EXIT`` | ``BYE``, and the process exits |

``kind`` is ``platform`` (a ``threading.Thread`` per calling thread, the default) or
``per-call`` (a new ``threading.Thread`` for every call). The fallback is a Python callable:
it sleeps ``fallback_us``, then answers 0 (zeros for features), or with ``fallback=throws``
raises after recording its reason. Keyed requests are built byte for byte like
``scz_chaos_proto::request``; row k of a features call ``s`` is input row ``(s + k) mod R``.
"""

import os
import struct
import subprocess
import sys
import threading
import time
import warnings

import sidecarzero
from sidecarzero import Unwound
from sidecarzero.codecs import Bytes, DenseF32ToScore
from sidecarzero.testing import arm_unwind

# os.fork() in a process with threads warns; forking here is the point.
warnings.filterwarnings("ignore", category=DeprecationWarning, message=r".*fork.*")

M64 = (1 << 64) - 1
MAGIC = int.from_bytes(b"SCZCHAOS", "little")
OPS = {"normal": 0, "delay": 1, "crash": 2, "hang": 3, "status": 4, "bad_len": 5}
N = 256
CAPS = "threads,per-call,frames,unwind,throws,nospin,fork,spawn" + (
    ",crashpoints" if getattr(sidecarzero._core, "HAS_CRASHPOINTS", False) else ""
)
_clock = time.clock_gettime_ns
_MONO = time.CLOCK_MONOTONIC


def splitmix64(x):
    x = (x + 0x9E3779B97F4A7C15) & M64
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & M64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & M64
    return x ^ (x >> 31)


def request(op, arg, thread, seq, seed, rows):
    n = (thread << 32) | seq
    words = [MAGIC, op & M64, arg & M64, n, seed & M64] + [splitmix64((seed ^ n ^ i) & M64) for i in range(rows)]
    return struct.pack("<%dQ" % len(words), *words)


def parse_op(s):
    if s in OPS:
        return OPS[s]
    try:
        return int(s)
    except ValueError:
        return 0


def kv(words):
    return dict(w.split("=", 1) for w in words if "=" in w)


def num(m, k, default):
    try:
        return int(m[k])
    except (KeyError, ValueError):
        return default


class Thrown(Exception):
    """What the fallback raises with ``fallback=throws``: it carries the reason it was given."""

    def __init__(self, reason):
        super().__init__(reason.value)
        self.reason = reason


def proc_counts():
    fds = len(os.listdir("/proc/self/fd"))
    with open("/proc/self/maps") as f:
        maps = sum(1 for line in f if "memfd:scz-" in line)
    threads = len(os.listdir("/proc/self/task"))
    return "PROC fds=%d maps=%d threads=%d" % (fds, maps, threads)


def read_inputs(path):
    with open(path, "rb") as f:
        b = f.read()
    row = N * 4
    if not b or len(b) % row:
        raise ValueError("%s: not rows of %d float32" % (path, N))
    return [b[i : i + row] for i in range(0, len(b), row)]


class Agent:
    def __init__(self, endpoint):
        self.endpoint = endpoint
        self.threads = 1
        self.per_call = False
        self.spin_off = False
        self.dense = None  # the inputs path while the features codec is in use
        self.inputs = None
        self.fallback_us = 0
        self.throws = False
        self.keyed = Bytes(response_len=8)
        self.features = DenseF32ToScore(N)
        self.spawned = []
        self.client = self.build()

    # ── the client

    def fallback(self, x, reason):
        us = self.fallback_us
        if us:
            time.sleep(us / 1e6)
        if self.throws:
            raise Thrown(reason)
        return 0

    def dense_fallback(self, x, reason):
        self.fallback(x, reason)
        return [0.0] * len(x)

    def build(self):
        if self.dense is None:
            c = sidecarzero.Client(self.endpoint, codec=self.keyed, fallback=self.fallback)
        else:
            self.inputs = read_inputs(self.dense)
            c = sidecarzero.Client(self.endpoint, codec=self.features, fallback=self.dense_fallback)
        c.set_spin_ceiling(0.0 if self.spin_off else None)
        return c

    # ── one call, as its protocol line

    def call(self, c, t, s, op, arg, seed, rows, budget_ms):
        if self.dense is None:
            x = request(op, arg, t, s, seed, rows)
        else:
            k = max(rows, 1)
            inputs = self.inputs
            x = memoryview(b"".join(inputs[(s + i) % len(inputs)] for i in range(k))).cast("f", (k, N))
        infer = c.infer
        t0 = _clock(_MONO)
        try:
            o = infer(x, budget_ms)
        except Thrown as e:
            t1 = _clock(_MONO)
            return "R t=%d s=%d src=thrown reason=%s t0=%d t1=%d" % (t, s, e.reason.value, t0, t1)
        except Unwound:
            t1 = _clock(_MONO)
            return "R t=%d s=%d src=unwound t0=%d t1=%d" % (t, s, t0, t1)
        t1 = _clock(_MONO)
        if o.is_model:
            if self.dense is None:
                value = "d=%#x" % int.from_bytes(o.value, "little")
            else:
                b = o.value.tobytes()
                bits = struct.unpack("<%dI" % (len(b) // 4), b)
                value = "v=" + ",".join("%#x" % v for v in bits)
            return "R t=%d s=%d src=model gen=%d %s t0=%d t1=%d" % (t, s, o.model_gen, value, t0, t1)
        return "R t=%d s=%d src=fallback reason=%s t0=%d t1=%d" % (t, s, o.reason.value, t0, t1)

    def run_calls(self, m):
        c = self.client
        count = num(m, "count", 1)
        op = parse_op(m.get("op", "normal"))
        arg, seed, rows = num(m, "arg", 0), num(m, "seed", 1), num(m, "rows", 0)
        budget_ms = num(m, "budget_us", 20000) / 1000.0
        per_call = self.per_call
        out = [None] * self.threads

        def one(t, s, cell):
            cell.append(self.call(c, t, s, op, arg, seed, rows, budget_ms))

        def caller(t):
            lines = []
            if per_call:
                for s in range(count):
                    th = threading.Thread(target=one, args=(t, s, lines))
                    th.start()
                    th.join()
            else:
                for s in range(count):
                    lines.append(self.call(c, t, s, op, arg, seed, rows, budget_ms))
            out[t] = lines

        ths = [threading.Thread(target=caller, args=(t,)) for t in range(self.threads)]
        for th in ths:
            th.start()
        for th in ths:
            th.join()
        lines = [line for per in out for line in (per or [])]
        lines.append("END calls=%d" % (len(lines)))
        return "\n".join(lines)

    def fork(self):
        r, w = os.pipe()
        pid = os.fork()
        if pid == 0:
            try:
                os.close(r)
                line = self.call(self.client, 0, 0, 0, 0, 1, 0, 20.0)
                f = kv(line.split()[1:])
                msg = "src=%s reason=%s" % (f.get("src", "?"), f.get("reason", "-"))
                os.write(w, msg.encode())
            finally:
                os._exit(0)
        os.close(w)
        chunks = []
        while True:
            b = os.read(r, 256)
            if not b:
                break
            chunks.append(b)
        os.close(r)
        _, st = os.waitpid(pid, 0)
        status = os.WEXITSTATUS(st) if os.WIFEXITED(st) else 128 + os.WTERMSIG(st)
        return "FORK %s status=%d" % (b"".join(chunks).decode(), status)

    def spawn(self):
        p = subprocess.Popen(["sleep", "600"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, close_fds=False)
        self.spawned.append(p)  # left running; kept so that it is never reaped behind our back
        return "SPAWNED pid=%d" % p.pid

    # ── the commands

    def config(self, m):
        kind = m.get("kind")
        if kind is not None and kind not in ("platform", "per-call"):
            return "ERR unsupported"
        self.threads = max(num(m, "threads", self.threads), 1)
        if kind is not None:
            self.per_call = kind == "per-call"
        self.fallback_us = num(m, "fallback_us", self.fallback_us)
        if "fallback" in m:
            self.throws = m["fallback"] == "throws"
        if "spin" in m:
            self.spin_off = m["spin"] == "off"
        codec = m.get("codec")
        dense = self.dense if codec is None else (m.get("inputs", "") if codec == "features" else None)
        if dense != self.dense and self.client is not None:
            # Another codec is another client: the old session ends first.
            self.dense = dense
            self.client.close()
            self.client = None
            try:
                self.client = self.build()
            except (OSError, ValueError) as e:
                return "ERR %s" % e
        if self.client is not None:
            self.client.set_spin_ceiling(0.0 if self.spin_off else None)
        return "OK"

    def stats(self):
        s = self.client.stats()
        fb = "".join(" fb.%s=%d" % (r.value, n) for r, n in s["fallbacks"].items() if n)
        return "STATS model=%d sessions=%d retired=%d late_wakes=%d%s" % (s["model"], s["sessions"], s["retired"], s["late_wakes"], fb)

    def handle(self, line):
        words = line.split()
        if not words:
            return None
        cmd, m = words[0], kv(words[1:])
        if cmd == "CONFIG":
            return self.config(m)
        if cmd == "CODEC_FAULT":
            mode = m.get("mode", "off")
            mode = mode if mode in ("encode", "decode") else "off"
            self.keyed._set_fault(mode)
            self.features._set_fault(mode)
            return "OK"
        if cmd == "UNWIND":
            arm_unwind()
            return "OK"
        if cmd == "CLOCK_SKEW":
            return "ERR unsupported"
        if cmd == "SPAWN":
            try:
                return self.spawn()
            except OSError as e:
                return "ERR %s" % e
        if cmd == "PROC":
            return proc_counts()
        if cmd == "EXIT":
            say("BYE")
            if self.client is not None:
                self.client.close()
            os._exit(0)
        if self.client is None:
            return "ERR closed"
        if cmd == "BOUND":
            return "BOUND %s" % ("true" if self.client.await_bound(num(m, "ms", 5000) / 1000.0) else "false")
        if cmd == "FORK":
            return self.fork()
        if cmd == "CALL":
            return self.run_calls(m)
        if cmd == "STATS":
            return self.stats()
        if cmd == "CLOSE":
            self.client.close()
            self.client = None
            return "CLOSED"
        return "ERR unknown command %s" % cmd


def say(s):
    sys.stdout.write(s + "\n")
    sys.stdout.flush()


def main():
    endpoint = None
    for a in sys.argv[1:]:
        if a.startswith("--endpoint="):
            endpoint = a[len("--endpoint=") :]
    if endpoint is None:
        sys.stderr.write("usage: scz-agent-python --endpoint=PATH\n")
        sys.exit(2)
    agent = Agent(endpoint)
    say("HELLO sdk=python version=%s caps=%s" % (sidecarzero.__version__, CAPS))
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        reply = agent.handle(line)
        if reply is not None:
            say(reply)
    if agent.client is not None:
        agent.client.close()
    os._exit(0)


if __name__ == "__main__":
    main()
