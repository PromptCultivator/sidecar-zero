"""The worker runner (`python -m sidecarzero.worker`, sdk-python §5) behind a real supervisor,
with a NumPy model (tests/models/numpy_model.py), called by the Python client and by the Rust
SDK's conformance agent.

The runner is started as the supervisor starts any worker, with the flags of sdk-python §5:
the interpreter as --worker, the module as --worker-arg, and Landlock read rules for the
interpreter's packages, this package, the model module, and the model file.
"""

import json
import os
import pathlib
import re
import shutil
import signal
import struct
import subprocess
import tempfile
import time
import unittest

import numpy as np

from sidecarzero import Client, FallbackReason, Source
from sidecarzero.codecs import DenseF32ToScore

from harness import BINS, ROOT, RUNNER_BINS, Sidecar

PY = os.environ.get("SCZ_PYTHON", "/opt/py/bin/python3")
PKG = str(ROOT / "target/sdk/python")
NORMAL, RAISE, ROWS, DTYPE, HANG, LEAK, SLOW, MISSING = range(8)
W = np.array([1.0, 0.25], np.float32)  # the model file: score = (x1 + 1000·x2)·w0 + w1


def row(x1, x2=0, op=NORMAL, arg=0):
    x = np.zeros(256, np.float32)
    x[0], x[1], x[2], x[3] = op, x1, x2, arg
    return x


def score(x1, x2=0):
    return float(np.float32((np.float32(x1) + np.float32(1000) * np.float32(x2)) * W[0] + W[1]))


def nan(x, reason):
    return float("nan")


class Runner:
    """A supervisor running the Python runner over `module` (in a directory of its own: Landlock
    does not apply rules to the bind-mounted source tree of the dev container)."""

    def __init__(self, module="numpy_model.py", args=(), env=None, socketpair=False):
        self.dir = pathlib.Path(tempfile.mkdtemp(prefix="scz-runner-"))
        for f in ("numpy_model.py", "net_model.py"):
            shutil.copy(ROOT / "sdks/python/tests/models" / f, self.dir)
        self.model = self.dir / "model.npy"
        np.save(self.model, W)
        a = [
            "--model=%s" % self.model,
            "--worker-arg=-B",
            "--worker-arg=-m",
            "--worker-arg=sidecarzero.worker",
            "--worker-arg=%s" % (self.dir / module),
            "--worker-read=/opt/py",  # the venv: pyvenv.cfg and site-packages (NumPy)
            "--worker-read=%s" % PKG,  # sidecarzero
            "--worker-read=%s" % self.dir,  # the model module, and the model file it re-opens
        ]
        if socketpair:
            a.append("--worker-socketpair")
        e = {"PYTHONPATH": PKG, "OPENBLAS_NUM_THREADS": "1", "PYTHONDONTWRITEBYTECODE": "1"}
        e.update(env or {})
        self.sc = Sidecar(PY, a + list(args), env=e, bins=RUNNER_BINS)

    def status(self):
        return json.loads(self.sc.status() or "{}")

    def log(self):
        return (self.sc.dir / "supervisor.log").read_text(errors="replace")

    def worker_pid(self, role="Consumer"):
        gen = next(w["gen"] for w in self.status()["workers"] if w["role"] == role)
        sup = self.sc.proc.pid
        for p in pathlib.Path("/proc/%d/task/%d/children" % (sup, sup)).read_text().split():
            if ("--gen=%d" % gen).encode() in pathlib.Path("/proc/%s/cmdline" % p).read_bytes().split(b"\0"):
                return int(p), gen
        raise LookupError("no worker of gen %d" % gen)

    def client(self, timeout=30.0):
        c = Client(self.sc.sock, codec=DenseF32ToScore(256), fallback=nan)
        if not c.await_bound(timeout):
            c.close()
            raise AssertionError("not bound: %s\n%s" % (self.sc.status(), self.log()[-3000:]))
        return c

    def stop(self):
        self.sc.stop()
        shutil.rmtree(self.dir, ignore_errors=True)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.stop()
        return False


def until_model(c, deadline_s, gen_not=None):
    """Call until the model answers (from a generation other than `gen_not`)."""
    t = time.monotonic()
    while True:
        o = c.infer(row(5, 1), 20.0)
        if o.source is Source.MODEL and o.model_gen != gen_not:
            return o
        if time.monotonic() - t > deadline_s:
            raise AssertionError("no model answer within %.0f s (last %r)" % (deadline_s, o))
        time.sleep(0.01)


class Serving(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.r = Runner(args=["--batch-slots=8", "--batch-slot-size=65536", "--batch-resp-cap=4096"])
        cls.r.sc.wait_healthy(30.0)

    @classmethod
    def tearDownClass(cls):
        cls.r.stop()

    def test_keyed_answers_one_row_and_batches(self):
        with self.r.client() as c:
            t = []
            for s in range(300):
                t0 = time.monotonic_ns()
                o = c.infer(row(s, s % 7), 50.0)
                t.append(time.monotonic_ns() - t0)
                self.assertIs(o.source, Source.MODEL, o)
                self.assertEqual(o.value, score(s, s % 7))
            x = np.stack([row(i, 3) for i in range(17)])
            b = c.infer(x, 50.0)
            self.assertIs(b.source, Source.MODEL, b)
            self.assertEqual(list(b.value), [score(i, 3) for i in range(17)])
            self.assertEqual(c.infer(row(9, 2).astype(np.float64), 50.0).value, score(9, 2))
            t.sort()
            print("\n  runner round trip, one row, from the Python client: median %.0f us, p99 %.0f us" % (t[150] / 1e3, t[297] / 1e3))

    def test_the_rust_sdk_calls_it(self):
        inputs = self.r.dir / "inputs.f32"
        rows = np.stack([row(i, 4) for i in range(10)])
        rows.astype("<f4").tofile(inputs)
        agent = subprocess.Popen([str(BINS / "scz-agent-rust"), "--endpoint=" + self.r.sc.sock], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        try:

            def ask(line):
                agent.stdin.write(line + "\n")
                agent.stdin.flush()
                return agent.stdout.readline().strip()

            self.assertTrue(agent.stdout.readline().startswith("HELLO sdk=rust"))
            self.assertEqual(ask("CONFIG codec=features inputs=%s" % inputs), "OK")
            self.assertEqual(ask("BOUND ms=20000"), "BOUND true")
            agent.stdin.write("CALL count=20 budget_us=50000 rows=2\n")
            agent.stdin.flush()
            for s in range(20):
                line = agent.stdout.readline()
                self.assertIn("src=model", line)
                bits = [int(v, 16) for v in re.search(r" v=(\S+)", line)[1].split(",")]
                want = [struct.unpack("<I", struct.pack("<f", score((s + k) % 10, 4)))[0] for k in range(2)]
                self.assertEqual(bits, want, line)
            self.assertEqual(agent.stdout.readline().strip(), "END calls=20")
            ask("EXIT")
        finally:
            agent.kill()
            agent.wait()
            agent.stdin.close()
            agent.stdout.close()

    def test_exceptions_and_mismatched_answers_are_model_errors(self):
        with self.r.client() as c:
            for op in (RAISE, ROWS, DTYPE, MISSING):
                o = c.infer(row(1, 0, op), 50.0)
                self.assertIs(o.reason, FallbackReason.MODEL_ERROR, (op, o))
                self.assertEqual(c.infer(row(2, 1), 50.0).value, score(2, 1))  # and it keeps serving
            self.assertEqual(self.r.status()["deaths"], 0)

    def test_a_slow_predict_is_abandoned_at_the_deadline(self):
        with self.r.client() as c:
            o = c.infer(row(1, 0, SLOW, 60), 5.0)
            self.assertIs(o.reason, FallbackReason.DEADLINE)
            until_model(c, 5.0)


class Faults(unittest.TestCase):
    def test_a_hang_is_killed_by_the_watchdog_and_the_standby_takes_over(self):
        with Runner(args=["--standby=warm"]) as r:
            with r.client() as c:
                gen = c.infer(row(1), 50.0).model_gen
                self.assertIs(c.infer(row(1, 0, HANG), 10.0).reason, FallbackReason.DEADLINE)
                o = until_model(c, 20.0, gen_not=gen)
                self.assertEqual(o.value, score(5, 1))
            st = r.status()
            self.assertGreaterEqual(st["condemned"], 1, st)
            self.assertGreaterEqual(st["promotions"], 1, st)

    def test_kill_9_and_the_standby_takes_over(self):
        with Runner(args=["--standby=warm"]) as r:
            with r.client() as c:
                until_model(c, 10.0)
                t = time.monotonic()
                while not any(w["role"] == "Warm" for w in r.status()["workers"]):
                    self.assertLess(time.monotonic() - t, 30.0, r.status())
                    time.sleep(0.05)
                pid, gen = r.worker_pid()
                os.kill(pid, signal.SIGKILL)
                o = until_model(c, 10.0, gen_not=gen)
                self.assertGreater(o.model_gen, gen)
            self.assertGreaterEqual(r.status()["promotions"], 1)

    def test_a_leak_runs_into_rlimit_data_and_the_runner_is_replaced(self):
        with Runner(args=["--worker-rlimit-data-mb=256"]) as r:
            with r.client() as c:
                gen = until_model(c, 10.0).model_gen
                seen = []
                for _ in range(40):
                    o = c.infer(row(1, 0, LEAK, 32), 100.0)
                    seen.append(o.reason)
                    if seen.count(FallbackReason.RESOURCE_EXHAUSTED) >= 3:
                        break
                self.assertGreaterEqual(seen.count(FallbackReason.RESOURCE_EXHAUSTED), 3, seen)
                self.assertIn(None, seen)  # the first leaks were served
                o = until_model(c, 30.0, gen_not=gen)
                self.assertEqual(o.value, score(5, 1))
            self.assertGreaterEqual(r.status()["deaths"], 1)
            self.assertIn("MemoryErrors in a row; exiting", r.log())


class Sandbox(unittest.TestCase):
    def test_a_module_that_opens_a_network_socket_fails_at_load(self):
        with Runner("net_model.py") as r:
            c = Client(r.sc.sock, codec=DenseF32ToScore(256), fallback=nan)
            self.assertFalse(c.await_bound(3.0))
            self.assertIn(c.infer(row(1), 5.0).reason, (FallbackReason.NOT_CONNECTED, FallbackReason.GATE_INIT, FallbackReason.GATE_DEAD))
            c.close()
            log = r.log()
            self.assertIn("loading %s" % (r.dir / "net_model.py"), log)
            self.assertIn("PermissionError: [Errno 1] Operation not permitted", log)
            self.assertIn("socket.socket(socket.AF_INET", log)

    def test_socketpair_for_asyncio_and_multiprocessing_only_when_allowed(self):
        # Allowed: an asyncio loop runs, multiprocessing.Pipe() carries a message, send(2) on a
        # socket pair works, and a sendto naming an address is refused (1). Not allowed:
        # socketpair itself is refused (-1).
        for allowed, want in ((True, 1.0), (False, -1.0)):
            with Runner(env={"SCZ_TEST_PROBE": "socketpair"}, socketpair=allowed) as r:
                with r.client() as c:
                    self.assertEqual(c.infer(row(1), 50.0).value, want, "socketpair allowed: %s" % allowed)

    def test_model_path_needs_the_model_file_in_the_read_view(self):
        # /proc/self/fd/N is re-opened through the model file's own path: a rule for /proc does
        # not let it through, one for the file does.
        r = Runner()
        try:
            r.sc.stop()
            other = pathlib.Path(tempfile.mkdtemp(prefix="scz-model-"))
            np.save(other / "model.npy", W)
            base = [a for a in r.sc.proc.args[1:] if not a.startswith(("--socket", "--worker=", "--admin", "--model=", "--hb-stale"))]
            for extra, ok in (([], False), (["--worker-read=/proc"], False), (["--worker-read=%s" % (other / "model.npy")], True)):
                sc = Sidecar(PY, base + ["--model=%s" % (other / "model.npy")] + extra, env={"PYTHONPATH": PKG, "OPENBLAS_NUM_THREADS": "1"}, bins=RUNNER_BINS)
                try:
                    c = Client(sc.sock, codec=DenseF32ToScore(256), fallback=nan)
                    self.assertEqual(c.await_bound(5.0 if not ok else 30.0), ok, (extra, (sc.dir / "supervisor.log").read_text()[-2000:]))
                    if not ok:
                        self.assertIn("PermissionError: [Errno 13] Permission denied: '/proc/self/fd/", (sc.dir / "supervisor.log").read_text())
                    c.close()
                finally:
                    sc.stop()
            shutil.rmtree(other, ignore_errors=True)
        finally:
            shutil.rmtree(r.dir, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
