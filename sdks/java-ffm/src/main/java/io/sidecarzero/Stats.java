package io.sidecarzero;

/** Counters summed over every caller entry, and the sampled histograms (sdk-contract §8), at one moment. */
public final class Stats {
    private final long model;
    private final long[] fallbacks;
    private final long sessions, retired, lateWakes, polls, clamped, modelGen;
    private final int breakerOpen, parkedCallers, state;
    private final long attempts;
    private final Histogram latencyModel, latencyFallback, fallbackTime;

    Stats(long model, long[] fallbacks, long sessions, long retired, long lateWakes, long polls, long clamped, int breakerOpen, int parkedCallers, int state,
          long attempts, long modelGen, Histogram latencyModel, Histogram latencyFallback, Histogram fallbackTime) {
        this.attempts = attempts;
        this.model = model;
        this.fallbacks = fallbacks;
        this.sessions = sessions;
        this.retired = retired;
        this.lateWakes = lateWakes;
        this.polls = polls;
        this.clamped = clamped;
        this.breakerOpen = breakerOpen;
        this.parkedCallers = parkedCallers;
        this.state = state;
        this.modelGen = modelGen;
        this.latencyModel = latencyModel;
        this.latencyFallback = latencyFallback;
        this.fallbackTime = fallbackTime;
    }

    /** Calls the model answered. */
    public long model() {
        return model;
    }

    /** Calls the fallback answered with this reason. */
    public long fallbacks(FallbackReason r) {
        return fallbacks[r.ordinal()];
    }

    /** Sessions bound so far. */
    public long sessions() {
        return sessions;
    }

    /** Arenas unmapped after their session ended. */
    public long retired() {
        return retired;
    }

    /** Waits that ended at their deadline and found the slot completed over 1 ms earlier (signaling §7). */
    public long lateWakes() {
        return lateWakes;
    }

    /** Calls that took the polling wait (virtual threads beyond the pin cap). */
    public long polls() {
        return polls;
    }

    /** Calls whose budget exceeded the sidecar's max_deadline_ns. */
    public long clamped() {
        return clamped;
    }

    /** Caller entries whose breaker is shedding. */
    public int breakerOpen() {
        return breakerOpen;
    }

    /** Virtual-thread callers parked in a futex downcall now, in the whole JVM ({@code scz_client_parked_callers}). */
    public int parkedCallers() {
        return parkedCallers;
    }

    /** Connection attempts the background thread has finished, successful or not. */
    public long connectAttempts() {
        return attempts;
    }

    /** The connection state: 0 disconnected, 1 connecting, 2 bound, 3 rejected (ABI), 4 schema rejected. */
    public int state() {
        return state;
    }

    /** The highest model generation that has answered any caller (unsigned 32 bits; 0 before the first answer). */
    public long modelGen() {
        return modelGen;
    }

    /** End-to-end call time of the sampled calls the model answered. */
    public Histogram latencyModel() {
        return latencyModel;
    }

    /** End-to-end call time of the sampled calls that fell back, the fallback included. */
    public Histogram latencyFallback() {
        return latencyFallback;
    }

    /** Time spent inside the fallback function, on the sampled calls that fell back. */
    public Histogram fallbackTime() {
        return fallbackTime;
    }
}
