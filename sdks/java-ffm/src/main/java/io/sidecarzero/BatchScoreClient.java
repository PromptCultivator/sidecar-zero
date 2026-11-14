package io.sidecarzero;

import java.time.Duration;
import java.util.Objects;
import java.util.function.Consumer;

/**
 * Rows of float32 features in, as many scores out, into a caller-supplied array, with no
 * allocation per call. The smallest slot class whose template holds the rows is used.
 */
public final class BatchScoreClient implements AutoCloseable {
    private final Engine engine;
    private final BatchFallback fallback;

    BatchScoreClient(Engine engine, BatchFallback fallback) {
        this.engine = engine;
        this.fallback = fallback;
    }

    public interface FallbackStage {
        BuildStage fallback(BatchFallback fallback);
    }

    public interface BuildStage {
        BuildStage options(Options o);

        /**
         * Called with the reason once per fallback, on the calling thread, just before the fallback
         * function, after the slot and the caller entry have been released (sdk-contract §8): for
         * sampled logging. Off by default, and not on the zero-allocation path: what it does costs
         * every fallback. What it throws propagates as the fallback's would.
         */
        BuildStage onFallback(Consumer<FallbackReason> hook);

        BatchScoreClient build();
    }

    static FallbackStage stage(String endpoint, DenseCodec codec) {
        return f -> {
            Objects.requireNonNull(f, "a fallback is mandatory");
            return new BuildStage() {
                Options o = Options.defaults();
                Consumer<FallbackReason> hook;

                @Override
                public BuildStage options(Options opts) {
                    o = Objects.requireNonNull(opts);
                    return this;
                }

                @Override
                public BuildStage onFallback(Consumer<FallbackReason> h) {
                    hook = Objects.requireNonNull(h);
                    return this;
                }

                @Override
                public BatchScoreClient build() {
                    return new BatchScoreClient(new Engine(endpoint, new Wire.Batch(codec), o, hook), f);
                }
            };
        };
    }

    /**
     * Score {@code rows} rows of {@code features} (row-major, at least {@code rows × N} floats) into
     * {@code scores[0..rows)}. Returns the packed outcome: {@link ScoreOutcome#fromModel},
     * {@link ScoreOutcome#reason}, and {@link ScoreOutcome#modelGen}.
     */
    public long scoreInto(float[] features, int rows, float[] scores, long budgetNanos) {
        Engine e = engine;
        long t0 = e.sampleStart();
        Caller c = e.enter();
        long st;
        if (c.index < 0) {
            st = e.unattached(c);
        } else {
            try {
                c.argRows = rows;
                st = e.run(c, features, scores, budgetNanos);
            } finally {
                e.exit(c);
            }
        }
        if (st >= 0) {
            if (t0 != Engine.UNTIMED) e.timedModel(t0);
            return ScoreOutcome.model((int) st);
        }
        FallbackReason r = FallbackReason.of((int) (-st - 1));
        long t1 = e.fallbackStart(t0, r);
        fallback.apply(features, rows, scores, r);
        if (t1 != Engine.UNTIMED) e.timedFallback(t0, t1);
        return ScoreOutcome.fallback(0, r);
    }

    public boolean awaitBound(Duration timeout) {
        return engine.awaitBound(timeout.toNanos());
    }

    public Stats stats() {
        return engine.stats();
    }

    /** Every metric of sdk-contract §8, in one pass, to {@code sink}. */
    public void export(MetricsSink sink) {
        engine.export(sink);
    }

    /** {@link #export} in the Prometheus text exposition format, for a service that serves {@code /metrics} itself. */
    public String prometheusText() {
        return engine.prometheusText();
    }

    @Override
    public void close() {
        engine.close();
    }
}
