package io.sidecarzero;

import java.time.Duration;

/** The generic {@link Client}: allocates its {@link Outcome}; use the primitive clients where that matters. */
final class GenericClient<I, O> implements Client<I, O> {
    private final Engine engine;
    private final FallbackPolicy<I, O> fallback;

    GenericClient(Engine engine, FallbackPolicy<I, O> fallback) {
        this.engine = engine;
        this.fallback = fallback;
    }

    @SuppressWarnings("unchecked")
    @Override
    public Outcome<O> infer(I input, long budgetNanos) {
        Engine e = engine;
        long t0 = e.sampleStart();
        Caller c = e.enter();
        long st;
        O v = null;
        if (c.index < 0) {
            st = e.unattached(c);
        } else {
            try {
                st = e.run(c, input, null, budgetNanos);
                if (st >= 0) v = (O) c.outObj;
                c.outObj = null;
            } finally {
                e.exit(c);
            }
        }
        if (st >= 0) {
            if (t0 != Engine.UNTIMED) e.timedModel(t0);
            return new Outcome<>(v, null, (int) st);
        }
        FallbackReason r = FallbackReason.of((int) (-st - 1));
        long t1 = e.fallbackStart(t0, r);
        O fv = fallback.apply(input, r);
        if (t1 != Engine.UNTIMED) e.timedFallback(t0, t1);
        return new Outcome<>(fv, r, 0);
    }

    @Override
    public boolean awaitBound(Duration timeout) {
        return engine.awaitBound(timeout.toNanos());
    }

    @Override
    public Stats stats() {
        return engine.stats();
    }

    @Override
    public void export(MetricsSink sink) {
        engine.export(sink);
    }

    @Override
    public String prometheusText() {
        return engine.prometheusText();
    }

    @Override
    public void close() {
        engine.close();
    }
}
