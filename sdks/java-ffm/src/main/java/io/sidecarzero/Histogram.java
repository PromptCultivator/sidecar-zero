package io.sidecarzero;

import java.util.Arrays;
import java.util.concurrent.atomic.AtomicLongArray;

/**
 * One sampled histogram as read (sdk-contract §8): its bucket counts, cumulative as the
 * Prometheus format has them, its sum in seconds, and its count. The buckets are fixed by the
 * contract, the same in every SDK: {@link #boundsSeconds()}, then +Inf. A duration lands in the
 * first bucket whose bound is at least as large.
 */
public final class Histogram {
    /** Upper bounds of the buckets, in seconds; a last bucket, +Inf, follows them. */
    private static final double[] BOUNDS = {1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4, 2e-4, 5e-4, 1e-3, 2e-3, 5e-3, 1e-2, 2e-2, 5e-2, 0.1};
    /** Buckets, +Inf included. */
    public static final int BUCKETS = 17;
    /** The longs one histogram takes where it is recorded ({@link Engine#record}): the bucket counts, then the sum in nanoseconds. */
    static final int LONGS = BUCKETS + 1;

    private final long[] cumulative;
    private final double sumSeconds;
    private final long count;

    private Histogram(long[] cumulative, double sumSeconds, long count) {
        this.cumulative = cumulative;
        this.sumSeconds = sumSeconds;
        this.count = count;
    }

    /** The upper bounds of the buckets in seconds, +Inf left out (a copy). */
    public static double[] boundsSeconds() {
        return BOUNDS.clone();
    }

    /** {@code cumulative()[i]}: durations of at most {@code boundsSeconds()[i]}; the last, every duration (a copy). */
    public long[] cumulative() {
        return cumulative.clone();
    }

    public long cumulative(int bucket) {
        return cumulative[bucket];
    }

    public double sumSeconds() {
        return sumSeconds;
    }

    public long count() {
        return count;
    }

    @Override
    public String toString() {
        return "Histogram[count=" + count + " sum=" + sumSeconds + "s cumulative=" + Arrays.toString(cumulative) + "]";
    }

    // ── summing, on read (sampled calls record with Engine.record) ──────────────────────────────

    /** The histogram at {@code base} of each of {@code shards} shards {@code stride} longs apart, summed. */
    static Histogram sum(AtomicLongArray a, int shards, int stride, int base) {
        long[] c = new long[BUCKETS];
        long sum = 0;
        for (int s = 0; s < shards; s++) {
            int o = s * stride + base;
            for (int i = 0; i < BUCKETS; i++) c[i] += a.get(o + i);
            sum += a.get(o + BUCKETS);
        }
        long acc = 0;
        for (int i = 0; i < BUCKETS; i++) {
            acc += c[i];
            c[i] = acc;
        }
        return new Histogram(c, sum / 1e9, acc);
    }
}
