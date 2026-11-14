package io.sidecarzero;

import io.sidecarzero.internal.Abi;
import io.sidecarzero.internal.Directory;
import io.sidecarzero.internal.Xxh3;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

/**
 * The SDK's unit tests, without a framework: {@code java ... io.sidecarzero.Tests ROOT}, where
 * ROOT is the repository (for {@code abi/golden}). Each test prints PASS or FAIL; the exit status
 * is the number of failures.
 */
public final class Tests {
    static int failed;
    static Path root;

    interface T {
        void run() throws Exception;
    }

    static void test(String name, T t) {
        try {
            t.run();
            System.out.println("PASS " + name);
        } catch (Throwable e) {
            failed++;
            System.out.println("FAIL " + name + ": " + e);
            e.printStackTrace(System.out);
        }
    }

    static void check(boolean ok, String what) {
        if (!ok) throw new AssertionError(what);
    }

    static int elem(String name) {
        for (int v = 1; v <= 12; v++) if (Abi.ElemType.name(v).equals(name)) return v;
        throw new IllegalArgumentException(name);
    }

    /** The notation of abi/golden/generate.py: `name:TYPE[size]`, `?` after TYPE when nullable, `,` between, `-` for none. */
    static List<Column> cols(String s) {
        List<Column> out = new ArrayList<>();
        if (s.equals("-")) return out;
        for (String c : s.split(",")) {
            int colon = c.lastIndexOf(':'), lb = c.lastIndexOf('[');
            String name = c.substring(0, colon), ty = c.substring(colon + 1, lb);
            int size = Integer.parseInt(c.substring(lb + 1, c.length() - 1));
            boolean nullable = ty.endsWith("?");
            out.add(new Column(name, elem(nullable ? ty.substring(0, ty.length() - 1) : ty), size, nullable));
        }
        return out;
    }

    static void fingerprints() throws Exception {
        int n = 0;
        for (String l : Files.readAllLines(root.resolve("abi/golden/fingerprints.txt"))) {
            if (l.startsWith("#") || l.isBlank()) continue;
            String[] w = l.split(" ");
            long want = Long.parseUnsignedLong(w[0].substring(2), 16);
            long got = new Schema(cols(w[1]), cols(w[2])).fingerprint();
            check(got == want, l + ": got 0x" + Long.toHexString(got));
            n++;
        }
        check(n >= 8, "only " + n + " vectors");
    }

    /** XXH3-64 for every length class, from the reference implementation (Python xxhash 3.5.0). */
    static final long[][] XXH3 = {
        {0, 0x2d06800538d394c2L}, {1, 0x13e608bc156defedL}, {2, 0x1c9074b93943b86cL}, {3, 0xa9088dda485b481cL},
        {4, 0x6d9253b16c8b1ed3L}, {7, 0x8e8291ad89127e2eL}, {8, 0x60539db630471163L}, {9, 0xfeff668361d723a8L},
        {15, 0x9ec169f4a98231c1L}, {16, 0xb8c859b0f030b585L}, {17, 0x714a04408e79b80fL}, {31, 0xed41ac283d0674b3L},
        {32, 0x19ff4ee1d6ba1a55L}, {33, 0x3e44983ad21679c8L}, {64, 0x287eb1fa9e4be2c1L}, {65, 0x829218de4d798646L},
        {96, 0xf084e7cfbc624743L}, {97, 0x1daa83271a8e7b7cL}, {127, 0x998fa0631fd78d0bL}, {128, 0x67425a03650261bfL},
        {129, 0xc664bf3311c6abc4L}, {150, 0xe2dc8795fca7eee2L}, {200, 0x746cd0025327bf5bL}, {239, 0x1d861931c1f3cafaL},
        {240, 0x64556dc6b462a6cfL}, {241, 0x8beadd3a8874fe17L}, {255, 0xb67b6637a76e6c39L}, {256, 0x3c38817f6d79c0daL},
        {500, 0xb8bc3e5683ce226aL}, {1023, 0xd26986a0b85dcc44L}, {1024, 0x9b81661c641c72b1L}, {1025, 0x806c2072ed713576L},
        {2048, 0xabe604813ba62ed1L}, {2049, 0x55aed42c9f1554b6L}, {5000, 0x799aaddd7339581dL},
    };

    static void xxh3() {
        for (long[] v : XXH3) {
            byte[] d = new byte[(int) v[0]];
            for (int i = 0; i < d.length; i++) d[i] = (byte) (i * 7 + 3);
            long got = Xxh3.hash(d);
            check(got == v[1], "length " + v[0] + ": got 0x" + Long.toHexString(got));
        }
    }

    /** The golden arena (abi/golden/arena.txt): its control header passes Directory.read, checksum included. */
    static void goldenArena() throws Exception {
        List<String> lines = Files.readAllLines(root.resolve("abi/golden/arena.txt"));
        long ctlLen = 0, dataLen = 0;
        for (String l : lines) {
            if (l.startsWith("segments")) {
                String[] w = l.split(" ");
                ctlLen = Long.decode(w[2]);
                dataLen = Long.decode(w[4]);
            }
        }
        try (Arena a = Arena.ofConfined()) {
            MemorySegment ctl = a.allocate(ctlLen, 4096), data = a.allocate(dataLen, 4096);
            for (String l : lines) {
                String[] w = l.split(" ");
                if (w.length != 3 || !(w[0].equals("control") || w[0].equals("data"))) continue;
                MemorySegment s = w[0].equals("control") ? ctl : data;
                s.set(ValueLayout.JAVA_LONG.withOrder(ByteOrder.LITTLE_ENDIAN), Long.decode(w[1]), Long.parseUnsignedLong(w[2].substring(2), 16));
            }
            Directory d = Directory.read(ctl.asReadOnly(), dataLen);
            check(d.arenaId == 0x0123456789ABCDEFL, "arena_id");
            check(d.rings == 1 && d.ringCap[0] == 8, "one ring of 8");
            check(d.classes == 1 && d.slotCount[0] == 16 && d.slotSize[0] == 4096 && d.reqCap[0] == 2048 && d.respCap[0] == 2048, "one class of 16 × 4 KB");
            check(Abi.StatusPage.magic(data, d.statusOff) == Abi.DATA_MAGIC, "data magic");
            check(d.maxDeadlineNs == Abi.Defaults.MAX_DEADLINE_NS && d.hbStaleNs == Abi.Defaults.HB_STALE_NS, "config");
            // Every cell starts with seq = its index.
            for (int i = 0; i < 8; i++) check(Abi.Cell.seq(data, d.ringOff[0] + Abi.RingHeader.CELLS_OFF + i * Abi.CELL_LEN) == i, "cell " + i);
            // A damaged directory is refused by its checksum.
            ctl.set(ValueLayout.JAVA_INT, Abi.ControlHeader.RING_DIR_OFF + Abi.RingDirEntry.CAPACITY, 1 << 30);
            boolean refused = false;
            try {
                Directory.read(ctl, dataLen);
            } catch (Directory.LayoutException e) {
                refused = e.getMessage().contains("checksum");
            }
            check(refused, "a damaged header must fail its checksum");
        }
    }

    /** A cell is written byte for byte as the Rust reference writes it (scz_shm::ring::write). */
    static void cellBytes() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment d = a.allocate(64, 64);
            d.fill((byte) 0x5A); // a cell's previous lap
            Engine.writeCell(d, 0, 0x1122334455667788L, 0xFFFF_FFFE, 1L << 63, 0x8000_0001, 0xFFFF, 42, Abi.CellFlag.VOID);
            ValueLayout.OfLong L = ValueLayout.JAVA_LONG.withOrder(ByteOrder.LITTLE_ENDIAN);
            long[] want = {0x5A5A5A5A5A5A5A5AL, 0x1122334455667788L, 0, 0xFFFF_FFFEL << 32, 1L << 63, 0x8000_0001L | 0xFFFFL << 32, 42, Abi.CellFlag.VOID};
            for (int i = 1; i < 8; i++) check(d.get(L, 8L * i) == want[i], "cell word " + i + ": 0x" + Long.toHexString(d.get(L, 8L * i)));
        }
    }

    /** An abandon during PROCESSING keeps OWNER; cancel and abandon refuse another epoch. */
    static void slotTransitions() {
        try (Arena a = Arena.ofConfined()) {
            MemorySegment d = a.allocate(64, 64);
            long owner2 = 2L << Abi.SlotWord.OWNER_SHIFT;
            Abi.SlotRecord.setWord(d, 0, Engine.word(7, Engine.PROCESSING) | owner2 | Engine.WAITER);
            check(Engine.abandon(d, 0, 7) == Engine.A_ABANDONED, "abandoned");
            long w = Abi.SlotRecord.word(d, 0);
            check(Engine.state(w) == Engine.ABANDONED && (w & Abi.SlotWord.OWNER_MASK) == owner2 && (w & Engine.WAITER) == 0, "OWNER kept: 0x" + Long.toHexString(w));
            Abi.SlotRecord.setWord(d, 0, Engine.word(8, Engine.READY));
            check(Engine.cancelIfReady(d, 0, 7) == Engine.C_NOT_READY, "another epoch is not ours");
            check(Engine.abandon(d, 0, 7) == Engine.A_GAVE_UP, "another epoch: gave up");
            check(Engine.cancelIfReady(d, 0, 8) == Engine.C_CANCELLED && Engine.state(Abi.SlotRecord.word(d, 0)) == Engine.FREE, "cancelled");
        }
    }

    static void packing() {
        long p = ScoreOutcome.model(Float.floatToRawIntBits(0.25f));
        check(ScoreOutcome.fromModel(p) && ScoreOutcome.score(p) == 0.25f && ScoreOutcome.reason(p) == null, "model");
        for (FallbackReason r : FallbackReason.values()) {
            long q = ScoreOutcome.fallback(Float.floatToRawIntBits(-1.5f), r);
            check(!ScoreOutcome.fromModel(q) && ScoreOutcome.reason(q) == r && ScoreOutcome.score(q) == -1.5f, r.label());
        }
        check(ScoreOutcome.modelGen(ScoreOutcome.model(0xFFFF_FFFE)) == 0xFFFF_FFFE, "generation bits");
    }

    static void mandatoryFallback() {
        boolean refused = false;
        try {
            SidecarZero.connect("/nonexistent").score(4).fallback(null);
        } catch (NullPointerException e) {
            refused = true;
        }
        check(refused, "a null fallback must be refused");
    }

    /** No sidecar: every call falls back with NOT_CONNECTED, counted, and close() returns. */
    static void notConnected() {
        Path p = Path.of(System.getProperty("java.io.tmpdir"), "scz-java-test-" + ProcessHandle.current().pid() + ".sock");
        try (ScoreClient c = SidecarZero.connect(p).score(4).fallback((f, r) -> f[0] + 1).build()) {
            float[] f = {1, 2, 3, 4};
            for (int i = 0; i < 10; i++) {
                long o = c.scoreOutcome(f, 1_000_000);
                check(!ScoreOutcome.fromModel(o) && ScoreOutcome.reason(o) == FallbackReason.NOT_CONNECTED && ScoreOutcome.score(o) == 2f, "fallback");
            }
            check(c.stats().fallbacks(FallbackReason.NOT_CONNECTED) == 10, "counted");
            check(!c.awaitBound(Duration.ofMillis(20)), "not bound");
        }
    }

    public static void main(String[] a) throws Exception {
        root = Path.of(a.length > 0 ? a[0] : ".");
        if (a.length > 1 && a[1].equals("offlinux")) {
            // Run with -Dos.name=Darwin: the SDK must construct, never link a downcall, and fall back.
            test("off Linux: nothing is linked, calls fall back with NOT_CONNECTED", () -> {
                check(!io.sidecarzero.internal.Native.LINUX, "os.name says Linux");
                notConnected();
            });
            System.exit(failed);
        }
        if (a.length > 2 && (a[1].equals("metrics") || a[1].equals("metrics-frames"))) {
            // In a JVM of their own, against a sidecar test.sh started: scz-worker-echo, or the fraud model.
            if (a[1].equals("metrics")) MetricsTests.live(a[2]);
            else MetricsTests.liveFrames(a[2]);
            System.out.println(failed == 0 ? a[1] + " tests: pass" : a[1] + " tests: " + failed + " failed");
            System.exit(failed);
        }
        test("schema fingerprints reproduce abi/golden/fingerprints.txt", Tests::fingerprints);
        test("XXH3-64 vectors for every length class", Tests::xxh3);
        test("the golden arena's control header reads back, checksum and all", Tests::goldenArena);
        test("a ring cell is written byte for byte like the Rust reference", Tests::cellBytes);
        test("slot transitions: abandon keeps OWNER, other epochs are refused", Tests::slotTransitions);
        test("packed score outcomes round-trip", Tests::packing);
        test("the fallback is mandatory", Tests::mandatoryFallback);
        test("without a sidecar every call falls back with NOT_CONNECTED", Tests::notConnected);
        MetricsTests.unit(); // before the stress test, whose threads would use up fresh thread ids
        test("stress: callers against a Java-side model of the worker", Stress::run);
        System.out.println(failed == 0 ? "unit tests: pass" : "unit tests: " + failed + " failed");
        System.exit(failed);
    }
}
