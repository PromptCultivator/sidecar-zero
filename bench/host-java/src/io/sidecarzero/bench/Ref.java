package io.sidecarzero.bench;

import io.sidecarzero.internal.Native;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.math.BigDecimal;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.locks.LockSupport;

/** What {@code bench/common} gives every host: the reference fallback, the reasons, the rows, the clock. */
final class Ref {
    private Ref() {}

    static final int FEATURES = 256;

    /** SidecarZero's reasons (sdk-contract §3.3, the SDK's enum order), then the baselines' wrapper's. */
    static final String[] REASONS = {
        "NOT_CONNECTED", "ABI_MISMATCH", "SCHEMA_MISMATCH", "REBINDING", "GATE_INIT", "GATE_SHEDDING", "GATE_RELOADING", "GATE_DEAD",
        "GATE_DRAINING", "STALE_HEARTBEAT", "CLOCK_FAULT", "OVERLOADED", "TOO_LARGE", "NO_SLOT", "CODEC_ERROR", "RING_FULL", "DEADLINE",
        "WORKER_FAILED", "MODEL_ERROR", "BAD_REQUEST", "RESOURCE_EXHAUSTED", "TRANSPORT_ERROR", "TRANSPORT_TIMEOUT", "OTHER",
    };
    static final int TRANSPORT_ERROR = 21, TRANSPORT_TIMEOUT = 22;

    /** Admission-phase reasons: decided before a slot is claimed (bench/common is_admission). */
    static boolean isAdmission(int r) {
        return r <= 11; // NOT_CONNECTED .. OVERLOADED
    }

    /** The reference fallback: w[i] = ((i * 37) mod 17 − 8) / 1000, no bias, in float32 as in Rust. */
    static float fallback(float[] x) {
        float s = 0f;
        for (int i = 0; i < FEATURES; i++) s += ((float) ((i * 37) % 17) - 8.0f) / 1000.0f * x[i];
        return s;
    }

    static float[][] readRows(String path) throws Exception {
        byte[] b = Files.readAllBytes(Path.of(path));
        if (b.length == 0 || b.length % (FEATURES * 4) != 0) throw new IllegalArgumentException(path + ": not rows of " + FEATURES + " float32");
        MemorySegment s = MemorySegment.ofArray(b);
        float[][] rows = new float[b.length / (FEATURES * 4)][FEATURES];
        for (int i = 0; i < rows.length; i++) {
            MemorySegment.copy(s, ValueLayout.JAVA_FLOAT_UNALIGNED.withOrder(ByteOrder.LITTLE_ENDIAN), (long) i * FEATURES * 4, rows[i], 0, FEATURES);
        }
        return rows;
    }

    /** CLOCK_MONOTONIC minus System.nanoTime() (zero on HotSpot/Linux, measured anyway). */
    static final long OFFSET;

    static {
        long off = 0;
        if (Native.LINUX) {
            MemorySegment ts = Arena.global().allocate(16, 8);
            long best = Long.MAX_VALUE;
            for (int i = 0; i < 200; i++) {
                long a = System.nanoTime(), m = Native.monotonicNs(ts), c = System.nanoTime();
                if (c - a < best) {
                    best = c - a;
                    off = m - (a + (c - a) / 2);
                }
            }
        }
        OFFSET = off;
    }

    /** CLOCK_MONOTONIC in nanoseconds: comparable with the supervisor's and the kill agent's stamps. */
    static long now() {
        return System.nanoTime() + OFFSET;
    }

    static long splitmix(long[] x) {
        x[0] += 0x9e3779b97f4a7c15L;
        long z = x[0];
        z = (z ^ (z >>> 30)) * 0xbf58476d1ce4e5b9L;
        z = (z ^ (z >>> 27)) * 0x94d049bb133111ebL;
        return z ^ (z >>> 31);
    }

    /** Sleep while more than 2.5 ms away, then spin (bench/common wait_until). */
    static void waitUntil(long t) {
        for (;;) {
            long now = now();
            if (now >= t) return;
            long left = t - now;
            if (left > 2_500_000) LockSupport.parkNanos(left - 2_000_000);
            else Thread.onSpinWait();
        }
    }

    /** An f64 as Rust's Display writes it: the shortest round-trip digits, never an exponent. */
    static String f64(double v) {
        if (v == Math.rint(v) && Math.abs(v) < 1e15) return Long.toString((long) v);
        return new BigDecimal(Double.toString(v)).stripTrailingZeros().toPlainString();
    }
}
