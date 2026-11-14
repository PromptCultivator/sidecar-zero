package io.sidecarzero;

import java.time.Duration;

/**
 * A client of the sidecar. {@link #infer} always returns an output: the model's if it answers
 * within the budget, else the fallback's (sdk-contract §1). Shared freely across threads,
 * platform and virtual.
 */
public interface Client<I, O> extends AutoCloseable {
    /** {@code budgetNanos}: how long the call may wait, counted from the call (not an absolute time). */
    Outcome<O> infer(I input, long budgetNanos);

    default Outcome<O> infer(I input, Duration budget) {
        return infer(input, budget.toNanos());
    }

    /** Wait until an arena is mapped and its gate open, for callers that prefer to start warm. */
    boolean awaitBound(Duration timeout);

    Stats stats();

    /** Every metric of sdk-contract §8, in one pass, to {@code sink}. */
    void export(MetricsSink sink);

    /** {@link #export} in the Prometheus text exposition format, for a service that serves {@code /metrics} itself. */
    default String prometheusText() {
        PrometheusText t = new PrometheusText();
        export(t);
        return t.finish();
    }

    /** Clear the snapshot, wait for calls inside the arena, unmap, close the socket (sdk-contract §6). */
    @Override
    void close();
}
