package io.sidecarzero;

import io.sidecarzero.internal.Abi;
import io.sidecarzero.internal.Clock;
import io.sidecarzero.internal.Directory;
import io.sidecarzero.internal.Native;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.util.concurrent.ThreadLocalRandom;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicLongArray;

import static io.sidecarzero.internal.Abi.Cell;
import static io.sidecarzero.internal.Abi.ControlHeader;
import static io.sidecarzero.internal.Abi.RingHeader;
import static io.sidecarzero.internal.Abi.SlotRecord;

/**
 * A stress test in the jcstress manner (sdk-java §11; testing §2.3), without jcstress: calling
 * threads run the real call path ({@link Engine#run}) against an arena in process memory and a
 * Java-side model of the worker (dequeue, claim keeping WAITER and setting OWNER, a random delay,
 * complete or free an abandoned slot, wake a parked caller), with random deadlines so that claims,
 * enqueues, cancels, abandons, and completions race. A call that unwinds between claiming a ring
 * cell and publishing it is injected now and then.
 *
 * <p>Forbidden outcomes: a model answer that is not its own request's, a slot left anything but
 * FREE with WAITER and OWNER clear, a ring whose head is not its tail, counters that disagree
 * with the outcomes, a call over its bound.
 */
final class Stress {
    static final int CAP = 8, SLOTS = 4;
    static final long RING = 4096, TABLE = 8192, SLOTDATA = 12288, DATA_LEN = 32768;

    static MemorySegment control(Arena a) {
        MemorySegment c = a.allocate(Abi.CTL_HEADER_LEN + Abi.CTL_REGISTRY_LEN, 4096);
        ControlHeader.setMagic(c, 0, Abi.CTL_MAGIC);
        ControlHeader.setAbiMajor(c, 0, Abi.ABI_MAJOR);
        ControlHeader.setAbiMinor(c, 0, Abi.ABI_MINOR);
        ControlHeader.setHeaderLen(c, 0, (int) Abi.CTL_HEADER_LEN);
        ControlHeader.setDataLen(c, 0, DATA_LEN);
        ControlHeader.setArenaId(c, 0, 0x5EED);
        ControlHeader.setCtlLen(c, 0, (int) c.byteSize());
        ControlHeader.setPageSize(c, 0, 4096);
        ControlHeader.setRingCount(c, 0, 1);
        ControlHeader.setSlabClassCount(c, 0, 1);
        ControlHeader.setHbIntervalNs(c, 0, Abi.Defaults.HB_INTERVAL_NS);
        ControlHeader.setHbStaleNs(c, 0, Abi.Defaults.HB_STALE_NS);
        ControlHeader.setSpinNs(c, 0, Abi.Defaults.SPIN_NS);
        ControlHeader.setDefaultDeadlineNs(c, 0, Abi.Defaults.DEFAULT_DEADLINE_NS);
        ControlHeader.setMaxDeadlineNs(c, 0, Abi.Defaults.MAX_DEADLINE_NS);
        long r = ControlHeader.RING_DIR_OFF;
        Abi.RingDirEntry.setRingOff(c, r, RING);
        Abi.RingDirEntry.setCapacity(c, r, CAP);
        Abi.RingDirEntry.setKind(c, r, Abi.RingKind.MPSC);
        Abi.RingDirEntry.setCellStride(c, r, (int) Abi.CELL_LEN);
        long s = ControlHeader.SLAB_DIR_OFF;
        Abi.SlabDirEntry.setTableOff(c, s, TABLE);
        Abi.SlabDirEntry.setDataOff(c, s, SLOTDATA);
        Abi.SlabDirEntry.setSlotSize(c, s, 4096);
        Abi.SlabDirEntry.setSlotCount(c, s, SLOTS);
        Abi.SlabDirEntry.setReqCap(c, s, 2048);
        Abi.SlabDirEntry.setRespCap(c, s, 2048);
        Abi.SlabDirEntry.setRecordStride(c, s, (int) Abi.SLOT_RECORD_LEN);
        Abi.DataMap.setStatusOff(c, ControlHeader.DATA_MAP_OFF, 0);
        Abi.DataMap.setStagingOff(c, ControlHeader.DATA_MAP_OFF, 4096);
        ControlHeader.setLayoutCrc32c(c, 0, Directory.layoutCrc(c));
        ControlHeader.setGate(c, 0, (1L << 32) | Abi.GateState.HEALTHY);
        return c;
    }

    static MemorySegment data(Arena a) {
        MemorySegment d = a.allocate(DATA_LEN, 4096);
        Abi.StatusPage.setMagic(d, 0, Abi.DATA_MAGIC);
        Abi.StatusPage.setArenaId(d, 0, 0x5EED);
        for (int i = 0; i < CAP; i++) Cell.setSeq(d, RING + RingHeader.CELLS_OFF + i * Abi.CELL_LEN, i);
        return d;
    }

    /** The answer the model worker gives: a fixed function of the request, so a misdelivered one is wrong. */
    static long answer(long req) {
        return Engine.mix(req ^ 0xC0FFEE);
    }

    /** The worker model: one consume thread, owner id 1. */
    static final class Worker extends Thread {
        final MemorySegment ctl, data;
        volatile boolean stop;
        final AtomicLong completed = new AtomicLong(), freed = new AtomicLong(), stale = new AtomicLong(), voids = new AtomicLong();

        Worker(MemorySegment ctl, MemorySegment data) {
            super("model-worker");
            this.ctl = ctl;
            this.data = data;
            setDaemon(true);
        }

        @Override
        public void run() {
            long head = 0;
            long owner = 1L << Abi.SlotWord.OWNER_SHIFT;
            ThreadLocalRandom rnd = ThreadLocalRandom.current();
            while (!stop) {
                ControlHeader.setSupervisorHbMonoNs(ctl, 0, Clock.now());
                long cell = RING + RingHeader.CELLS_OFF + (head & (CAP - 1)) * Abi.CELL_LEN;
                if (Cell.seq(data, cell) != head + 1) {
                    Thread.onSpinWait();
                    continue;
                }
                int idx = Cell.slotIndex(data, cell), ep = Cell.slotEpoch(data, cell), flags = Cell.flags(data, cell);
                Cell.setSeq(data, cell, head + CAP);
                RingHeader.setHead(data, RING, ++head);
                if ((flags & Abi.CellFlag.VOID) != 0) {
                    voids.incrementAndGet();
                    continue;
                }
                long rec = TABLE + (long) idx * Abi.SLOT_RECORD_LEN;
                boolean claimed = false;
                for (int a = 0; a < Abi.CAS_ATTEMPTS; a++) {
                    long w = SlotRecord.word(data, rec);
                    if (Engine.epoch(w) != ep || Engine.state(w) != Engine.READY) break;
                    if (SlotRecord.casWord(data, rec, w, Engine.word(ep, Engine.PROCESSING) | (w & Engine.WAITER) | owner)) {
                        claimed = true;
                        break;
                    }
                }
                if (!claimed) {
                    stale.incrementAndGet();
                    continue;
                }
                int delay = rnd.nextInt(100) < 70 ? 0 : rnd.nextInt(800);
                long end = System.nanoTime() + delay * 1000L;
                while (System.nanoTime() < end) Thread.onSpinWait();
                long req = SLOTDATA + (long) idx * 4096;
                long q = data.get(Le.LONG, req);
                data.set(Le.LONG, req + 2048, answer(q));
                SlotRecord.setRespLen(data, rec, 8);
                SlotRecord.setStatus(data, rec, rnd.nextInt(50) == 0 ? Abi.Status.MODEL_ERROR : Abi.Status.OK);
                SlotRecord.setModelGen(data, rec, 1);
                SlotRecord.setTDoneNs(data, rec, Clock.now());
                for (int a = 0; a < Abi.CAS_ATTEMPTS; a++) {
                    long w = SlotRecord.word(data, rec);
                    if (Engine.epoch(w) != ep) break;
                    if (Engine.state(w) == Engine.ABANDONED) {
                        if (SlotRecord.casWord(data, rec, w, Engine.word(ep, Engine.FREE))) {
                            freed.incrementAndGet();
                            break;
                        }
                    } else if (Engine.state(w) == Engine.PROCESSING) {
                        if (SlotRecord.casWord(data, rec, w, Engine.word(ep, Engine.DONE))) {
                            completed.incrementAndGet();
                            if ((w & Engine.WAITER) != 0) Native.futexWake(data.address() + rec, 1);
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    }

    static void run() throws Exception {
        if (!Native.LINUX) return;
        int threads = 4, calls = 20_000;
        try (Arena a = Arena.ofShared()) {
            MemorySegment ctl = control(a), data = data(a);
            Directory dir = Directory.read(ctl.asReadOnly(), DATA_LEN);
            Engine e = new Engine("/nonexistent/scz-stress.sock", new AllocGate.KeyedWire(), Options.defaults());
            Conn conn = new Conn(-1, Arena.ofShared(), ctl.asReadOnly(), data, dir, 1);
            Snapshot snap = new Snapshot(1, conn, new Binding(Binding.RAW, 0, null, 1));
            conn.firstSnap = conn.lastSnap = 1;
            e.current = snap;
            Worker w = new Worker(ctl, data);
            w.start();
            AtomicLongArray seen = new AtomicLongArray(FallbackReason.count() + 2);
            AtomicLong wrong = new AtomicLong(), late = new AtomicLong(), unwound = new AtomicLong();
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final int tt = t;
                ts[t] = new Thread(() -> {
                    ThreadLocalRandom rnd = ThreadLocalRandom.current();
                    byte[] req = new byte[8];
                    for (int s = 0; s < calls; s++) {
                        long q = ((long) tt << 40) | s;
                        for (int k = 0; k < 8; k++) req[k] = (byte) (q >>> (8 * k));
                        long budget = 50_000 + rnd.nextInt(1_500_000);
                        if (tt == 0 && s % 2000 == 1000) Hooks.armUnwind();
                        long t0 = System.nanoTime();
                        Caller c = e.enter();
                        long st;
                        long out = 0;
                        try {
                            try {
                                st = e.run(c, req, null, budget);
                                out = c.outBits;
                            } finally {
                                e.exit(c);
                            }
                        } catch (Hooks.Unwound u) {
                            unwound.incrementAndGet();
                            continue;
                        }
                        if (System.nanoTime() - t0 > budget + 20_000_000L) late.incrementAndGet();
                        if (st >= 0) {
                            seen.incrementAndGet(FallbackReason.count());
                            if (out != answer(q)) wrong.incrementAndGet();
                        } else {
                            seen.incrementAndGet((int) (-st - 1));
                        }
                    }
                }, "stress-" + t);
                ts[t].start();
            }
            for (Thread t : ts) t.join();
            Thread.sleep(50); // the worker frees what was abandoned
            w.stop = true;
            w.join();
            Stats st = e.stats();
            StringBuilder tally = new StringBuilder("model " + seen.get(FallbackReason.count()));
            long total = seen.get(FallbackReason.count()) + unwound.get();
            for (FallbackReason r : FallbackReason.values()) {
                long k = seen.get(r.ordinal());
                total += k;
                if (k > 0) tally.append(", ").append(r.label()).append(' ').append(k);
                Tests.check(st.fallbacks(r) == k, "counter " + r + " " + st.fallbacks(r) + " != " + k);
            }
            tally.append(", unwound ").append(unwound.get()).append("; worker completed ").append(w.completed.get()).append(", freed abandoned ")
                    .append(w.freed.get()).append(", stale ").append(w.stale.get()).append(", void cells ").append(w.voids.get());
            System.out.println("     " + tally);
            Tests.check(total == (long) threads * calls, "every call returned");
            Tests.check(st.model() == seen.get(FallbackReason.count()), "model counter");
            Tests.check(wrong.get() == 0, wrong.get() + " model answers were another request's");
            Tests.check(late.get() == 0, late.get() + " calls over their bound");
            Tests.check(unwound.get() > 0 && w.voids.get() == unwound.get(), "every unwound call left a void cell");
            Tests.check(seen.get(FallbackReason.DEADLINE.ordinal()) > 0, "some deadlines");
            for (int i = 0; i < SLOTS; i++) {
                long x = SlotRecord.word(data, TABLE + i * Abi.SLOT_RECORD_LEN);
                Tests.check(Engine.state(x) == Engine.FREE && (x & (Engine.WAITER | Engine.OWNER)) == 0, "slot " + i + " left 0x" + Long.toHexString(x));
            }
            Tests.check(RingHeader.tail(data, RING) == RingHeader.head(data, RING), "ring drained");
            e.current = null;
            e.close();
        }
    }
}
