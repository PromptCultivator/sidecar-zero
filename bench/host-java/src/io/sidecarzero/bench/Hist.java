package io.sidecarzero.bench;

/**
 * The latency histogram of {@code bench/common} ({@code Hist}), bucket for bucket: exact below
 * 128 ns, then 64 sub-buckets per power of two, up to 2^40 ns, so that the report can merge
 * buckets across hosts. Recording allocates nothing.
 */
final class Hist {
    static final long EXACT = 128, SUB = 64;
    static final int BUCKETS = (int) (EXACT + (40 - 7 + 1) * SUB);

    final long[] counts = new long[BUCKETS];
    long count, min = -1L /* u64::MAX */, max;

    static int index(long v) {
        if (Long.compareUnsigned(v, EXACT) < 0) return (int) v;
        long e = Math.min(63 - Long.numberOfLeadingZeros(v), 40);
        long m = Math.min((v >>> (e - 6)) - SUB, SUB - 1);
        return (int) (EXACT + (e - 7) * SUB + m);
    }

    static long lower(int i) {
        if (i < EXACT) return i;
        long e = 7 + (i - EXACT) / SUB, m = (i - EXACT) % SUB;
        return (SUB + m) << (e - 6);
    }

    void record(long v) {
        counts[index(v)]++;
        count++;
        if (Long.compareUnsigned(v, min) < 0) min = v;
        if (Long.compareUnsigned(v, max) > 0) max = v;
    }

    void merge(Hist o) {
        for (int i = 0; i < BUCKETS; i++) counts[i] += o.counts[i];
        count += o.count;
        if (Long.compareUnsigned(o.min, min) < 0) min = o.min;
        if (Long.compareUnsigned(o.max, max) > 0) max = o.max;
    }

    /** The value at quantile q (0..=1): the lower edge of its bucket, or the maximum. */
    long quantile(double q) {
        if (count == 0) return 0;
        long rank = Math.min(Math.max((long) Math.ceil(q * count), 1), count);
        long seen = 0;
        for (int i = 0; i < BUCKETS; i++) {
            seen += counts[i];
            if (seen >= rank) {
                long l = lower(i);
                if (Long.compareUnsigned(l, max) > 0) l = max;
                if (Long.compareUnsigned(l, min) < 0) l = min;
                return l;
            }
        }
        return max;
    }

    String json() {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < BUCKETS; i++) {
            if (counts[i] > 0) {
                if (b.length() > 0) b.append(',');
                b.append('[').append(lower(i)).append(',').append(counts[i]).append(']');
            }
        }
        return "{\"count\":" + count + ",\"min\":" + (count == 0 ? "0" : Long.toUnsignedString(min)) + ",\"max\":" + Long.toUnsignedString(max)
                + ",\"p50\":" + quantile(0.5) + ",\"p90\":" + quantile(0.9) + ",\"p99\":" + quantile(0.99) + ",\"p999\":" + quantile(0.999)
                + ",\"buckets\":[" + b + "]}";
    }
}
