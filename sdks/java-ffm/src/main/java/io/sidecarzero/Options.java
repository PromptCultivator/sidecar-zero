package io.sidecarzero;

import java.time.Duration;

/** Client options. Mutable; read once when the client is built. */
public final class Options {
    /** One call in this many, per calling thread, is timed for the latency histograms by default (sdk-contract §8). */
    public static final int DEFAULT_SAMPLE_EVERY = 64;

    long fallbackReserveNanos = 0;
    long connectTimeoutMillis = 1000;
    int callers = 256;
    long spinCeilingNanos = -1;
    boolean virtualPolling = false;
    int sampleEvery = DEFAULT_SAMPLE_EVERY;

    public static Options defaults() {
        return new Options();
    }

    /** Stop waiting this long before the deadline, to leave the fallback its time (sdk-contract §1). */
    public Options fallbackReserve(Duration d) {
        fallbackReserveNanos = Math.max(0, d.toNanos());
        return this;
    }

    /** Applied to the connect and to each handshake message. */
    public Options connectTimeout(Duration d) {
        connectTimeoutMillis = Math.max(1, d.toMillis());
        return this;
    }

    /** Caller-state entries: at most this many calls are in flight at once (sdk-contract §7). */
    public Options callers(int n) {
        callers = Math.max(1, n);
        return this;
    }

    /** Fix the process-wide spin ceiling (zero: never spin); null for the adaptive default. */
    public Options spinCeiling(Duration d) {
        spinCeilingNanos = d == null ? -1 : Math.max(0, d.toNanos());
        return this;
    }

    /** Virtual threads always take the polling wait instead of pinning a carrier (sdk-java §7). */
    public Options virtualPolling(boolean on) {
        virtualPolling = on;
        return this;
    }

    /**
     * Time one call in this many on each calling thread, counted over every client it calls, for
     * the latency histograms (sdk-contract §8, "Sampling"): those whose number is a multiple of
     * {@code n}, the thread's first call among them. 0 times none. Counters are exact either way.
     */
    public Options sampleEvery(int n) {
        sampleEvery = Math.max(0, n);
        return this;
    }
}
