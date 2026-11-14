package io.sidecarzero;

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.atomic.AtomicLong;

/**
 * The allocation gate (testing §7) against a real supervisor: bytes allocated per call on every
 * path, after warm-up, from {@code com.sun.management.ThreadMXBean} allocated-bytes deltas over
 * many calls, compared with an empty loop measured the same way in the same run.
 *
 * <pre>
 *   java ... io.sidecarzero.AllocGate keyed  SOCKET    against scz-worker-chaos (opaque bytes)
 *   java ... io.sidecarzero.AllocGate frames SOCKET    against sidecarzero-worker with the fraud model
 * </pre>
 *
 * <p>Platform threads read their own counter. A virtual thread's own counter is not available
 * (it reads -1), and a virtual thread may run on any carrier, so for virtual threads the counters
 * of every carrier of the scheduler are summed before and after (the loop runs alone on the
 * scheduler); the empty baseline is a virtual thread measured the same way, which absorbs the
 * thread's own start and end.
 *
 * <p>The gate: per-call bytes above the empty loop's below {@link #TOLERANCE} (one 16-byte object
 * in 1,600 calls would fail it). The polling wait is reported, not gated: it parks the virtual
 * thread with {@code LockSupport.parkNanos}, and the JDK allocates its timer task and the
 * unmounted stack (sdk-java §7 exempts it).
 */
public final class AllocGate {
    static final com.sun.management.ThreadMXBean MX = (com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean();
    static final double TOLERANCE = 0.01;
    static volatile long sink;
    static int failures;
    static final List<String> report = new ArrayList<>();

    // ── a zero-allocation front end for opaque bytes, over the engine (test only) ──────────────

    /** Request bytes in, the 8-byte keyed answer out, into the entry: allocates nothing. */
    static final class KeyedWire extends Wire {
        KeyedWire() {
            super(null);
        }

        @Override
        int size(Caller c, Object in) {
            return ((byte[]) in).length;
        }

        @Override
        void encode(Caller c, Object in, SlotWriter w) {
            w.write((byte[]) in);
        }

        @Override
        void decode(Caller c, Object out, SlotReader r) {
            if (r.length() != 8) throw CodecException.SHAPE;
            c.outBits = r.readLong(0);
        }
    }

    /** What the front ends do around a call, the sampled timing and the hook included (sdk-contract §8). */
    static long call(Engine e, byte[] req, long budget) {
        long t0 = e.sampleStart();
        Caller c = e.enter();
        long st;
        if (c.index < 0) {
            st = e.unattached(c);
        } else {
            try {
                st = e.run(c, req, null, budget);
                if (st >= 0) sink += c.outBits;
            } finally {
                e.exit(c);
            }
        }
        if (st >= 0) {
            if (t0 != Engine.UNTIMED) e.timedModel(t0);
        } else {
            long t1 = e.fallbackStart(t0, FallbackReason.of((int) (-st - 1)));
            sink += 1; // the fallback
            if (t1 != Engine.UNTIMED) e.timedFallback(t0, t1);
        }
        sink += st;
        return st;
    }

    // ── the chaos request format (conformance/proto) ────────────────────────────────────────────

    static byte[] request(long op, long arg, int thread, int seq, long seed, int rows) {
        long magic = 0;
        byte[] m = "SCZCHAOS".getBytes(java.nio.charset.StandardCharsets.US_ASCII);
        for (int i = 7; i >= 0; i--) magic = magic << 8 | (m[i] & 0xFF);
        long nonce = ((long) thread << 32) | (seq & 0xFFFF_FFFFL);
        long[] w = new long[5 + rows];
        w[0] = magic;
        w[1] = op;
        w[2] = arg;
        w[3] = nonce;
        w[4] = seed;
        byte[] b = new byte[w.length * 8];
        for (int i = 0; i < w.length; i++) for (int k = 0; k < 8; k++) b[8 * i + k] = (byte) (w[i] >>> (8 * k));
        return b;
    }

    // ── measuring ───────────────────────────────────────────────────────────────────────────────

    interface Body {
        void run(int i);
    }

    static final Body EMPTY = i -> sink += i;

    /** Bytes the current (platform) thread allocates over {@code n} runs of {@code b}, after warm-up. */
    static long platform(Body b, int warm, int n) {
        for (int i = 0; i < warm; i++) b.run(i);
        long b0 = MX.getCurrentThreadAllocatedBytes();
        for (int i = 0; i < n; i++) b.run(i);
        return MX.getCurrentThreadAllocatedBytes() - b0;
    }

    static long[] carriers() {
        Set<Long> ids = new HashSet<>();
        for (ThreadInfo t : MX.getThreadInfo(MX.getAllThreadIds())) {
            if (t != null && t.getThreadName().startsWith("ForkJoinPool") && t.getThreadName().contains("worker")) ids.add(t.getThreadId());
        }
        return ids.stream().mapToLong(Long::longValue).toArray();
    }

    static long sum(long[] ids) {
        long s = 0;
        for (long v : MX.getThreadAllocatedBytes(ids)) s += Math.max(0, v);
        return s;
    }

    /** The same on a virtual thread: the carriers' counters summed around a virtual thread that runs it. */
    static long virtual(Body b, int warm, int n) throws InterruptedException {
        Thread.ofVirtual().start(() -> {
            for (int i = 0; i < warm; i++) b.run(i);
        }).join();
        long[] ids = carriers();
        long b0 = sum(ids);
        Thread t = Thread.ofVirtual().start(() -> {
            for (int i = 0; i < n; i++) b.run(i);
        });
        t.join();
        long b1 = sum(ids);
        return b1 - b0;
    }

    static void gate(String path, long bytes, long empty, int n, boolean gated, String outcome) {
        double per = (bytes - empty) / (double) n;
        boolean ok = per < TOLERANCE;
        String line = String.format("%-44s %10d B over %7d calls, empty %8d B: %8.4f B/call %s  [%s]", path, bytes, n, empty, per,
                ok ? "ok" : gated ? "FAIL" : "(reported, not gated)", outcome);
        report.add(line);
        System.out.println(line);
        if (!ok && gated) failures++;
    }

    static String outcomes(Stats before, Stats after) {
        StringBuilder s = new StringBuilder("model " + (after.model() - before.model()));
        for (FallbackReason r : FallbackReason.values()) {
            long d = after.fallbacks(r) - before.fallbacks(r);
            if (d > 0) s.append(", ").append(r.label()).append(' ').append(d);
        }
        return s.toString();
    }

    // ── the paths ───────────────────────────────────────────────────────────────────────────────

    static void keyed(String socket) throws Exception {
        Engine e = new Engine(socket, new KeyedWire(), Options.defaults());
        if (!e.awaitBound(20_000_000_000L)) throw new IllegalStateException("not bound to " + socket);
        final long ms = 1_000_000L;
        byte[] normal = request(0, 0, 0, 0, 1, 0);
        byte[] big = new byte[4096]; // more than any request region: TOO_LARGE at admission
        byte[] slow = request(1, 2500, 0, 1, 1, 0); // delay 2.5 ms
        int n = 20_000;
        long empty = platform(EMPTY, 20_000, n);

        Hooks.fixSpinCeiling(-1);
        Stats s0 = e.stats();
        gate("model answer, spinning (platform)", platform(i -> call(e, normal, 20 * ms), 20_000, n), empty, n, true, outcomes(s0, s0 = e.stats()));

        Hooks.fixSpinCeiling(0);
        gate("model answer, after parking (platform)", platform(i -> call(e, normal, 20 * ms), 40_000, n), empty, n, true, outcomes(s0, s0 = e.stats()));
        Hooks.fixSpinCeiling(-1);

        gate("fallback at admission: TOO_LARGE (platform)", platform(i -> call(e, big, 20 * ms), 20_000, n), empty, n, true, outcomes(s0, s0 = e.stats()));

        // A deadline miss then a model answer, so the breaker never opens: the misses are
        // abandons (the worker still processing) or the worker's DEADLINE_EXCEEDED.
        int pairs = 600;
        gate("abandon at the deadline + model answer (platform)", platform(i -> {
            call(e, slow, 1 * ms);
            call(e, normal, 20 * ms);
        }, 200, pairs), platform(EMPTY, 200, pairs), pairs, true, outcomes(s0, s0 = e.stats()));

        // Misses in a row open the breaker: the calls shed with OVERLOADED, with a probe now and then.
        Thread.sleep(50);
        gate("shed call: OVERLOADED, and probes (platform)", platform(i -> call(e, slow, 1 * ms), 2_000, 200_000), platform(EMPTY, 2_000, 200_000), 200_000, true,
                outcomes(s0, s0 = e.stats()));
        Thread.sleep(200); // the breaker closes after its interval; the worker catches up

        // First call from a thread that has never called: each new thread reads its own counter
        // around its first call, after the code path is warm (above).
        int threads = 200;
        AtomicLong firstBytes = new AtomicLong(), emptyBytes = new AtomicLong();
        for (int k = 0; k < threads; k++) {
            Thread t = new Thread(() -> {
                long a0 = MX.getCurrentThreadAllocatedBytes();
                sink += 1;
                long a1 = MX.getCurrentThreadAllocatedBytes();
                call(e, normal, 20 * ms);
                long a2 = MX.getCurrentThreadAllocatedBytes();
                emptyBytes.addAndGet(a1 - a0);
                firstBytes.addAndGet(a2 - a1);
            });
            t.start();
            t.join();
        }
        gate("first call from a new thread (platform)", firstBytes.get(), emptyBytes.get(), threads, true, outcomes(s0, s0 = e.stats()));

        // Virtual threads: pinned in the futex (under the cap), and the polling wait.
        long vEmpty = virtual(EMPTY, 20_000, n);
        gate("model answer, spinning (virtual)", virtual(i -> call(e, normal, 20 * ms), 20_000, n), vEmpty, n, true, outcomes(s0, s0 = e.stats()));
        Hooks.fixSpinCeiling(0);
        gate("model answer, parked in the futex (virtual)", virtual(i -> call(e, normal, 20 * ms), 40_000, n), vEmpty, n, true, outcomes(s0, s0 = e.stats()));
        e.forcePoll = true;
        gate("model answer, polling wait (virtual)", virtual(i -> call(e, normal, 20 * ms), 5_000, n), vEmpty, n, false, outcomes(s0, s0 = e.stats()));
        e.forcePoll = false;
        Hooks.fixSpinCeiling(-1);
        gate("fallback at admission: TOO_LARGE (virtual)", virtual(i -> call(e, big, 20 * ms), 20_000, n), vEmpty, n, true, outcomes(s0, s0 = e.stats()));
        System.out.println("polls counted by the SDK: " + e.stats().polls());
        e.close();

        // Every call timed, with a hook (sdk-contract §8): the sampled path and the hook's call
        // allocate nothing either (the paths above time one call in 64).
        Engine timed = new Engine(socket, new KeyedWire(), Options.defaults().sampleEvery(1), r -> sink++);
        if (!timed.awaitBound(20_000_000_000L)) throw new IllegalStateException("not bound to " + socket);
        s0 = timed.stats();
        gate("model answer, every call timed (platform)", platform(i -> call(timed, normal, 20 * ms), 20_000, n), empty, n, true, outcomes(s0, s0 = timed.stats()));
        gate("TOO_LARGE, every call timed, hook (platform)", platform(i -> call(timed, big, 20 * ms), 20_000, n), empty, n, true, outcomes(s0, s0 = timed.stats()));
        gate("model answer, every call timed (virtual)", virtual(i -> call(timed, normal, 20 * ms), 20_000, n), vEmpty, n, true, outcomes(s0, s0 = timed.stats()));
        Stats ts = timed.stats();
        long fbs = 0;
        for (FallbackReason r : FallbackReason.values()) fbs += ts.fallbacks(r);
        if (ts.latencyModel().count() != ts.model() || ts.fallbackTime().count() != fbs || ts.latencyFallback().count() != fbs) {
            throw new IllegalStateException("every call timed: " + ts.latencyModel() + ", " + ts.fallbackTime());
        }
        timed.close();

        // No snapshot at all: a client whose sidecar does not exist.
        Engine none = new Engine(socket + ".absent", new KeyedWire(), Options.defaults());
        // (one call in 64 timed here too)
        Stats z = none.stats();
        gate("fallback with no arena: NOT_CONNECTED (platform)", platform(i -> call(none, normal, 20 * ms), 20_000, n), empty, n, true, outcomes(z, none.stats()));
        none.close();
    }

    static void frames(String socket) throws Exception {
        float[] f = new float[256];
        for (int i = 0; i < f.length; i++) f[i] = (i % 7) * 0.1f;
        float[] rows3 = new float[3 * 256];
        for (int i = 0; i < rows3.length; i++) rows3[i] = (i % 5) * 0.2f;
        float[] scores = new float[3];
        ScoreClient sc = SidecarZero.connect(socket).score(256).fallback((x, r) -> -1f).build();
        if (!sc.awaitBound(java.time.Duration.ofSeconds(30))) throw new IllegalStateException("not bound to " + socket);
        long budget = 20_000_000L;
        int n = 20_000;
        long empty = platform(EMPTY, 20_000, n);
        Stats s0 = sc.stats();
        Hooks.fixSpinCeiling(-1);
        gate("ScoreClient.scoreOutcome, spinning (platform)", platform(i -> sink += sc.scoreOutcome(f, budget), 20_000, n), empty, n, true, outcomes(s0, s0 = sc.stats()));
        Hooks.fixSpinCeiling(0);
        gate("ScoreClient.scoreOutcome, after parking (platform)", platform(i -> sink += sc.scoreOutcome(f, budget), 40_000, n), empty, n, true, outcomes(s0, s0 = sc.stats()));
        Hooks.fixSpinCeiling(-1);
        long vEmpty = virtual(EMPTY, 20_000, n);
        gate("ScoreClient.scoreOutcome, spinning (virtual)", virtual(i -> sink += sc.scoreOutcome(f, budget), 20_000, n), vEmpty, n, true, outcomes(s0, s0 = sc.stats()));
        sc.close();
        // The batch client binds after the score client's session has ended.
        BatchScoreClient bc = SidecarZero.connect(socket).batch(new DenseCodec(256)).fallback((x, k, out, r) -> {}).build();
        if (!bc.awaitBound(java.time.Duration.ofSeconds(30))) throw new IllegalStateException("not bound to " + socket);
        Stats b0 = bc.stats();
        gate("BatchScoreClient.scoreInto, 3 rows (platform)", platform(i -> sink += bc.scoreInto(rows3, 3, scores, budget), 20_000, n), empty, n, true, outcomes(b0, bc.stats()));
        bc.close();
    }

    public static void main(String[] a) throws Exception {
        switch (a[0]) {
            case "keyed" -> keyed(a[1]);
            case "frames" -> frames(a[1]);
            default -> throw new IllegalArgumentException(a[0]);
        }
        System.out.println(failures == 0 ? "allocation gate: pass" : "allocation gate: " + failures + " path(s) allocate");
        System.exit(failures == 0 ? 0 : 1);
    }
}
