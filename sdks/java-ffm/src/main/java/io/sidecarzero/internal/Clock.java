package io.sidecarzero.internal;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;

/**
 * The cheap clock (sdk-java §6): {@code System.nanoTime()} plus an offset to {@code CLOCK_MONOTONIC},
 * the clock the supervisor and the worker use. Process-wide.
 *
 * <p>The offset comes from the tightest of several back-to-back samples of both clocks. The
 * background thread re-verifies it; a request thread that sees a heartbeat that its clock cannot
 * explain (in the future, or stale) re-verifies it too, on that rare path only. Either one that
 * finds the clocks apart raises the fault: calls fall back with {@code CLOCK_FAULT} until the
 * background thread has recalibrated, which it does once the fault is {@link #HOLD_NS} old.
 * {@link #skew} is the conformance agent's {@code CLOCK_SKEW}: it shifts the cheap clock.
 */
public final class Clock {
    private Clock() {}

    /** CLOCK_MONOTONIC minus System.nanoTime(), plus any skew a test applied. */
    private static volatile long adj;
    private static volatile boolean fault;
    private static volatile long faultSince;
    private static volatile boolean announced;

    /** How far apart the clocks may be before it is a fault (background check). */
    static final long TOL_NS = 250_000;
    /** How far in the future of a call's clock a heartbeat may be before the clock is checked. */
    public static final long ADMIT_TOL_NS = 1_000_000;
    /** How long a fault lasts at least before the background thread recalibrates. */
    static final long HOLD_NS = 10_000_000;

    private static final MemorySegment TS = Native.LINUX ? Arena.global().allocate(16, 8) : null;

    static {
        if (Native.LINUX) {
            calibrate();
        }
    }

    public static long now() {
        return System.nanoTime() + adj;
    }

    public static boolean fault() {
        return fault;
    }

    /** CLOCK_SKEW: add {@code ns} (signed) to the cheap clock. */
    public static synchronized void skew(long ns) {
        adj += ns;
    }

    /** The cheap clock's error against CLOCK_MONOTONIC, from the tightest of a few samples. */
    private static long error(MemorySegment ts) {
        long best = Long.MAX_VALUE, err = 0;
        for (int i = 0; i < 5; i++) {
            long a = now();
            long m = Native.monotonicNs(ts);
            long b = now();
            if (b - a < best) {
                best = b - a;
                err = m - (a + (b - a) / 2);
            }
        }
        return err;
    }

    private static synchronized void calibrate() {
        long best = Long.MAX_VALUE, off = 0;
        for (int i = 0; i < 200; i++) {
            long a = System.nanoTime();
            long m = Native.monotonicNs(TS);
            long b = System.nanoTime();
            if (b - a < best) {
                best = b - a;
                off = m - (a + (b - a) / 2);
            }
        }
        adj = off;
    }

    /**
     * A request thread's check, on the path where the heartbeat does not fit the call's clock.
     * {@code ts} is the caller entry's own buffer. Returns whether the clock is at fault (and
     * raises it).
     */
    public static boolean suspect(MemorySegment ts) {
        if (!Native.LINUX) return false;
        if (fault) return true;
        long e = error(ts);
        if (Math.abs(e) > ADMIT_TOL_NS / 4) {
            raise();
            return true;
        }
        return false;
    }

    private static void raise() {
        if (!fault) {
            faultSince = System.nanoTime();
            fault = true;
        }
    }

    /** The background thread's periodic check: verify, or recalibrate a fault that has lasted. */
    public static synchronized void verify() {
        if (!Native.LINUX) return;
        if (!fault) {
            long e = error(TS);
            if (Math.abs(e) > TOL_NS) {
                raise();
            } else {
                return;
            }
        }
        if (!announced) {
            announced = true;
            System.err.println("sidecarzero: the cheap clock disagrees with CLOCK_MONOTONIC by " + error(TS)
                    + " ns; calls fall back with CLOCK_FAULT until it is recalibrated");
        }
        if (System.nanoTime() - faultSince < HOLD_NS) return;
        calibrate();
        if (Math.abs(error(TS)) <= TOL_NS) {
            fault = false;
            announced = false;
            System.err.println("sidecarzero: the cheap clock is recalibrated");
        }
    }
}
