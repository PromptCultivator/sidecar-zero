package io.sidecarzero;

import static io.sidecarzero.Tests.check;

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicLongArray;

/**
 * sdk-contract §8: the sampled histograms, the export and its text format, the hook. Mirrors
 * {@code sdks/rust/tests/metrics.rs} and the unit tests of {@code sdks/rust/src/metrics.rs}.
 * The ones without a sidecar run with the unit tests (before the stress test, so that the thread
 * ids of the threads they start have never called); {@link #live} and {@link #liveFrames} run
 * in a JVM of their own against {@code scz-worker-echo} and the fraud model.
 */
final class MetricsTests {
    private MetricsTests() {}

    static final String ABSENT = "/nonexistent/scz.sock";

    /** float32[n] as opaque bytes, for the echo worker: it answers with the request. */
    static final class F32 implements Codec<float[], float[]> {
        @Override
        public int requestLen(float[] in) {
            return in.length * 4;
        }

        @Override
        public void encode(float[] in, SlotWriter w) {
            w.writeFloats(0, in, 0, in.length);
        }

        @Override
        public float[] decode(SlotReader r) {
            float[] out = new float[r.length() / 4];
            r.readFloats(0, out, 0, out.length);
            return out;
        }
    }

    static Options every(int n) {
        return Options.defaults().sampleEvery(n);
    }

    /** Run {@code r} on a thread of its own: its sample count starts at zero. */
    static void onFreshThread(Runnable r) throws InterruptedException {
        Throwable[] err = new Throwable[1];
        Thread t = new Thread(() -> {
            try {
                r.run();
            } catch (Throwable e) {
                err[0] = e;
            }
        });
        t.start();
        t.join();
        if (err[0] instanceof RuntimeException e) throw e;
        if (err[0] instanceof Error e) throw e;
        if (err[0] != null) throw new AssertionError(err[0]);
    }

    /** Busy for more than {@code ns} of System.nanoTime(), so the time is at least that whatever the timer does. */
    static void busy(long ns) {
        long t = System.nanoTime();
        while (System.nanoTime() - t <= ns) Thread.onSpinWait();
    }

    // ── without a sidecar ───────────────────────────────────────────────────────────────────────

    static void bucketPlacement() {
        AtomicLongArray a = new AtomicLongArray(Histogram.LONGS);
        for (long ns : new long[] {0, 1_000, 1_001, 100_000_000, 100_000_001, Long.MAX_VALUE / 2}) Engine.record(a, 0, ns);
        Histogram r = Histogram.sum(a, 1, Histogram.LONGS, 0);
        check(r.cumulative(0) == 2, "0 and exactly 1 µs: " + r);
        check(r.cumulative(1) == 3, "1.001 µs: " + r);
        check(r.cumulative(15) == 4, "exactly 100 ms: " + r);
        check(r.cumulative(16) == 6 && r.count() == 6, "+Inf holds the rest: " + r);
        check(Histogram.BUCKETS == 17 && Histogram.boundsSeconds().length == 16, "16 bounds and +Inf");
    }

    static void textFormat() {
        PrometheusText t = new PrometheusText();
        t.counter("a_total", new String[] {"source", "fallback", "reason", "x\"y"}, 3);
        t.counter("a_total", new String[] {"source", "model"}, 4);
        t.gauge("b", new String[0], 1.5);
        long[] cum = new long[Histogram.BUCKETS];
        cum[16] = 2;
        t.histogram("c_seconds", new String[0], Histogram.boundsSeconds(), cum, 0.25, 2);
        String s = t.finish();
        check(s.split("# TYPE a_total counter", -1).length == 2, "one TYPE line per family:\n" + s);
        check(s.contains("a_total{source=\"fallback\",reason=\"x\\\"y\"} 3\n"), s);
        check(s.contains("b 1.5\n") && s.contains("c_seconds_bucket{le=\"0.000001\"} 0\n") && s.contains("c_seconds_bucket{le=\"+Inf\"} 2\n"), s);
        check(s.contains("c_seconds_sum 0.25\nc_seconds_count 2\n"), s);
        // Every le as Rust's Display writes the f64 bound; and the escapes, in order.
        String[] le = {"0.000001", "0.000002", "0.000005", "0.00001", "0.00002", "0.00005", "0.0001", "0.0002", "0.0005", "0.001", "0.002", "0.005", "0.01",
            "0.02", "0.05", "0.1"};
        for (String l : le) check(s.contains("c_seconds_bucket{le=\"" + l + "\"} 0\n"), "le " + l + ":\n" + s);
        check(s.split("c_seconds_bucket", -1).length == 18, "17 buckets");
        PrometheusText e = new PrometheusText();
        e.gauge("g", new String[] {"k", "a\\b\nc\"d"}, 0);
        check(e.finish().equals("# TYPE g gauge\ng{k=\"a\\\\b\\nc\\\"d\"} 0\n"), e.finish());
        String[][] nums = {{"0", "0"}, {"2", "2"}, {"1.5", "1.5"}, {"1e-6", "0.000001"}, {"123456789", "123456789"}, {"1e21", "1000000000000000000000"},
            {"0.1", "0.1"}, {"0.30000000000000004", "0.30000000000000004"}, {"4.5e-7", "0.00000045"},
            // Exactly halfway between two shortest decimals: Rust takes the one further from zero, Java's toString the even one.
            {"751435862906.78125", "751435862906.7813"}, {"-751435862906.78125", "-751435862906.7813"}, {"2647518583414.28125", "2647518583414.2813"}};
        for (String[] n : nums) check(PrometheusText.number(Double.parseDouble(n[0])).equals(n[1]), n[0] + " -> " + PrometheusText.number(Double.parseDouble(n[0])));
        check(PrometheusText.number(-0.0).equals("-0") && PrometheusText.number(Double.POSITIVE_INFINITY).equals("inf"), "Rust spellings");
    }

    /** The count is the thread's, over every client: two clients called in turn share it. */
    static void sharedAcrossClients() throws Exception {
        try (ScoreClient a = SidecarZero.connect(ABSENT).score(4).fallback((f, r) -> 0f).options(every(4)).build();
             ScoreClient b = SidecarZero.connect(ABSENT).score(4).fallback((f, r) -> 0f).options(every(4)).build();
             ScoreClient c = SidecarZero.connect(ABSENT).score(4).fallback((f, r) -> 0f).options(every(3)).build()) {
            float[] x = new float[4];
            onFreshThread(() -> {
                for (int i = 0; i < 8; i++) (i % 2 == 0 ? a : b).scoreOutcome(x, 1_000_000);
            });
            // The thread's calls 0..7: 0 and 4 are timed, both a's.
            check(a.stats().latencyFallback().count() == 2 && b.stats().latencyFallback().count() == 0,
                    "a " + a.stats().latencyFallback() + ", b " + b.stats().latencyFallback());
            // Not a power of two: calls 0, 3, 6, 9 of 10.
            onFreshThread(() -> {
                for (int i = 0; i < 10; i++) c.scoreOutcome(x, 1_000_000);
            });
            check(c.stats().latencyFallback().count() == 4 && c.stats().fallbackTime().count() == 4, "one in three: " + c.stats().latencyFallback());
        }
    }

    /** Nothing listens there: every call falls back with NOT_CONNECTED. */
    static void hookAndFallbackTiming() throws Exception {
        AtomicLong seen = new AtomicLong();
        try (Client<float[], float[]> c = SidecarZero.connect(ABSENT).codec(new F32()).fallback((float[] x, FallbackReason r) -> {
            busy(200_000);
            return x;
        }).onFallback(r -> {
            check(r == FallbackReason.NOT_CONNECTED, "reason " + r);
            seen.incrementAndGet();
        }).options(every(1)).build()) {
            onFreshThread(() -> {
                for (int i = 0; i < 20; i++) check(!c.infer(new float[4], 1_000_000).fromModel(), "fallback");
            });
            check(seen.get() == 20, "the hook saw " + seen.get());
            Stats s = c.stats();
            check(s.fallbacks(FallbackReason.NOT_CONNECTED) == 20 && s.fallbackTime().count() == 20 && s.latencyFallback().count() == 20, s.fallbackTime().toString());
            check(s.fallbackTime().sumSeconds() >= 20 * 200e-6, s.fallbackTime().toString());
            check(s.latencyFallback().sumSeconds() >= s.fallbackTime().sumSeconds(), s.latencyFallback() + " vs " + s.fallbackTime());
            check(s.fallbackTime().cumulative(7) == 0, "nothing at or under 200 µs: " + s.fallbackTime());
            check(s.latencyModel().count() == 0, "no model answers");
        }
        // The primitive clients get the same.
        AtomicLong hs = new AtomicLong(), hb = new AtomicLong();
        try (ScoreClient sc = SidecarZero.connect(ABSENT).score(4).fallback((f, r) -> 1f).onFallback(r -> hs.incrementAndGet()).options(every(1)).build();
             BatchScoreClient bc = SidecarZero.connect(ABSENT).batch(new DenseCodec(4)).fallback((f, n, out, r) -> {}).onFallback(r -> hb.incrementAndGet())
                     .options(every(1)).build()) {
            float[] x = new float[8], out = new float[2];
            for (int i = 0; i < 5; i++) {
                check(ScoreOutcome.score(sc.scoreOutcome(x, 1_000_000)) == 1f, "score fallback");
                bc.scoreInto(x, 2, out, 1_000_000);
            }
            check(hs.get() == 5 && sc.stats().fallbackTime().count() == 5 && sc.stats().latencyFallback().count() == 5, "ScoreClient: " + hs + " " + sc.stats().fallbackTime());
            check(hb.get() == 5 && bc.stats().fallbackTime().count() == 5 && bc.stats().latencyFallback().count() == 5, "BatchScoreClient: " + hb + " " + bc.stats().fallbackTime());
        }
    }

    /** What the hook throws propagates; the fallback does not run, and the call is still counted. */
    static void hookThrows() {
        AtomicLong fallbacks = new AtomicLong();
        IllegalStateException boom = new IllegalStateException("from the hook");
        try (ScoreClient c = SidecarZero.connect(ABSENT).score(4).fallback((f, r) -> {
            fallbacks.incrementAndGet();
            return 0f;
        }).onFallback(r -> {
            throw boom;
        }).build()) {
            Throwable got = null;
            try {
                c.scoreOutcome(new float[4], 1_000_000);
            } catch (Throwable e) {
                got = e;
            }
            check(got == boom, "propagated: " + got);
            check(fallbacks.get() == 0 && c.stats().fallbacks(FallbackReason.NOT_CONNECTED) == 1, "the fallback did not run; the call is counted");
        }
    }

    static void samplingOff() {
        try (ScoreClient c = SidecarZero.connect(ABSENT).score(4).fallback((f, r) -> 0f).options(every(0)).build()) {
            for (int i = 0; i < 100; i++) c.scoreOutcome(new float[4], 1_000_000);
            Stats s = c.stats();
            check(s.fallbacks(FallbackReason.NOT_CONNECTED) == 100 && s.latencyFallback().count() == 0 && s.fallbackTime().count() == 0, "nothing timed");
            String text = c.prometheusText();
            check(text.contains("scz_client_calls_total{source=\"fallback\",reason=\"NOT_CONNECTED\"} 100\n"), text);
            check(text.contains("scz_client_fallback_seconds_count 0\n") && text.contains("scz_client_model_gen 0\n"), text);
        }
    }

    static void unit() {
        Tests.test("metrics: a duration lands in the first bucket at least as large", MetricsTests::bucketPlacement);
        Tests.test("metrics: the text format names each family once, numbers as Rust writes them", MetricsTests::textFormat);
        Tests.test("metrics: a thread's sample count is shared by every client it calls", MetricsTests::sharedAcrossClients);
        Tests.test("metrics: the hook sees every fallback, and the fallback is timed", MetricsTests::hookAndFallbackTiming);
        Tests.test("metrics: what the hook throws propagates", MetricsTests::hookThrows);
        Tests.test("metrics: sampling can be turned off", MetricsTests::samplingOff);
    }

    // ── against scz-worker-echo ─────────────────────────────────────────────────────────────────

    static void sampledCallsTimedAndExported(String socket) throws Exception {
        try (Client<float[], float[]> c = SidecarZero.connect(socket).codec(new F32()).fallback((float[] x, FallbackReason r) -> x).options(every(4)).build()) {
            check(c.awaitBound(java.time.Duration.ofSeconds(10)), "not bound to " + socket);
            onFreshThread(() -> {
                for (int i = 0; i < 400; i++) {
                    Outcome<float[]> o = c.infer(new float[] {1, 1, 1, 1}, 50_000_000L);
                    check(o.fromModel() && o.value()[3] == 1f, "model: " + o);
                }
                // A budget above the sidecar's 100 ms maximum is clamped, and counted.
                check(c.infer(new float[] {1, 1, 1, 1}, 1_000_000_000L).fromModel(), "model");
            });
            Stats s = c.stats();
            check(s.model() == 401, "model " + s.model());
            check(s.latencyModel().count() == 101, "the first call, then one in four: " + s.latencyModel());
            check(s.latencyModel().sumSeconds() > 0 && s.latencyModel().cumulative(16) == 101, s.latencyModel().toString());
            check(s.latencyFallback().count() == 0, s.latencyFallback().toString());
            check(s.clamped() == 1, "clamped " + s.clamped());
            check(s.modelGen() >= 1, "model gen " + s.modelGen());
            String text = c.prometheusText();
            for (String line : new String[] {
                "scz_client_calls_total{source=\"model\"} 401\n",
                "scz_client_calls_total{source=\"fallback\",reason=\"DEADLINE\"} 0\n",
                "scz_client_latency_seconds_count{source=\"model\"} 101\n",
                "scz_client_latency_seconds_bucket{source=\"model\",le=\"+Inf\"} 101\n",
                "scz_client_budget_clamped_total 1\n",
                "scz_client_reconnects_total 0\n",
                "scz_client_state 2\n",
                "# TYPE scz_client_breaker_open gauge\n",
                "# TYPE scz_client_fallback_seconds histogram\n",
            }) {
                check(text.contains(line), "missing " + line.strip() + " in\n" + text);
            }
            check(text.split("scz_client_calls_total\\{", -1).length - 1 == 1 + FallbackReason.count(), "every reason, zeros included:\n" + text);
            // The families in the order every SDK exports them.
            String[] order = {"# TYPE scz_client_calls_total counter", "# TYPE scz_client_latency_seconds histogram", "# TYPE scz_client_fallback_seconds histogram",
                "# TYPE scz_client_budget_clamped_total counter", "# TYPE scz_client_breaker_open gauge", "# TYPE scz_client_late_wakes_total counter",
                "# TYPE scz_client_reconnects_total counter", "# TYPE scz_client_state gauge", "# TYPE scz_client_model_gen gauge"};
            int at = -1;
            for (String o : order) {
                int i = text.indexOf(o + "\n");
                check(i > at, o + " out of order:\n" + text);
                at = i;
            }
            check(text.split("# TYPE ", -1).length - 1 == order.length, "one TYPE line per family:\n" + text);
            // A sink sees the same pass.
            StringBuilder seen = new StringBuilder();
            c.export(new MetricsSink() {
                @Override
                public void counter(String name, String[] labels, long value) {
                    seen.append('c');
                }

                @Override
                public void gauge(String name, String[] labels, double value) {
                    seen.append('g');
                }

                @Override
                public void histogram(String name, String[] labels, double[] bounds, long[] cumulative, double sum, long count) {
                    check(bounds.length == 16 && cumulative.length == 17 && cumulative[16] == count, name);
                    seen.append('h');
                }
            });
            check(seen.toString().equals("c".repeat(1 + FallbackReason.count()) + "hhhcgccgg"), "the pass: " + seen);
        }
    }

    /** A hook that throws on a fallback taken with an entry held (TOO_LARGE): the entry is released. */
    static void hookThrowsReleases(String socket) throws Exception {
        RuntimeException boom = new RuntimeException("from the hook");
        try (Client<float[], float[]> c = SidecarZero.connect(socket).codec(new F32()).fallback((float[] x, FallbackReason r) -> x).onFallback(r -> {
            throw boom;
        }).options(Options.defaults().callers(2).sampleEvery(1)).build()) {
            check(c.awaitBound(java.time.Duration.ofSeconds(10)), "not bound to " + socket);
            float[] big = new float[1 << 16]; // more than any request region
            for (int i = 0; i < 10; i++) {
                Throwable got = null;
                try {
                    c.infer(big, 50_000_000L);
                } catch (Throwable e) {
                    got = e;
                }
                check(got == boom, "propagated: " + got);
            }
            // With two entries, a leaked one would show at once as NO_SLOT.
            for (int i = 0; i < 10; i++) check(c.infer(new float[4], 50_000_000L).fromModel(), "model after the hook threw");
            Stats s = c.stats();
            check(s.fallbacks(FallbackReason.TOO_LARGE) == 10 && s.model() == 10, "counted");
            check(s.fallbackTime().count() == 0 && s.latencyModel().count() == 10, "a throwing hook times nothing: " + s.fallbackTime());
        }
    }

    static void live(String socket) {
        Tests.test("metrics: sampled calls are timed and exported (scz-worker-echo)", () -> sampledCallsTimedAndExported(socket));
        Tests.test("metrics: a throwing hook releases what the call held (scz-worker-echo)", () -> hookThrowsReleases(socket));
    }

    // ── against the fraud model: the primitive clients' model path ──────────────────────────────

    static void primitiveModelPath(String socket) throws Exception {
        float[] f = new float[256], rows3 = new float[3 * 256], scores = new float[3];
        try (ScoreClient sc = SidecarZero.connect(socket).score(256).fallback((x, r) -> -1f).options(every(1)).build()) {
            check(sc.awaitBound(java.time.Duration.ofSeconds(30)), "not bound to " + socket);
            for (int i = 0; i < 50; i++) sc.scoreOutcome(f, 20_000_000L);
            Stats s = sc.stats();
            long fb = 0;
            for (FallbackReason r : FallbackReason.values()) fb += s.fallbacks(r);
            check(s.model() > 0 && s.latencyModel().count() == s.model() && s.latencyFallback().count() == fb, "ScoreClient: " + s.latencyModel());
            check(s.modelGen() >= 1 && sc.prometheusText().contains("scz_client_latency_seconds_count{source=\"model\"} " + s.model() + "\n"), "ScoreClient text");
        }
        try (BatchScoreClient bc = SidecarZero.connect(socket).batch(new DenseCodec(256)).fallback((x, k, out, r) -> {}).options(every(2)).build()) {
            check(bc.awaitBound(java.time.Duration.ofSeconds(30)), "not bound to " + socket);
            onFreshThread(() -> {
                for (int i = 0; i < 50; i++) bc.scoreInto(rows3, 3, scores, 20_000_000L);
            });
            Stats s = bc.stats();
            check(s.model() > 0 && s.latencyModel().count() + s.latencyFallback().count() == 25, "BatchScoreClient: one in two of 50: " + s.latencyModel() + " "
                    + s.latencyFallback());
        }
    }

    static void liveFrames(String socket) {
        Tests.test("metrics: the primitive clients time their model answers (fraud model)", () -> primitiveModelPath(socket));
    }
}
