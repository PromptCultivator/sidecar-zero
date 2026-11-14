package io.sidecarzero;

import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/** Padding before the in-call marker, so it does not share a cache line with the previous entry. */
abstract class CallerPadL {
    long p01, p02, p03, p04, p05, p06, p07, p08;
}

/** The in-call marker: the entry's claim word. */
abstract class CallerHot extends CallerPadL {
    /** {@code snapshot id << 32} while a call holds the entry (Java leaves the thread id zero); 0 when free. */
    volatile long inCall;
}

abstract class CallerPadR extends CallerHot {
    long q01, q02, q03, q04, q05, q06, q07, q08;
}

/**
 * One caller-state entry (sdk-contract §7, sdk-java §5): claimed for the length of one call with
 * a compare-and-set on {@link #inCall}, which is also the call's in-call marker for arena
 * retirement (sdk-contract §6). Everything here is written only by the call holding the entry;
 * the next holder sees it through the claim (a volatile read of the released marker), and the
 * background thread only sums the counters and reads the markers. Nothing is allocated per call.
 */
final class Caller extends CallerPadR {
    static final VarHandle IN_CALL;

    static {
        try {
            IN_CALL = MethodHandles.lookup().findVarHandle(CallerHot.class, "inCall", long.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    final int index;
    /** The snapshot the holder entered under. */
    Snapshot snap;

    // the call algorithm's per-caller state
    final int[] cached = new int[8];
    int ringCursor;
    long nextRequest;
    long ewmaNs;
    long lastCallNs;
    long budgetNs;
    long tEnqueue;
    long cancelGate;

    // the breaker (sdk-contract §4)
    int misses;
    long brkUntil;
    long brkInterval;
    /** The session the breaker's state is about; a new session starts it closed. */
    Conn brkConn;

    // counters, summed on read
    long model;
    final long[] fb = new long[FallbackReason.count()];
    long lateWakes;
    long polls;
    long clamped;
    /** The model generation that last answered through this entry (unsigned 32 bits). */
    long modelGen;

    // what the scope guard must undo if the call unwinds (Engine.guard)
    int phase;
    MemorySegment gData;
    long gRec, gCell, gPos, gRingOff, gAddrBase;
    int gEpoch;

    // the futex timespec, and the views handed to codecs
    final MemorySegment ts;
    final SlotWriter sw = new SlotWriter();
    final SlotReader sr = new SlotReader();
    final FrameWriter fw = new FrameWriter();
    final FrameReader fr = new FrameReader();

    // a call's arguments and decoded result, for the front ends
    int argRows;
    long outBits;
    Object outObj;

    Caller(int index, MemorySegment ts) {
        this.index = index;
        this.ts = ts;
        this.ringCursor = index;
        // Each entry's first probe starts at its own place in each class, so that callers making
        // their first calls at once do not all probe the same slots.
        for (int k = 0; k < cached.length; k++) cached[k] = (int) (Engine.mix(((long) index << 3) | k) >>> 33);
    }

    boolean claim(long marker) {
        return IN_CALL.compareAndSet(this, 0L, marker);
    }

    void release() {
        IN_CALL.setRelease(this, 0L);
    }

    long marker() {
        return (long) IN_CALL.getVolatile(this);
    }
}
