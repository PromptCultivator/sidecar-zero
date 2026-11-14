"""The SDK without a sidecar: construction, the fingerprint golden vectors, the codecs' input
checks through the in-process test double, native fallbacks."""

import array
import threading
import re
import time
import tomllib
import unittest
import warnings

import numpy as np

import sidecarzero
from sidecarzero import Client, FallbackReason, Outcome, Source, fallbacks
from sidecarzero._core import fingerprint
from sidecarzero.codecs import Bytes, DenseF32ToScore
from sidecarzero.testing import Fault, InProcessClient

from harness import ROOT

NOWHERE = "/nonexistent/scz.sock"


def const(x, reason):
    return 0.5


class Construction(unittest.TestCase):
    def test_the_fallback_is_a_required_keyword(self):
        with self.assertRaises(TypeError):
            Client(NOWHERE, codec=Bytes())  # no fallback
        with self.assertRaises(TypeError):
            Client(NOWHERE, Bytes(), const)  # not by keyword
        with self.assertRaises(ValueError):
            Client(NOWHERE, codec=Bytes(), fallback=None)
        with self.assertRaises(TypeError):
            Client(NOWHERE, codec=Bytes(), fallback=42)
        with self.assertRaises(TypeError):
            Client(NOWHERE, codec="bytes", fallback=const)

    def test_no_sidecar_falls_back_with_not_connected(self):
        with Client(NOWHERE, codec=DenseF32ToScore(4), fallback=const) as c:
            o = c.infer(np.zeros(4, np.float32), budget_ms=1.0)
            self.assertIs(o.source, Source.FALLBACK)
            self.assertIs(o.reason, FallbackReason.NOT_CONNECTED)
            self.assertEqual(o.value, 0.5)
            self.assertFalse(o.is_model)
            self.assertFalse(c.await_bound(0.05))
            self.assertEqual(c.stats()["fallbacks"], {FallbackReason.NOT_CONNECTED: 1})
        self.assertEqual(c.state, "CLOSED")
        self.assertIs(c.infer(b"x" * 16, 1).reason, FallbackReason.NOT_CONNECTED)

    def test_the_fallback_sees_the_callers_input_and_its_exceptions_propagate(self):
        seen = []

        def fb(x, reason):
            seen.append((x, reason))
            raise KeyError("from the fallback")

        c = Client(NOWHERE, codec=Bytes(), fallback=fb)
        x = bytearray(b"abc")
        with self.assertRaises(KeyError):
            c.infer(x, 1.0)
        self.assertIs(seen[0][0], x)
        self.assertIs(seen[0][1], FallbackReason.NOT_CONNECTED)
        self.assertEqual(c.stats()["fallbacks"][FallbackReason.NOT_CONNECTED], 1)  # counted before the call
        c.close()

    def test_generic_aliases_work_at_run_time(self):
        self.assertIs(Outcome[float], Outcome)
        self.assertIs(Client[bytes, bytes], Client)

    def test_reasons_are_the_cores(self):
        self.assertEqual([r.value for r in FallbackReason], list(sidecarzero._core.REASONS))


class Fingerprint(unittest.TestCase):
    def test_golden_vectors(self):
        elem = tomllib.loads((ROOT / "abi/scz_abi.toml").read_text())["enums"]["elem_type"]
        n = 0
        for line in (ROOT / "abi/golden/fingerprints.txt").read_text().splitlines():
            if not line or line.startswith("#"):
                continue
            want, req, resp = line.split(" ")

            def cols(s):
                if s == "-":
                    return []
                out = []
                for c in s.split(","):
                    m = re.fullmatch(r"(.+):([A-Z_0-9]+)(\?)?\[(\d+)\]", c)
                    out.append((m[1], elem[m[2]], int(m[4]), m[3] == "?"))
                return out

            self.assertEqual(fingerprint(cols(req), cols(resp)), int(want, 16), line)
            n += 1
        self.assertGreaterEqual(n, 8)
        self.assertEqual(DenseF32ToScore(256).schema_fingerprint(), 0x57078E8D7A2E3733)
        self.assertIsNone(Bytes().schema_fingerprint())


def doubled_sum(x):
    return [float(r.sum()) * 2 for r in x]


class InProcess(unittest.TestCase):
    def client(self, **kw):
        return InProcessClient(DenseF32ToScore(4, **kw), model=doubled_sum, fallback=lambda x, r: -1.0)

    def test_dtypes_are_converted_during_the_copy(self):
        c = self.client()
        cases = [
            (np.arange(4, dtype=np.float64), 12.0),
            (np.arange(4, dtype=np.int64), 12.0),
            (np.arange(4, dtype=np.float16), 12.0),
            (np.array([2**-24, 0, 0, 0], dtype=np.float16), 2.0**-23),  # a subnormal
            (np.arange(4, dtype=np.uint8), 12.0),
            (np.array([1, 0, 1, 1], dtype=bool), 6.0),
            (np.arange(4, dtype=np.dtype(">f8")), 12.0),
            (np.arange(4, dtype=np.dtype(">f4")), 12.0),
            (array.array("d", [0, 1, 2, 3]), 12.0),
            (np.arange(8, dtype=np.float32)[::2], 24.0),  # strided
            (np.arange(16, dtype=np.float64).reshape(4, 4)[:, 1], 56.0),  # a column
        ]
        for x, want in cases:
            o = c.infer(x, 1.0)
            self.assertIs(o.source, Source.MODEL, x)
            self.assertEqual(o.value, want, x)
        b = c.infer(np.ones((3, 4)), 1.0)
        self.assertEqual(list(b.value), [8.0, 8.0, 8.0])
        self.assertEqual(b.value.dtype, np.float32)
        raw = c.infer(np.ones(4, np.float32).tobytes(), 1.0)
        self.assertEqual(raw.value, 8.0)

    def test_a_wrong_shape_falls_back_with_codec_error_and_does_not_raise(self):
        c = self.client()
        for x in (np.zeros(5, np.float32), np.zeros((2, 3)), "text", 3.0, np.zeros((2, 2, 4))):
            o = c.infer(x, 1.0)
            self.assertIs(o.reason, FallbackReason.CODEC_ERROR, x)
            self.assertEqual(o.value, -1.0)

    def test_strict_refuses_conversion(self):
        c = self.client(strict=True)
        self.assertIs(c.infer(np.zeros(4), 1.0).reason, FallbackReason.CODEC_ERROR)
        self.assertIs(c.infer(np.zeros(4, np.float32), 1.0).source, Source.MODEL)

    def test_lists_work(self):
        c = self.client()
        self.assertEqual(c.infer([1, 2, 3, 4], 1.0).value, 20.0)
        self.assertEqual(list(c.infer([[1, 1, 1, 1], [0, 0, 0, 1]], 1.0).value), [8.0, 2.0])

    def test_faults_and_model_errors(self):
        c = self.client()
        c.inject(Fault.GATE_DEAD)
        self.assertIs(c.infer(np.zeros(4), 1).reason, FallbackReason.GATE_DEAD)
        c.inject(None)

        def boom(x):
            raise RuntimeError

        e = InProcessClient(Bytes(), model=boom, fallback=lambda x, r: b"")
        self.assertIs(e.infer(b"abc", 1).reason, FallbackReason.MODEL_ERROR)
        echo = InProcessClient(Bytes(response_len=3), model=bytes, fallback=lambda x, r: b"")
        self.assertEqual(echo.infer(b"abc", 1).value, b"abc")
        self.assertIs(echo.infer(b"abcd", 1).reason, FallbackReason.CODEC_ERROR)
        with self.assertRaises(ValueError):
            InProcessClient(Bytes(), model=bytes, fallback=None)


class Native(unittest.TestCase):
    def test_checked_against_the_codec_at_construction(self):
        with self.assertRaises(ValueError):
            Client(NOWHERE, codec=DenseF32ToScore(4), fallback=fallbacks.threshold(feature=4, gt=0, then=1, otherwise=0))
        with self.assertRaises(ValueError):
            Client(NOWHERE, codec=DenseF32ToScore(4), fallback=fallbacks.linear(weights=[1, 2, 3]))
        with self.assertRaises(ValueError):
            Client(NOWHERE, codec=Bytes(), fallback=fallbacks.threshold(feature=0, gt=0, then=1, otherwise=0))
        with self.assertRaises(ValueError):
            fallbacks.linear(weights=[1.0], link="tanh")
        Client(NOWHERE, codec=Bytes(), fallback=fallbacks.constant(b"x")).close()

    def test_evaluated_without_python(self):
        x = np.array([0.1, 0.9, 0.0, 2.5], np.float32)
        cases = [
            (fallbacks.constant(7), 7),
            (fallbacks.threshold(feature=1, gt=0.8, then=0.9, otherwise=0.1), np.float32(0.9)),
            (fallbacks.linear(weights=[1, 1, 1, 1], bias=-3.5), np.float32(0.0)),
            (fallbacks.linear(weights=[0, 0, 0, 0], link="sigmoid"), 0.5),
            (fallbacks.lookup(feature=3, table=[10, 20, 30], otherwise=-1), 30.0),
        ]
        for fb, want in cases:
            with Client(NOWHERE, codec=DenseF32ToScore(4), fallback=fb) as c:
                o = c.infer(x, 1.0)
                self.assertIs(o.reason, FallbackReason.NOT_CONNECTED)
                self.assertAlmostEqual(float(o.value), float(want), places=5, msg=repr(fb))
        with Client(NOWHERE, codec=DenseF32ToScore(4), fallback=fallbacks.threshold(feature=1, gt=0.5, then=1, otherwise=0)) as c:
            self.assertEqual(list(c.infer(np.array([[0, 1, 0, 0], [0, 0, 0, 0]]), 1).value), [1.0, 0.0])
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                self.assertEqual(c.infer("not numbers", 1).value, 0.0)  # any input: `otherwise`



class Metrics(unittest.TestCase):
    """sdk-contract §8: the hook, the sampled histograms, the export and its text format."""

    def test_the_hook_sees_each_fallback_and_the_fallback_is_timed(self):
        seen = []

        def slow(x, reason):
            time.sleep(0.0002)
            return b""

        c = Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=slow, on_fallback=seen.append, sample_every=1)
        for _ in range(10):
            c.infer(b"x", budget_ms=1.0)  # every call: one in one is timed whatever this thread's count
        self.assertEqual(seen, [FallbackReason.NOT_CONNECTED] * 10)
        st = c.stats()
        self.assertEqual((st["fallback_time"]["count"], st["latency_fallback"]["count"], st["latency_model"]["count"]), (10, 10, 0))
        self.assertGreaterEqual(st["fallback_time"]["sum"], 10 * 0.0002)
        self.assertEqual(len(st["fallback_time"]["cumulative"]), len(st["fallback_time"]["le"]) + 1)
        c.close()

    def test_a_hook_that_raises_propagates_like_the_fallback(self):
        def boom(reason):
            raise KeyError("hook")

        c = Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=lambda x, r: b"", on_fallback=boom)
        with self.assertRaises(KeyError):
            c.infer(b"x", budget_ms=1.0)
        with self.assertRaises(TypeError):
            Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=lambda x, r: b"", on_fallback=3)
        c.close()

    def test_sampling_one_in_n_per_thread(self):
        c = Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=lambda x, r: b"", sample_every=8)
        t = threading.Thread(target=lambda: [c.infer(b"x", budget_ms=1.0) for _ in range(80)])
        t.start()
        t.join()
        self.assertEqual(c.stats()["latency_fallback"]["count"], 10, "the first call, then one in eight")
        c.close()

    def test_export_and_the_text_format(self):
        c = Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=lambda x, r: b"", sample_every=1)
        c.infer(b"x", budget_ms=1.0)

        class Sink:
            def __init__(self):
                self.rows = []

            def counter(self, name, labels, value):
                self.rows.append(("counter", name, labels, value))

            def gauge(self, name, labels, value):
                self.rows.append(("gauge", name, labels, value))

            def histogram(self, name, labels, bounds, cumulative, sum, count):
                self.rows.append(("histogram", name, labels, count))

        s = Sink()
        c.export(s)
        names = {r[1] for r in s.rows}
        self.assertEqual(names, {
            "scz_client_calls_total", "scz_client_latency_seconds", "scz_client_fallback_seconds", "scz_client_budget_clamped_total",
            "scz_client_breaker_open", "scz_client_late_wakes_total", "scz_client_reconnects_total", "scz_client_state", "scz_client_model_gen",
        })
        calls = [r for r in s.rows if r[1] == "scz_client_calls_total"]
        self.assertEqual(len(calls), 1 + len(FallbackReason))
        self.assertIn(("counter", "scz_client_calls_total", {"source": "fallback", "reason": "NOT_CONNECTED"}, 1), calls)
        text = c.prometheus_text()
        self.assertIn('scz_client_calls_total{source="fallback",reason="NOT_CONNECTED"} 1\n', text)
        self.assertIn('scz_client_latency_seconds_bucket{source="fallback",le="+Inf"} 1\n', text)
        self.assertEqual(text.count("# TYPE scz_client_calls_total counter"), 1)

        class Broken(Sink):
            def gauge(self, name, labels, value):
                raise ValueError("sink")

        with self.assertRaises(ValueError):
            c.export(Broken())
        c.close()


if __name__ == "__main__":
    unittest.main()
