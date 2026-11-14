package io.sidecarzero;

import io.sidecarzero.internal.Abi;
import io.sidecarzero.internal.Clock;
import io.sidecarzero.internal.Directory;
import io.sidecarzero.internal.Native;
import io.sidecarzero.internal.Registry;
import io.sidecarzero.internal.Registry.Template;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicLongArray;
import java.util.concurrent.locks.LockSupport;
import java.util.function.Consumer;

import static io.sidecarzero.internal.Abi.Cell;
import static io.sidecarzero.internal.Abi.ControlHeader;
import static io.sidecarzero.internal.Abi.RingHeader;
import static io.sidecarzero.internal.Abi.SlotRecord;
import static io.sidecarzero.internal.Abi.StatusPage;

/**
 * One client's machinery, shared by every front end: the caller entries, the published
 * snapshot, the call algorithm of sdk-contract §4, and the background thread of §6.
 *
 * <p>A call's status is a {@code long}: {@code >= 0} is a model answer (the value is the model
 * generation, unsigned), {@code < 0} is a fallback with reason ordinal {@code -status - 1}.
 */
final class Engine {
    // ── constants ───────────────────────────────────────────────────────────────────────────────
    static final int FREE = Abi.SlotState.FREE, CLAIMED = Abi.SlotState.CLAIMED, READY = Abi.SlotState.READY;
    static final int PROCESSING = Abi.SlotState.PROCESSING, DONE = Abi.SlotState.DONE, FAILED = Abi.SlotState.FAILED;
    static final int ABANDONED = Abi.SlotState.ABANDONED;
    static final long WAITER = Abi.SlotWord.WAITER_MASK, OWNER = Abi.SlotWord.OWNER_MASK;
    static final int ENTRY_PROBE = 8;
    static final int BREAKER_AFTER = 8;
    static final long BREAKER_MAX_NS = 100_000_000L;
    static final int SHARDS = 64;
    /** SCZ_JAVA_DEBUG=1: the background thread logs each connection attempt to stderr. */
    static final boolean DEBUG = System.getenv("SCZ_JAVA_DEBUG") != null;

    static final FallbackReason[] R = FallbackReason.values();

    static long r(FallbackReason f) {
        return -1L - f.ordinal();
    }

    static final long NOT_CONNECTED = r(FallbackReason.NOT_CONNECTED), CLOCK_FAULT = r(FallbackReason.CLOCK_FAULT);
    static final long STALE_HEARTBEAT = r(FallbackReason.STALE_HEARTBEAT), REBINDING = r(FallbackReason.REBINDING);
    static final long SCHEMA_MISMATCH = r(FallbackReason.SCHEMA_MISMATCH), OVERLOADED = r(FallbackReason.OVERLOADED);
    static final long TOO_LARGE = r(FallbackReason.TOO_LARGE), NO_SLOT = r(FallbackReason.NO_SLOT);
    static final long CODEC_ERROR = r(FallbackReason.CODEC_ERROR), RING_FULL = r(FallbackReason.RING_FULL);
    static final long DEADLINE = r(FallbackReason.DEADLINE), WORKER_FAILED = r(FallbackReason.WORKER_FAILED);
    static final long ABI_MISMATCH = r(FallbackReason.ABI_MISMATCH);

    // what the scope guard holds (Caller.phase)
    static final int P_NONE = 0, P_CLAIMED = 1, P_READY = 2, P_CELL = 3, P_QUEUED = 4, P_DONE = 5;

    // client states (sdk-contract §6)
    static final int DISCONNECTED = 0, CONNECTING = 1, BOUND = 2, REJECTED = 3, SCHEMA_REJECTED = 4;

    // ── process-wide ────────────────────────────────────────────────────────────────────────────
    /** The application's fixed spin ceiling, or -1 for adaptive (sdk-contract §7). */
    static volatile long spinFixed = -1;
    /** Callers parked in a futex downcall from a virtual thread, in the whole JVM (sdk-java §7). */
    static final AtomicInteger PINNED = new AtomicInteger();
    static final int CARRIERS = Integer.getInteger("jdk.virtualThreadScheduler.parallelism", Runtime.getRuntime().availableProcessors());
    static final int PIN_CAP = CARRIERS - 1;

    // ── metrics (sdk-contract §8) ───────────────────────────────────────────────────────────────
    /** What {@link #sampleStart} returns for a call it does not time. */
    static final long UNTIMED = Long.MIN_VALUE;
    /**
     * The sampling counts: each calling thread's calls, over every client, in the stripe of its
     * thread id, one 128-byte line per stripe so that threads never write a line another one
     * writes. A {@code ThreadLocal} would allocate on a thread's first call (the allocation gate
     * forbids it) and, with a virtual thread per request, on every call. Threads whose ids are
     * {@link #SAMPLE_STRIPES} apart share a count; a count's increments are plain, so two threads
     * sharing one may lose one now and then, which shifts only which calls are timed.
     */
    static final int SAMPLE_STRIPES = 1024, SAMPLE_PAD = 32;
    static final int[] SAMPLE = new int[(SAMPLE_STRIPES + 1) * SAMPLE_PAD];
    /** The sampled histograms, in shards chosen by the thread id, each shard 512 bytes. */
    static final int HIST_SHARDS = 16, HIST_STRIDE = 64;
    static final int H_MODEL = 0, H_FALLBACK = Histogram.LONGS, H_FALLBACK_FN = 2 * Histogram.LONGS;
    /** {@link Histogram#boundsSeconds()} in nanoseconds. */
    static final long[] BOUNDS_NS = {1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000, 1_000_000, 2_000_000, 5_000_000,
        10_000_000, 20_000_000, 50_000_000, 100_000_000};

    // ── per client ──────────────────────────────────────────────────────────────────────────────
    final String endpoint;
    final Wire wire;
    final Long want;
    final long reserveNs;
    final long connectTimeoutMs;
    /** Virtual threads take the polling wait (Options.virtualPolling); settable for tests. */
    volatile boolean forcePoll;
    final Caller[] callers;
    private final Arena entryMem;
    final AtomicLongArray unattached = new AtomicLongArray(SHARDS * FallbackReason.count());
    final AtomicLong sessions = new AtomicLong(), retired = new AtomicLong(), attempts = new AtomicLong();
    /** Options.sampleEvery (0: none timed), and its mask when a power of two (else -1). */
    final int sampleEvery, sampleMask;
    /** The builder's onFallback hook, or null. */
    final Consumer<FallbackReason> hook;
    final AtomicLongArray hists = new AtomicLongArray(HIST_SHARDS * HIST_STRIDE);

    volatile Snapshot current;
    volatile int state = DISCONNECTED;
    volatile boolean stop;
    volatile boolean rebind;
    /** The adaptive spin ceiling, written by the background thread only. */
    volatile long spinCeiling = Abi.Defaults.SPIN_NS;
    private long nextSnap = 1;
    private Thread bg;

    Engine(String endpoint, Wire wire, Options o) {
        this(endpoint, wire, o, null);
    }

    Engine(String endpoint, Wire wire, Options o, Consumer<FallbackReason> hook) {
        this.endpoint = endpoint;
        this.hook = hook;
        this.sampleEvery = Math.max(0, o.sampleEvery);
        this.sampleMask = Integer.bitCount(sampleEvery) == 1 ? sampleEvery - 1 : -1;
        this.wire = wire;
        this.want = wire.schema == null ? null : wire.schema.fingerprint();
        this.reserveNs = o.fallbackReserveNanos;
        this.connectTimeoutMs = o.connectTimeoutMillis;
        this.forcePoll = o.virtualPolling;
        if (o.spinCeilingNanos >= 0) spinFixed = o.spinCeilingNanos;
        int n = Math.max(1, o.callers);
        this.callers = new Caller[n];
        this.entryMem = Arena.ofShared();
        MemorySegment ts = entryMem.allocate(16L * n, 64);
        for (int i = 0; i < n; i++) callers[i] = new Caller(i, ts.asSlice(16L * i, 16));
        if (Native.LINUX) {
            if (!Engine.class.getModule().isNativeAccessEnabled()) {
                entryMem.close();
                throw new IllegalCallerException("SidecarZero needs native access: run the JVM with --enable-native-access=ALL-UNNAMED "
                        + "(or --enable-native-access=io.sidecarzero on the module path)");
            }
            bg = new Thread(this::background, "sidecarzero");
            bg.setDaemon(true);
            bg.start();
        }
    }

    // ── caller entries ──────────────────────────────────────────────────────────────────────────

    /** Returned by {@link #enter} when nothing is published, or no entry was free. */
    static final Caller NO_SNAPSHOT = new Caller(-1, null), NO_ENTRY = new Caller(-2, null);

    static long mix(long x) {
        x += 0x9e3779b97f4a7c15L;
        x = (x ^ (x >>> 30)) * 0xbf58476d1ce4e5b9L;
        x = (x ^ (x >>> 27)) * 0x94d049bb133111ebL;
        return x ^ (x >>> 31);
    }

    /**
     * Claim an entry for the current snapshot: the marker, then the re-check that the snapshot is
     * still current (sdk-contract §6, "Retiring an old arena"). Both sides are sequentially
     * consistent: either the call sees the replacement and backs out, or the background scan sees
     * the marker and waits.
     */
    Caller enter() {
        int n = callers.length;
        int start = (int) Long.remainderUnsigned(mix(Thread.currentThread().threadId()), n);
        for (int attempt = 0; attempt < 4; attempt++) {
            Snapshot s = current;
            if (s == null) return NO_SNAPSHOT;
            long marker = s.id << 32;
            Caller c = null;
            int probes = Math.min(ENTRY_PROBE, n);
            for (int k = 0; k < probes; k++) {
                Caller e = callers[(start + k) % n];
                if (e.claim(marker)) {
                    c = e;
                    break;
                }
            }
            if (c == null) return NO_ENTRY;
            if (current != s) {
                c.release();
                continue;
            }
            c.snap = s;
            return c;
        }
        return NO_SNAPSHOT;
    }

    void exit(Caller c) {
        c.snap = null;
        c.release();
    }

    /** A call that found no snapshot or no entry: counted in the shards, not in an entry. */
    long unattached(Caller sentinel) {
        long st = sentinel == NO_ENTRY ? NO_SLOT : (state == REJECTED ? ABI_MISMATCH : NOT_CONNECTED);
        int shard = (int) (mix(Thread.currentThread().threadId()) & (SHARDS - 1));
        unattached.getAndIncrement(shard * FallbackReason.count() + (int) (-st - 1));
        return st;
    }

    // ── sampling and the hook (sdk-contract §8), called by every front end ─────────────────────

    /**
     * At the start of a call: whether this thread times it, those whose number on this thread
     * (counted over every client) is a multiple of {@code sampleEvery}, its first call among them.
     * Returns the call's start, or {@link #UNTIMED}; an untimed call reads no clock here.
     */
    long sampleStart() {
        int every = sampleEvery;
        if (every == 0) return UNTIMED;
        int i = (((int) Thread.currentThread().threadId() & (SAMPLE_STRIPES - 1)) + 1) * SAMPLE_PAD;
        int n = SAMPLE[i];
        SAMPLE[i] = n + 1;
        int m = sampleMask;
        if (m >= 0 ? (n & m) != 0 : Integer.remainderUnsigned(n, every) != 0) return UNTIMED;
        long t = System.nanoTime();
        return t == UNTIMED ? t + 1 : t;
    }

    /** The bucket a duration lands in: the first whose bound is at least as large; +Inf is the last. */
    static int bucket(long ns) {
        int i = 0;
        while (i < BOUNDS_NS.length && ns > BOUNDS_NS[i]) i++;
        return i;
    }

    /**
     * Add one duration to the histogram at {@code base} in {@code a}. No allocation. Here rather
     * than in {@link Histogram}: the first C2 compile of a method resolves every string constant
     * of its class, on the thread that asked for it, and Histogram has some (its toString). That
     * cost 80 bytes, once, on whichever request thread made this method hot. Engine's constants
     * are resolved by the first compile of the call path, long before.
     */
    static void record(AtomicLongArray a, int base, long ns) {
        long d = Math.max(0, ns);
        a.getAndAdd(base + bucket(d), 1);
        a.getAndAdd(base + Histogram.BUCKETS, d);
    }

    private static int histShard() {
        return ((int) Thread.currentThread().threadId() & (HIST_SHARDS - 1)) * HIST_STRIDE;
    }

    /** A timed call the model answered: its end-to-end time. */
    void timedModel(long t0) {
        record(hists, histShard() + H_MODEL, System.nanoTime() - t0);
    }

    /**
     * Just before the fallback function, once per fallback, after the slot and the entry have been
     * released: the hook, which may throw (the fallback then never runs), then on a timed call the
     * fallback's start, else {@link #UNTIMED}.
     */
    long fallbackStart(long t0, FallbackReason r) {
        Consumer<FallbackReason> h = hook;
        if (h != null) h.accept(r);
        return t0 == UNTIMED ? UNTIMED : System.nanoTime();
    }

    /** After the fallback function of a timed call: its time, and the call's end to end. */
    void timedFallback(long t0, long t1) {
        long t2 = System.nanoTime();
        int s = histShard();
        record(hists, s + H_FALLBACK_FN, t2 - t1);
        record(hists, s + H_FALLBACK, t2 - t0);
    }

    // ── the call ────────────────────────────────────────────────────────────────────────────────

    /** The call algorithm from admission to resolution, then the breaker and the counters. */
    long run(Caller c, Object in, Object out, long budgetNs) {
        long st;
        try {
            st = call(c, in, out, budgetNs);
        } catch (IllegalStateException e) {
            st = NOT_CONNECTED; // the arena was closed under the call (sdk-java §3, §8): a fallback, never a crash
        } catch (RuntimeException e) {
            st = WORKER_FAILED; // a bad offset or length from shared memory
        }
        finish(c, st);
        return st;
    }

    private void finish(Caller c, long st) {
        Conn cn = c.snap.conn;
        if (c.brkConn != cn) {
            // A new session: misses against an arena that is gone say nothing about this one.
            c.misses = 0;
            c.brkUntil = 0;
            c.brkConn = cn;
        }
        if (st >= 0) {
            c.model++;
            if (c.modelGen != st) c.modelGen = st;
            c.misses = 0;
            c.brkUntil = 0;
            return;
        }
        int ri = (int) (-st - 1);
        c.fb[ri]++;
        if (st == OVERLOADED) return;
        if (st == DEADLINE || st == RING_FULL) {
            if (c.brkUntil != 0) {
                open(c, Math.min(c.brkInterval * 2, BREAKER_MAX_NS)); // the probe missed
            } else if (++c.misses >= BREAKER_AFTER) {
                c.misses = 0;
                open(c, Math.max(1, Math.min(c.budgetNs * 2, BREAKER_MAX_NS)));
            }
        } else {
            c.misses = 0;
            c.brkUntil = 0;
        }
    }

    /** Open the breaker, jittered between half and one and a half intervals. */
    private static void open(Caller c, long interval) {
        long now = Clock.now();
        long j = Long.remainderUnsigned(mix(now ^ c.index), Math.max(1, interval));
        c.brkInterval = interval;
        c.brkUntil = now + interval / 2 + j;
        if (c.brkUntil == 0) c.brkUntil = 1;
    }

    static long gateReason(long g) {
        return switch ((int) (g & 0xFF)) {
            case Abi.GateState.INIT -> r(FallbackReason.GATE_INIT);
            case Abi.GateState.PSI_SHEDDING -> r(FallbackReason.GATE_SHEDDING);
            case Abi.GateState.RELOADING -> r(FallbackReason.GATE_RELOADING);
            case Abi.GateState.DRAINING -> r(FallbackReason.GATE_DRAINING);
            default -> r(FallbackReason.GATE_DEAD);
        };
    }

    static long statusReason(int s) {
        return switch (s) {
            case Abi.Status.DEADLINE_EXCEEDED -> DEADLINE;
            case Abi.Status.BAD_REQUEST -> r(FallbackReason.BAD_REQUEST);
            case Abi.Status.MODEL_ERROR -> r(FallbackReason.MODEL_ERROR);
            case Abi.Status.RESOURCE_EXHAUSTED -> r(FallbackReason.RESOURCE_EXHAUSTED);
            case Abi.Status.STALE_REGISTRY -> REBINDING;
            default -> WORKER_FAILED;
        };
    }

    static boolean healthy(long g) {
        return (g & 0xFF) == Abi.GateState.HEALTHY;
    }

    static int state(long w) {
        return (int) (w & 0xFF);
    }

    static int epoch(long w) {
        return (int) (w >>> 32);
    }

    static long word(int epoch, int state) {
        return ((long) epoch << 32) | (state & 0xFFL);
    }

    /**
     * When the gate shows a generation the binding was not confirmed under, read the registry
     * epoch under the seqlock: unchanged confirms the binding, changed asks for a rebind.
     */
    boolean confirm(Snapshot snap, int gen) {
        Binding b = snap.binding;
        long e = Session.epoch(snap.conn.ctl, 64);
        if (e >= 0 && (int) e == b.epoch && b.mode != Binding.UNBOUND) {
            b.confirmedGen = gen;
            return true;
        }
        rebind = true;
        return false;
    }

    private long call(Caller c, Object in, Object out, long budgetNs) {
        Snapshot snap = c.snap;
        Conn conn = snap.conn;
        MemorySegment ctl = conn.ctl, data = conn.data;
        Directory dir = conn.dir;
        if (Clock.fault()) return CLOCK_FAULT;
        long t0 = Clock.now();
        c.lastCallNs = t0;
        long budget = Math.max(0, budgetNs);
        if (budget > dir.maxDeadlineNs) {
            budget = dir.maxDeadlineNs;
            c.clamped++;
        }
        c.budgetNs = budget;
        long waitUntil = t0 + budget - reserveNs;

        // ── Admission: one clock read and two loads from one cache line.
        long g = ControlHeader.gate(ctl, 0);
        if (!healthy(g)) return gateReason(g);
        long hb = ControlHeader.supervisorHbMonoNs(ctl, 0);
        if (hb + dir.hbStaleNs < t0 || hb > t0 + Clock.ADMIT_TOL_NS) {
            // A heartbeat the call's clock cannot explain: check the clock (this path only).
            if (Clock.suspect(c.ts)) return CLOCK_FAULT;
            if (hb + dir.hbStaleNs < t0) return STALE_HEARTBEAT;
        }
        Binding b = snap.binding;
        int gen = (int) (g >>> 32);
        if (b.confirmedGen != gen && !confirm(snap, gen)) return REBINDING;
        Registry reg = null;
        switch (b.mode) {
            case Binding.RAW -> {}
            case Binding.FRAMES -> reg = b.reg;
            case Binding.MISMATCH -> {
                return SCHEMA_MISMATCH;
            }
            default -> {
                rebind = true;
                return REBINDING;
            }
        }
        if (c.brkUntil != 0 && t0 < c.brkUntil && c.brkConn == conn) return OVERLOADED;

        // ── Claim a slot in the smallest class that holds the request: the cached one, then a
        //    bounded probe.
        int n;
        try {
            n = wire.size(c, in);
        } catch (Throwable t) {
            return CODEC_ERROR;
        }
        if (n < 0) return CODEC_ERROR;
        int cls = -1;
        if (reg == null) {
            for (int i = 0; i < dir.classes; i++) {
                if (dir.reqCap[i] >= n) {
                    cls = i;
                    break;
                }
            }
        } else {
            cls = reg.classFor(n);
        }
        if (cls < 0) return TOO_LARGE;
        int count = dir.slotCount[cls];
        int start = c.cached[cls];
        int idx = -1, ep = 0;
        long rec = 0;
        int probes = Math.min((int) Abi.SLOT_PROBE, count);
        for (int k = 0; k < probes; k++) {
            int i = Integer.remainderUnsigned(start + k, count);
            long rr = dir.rec(cls, i);
            long w = SlotRecord.word(data, rr);
            if (state(w) == FREE) {
                int e = epoch(w) + 1;
                if (SlotRecord.casWord(data, rr, w, word(e, CLAIMED))) {
                    idx = i;
                    ep = e;
                    rec = rr;
                    break;
                }
            }
        }
        if (idx < 0) return NO_SLOT;
        c.cached[cls] = idx;
        // From here on, every way out releases the slot or hands it to the worker; the guard
        // covers an exception anywhere in between.
        c.gData = data;
        c.gRec = rec;
        c.gEpoch = ep;
        c.gAddrBase = conn.dataAddr;
        c.phase = P_CLAIMED;
        try {
            return claimed(c, snap, data, ctl, dir, reg, in, out, cls, idx, rec, ep, n, waitUntil);
        } catch (Throwable t) {
            guard(c);
            forget(c, cls, idx, count);
            if (t instanceof Error e) throw e; // an unwind (a stack overflow, the test hook): after the guard
            return t instanceof IllegalStateException ? NOT_CONNECTED : WORKER_FAILED;
        }
    }

    private static void forget(Caller c, int cls, int idx, int count) {
        c.cached[cls] = (idx + 1) % count;
    }

    private long claimed(Caller c, Snapshot snap, MemorySegment data, MemorySegment ctl, Directory dir, Registry reg, Object in, Object out,
                         int cls, int idx, long rec, int ep, int n, long waitUntil) {
        int count = dir.slotCount[cls];
        long reqOff = dir.req(cls, idx);
        int epochR = snap.binding.epoch;

        // ── Encode. A codec error or exception releases the slot.
        boolean ok = true;
        int reqLen;
        if (reg == null) {
            c.sw.reset(data, reqOff, dir.reqCap[cls]);
            try {
                wire.encode(c, in, c.sw);
            } catch (Throwable t) {
                ok = false;
            }
            reqLen = c.sw.length();
        } else {
            Template t = reg.req[cls];
            prepare(data, reqOff, rec, t, t.stamp(epochR), n);
            c.fw.reset(data, reqOff, t, n);
            try {
                wire.encodeFrame(c, in, c.fw);
            } catch (Throwable x) {
                ok = false;
            }
            reqLen = t.frameCap;
        }
        if (!ok) {
            SlotRecord.setWord(data, rec, word(ep, FREE));
            c.phase = P_NONE;
            return CODEC_ERROR;
        }
        long rid = ++c.nextRequest;
        long tEnq = Clock.now();
        c.tEnqueue = tEnq;
        SlotRecord.setRequestId(data, rec, rid);
        SlotRecord.setReqLen(data, rec, reqLen);
        SlotRecord.setTEnqueueNs(data, rec, tEnq);
        SlotRecord.setWord(data, rec, word(ep, READY)); // the request region is the worker's from here
        c.phase = P_READY;

        // ── Enqueue: round robin per caller, skipping a full ring.
        int nr = dir.rings;
        int cur = c.ringCursor;
        c.ringCursor = cur + 1;
        boolean sent = false;
        for (int k = 0; k < nr && !sent; k++) {
            int ri = Integer.remainderUnsigned(cur + k, nr);
            int e = enqueue(c, data, dir.ringOff[ri], dir.ringCap[ri], rid, idx, waitUntil, ep, cls, tEnq);
            if (e == ENQ_OK) sent = true;
            else if (e == ENQ_EXHAUSTED) ringFault(data, dir);
        }
        if (!sent) {
            // No descriptor exists, but the sweep may have failed the slot meanwhile.
            int x = cancelIfReady(data, rec, ep);
            if (x == C_NOT_READY) {
                SlotRecord.setWord(data, rec, word(ep, FREE));
            } else if (x == C_GAVE_UP) {
                ringFault(data, dir);
                forget(c, cls, idx, count);
            }
            c.phase = P_NONE;
            return RING_FULL;
        }

        // ── Wait: gate re-check, spin, then park.
        int w = await(c, ctl, data, rec, ep, waitUntil, spinBudget(c, dir));
        switch (w) {
            case W_DONE:
                return resolve(c, snap, data, dir, reg, out, cls, idx, rec, ep, n, epochR);
            case W_FAILED:
                SlotRecord.setWord(data, rec, word(ep, FREE));
                c.phase = P_NONE;
                return WORKER_FAILED;
            case W_CANCELLED:
                c.phase = P_NONE; // the cancel freed the slot
                return gateReason(c.cancelGate);
            case W_GAVE_UP:
                ringFault(data, dir);
                forget(c, cls, idx, count);
                c.phase = P_NONE;
                return WORKER_FAILED;
            default:
                break;
        }
        // ── Timeout: abandon.
        if (DEBUG) {
            long ro = dir.ringOff[0];
            System.err.println("sidecarzero: timeout at " + System.nanoTime() / 1000 + " us: word 0x" + Long.toHexString(SlotRecord.word(data, rec)) + ", "
                    + (Clock.now() - tEnq) / 1000 + " us after enqueue, head " + RingHeader.head(data, ro) + " tail " + RingHeader.tail(data, ro)
                    + " wait " + RingHeader.consumerWait(data, ro) + " spin " + spinBudget(c, dir) + " ewma " + c.ewmaNs + " thread " + Thread.currentThread().getName());
        }
        switch (abandon(data, rec, ep)) {
            case A_FREED:
                c.phase = P_NONE;
                return DEADLINE;
            case A_ABANDONED:
                forget(c, cls, idx, count); // the worker, or the sweep, frees it
                c.phase = P_NONE;
                return DEADLINE;
            case A_DONE:
                lateCheck(c, data, rec); // the worker won the race
                return resolve(c, snap, data, dir, reg, out, cls, idx, rec, ep, n, epochR);
            case A_FAILED:
                SlotRecord.setWord(data, rec, word(ep, FREE));
                c.phase = P_NONE;
                return WORKER_FAILED;
            default:
                ringFault(data, dir);
                forget(c, cls, idx, count);
                c.phase = P_NONE;
                return WORKER_FAILED;
        }
    }

    /**
     * Make the request region a frame of {@code t} carrying {@code rows} rows (arrow-data-plane
     * §7.1): copy the prefix in if the region's stamp is not {@code want}, then rewrite the length
     * fields if the row count differs from the one the frame last held.
     */
    private static void prepare(MemorySegment data, long region, long rec, Template t, int want, int rows) {
        if (SlotRecord.reqStamp(data, rec) != want) {
            MemorySegment.copy(t.prefixSeg, 0, data, region, t.prefix.length);
            SlotRecord.setReqStamp(data, rec, want);
        }
        long r = rows & 0xFFFF_FFFFL;
        if (data.get(Le.LONG, region + t.rowsFieldOff) != r) {
            for (int k = 0; k < t.patchOff.length; k++) data.set(Le.LONG, region + t.patchOff[k], t.patchValue(k, r));
        }
    }

    private static void ringFault(MemorySegment data, Directory dir) {
        StatusPage.addClientRingFaults(data, dir.statusOff, 1);
    }

    /** A wake-up more than 1 ms after the worker completed: how a lost wake-up shows (signaling §7). */
    private static void lateCheck(Caller c, MemorySegment data, long rec) {
        long done = SlotRecord.tDoneNs(data, rec);
        long lim = done + 1_000_000L;
        if (Long.compareUnsigned(lim, done) < 0) lim = -1L; // saturating, as the value is the worker's
        if (done != 0 && Long.compareUnsigned(Clock.now(), lim) > 0) c.lateWakes++;
    }

    /** A DONE slot: each result field read once, checked, decoded, released. */
    private long resolve(Caller c, Snapshot snap, MemorySegment data, Directory dir, Registry reg, Object out, int cls, int idx,
                         long rec, int ep, int rows, int epochR) {
        c.phase = P_DONE;
        int status = SlotRecord.status(data, rec);
        int gen = SlotRecord.modelGen(data, rec);
        int respLen = SlotRecord.respLen(data, rec);
        if (status != Abi.Status.OK) {
            if (DEBUG) {
                long now = Clock.now();
                long ro = dir.ringOff[0];
                System.err.println("sidecarzero: status " + status + " at " + System.nanoTime() / 1000 + " us: " + (now - c.tEnqueue) / 1000 + " us after enqueue, dequeued "
                        + (SlotRecord.tDequeueNs(data, rec) - c.tEnqueue) / 1000 + " us, done " + (SlotRecord.tDoneNs(data, rec) - c.tEnqueue) / 1000
                        + " us; head " + RingHeader.head(data, ro) + " tail " + RingHeader.tail(data, ro) + " wait " + RingHeader.consumerWait(data, ro)
                        + " " + Thread.currentThread().getName());
            }
            SlotRecord.setWord(data, rec, word(ep, FREE));
            c.phase = P_NONE;
            if (status == Abi.Status.STALE_REGISTRY) rebind = true;
            return statusReason(status);
        }
        long respOff = dir.req(cls, idx) + dir.reqCap[cls];
        boolean ok = true;
        if (reg == null) {
            if ((respLen & 0xFFFF_FFFFL) > dir.respCap[cls]) return refuse(c, data, dir, rec, ep);
            c.sr.reset(data, respOff, respLen);
            try {
                wire.decode(c, out, c.sr);
            } catch (Throwable t) {
                ok = false;
            }
        } else {
            // arrow-data-plane §7.3: this template's, under this registry epoch, its frame length,
            // and as many rows as were asked.
            Template t = reg.resp[cls];
            if (t == null) return refuse(c, data, dir, rec, ep);
            int stamp = SlotRecord.respStamp(data, rec);
            if (stamp != t.stamp(epochR) || respLen != t.frameCap || data.get(Le.LONG, respOff + t.rowsFieldOff) != (rows & 0xFFFF_FFFFL)) {
                return refuse(c, data, dir, rec, ep);
            }
            c.fr.reset(data, respOff, t, rows);
            try {
                wire.decodeFrame(c, out, c.fr);
            } catch (Throwable x) {
                ok = false;
            }
        }
        SlotRecord.setWord(data, rec, word(ep, FREE));
        c.phase = P_NONE;
        if (!ok) return CODEC_ERROR;
        long sample = Clock.now() - c.tEnqueue;
        c.ewmaNs = c.ewmaNs == 0 ? sample : c.ewmaNs + (sample - c.ewmaNs) / 8;
        return gen & 0xFFFF_FFFFL;
    }

    /** A response that fails its checks: counted for the supervisor, and the call falls back. */
    private static long refuse(Caller c, MemorySegment data, Directory dir, long rec, int ep) {
        StatusPage.addClientRejects(data, dir.statusOff, 1);
        SlotRecord.setWord(data, rec, word(ep, FREE));
        c.phase = P_NONE;
        return WORKER_FAILED;
    }

    // ── ring ────────────────────────────────────────────────────────────────────────────────────

    static final int ENQ_OK = 0, ENQ_FULL = 1, ENQ_EXHAUSTED = 2;

    /** Producer side (shm-abi §4.3). Publishes the descriptor and wakes the consumer if parked. */
    static int enqueue(Caller c, MemorySegment data, long ringOff, long cap, long rid, int idx, long deadline, int ep, int cls, long tEnq) {
        for (int a = 0; a < Abi.ENQUEUE_ATTEMPTS; a++) {
            long pos = RingHeader.tail(data, ringOff);
            long cell = ringOff + RingHeader.CELLS_OFF + (pos & (cap - 1)) * Abi.CELL_LEN;
            long seq = Cell.seq(data, cell);
            if (seq == pos) {
                if (RingHeader.casTail(data, ringOff, pos, pos + 1)) {
                    c.gCell = cell;
                    c.gPos = pos;
                    c.gRingOff = ringOff;
                    c.phase = P_CELL;
                    if (Hooks.CRASHPOINTS && c.index >= 0) Hooks.crashPoint("ring.enqueue.claimed");
                    if (Hooks.unwind && c.index >= 0 && Hooks.takeUnwind()) throw Hooks.UNWOUND;
                    writeCell(data, cell, rid, idx, deadline, ep, cls, tEnq, 0);
                    if (Hooks.CRASHPOINTS && c.index >= 0) Hooks.crashPoint("ring.enqueue.written");
                    Cell.setSeqVolatile(data, cell, pos + 1); // publish
                    wakeIfParked(c.gAddrBase, data, ringOff);
                    c.phase = P_QUEUED;
                    return ENQ_OK;
                }
            } else if (Long.compareUnsigned(seq, pos) < 0) {
                return ENQ_FULL;
            }
            Thread.onSpinWait();
        }
        return ENQ_EXHAUSTED;
    }

    static void writeCell(MemorySegment data, long cell, long rid, int idx, long deadline, int ep, int cls, long tEnq, int flags) {
        Cell.setRequestId(data, cell, rid);
        Cell.zeroGaps(data, cell);
        Cell.setSlotIndex(data, cell, idx);
        Cell.setDeadlineMonoNs(data, cell, deadline);
        Cell.setSlotEpoch(data, cell, ep);
        Cell.setSlabClass(data, cell, cls);
        Cell.setEnqueueMonoNs(data, cell, tEnq);
        Cell.setFlags(data, cell, flags);
        Cell.setRoute(data, cell, 0);
    }

    /** The waker rule (signaling §3.1), after a sequentially consistent publish. */
    static void wakeIfParked(long base, MemorySegment data, long ringOff) {
        int w = RingHeader.consumerWaitVolatile(data, ringOff);
        if (w == Abi.ConsumerWait.AWAKE) return;
        if (w == Abi.ConsumerWait.PARKED) RingHeader.casConsumerWait(data, ringOff, Abi.ConsumerWait.PARKED, Abi.ConsumerWait.WAKING);
        Native.futexWake(base + ringOff + RingHeader.CONSUMER_WAIT, 1);
    }

    // ── slot transitions (shm-abi §6.1): every exit from READY or PROCESSING is a bounded CAS loop

    static final int C_CANCELLED = 0, C_NOT_READY = 1, C_GAVE_UP = 2;

    static int cancelIfReady(MemorySegment data, long rec, int ep) {
        for (int a = 0; a < Abi.CAS_ATTEMPTS; a++) {
            long w = SlotRecord.word(data, rec);
            if (state(w) != READY || epoch(w) != ep) return C_NOT_READY;
            if (SlotRecord.casWord(data, rec, w, word(ep, FREE))) return C_CANCELLED;
        }
        return C_GAVE_UP;
    }

    static final int A_FREED = 0, A_ABANDONED = 1, A_DONE = 2, A_FAILED = 3, A_GAVE_UP = 4;

    /** What a caller does when its deadline has passed. An abandon keeps OWNER. */
    static int abandon(MemorySegment data, long rec, int ep) {
        for (int a = 0; a < Abi.CAS_ATTEMPTS; a++) {
            long w = SlotRecord.word(data, rec);
            if (epoch(w) != ep) return A_GAVE_UP;
            switch (state(w)) {
                case READY -> {
                    if (SlotRecord.casWord(data, rec, w, word(ep, FREE))) return A_FREED;
                }
                case PROCESSING -> {
                    if (SlotRecord.casWord(data, rec, w, word(ep, ABANDONED) | (w & OWNER))) return A_ABANDONED;
                }
                case DONE -> {
                    return A_DONE;
                }
                case FAILED -> {
                    return A_FAILED;
                }
                default -> {
                    return A_GAVE_UP;
                }
            }
        }
        return A_GAVE_UP;
    }

    /**
     * The scope guard: undo whatever the call held when it unwound. A claimed ring cell is
     * published as void, with the waker rule, and then the slot is cancelled (shm-abi §4.3).
     */
    static void guard(Caller c) {
        MemorySegment data = c.gData;
        long rec = c.gRec;
        int ep = c.gEpoch;
        int p = c.phase;
        c.phase = P_NONE;
        try {
            switch (p) {
                case P_CLAIMED, P_DONE -> SlotRecord.setWord(data, rec, word(ep, FREE));
                case P_CELL -> {
                    writeCell(data, c.gCell, 0, 0, 0, 0, 0, 0, Abi.CellFlag.VOID);
                    Cell.setSeqVolatile(data, c.gCell, c.gPos + 1);
                    wakeIfParked(c.gAddrBase, data, c.gRingOff);
                    if (cancelIfReady(data, rec, ep) == C_NOT_READY) SlotRecord.setWord(data, rec, word(ep, FREE));
                }
                case P_READY -> {
                    if (cancelIfReady(data, rec, ep) == C_NOT_READY) SlotRecord.setWord(data, rec, word(ep, FREE));
                }
                case P_QUEUED -> {
                    int a = abandon(data, rec, ep);
                    if (a == A_DONE || a == A_FAILED) SlotRecord.setWord(data, rec, word(ep, FREE));
                }
                default -> {}
            }
        } catch (RuntimeException e) {
            // The segment is gone, or an offset was bad: nothing more can be undone here.
        }
    }

    // ── waiting (signaling §3.2, sdk-java §7) ───────────────────────────────────────────────────

    static final int W_DONE = 0, W_FAILED = 1, W_CANCELLED = 2, W_TIMEOUT = 3, W_GAVE_UP = 4;

    private long spinBudget(Caller c, Directory dir) {
        long fixed = spinFixed;
        long ceiling = fixed >= 0 ? fixed : Math.min(spinCeiling, dir.spinNs);
        long e = c.ewmaNs;
        if (e == 0 || ceiling == 0) return ceiling;
        return e > 4 * ceiling ? 0 : Math.min(ceiling, 2 * e);
    }

    private int await(Caller c, MemorySegment ctl, MemorySegment data, long rec, int ep, long waitUntil, long spinNs) {
        // The gate re-check after publishing: pairs with the supervisor's store-gate-then-sweep.
        long g = ControlHeader.gateVolatile(ctl, 0);
        if (!healthy(g) && cancelIfReady(data, rec, ep) == C_CANCELLED) {
            c.cancelGate = g;
            return W_CANCELLED;
        }
        long spinEnd = Math.min(Clock.now() + spinNs, waitUntil);
        while (Clock.now() < spinEnd) {
            long w = SlotRecord.word(data, rec);
            if (epoch(w) != ep) return W_GAVE_UP;
            int s = state(w);
            if (s == DONE) return W_DONE;
            if (s == FAILED) return W_FAILED;
            if (s == READY) {
                g = ControlHeader.gate(ctl, 0);
                if (!healthy(g) && cancelIfReady(data, rec, ep) == C_CANCELLED) {
                    c.cancelGate = g;
                    return W_CANCELLED;
                }
            }
            Thread.onSpinWait();
        }
        boolean virtual = Thread.currentThread().isVirtual();
        if (virtual && (forcePoll || !pin())) return poll(c, data, rec, ep, waitUntil);
        try {
            return park(c, data, rec, ep, waitUntil);
        } finally {
            if (virtual) PINNED.decrementAndGet();
        }
    }

    /** Take one of the carriers − 1 futex permits for a virtual thread. */
    private static boolean pin() {
        for (int a = 0; a < 64; a++) {
            int p = PINNED.get();
            if (p >= PIN_CAP) return false;
            if (PINNED.compareAndSet(p, p + 1)) return true;
        }
        return false;
    }

    /** Park in the futex: announce with the WAITER bit, then sleep on the word's low half. */
    private static int park(Caller c, MemorySegment data, long rec, int ep, long waitUntil) {
        int attempts = 0;
        boolean parked = false;
        long addr = c.gAddrBase + rec + SlotRecord.WORD;
        for (;;) {
            long w = SlotRecord.word(data, rec);
            if (epoch(w) != ep) return W_GAVE_UP;
            int s = state(w);
            if (s == DONE) {
                // Found DONE only once the deadline had passed: a late wake-up if the worker
                // completed more than 1 ms ago (signaling §7).
                if (parked && Clock.now() >= waitUntil) lateCheck(c, data, rec);
                return W_DONE;
            }
            if (s == FAILED) return W_FAILED;
            if (Clock.now() >= waitUntil) return W_TIMEOUT;
            if ((w & WAITER) == 0) {
                if (!SlotRecord.casWord(data, rec, w, w | WAITER)) {
                    if (++attempts >= Abi.CAS_ATTEMPTS) return W_GAVE_UP;
                    continue;
                }
                w |= WAITER;
            }
            parked = true;
            Native.futexWait(addr, (int) w, waitUntil, c.ts);
        }
    }

    /**
     * The polling wait, for a virtual thread beyond the pin cap: park the virtual thread for a
     * short, growing interval and re-read the word. It gives up its carrier; it never yields in a
     * loop (spike S3: a yielding thread starved every other virtual thread).
     */
    private static int poll(Caller c, MemorySegment data, long rec, int ep, long waitUntil) {
        c.polls++;
        long interval = 2_000;
        boolean waited = false;
        for (;;) {
            long w = SlotRecord.word(data, rec);
            if (epoch(w) != ep) return W_GAVE_UP;
            int s = state(w);
            long now = Clock.now();
            if (s == DONE) {
                if (waited && now >= waitUntil) lateCheck(c, data, rec);
                return W_DONE;
            }
            if (s == FAILED) return W_FAILED;
            if (now >= waitUntil) return W_TIMEOUT;
            LockSupport.parkNanos(Math.min(interval, waitUntil - now));
            waited = true;
            interval = Math.min(interval * 2, 200_000);
        }
    }

    // ── await_bound, stats, close ───────────────────────────────────────────────────────────────

    /** The gate is open and the binding current and usable. */
    boolean gateOpen() {
        Caller c = enter();
        if (c.index < 0) return false;
        try {
            Snapshot s = c.snap;
            long g = ControlHeader.gate(s.conn.ctl, 0);
            int gen = (int) (g >>> 32);
            // A gate frozen open in a session whose supervisor has just gone is not bound: the
            // background thread may not have seen the end of the stream yet (not on the call path).
            return healthy(g) && (s.binding.confirmedGen == gen || confirm(s, gen)) && (s.binding.mode == Binding.RAW || s.binding.mode == Binding.FRAMES)
                    && !Session.peerClosed(s.conn.sock);
        } catch (RuntimeException e) {
            return false;
        } finally {
            exit(c);
        }
    }

    boolean awaitBound(long timeoutNanos) {
        long end = System.nanoTime() + timeoutNanos;
        for (;;) {
            if (gateOpen()) return true;
            if (System.nanoTime() - end >= 0) return false;
            LockSupport.parkNanos(1_000_000);
        }
    }

    Stats stats() {
        long[] fb = new long[FallbackReason.count()];
        long model = 0, late = 0, polls = 0, clamped = 0, gen = 0;
        int open = 0;
        long now = Clock.now();
        for (Caller c : callers) {
            model += c.model;
            late += c.lateWakes;
            polls += c.polls;
            clamped += c.clamped;
            gen = Math.max(gen, c.modelGen);
            if (c.brkUntil != 0 && now < c.brkUntil) open++;
            for (int i = 0; i < fb.length; i++) fb[i] += c.fb[i];
        }
        for (int s = 0; s < SHARDS; s++) {
            for (int i = 0; i < fb.length; i++) fb[i] += unattached.get(s * fb.length + i);
        }
        return new Stats(model, fb, sessions.get(), retired.get(), late, polls, clamped, open, PINNED.get(), state, attempts.get(), gen,
                Histogram.sum(hists, HIST_SHARDS, HIST_STRIDE, H_MODEL), Histogram.sum(hists, HIST_SHARDS, HIST_STRIDE, H_FALLBACK),
                Histogram.sum(hists, HIST_SHARDS, HIST_STRIDE, H_FALLBACK_FN));
    }

    /**
     * Every metric of sdk-contract §8, in one pass, in the order every SDK uses: the calls by
     * source (every reason, zeros included), the two latency histograms, the fallback histogram,
     * then the unlabelled counters and gauges.
     */
    void export(MetricsSink sink) {
        Stats s = stats();
        sink.counter("scz_client_calls_total", new String[] {"source", "model"}, s.model());
        for (FallbackReason r : R) sink.counter("scz_client_calls_total", new String[] {"source", "fallback", "reason", r.label()}, s.fallbacks(r));
        histogram(sink, "scz_client_latency_seconds", new String[] {"source", "model"}, s.latencyModel());
        histogram(sink, "scz_client_latency_seconds", new String[] {"source", "fallback"}, s.latencyFallback());
        histogram(sink, "scz_client_fallback_seconds", new String[0], s.fallbackTime());
        sink.counter("scz_client_budget_clamped_total", new String[0], s.clamped());
        sink.gauge("scz_client_breaker_open", new String[0], s.breakerOpen());
        sink.counter("scz_client_late_wakes_total", new String[0], s.lateWakes());
        sink.counter("scz_client_reconnects_total", new String[0], Math.max(0, s.sessions() - 1));
        sink.gauge("scz_client_state", new String[0], s.state());
        sink.gauge("scz_client_model_gen", new String[0], s.modelGen());
    }

    private static void histogram(MetricsSink sink, String name, String[] labels, Histogram h) {
        sink.histogram(name, labels, Histogram.boundsSeconds(), h.cumulative(), h.sumSeconds(), h.count());
    }

    String prometheusText() {
        PrometheusText t = new PrometheusText();
        export(t);
        return t.finish();
    }

    void close() {
        Thread t = bg;
        if (t != null) {
            stop = true;
            LockSupport.unpark(t);
            try {
                t.join();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            bg = null;
        }
    }

    // ── the background thread (sdk-contract §6) ─────────────────────────────────────────────────

    private void nap(long ns) {
        long end = System.nanoTime() + ns;
        while (!stop) {
            long left = end - System.nanoTime();
            if (left <= 0) return;
            LockSupport.parkNanos(left);
        }
    }

    private static long jitter(long ns) {
        long half = ns / 2;
        return half + Long.remainderUnsigned(mix(System.nanoTime()), Math.max(1, half));
    }

    private void background() {
        long tStart = System.nanoTime();
        Native.link();
        Clock.now(); // calibrates, here rather than on a request thread
        if (DEBUG) System.err.println("sidecarzero: link " + (System.nanoTime() - tStart) / 1000 + " us");
        Session s = new Session();
        long backoff = 10_000_000L;
        long busySince = -1;
        boolean warnedAbi = false;
        try {
            while (!stop) {
                state = CONNECTING;
                Conn conn = s.connect(endpoint, connectTimeoutMs);
                if (conn == null) attempts.incrementAndGet();
                if (DEBUG) System.err.println("sidecarzero: connect " + (conn != null ? "ok" : s.why) + " at " + System.nanoTime() / 1_000_000 + " ms");
                if (conn != null) {
                    busySince = -1;
                    backoff = 10_000_000L;
                    warnedAbi = false;
                    conn.firstSnap = nextSnap;
                    conn.lastSnap = nextSnap;
                    try {
                        Binding b = Session.bind(conn, want);
                        warmUp(); // the first session only: before the first call can run
                        Snapshot snap = new Snapshot(nextSnap++, conn, b);
                        if (spinFixed < 0) spinCeiling = conn.dir.spinNs;
                        current = snap;
                        state = b.mode == Binding.MISMATCH ? SCHEMA_REJECTED : BOUND;
                        if (DEBUG) System.err.println("sidecarzero: published, mode " + b.mode + " at " + System.nanoTime() / 1_000_000 + " ms");
                        sessions.incrementAndGet();
                        attempts.incrementAndGet();
                        hold(s, conn);
                    } catch (RuntimeException e) {
                        // A bug, or a segment that went away: leave this session and reconnect.
                        System.err.println("sidecarzero: the session ended on " + e);
                    }
                    // Retirement starts now, before any reconnect (sdk-contract §6).
                    state = DISCONNECTED;
                    current = null;
                    retire(conn);
                    continue;
                }
                warmUp(); // after the first attempt, so that it does not delay that one
                switch (s.code) {
                    case Session.BUSY -> {
                        state = DISCONNECTED;
                        if (busySince < 0) busySince = System.nanoTime();
                        if (System.nanoTime() - busySince < 1_000_000_000L) {
                            nap(20_000_000L);
                        } else {
                            nap(jitter(backoff));
                            backoff = Math.min(backoff * 2, 1_000_000_000L);
                        }
                    }
                    case Session.ABI -> {
                        state = REJECTED;
                        if (!warnedAbi) {
                            System.err.println("sidecarzero: the sidecar at " + endpoint + " does not speak this SDK's ABI ("
                                    + Abi.ABI_MAJOR + "." + Abi.ABI_MINOR + "); serving fallbacks, retrying every 30 s");
                            warnedAbi = true;
                        }
                        nap(30_000_000_000L);
                    }
                    default -> {
                        state = DISCONNECTED;
                        nap(jitter(backoff));
                        backoff = Math.min(backoff * 2, 1_000_000_000L);
                    }
                }
            }
        } finally {
            state = DISCONNECTED;
            s.close();
        }
    }

    /**
     * Keep the session until its socket ends or the client stops: rebind when asked, re-check a
     * rejected schema once a second, verify the cheap clock, and adapt the spin ceiling.
     */
    private void hold(Session s, Conn conn) {
        long lastSchema = System.nanoTime(), lastClock = System.nanoTime(), lastSpin = System.nanoTime();
        long throttled = -1;
        int throttleShift = 0;
        while (!stop) {
            Snapshot cur = current;
            if (rebind || (cur.binding.mode == Binding.MISMATCH && System.nanoTime() - lastSchema > 1_000_000_000L
                    && Session.epoch(conn.ctl, 1000) != (cur.binding.epoch & 0xFFFF_FFFFL))) {
                rebind = false;
                lastSchema = System.nanoTime();
                Binding b = Session.bind(conn, want);
                Snapshot snap = new Snapshot(nextSnap++, conn, b);
                conn.lastSnap = snap.id;
                current = snap;
                state = b.mode == Binding.MISMATCH ? SCHEMA_REJECTED : BOUND;
            }
            if (cur.binding.mode == Binding.MISMATCH && System.nanoTime() - lastSchema > 1_000_000_000L) lastSchema = System.nanoTime();
            if (System.nanoTime() - lastClock > 250_000_000L || (Clock.fault() && System.nanoTime() - lastClock > 5_000_000L)) {
                lastClock = System.nanoTime();
                Clock.verify();
            }
            if (System.nanoTime() - lastSpin > 1_000_000_000L) {
                lastSpin = System.nanoTime();
                long nt = readThrottled();
                if (nt >= 0 && throttled >= 0 && nt > throttled) throttleShift = Math.min(throttleShift + 1, 16);
                else if (throttleShift > 0) throttleShift--;
                throttled = nt;
                adaptSpin(conn.dir.spinNs, throttleShift);
            }
            if (!s.alive(conn.sock, 5)) return;
        }
    }

    private static boolean warmed;

    /**
     * Run the request path's ring, slot, and wait code a few hundred times over process-local
     * memory, once per JVM, on the background thread before the first connection: the first real
     * calls otherwise run in the interpreter, slowly enough that a caller can take tens of
     * microseconds between publishing and parking. Nothing shared is touched.
     */
    private void warmUp() {
        synchronized (Engine.class) {
            if (warmed) return;
            warmed = true;
        }
        try (Arena a = Arena.ofConfined()) {
            MemorySegment ctl = a.allocate(Abi.CTL_HEADER_LEN, 4096), data = a.allocate(16384, 4096);
            ControlHeader.setGate(ctl, 0, (1L << 32) | Abi.GateState.HEALTHY);
            long ringOff = 0, cap = 4, rec = 8192;
            Caller c = new Caller(-3, a.allocate(16, 8));
            c.gAddrBase = data.address();
            c.gData = data;
            long t0 = System.nanoTime();
            for (int i = 0; i < 400; i++) {
                for (long k = 0; k < cap; k++) Cell.setSeq(data, ringOff + RingHeader.CELLS_OFF + k * Abi.CELL_LEN, k);
                RingHeader.setTail(data, ringOff, 0);
                int ep = i + 1;
                SlotRecord.setWord(data, rec, word(ep, READY));
                enqueue(c, data, ringOff, cap, i, 0, 0, ep, 0, 0);
                cancelIfReady(data, rec, ep);
                SlotRecord.setWord(data, rec, word(ep, PROCESSING));
                abandon(data, rec, ep);
                SlotRecord.setWord(data, rec, word(ep, DONE));
                await(c, ctl, data, rec, ep, Clock.now() + 1_000_000, (i & 1) == 0 ? 0 : 1_000);
                lateCheck(c, data, rec);
                if (i % 10 == 0) { // through the futex: a wait that times out at once
                    SlotRecord.setWord(data, rec, word(ep, PROCESSING));
                    await(c, ctl, data, rec, ep, Clock.now() + 2_000, 0);
                }
            }
            if (DEBUG) System.err.println("sidecarzero: warm-up " + (System.nanoTime() - t0) / 1000 + " us");
        } catch (Throwable e) {
            // a warm-up only; it must never end the background thread
        }
    }

    /** The spin ceiling of signaling §4: scaled down when callers outnumber CPUs, halved per throttling step. */
    private void adaptSpin(long spinNs, int throttleShift) {
        long now = Clock.now();
        int active = 0;
        for (Caller c : callers) {
            if (c.lastCallNs != 0 && now - c.lastCallNs < 1_000_000_000L) active++;
        }
        int cpus = Runtime.getRuntime().availableProcessors();
        long ceiling = active > cpus ? spinNs * cpus / active : spinNs;
        spinCeiling = ceiling >> throttleShift;
    }

    private static long readThrottled() {
        try {
            for (String l : Files.readAllLines(Path.of("/sys/fs/cgroup/cpu.stat"))) {
                if (l.startsWith("nr_throttled ")) return Long.parseLong(l.substring(13).trim());
            }
        } catch (Exception e) {
            // not readable here
        }
        return -1;
    }

    /** Unmap a session's arena once no call is inside any of its snapshots, then close its socket. */
    private void retire(Conn conn) {
        long t0 = System.nanoTime();
        for (;;) {
            boolean inside = false;
            long lo = conn.firstSnap, hi = conn.lastSnap;
            for (Caller c : callers) {
                long id = c.marker() >>> 32;
                if (id >= lo && id <= hi) {
                    inside = true;
                    break;
                }
            }
            if (!inside) {
                conn.close();
                retired.incrementAndGet();
                return;
            }
            if (stop && System.nanoTime() - t0 > 5_000_000_000L) {
                // A call has been inside for longer than any deadline allows: leave the mapping
                // and the socket rather than unmap under it (sdk-contract §6, close()).
                System.err.println("sidecarzero: an arena is still in use after 5 s; leaving it mapped");
                return;
            }
            LockSupport.parkNanos(1_000_000);
        }
    }
}
