package io.sidecarzero;

/**
 * Where a client's {@code export} writes each metric of sdk-contract §8. Adapters for a metrics
 * library implement it; {@link PrometheusText} is the one in the core, which has no dependency.
 *
 * <p>{@code labels} are name, value pairs in one array, in order:
 * {@code {"source", "fallback", "reason", "DEADLINE"}} (the form Micrometer's {@code Tags.of}
 * takes); an empty array for none. Each call gets arrays of its own, which the sink may keep.
 */
public interface MetricsSink {
    /** A counter's value: exact, never decreasing, unsigned. */
    void counter(String name, String[] labels, long value);

    void gauge(String name, String[] labels, double value);

    /**
     * A sampled histogram. {@code bounds} are {@link Histogram#boundsSeconds()}; {@code cumulative}
     * has one more entry, for +Inf, which equals {@code count}. {@code sum} is in seconds.
     */
    void histogram(String name, String[] labels, double[] bounds, long[] cumulative, double sum, long count);
}
