package io.sidecarzero;

import java.time.Duration;
import java.util.Objects;
import java.util.function.Consumer;

/**
 * The primitive specialization for the launch use case (sdk-java §4): one row of float32
 * features in, one score out, no allocation per call on any path. The outcome travels in the
 * return value ({@link ScoreOutcome}), never in per-thread state.
 */
public final class ScoreClient implements AutoCloseable {
    private final Engine engine;
    private final ScoreFallback fallback;

    ScoreClient(Engine engine, ScoreFallback fallback) {
        this.engine = engine;
        this.fallback = fallback;
    }

    /** The second stage: the mandatory fallback. */
    public interface FallbackStage {
        BuildStage fallback(ScoreFallback fallback);
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

        ScoreClient build();
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
                public ScoreClient build() {
                    return new ScoreClient(new Engine(endpoint, new Wire.Score(codec), o, hook), f);
                }
            };
        };
    }

    /** The score alone, for callers that do not need to know where it came from. */
    public float score(float[] features, long budgetNanos) {
        return ScoreOutcome.score(scoreOutcome(features, budgetNanos));
    }

    /** The score with its source and reason, packed into a long ({@link ScoreOutcome}). */
    public long scoreOutcome(float[] features, long budgetNanos) {
        Engine e = engine;
        long t0 = e.sampleStart();
        Caller c = e.enter();
        long st;
        int bits = 0;
        if (c.index < 0) {
            st = e.unattached(c);
        } else {
            try {
                st = e.run(c, features, null, budgetNanos);
                bits = (int) c.outBits;
            } finally {
                e.exit(c);
            }
        }
        if (st >= 0) {
            if (t0 != Engine.UNTIMED) e.timedModel(t0);
            return ScoreOutcome.model(bits);
        }
        FallbackReason r = FallbackReason.of((int) (-st - 1));
        long t1 = e.fallbackStart(t0, r);
        float v = fallback.apply(features, r);
        if (t1 != Engine.UNTIMED) e.timedFallback(t0, t1);
        return ScoreOutcome.fallback(Float.floatToRawIntBits(v), r);
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
