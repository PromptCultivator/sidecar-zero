"""The SDK against a real supervisor: the chaos worker for opaque bytes, the ONNX Runtime worker
with the fraud model for the dense codec. Linux only (the container)."""

import os
import signal
import struct
import threading
import time
import unittest
import warnings

import numpy as np

from sidecarzero import Client, FallbackReason, Source, fallbacks
from sidecarzero.codecs import Bytes, DenseF32ToScore

from harness import DELAY, MODELS, NORMAL, Sidecar, answer, request


def zero(x, reason):
    return b""


def arena_maps():
    with open("/proc/self/maps") as f:
        return sum(1 for line in f if "memfd:scz-" in line)


class Chaos(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.sc = Sidecar()
        cls.sc.wait_healthy()

    @classmethod
    def tearDownClass(cls):
        cls.sc.stop()

    def client(self, **kw):
        c = Client(self.sc.sock, codec=Bytes(response_len=8), fallback=zero, **kw)
        self.assertTrue(c.await_bound(10.0), self.sc.status())
        return c

    def test_every_answer_is_keyed_to_its_own_request(self):
        with self.client() as c:
            self.assertEqual(c.state, "BOUND")
            for s in range(200):
                req = request(NORMAL, 0, 0, s, seed=7, rows=s % 9)
                o = c.infer(req, budget_ms=20.0)
                self.assertIs(o.source, Source.MODEL, o)
                self.assertEqual(o.value, answer(req))
                self.assertGreaterEqual(o.model_gen, 1)
            st = c.stats()
            self.assertEqual((st["model"], st["fallbacks"], st["sessions"]), (200, {}, 1))
            # sdk-contract §8: one call in 64 timed, on this thread's count.
            self.assertGreaterEqual(st["latency_model"]["count"], 3)
            self.assertGreaterEqual(st["model_gen"], 1)
            self.assertIn('scz_client_calls_total{source="model"} 200\n', c.prometheus_text())
            self.assertIn("scz_client_state 2\n", c.prometheus_text())
        self.assertEqual(arena_maps(), 0)

    def test_threads_and_a_new_thread_per_call(self):
        with self.client() as c:
            bad = []

            def run(t, n, per_call):
                for s in range(n):
                    req = request(NORMAL, 0, t, s)
                    if per_call:
                        box = []
                        th = threading.Thread(target=lambda: box.append(c.infer(req, 20.0)))
                        th.start()
                        th.join()
                        o = box[0]
                    else:
                        o = c.infer(req, 20.0)
                    if o.source is not Source.MODEL or o.value != answer(req):
                        bad.append(o)

            ths = [threading.Thread(target=run, args=(t, 300, t % 2 == 1)) for t in range(6)]
            for th in ths:
                th.start()
            for th in ths:
                th.join()
            self.assertEqual(bad, [])
            self.assertEqual(c.stats()["model"], 1800)

    def test_the_gil_is_released_while_a_call_waits(self):
        with self.client() as c:
            spins = [0]
            done = threading.Event()

            def other():
                while not done.is_set():
                    spins[0] += 1

            th = threading.Thread(target=other)
            th.start()
            time.sleep(0.01)
            before = spins[0]
            t0 = time.monotonic()
            o = c.infer(request(DELAY, 50_000, 0, 1), budget_ms=200.0)
            took = time.monotonic() - t0
            during = spins[0] - before
            done.set()
            th.join()
            self.assertIs(o.source, Source.MODEL)
            self.assertGreaterEqual(took, 0.045)
            # Python code ran on the other thread for most of the 50 ms wait.
            self.assertGreater(during, 10_000, "the other thread barely ran: %d" % during)

    def test_deadline_and_close_with_calls_in_flight(self):
        c = self.client()
        o = c.infer(request(DELAY, 30_000, 0, 1), budget_ms=2.0)
        self.assertIs(o.reason, FallbackReason.DEADLINE)
        results = []
        stop = threading.Event()

        def run(t):
            s = 0
            while not stop.is_set():
                results.append(c.infer(request(DELAY, 2000, t, s), 20.0))
                s += 1

        ths = [threading.Thread(target=run, args=(t,)) for t in range(4)]
        for th in ths:
            th.start()
        time.sleep(0.05)
        self.assertTrue(c.close())
        self.assertEqual(arena_maps(), 0)
        stop.set()
        for th in ths:
            th.join()
        self.assertEqual(c.state, "CLOSED")
        self.assertIs(c.infer(request(NORMAL, 0, 9, 9), 20.0).reason, FallbackReason.NOT_CONNECTED)
        sources = {(o.source, o.reason) for o in results}
        self.assertLessEqual(sources, {(Source.MODEL, None), (Source.FALLBACK, FallbackReason.NOT_CONNECTED), (Source.FALLBACK, FallbackReason.DEADLINE)})

    def test_a_stale_heartbeat_falls_back_without_a_reconnect(self):
        with self.client() as c:
            self.sc.signal(signal.SIGSTOP)
            try:
                time.sleep(0.25)
                o = c.infer(request(NORMAL, 0, 0, 1), 5.0)
                self.assertIs(o.reason, FallbackReason.STALE_HEARTBEAT)
            finally:
                self.sc.signal(signal.SIGCONT)
            t = time.monotonic()
            while c.infer(request(NORMAL, 0, 0, 2), 20.0).source is not Source.MODEL:
                self.assertLess(time.monotonic() - t, 5.0)
                time.sleep(0.01)
            self.assertEqual(c.stats()["sessions"], 1)

    def test_a_forked_child_falls_back_and_exits_cleanly(self):
        with self.client() as c:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", DeprecationWarning)
                pid = os.fork()
            if pid == 0:
                o = c.infer(request(NORMAL, 0, 0, 1), 20.0)
                os._exit(0 if o.reason is FallbackReason.NOT_CONNECTED else 3)
            _, st = os.waitpid(pid, 0)
            self.assertEqual(os.waitstatus_to_exitcode(st), 0)
            req = request(NORMAL, 0, 0, 2)
            self.assertEqual(c.infer(req, 20.0).value, answer(req))

    def test_codec_errors(self):
        with self.client() as c:
            with warnings.catch_warnings(record=True) as w:
                warnings.simplefilter("always")
                self.assertIs(c.infer("not a buffer", 20.0).reason, FallbackReason.CODEC_ERROR)
                self.assertIs(c.infer(12, 20.0).reason, FallbackReason.CODEC_ERROR)
            self.assertEqual(len(w), 1, [str(x.message) for x in w])  # once per client
            # The response must be exactly 8 bytes: a request without the magic is echoed.
            self.assertIs(c.infer(b"0123456789", 20.0).reason, FallbackReason.CODEC_ERROR)
            self.assertIs(c.infer(b"01234567", 20.0).source, Source.MODEL)
            self.assertIs(c.infer(request(NORMAL, 0, 0, 0, rows=100_000), 20.0).reason, FallbackReason.TOO_LARGE)
            self.assertEqual(c._census()[0], sum(c._census()))  # every slot FREE


class Fraud(unittest.TestCase):
    """One session at a time: the supervisor admits one client process, so each test's client
    is closed before the next one connects."""

    @classmethod
    def setUpClass(cls):
        cls.sc = Sidecar("sidecarzero-worker", ["--model=%s" % (MODELS / "fraud.ort"), "--batch-slots=8", "--batch-slot-size=65536", "--batch-resp-cap=4096"])
        cls.x = np.fromfile(MODELS / "reference-features.f32", dtype="<f4").reshape(-1, 256)
        cls.y = np.fromfile(MODELS / "reference-scores.f32", dtype="<f4")
        cls.sc.wait_healthy(30.0)

    @classmethod
    def tearDownClass(cls):
        cls.sc.stop()

    def setUp(self):
        self.c = Client(self.sc.sock, codec=DenseF32ToScore(256), fallback=lambda x, r: float("nan"))
        self.assertTrue(self.c.await_bound(10.0), self.sc.status())

    def tearDown(self):
        self.c.close()

    def test_one_row_is_a_float_and_matches_the_reference(self):
        for i in range(len(self.x)):
            o = self.c.infer(self.x[i], budget_ms=50.0)
            self.assertIs(o.source, Source.MODEL, o)
            self.assertIsInstance(o.value, float)
            self.assertLess(abs(o.value - self.y[i]), 1e-5)
            # float64 is converted during the copy, and lands on the same score
            self.assertLess(abs(self.c.infer(self.x[i].astype(np.float64), 50.0).value - self.y[i]), 1e-5)

    def test_batches_are_arrays(self):
        for rows in (1, 3, 32):
            o = self.c.infer(self.x[:rows], budget_ms=50.0)
            self.assertIs(o.source, Source.MODEL, o)
            self.assertEqual(o.value.dtype, np.float32)
            self.assertEqual(o.value.shape, (rows,))
            self.assertLess(float(np.abs(o.value - self.y[:rows]).max()), 1e-5)
        raw = self.c.infer(self.x[:2].tobytes(), 50.0)
        self.assertLess(float(np.abs(raw.value - self.y[:2]).max()), 1e-5)
        self.assertIs(self.c.infer(np.zeros((100_000, 256), np.float32), 50.0).reason, FallbackReason.TOO_LARGE)

    def test_wrong_shapes_and_strict(self):
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            self.assertIs(self.c.infer(np.zeros(255, np.float32), 50.0).reason, FallbackReason.CODEC_ERROR)
            self.assertIs(self.c.infer(np.zeros((2, 128), np.float32), 50.0).reason, FallbackReason.CODEC_ERROR)
            o = self.c.infer(list(map(float, self.x[0])), 50.0)
            self.assertLess(abs(o.value - self.y[0]), 1e-5)
        self.c.close()
        with Client(self.sc.sock, codec=DenseF32ToScore(256, strict=True), fallback=fallbacks.constant(-1.0)) as s:
            self.assertTrue(s.await_bound(10.0))
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                self.assertEqual(s.infer(self.x[0].astype(np.float64), 50.0).reason, FallbackReason.CODEC_ERROR)
            self.assertIs(s.infer(self.x[0], 50.0).source, Source.MODEL)

    def test_an_opaque_codec_is_refused_by_a_model_with_columns(self):
        self.c.close()
        with Client(self.sc.sock, codec=Bytes(), fallback=zero) as b:
            t = time.monotonic()
            while (r := b.infer(b"x" * 8, 5.0).reason) is not FallbackReason.SCHEMA_MISMATCH:
                self.assertIn(r, (FallbackReason.NOT_CONNECTED, FallbackReason.REBINDING))
                self.assertLess(time.monotonic() - t, 10.0)
                time.sleep(0.01)
            self.assertEqual(b.state, "SCHEMA_REJECTED")
        with Client(self.sc.sock, codec=DenseF32ToScore(128), fallback=fallbacks.constant(0.0)) as d:
            t = time.monotonic()
            while d.infer(np.zeros(128, np.float32), 5.0).reason is not FallbackReason.SCHEMA_MISMATCH:
                self.assertLess(time.monotonic() - t, 10.0)
                time.sleep(0.01)


if __name__ == "__main__":
    unittest.main()
