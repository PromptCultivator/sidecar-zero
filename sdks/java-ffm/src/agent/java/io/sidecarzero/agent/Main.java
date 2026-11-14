package io.sidecarzero.agent;

import io.sidecarzero.BatchScoreClient;
import io.sidecarzero.Client;
import io.sidecarzero.Codec;
import io.sidecarzero.CodecException;
import io.sidecarzero.DenseCodec;
import io.sidecarzero.FallbackReason;
import io.sidecarzero.FrameReader;
import io.sidecarzero.FrameWriter;
import io.sidecarzero.Hooks;
import io.sidecarzero.Outcome;
import io.sidecarzero.ScoreOutcome;
import io.sidecarzero.SidecarZero;
import io.sidecarzero.SlotReader;
import io.sidecarzero.SlotWriter;
import io.sidecarzero.Stats;
import io.sidecarzero.internal.Clock;
import io.sidecarzero.internal.Native;

import java.io.BufferedReader;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;
import java.util.stream.Stream;

/**
 * scz-agent-java: the Java SDK's conformance agent (testing §3, protocol v2 of
 * {@code sdks/rust/conformance-agent/src/main.rs}, which documents the line protocol).
 *
 * <p>One client for {@code --endpoint=PATH}; one command per line on stdin, one reply line on
 * stdout. Differences from the Rust agent: {@code kind=virtual} runs the calling threads as
 * virtual threads; {@code CLOCK_SKEW} shifts the SDK's cheap clock; {@code FORK} is
 * unsupported (no fork without exec in Java); {@code SPAWN} starts its child with
 * {@code posix_spawnp}, so the child inherits exactly what an exec'd child inherits.
 */
public final class Main {
    static final int ENCODE = 1, DECODE = 2;
    static final int N = 256;
    static final String[] OPS = {"normal", "delay", "crash", "hang", "status", "bad_len", "stall_wake"};
    static final long MAGIC = ByteOrderLe.u64("SCZCHAOS".getBytes(StandardCharsets.US_ASCII), 0);

    static volatile int fault;
    static volatile long fallbackUs;
    static volatile boolean throwing;

    // ── the request format of conformance/proto ────────────────────────────────────────────────

    static final class ByteOrderLe {
        static long u64(byte[] b, int at) {
            long v = 0;
            for (int i = 7; i >= 0; i--) v = v << 8 | (b[at + i] & 0xFF);
            return v;
        }

        static void put(byte[] b, int at, long v) {
            for (int i = 0; i < 8; i++) b[at + i] = (byte) (v >>> (8 * i));
        }
    }

    static long splitmix64(long x) {
        x += 0x9e3779b97f4a7c15L;
        x = (x ^ (x >>> 30)) * 0xbf58476d1ce4e5b9L;
        x = (x ^ (x >>> 27)) * 0x94d049bb133111ebL;
        return x ^ (x >>> 31);
    }

    static byte[] request(long op, long arg, int thread, int seq, long seed, int rows) {
        long nonce = ((long) thread << 32) | (seq & 0xFFFF_FFFFL);
        byte[] b = new byte[(5 + rows) * 8];
        ByteOrderLe.put(b, 0, MAGIC);
        ByteOrderLe.put(b, 8, op);
        ByteOrderLe.put(b, 16, arg);
        ByteOrderLe.put(b, 24, nonce);
        ByteOrderLe.put(b, 32, seed);
        for (int i = 0; i < rows; i++) ByteOrderLe.put(b, 40 + 8 * i, splitmix64(seed ^ nonce ^ i));
        return b;
    }

    // ── codecs and the fallback ─────────────────────────────────────────────────────────────────

    /** Request bytes in, the 8-byte keyed answer out. */
    static final class Keyed implements Codec<byte[], Long> {
        @Override
        public int requestLen(byte[] in) {
            return in.length;
        }

        @Override
        public void encode(byte[] in, SlotWriter w) {
            if (fault == ENCODE) throw CodecException.SHAPE;
            w.write(in);
        }

        @Override
        public Long decode(SlotReader r) {
            if (fault == DECODE || r.length() != 8) throw CodecException.SHAPE;
            return r.readLong(0);
        }
    }

    /** The prebuilt dense codec with the agent's fault switch. */
    static final class Dense extends DenseCodec {
        Dense() {
            super(N);
        }

        @Override
        public void encodeRows(float[] features, FrameWriter w) {
            if (fault == ENCODE) throw CodecException.SHAPE;
            super.encodeRows(features, w);
        }

        @Override
        public void decodeInto(FrameReader r, float[] scores) {
            if (fault == DECODE) throw CodecException.SHAPE;
            super.decodeInto(r, scores);
        }
    }

    /** What a fallback that throws throws: the reason it was given. */
    static final class Thrown extends RuntimeException {
        final FallbackReason reason;

        Thrown(FallbackReason r) {
            super(r.label(), null, false, false);
            reason = r;
        }
    }

    /** Takes fallback_us (spinning up to 100 µs, sleeping beyond), then answers, or throws. */
    static void slow(FallbackReason r) {
        long us = fallbackUs;
        if (us > 0) {
            long end = System.nanoTime() + us * 1000;
            if (us > 100) {
                for (long left; (left = end - System.nanoTime()) > 0; ) LockSupport.parkNanos(left);
            } else {
                while (System.nanoTime() < end) Thread.onSpinWait();
            }
        }
        if (throwing) throw new Thrown(r);
    }

    // ── the client ──────────────────────────────────────────────────────────────────────────────

    static String endpoint;
    static Client<byte[], Long> keyed;
    static BatchScoreClient dense;
    static float[][] inputs;

    static void build(String codec, String inputsPath) throws IOException {
        if ("features".equals(codec)) {
            byte[] b = Files.readAllBytes(Path.of(inputsPath));
            if (b.length == 0 || b.length % (N * 4) != 0) throw new IOException(inputsPath + ": not rows of " + N + " float32");
            MemorySegment s = MemorySegment.ofArray(b);
            float[][] rows = new float[b.length / (N * 4)][N];
            for (int i = 0; i < rows.length; i++) {
                MemorySegment.copy(s, ValueLayout.JAVA_FLOAT_UNALIGNED.withOrder(ByteOrder.LITTLE_ENDIAN), (long) i * N * 4, rows[i], 0, N);
            }
            inputs = rows;
            dense = SidecarZero.connect(endpoint).batch(new Dense()).fallback((f, n, scores, r) -> {
                slow(r);
                Arrays.fill(scores, 0, Math.min(n, scores.length), 0f);
            }).build();
        } else {
            keyed = SidecarZero.connect(endpoint).codec(new Keyed()).fallback((in, r) -> {
                slow(r);
                return 0L;
            }).build();
        }
    }

    static boolean open() {
        return keyed != null || dense != null;
    }

    static void closeClient() {
        if (keyed != null) keyed.close();
        if (dense != null) dense.close();
        keyed = null;
        dense = null;
    }

    static final ThreadLocal<MemorySegment> CLOCK_BUF = ThreadLocal.withInitial(() -> Arena.global().allocate(16, 8));

    /** CLOCK_MONOTONIC, read directly (never the SDK's cheap clock, which a test may skew). */
    static long mono() {
        return Native.LINUX ? Native.monotonicNs(CLOCK_BUF.get()) : System.nanoTime();
    }

    record Req(long op, long arg, long seed, int rows, long budgetNs) {}

    static String call(Req q, int t, int s) {
        long t0 = mono();
        String body;
        try {
            if (dense != null) {
                int k = Math.max(1, q.rows);
                float[] f = new float[k * N];
                for (int i = 0; i < k; i++) System.arraycopy(inputs[(int) (((long) s + i) % inputs.length)], 0, f, i * N, N);
                float[] scores = new float[k];
                long p = dense.scoreInto(f, k, scores, q.budgetNs);
                if (ScoreOutcome.fromModel(p)) {
                    StringBuilder v = new StringBuilder();
                    for (int i = 0; i < k; i++) v.append(i == 0 ? "" : ",").append("0x").append(Integer.toHexString(Float.floatToRawIntBits(scores[i])));
                    body = "src=model gen=" + Integer.toUnsignedString(ScoreOutcome.modelGen(p)) + " v=" + v;
                } else {
                    body = "src=fallback reason=" + ScoreOutcome.reason(p).label();
                }
            } else {
                Outcome<Long> o = keyed.infer(request(q.op, q.arg, t, s, q.seed, q.rows), q.budgetNs);
                body = o.fromModel() ? "src=model gen=" + Integer.toUnsignedString(o.modelGen()) + " d=0x" + Long.toHexString(o.value())
                        : "src=fallback reason=" + o.reason().label();
            }
        } catch (Thrown x) {
            body = "src=thrown reason=" + x.reason.label();
        } catch (Hooks.Unwound u) {
            body = "src=unwound";
        } catch (Throwable x) {
            System.err.println("scz-agent-java: a call threw " + x);
            x.printStackTrace();
            body = "src=unwound";
        }
        long t1 = mono();
        return "R t=" + t + " s=" + s + " " + body + " t0=" + t0 + " t1=" + t1;
    }

    // ── PROC, SPAWN, virtual-thread warm-up ─────────────────────────────────────────────────────

    static long count(String dir) {
        try (Stream<Path> s = Files.list(Path.of(dir))) {
            return s.count();
        } catch (IOException e) {
            return 0;
        }
    }

    static String proc() {
        long maps;
        try {
            maps = Files.readAllLines(Path.of("/proc/self/maps")).stream().filter(l -> l.contains("memfd:scz-")).count();
        } catch (IOException e) {
            maps = 0;
        }
        return "PROC fds=" + count("/proc/self/fd") + " maps=" + maps + " threads=" + count("/proc/self/task");
    }

    static boolean warmedVirtual;

    /**
     * Start every carrier of the virtual-thread scheduler, and its timer, now: they are created on
     * first use and then stay, so starting them during a CALL would change the thread count the
     * runner compares with the one it recorded at BOUND.
     */
    static void warmVirtual() throws InterruptedException {
        if (warmedVirtual) return;
        warmedVirtual = true;
        int n = Integer.getInteger("jdk.virtualThreadScheduler.parallelism", Runtime.getRuntime().availableProcessors());
        AtomicInteger started = new AtomicInteger();
        Thread[] ts = new Thread[n];
        for (int i = 0; i < n; i++) {
            ts[i] = Thread.ofVirtual().start(() -> {
                started.incrementAndGet();
                long end = System.nanoTime() + 200_000_000L;
                while (started.get() < n && System.nanoTime() < end) Thread.onSpinWait();
                LockSupport.parkNanos(1_000_000);
            });
        }
        for (Thread t : ts) t.join();
    }

    // ── the loop ────────────────────────────────────────────────────────────────────────────────

    static Map<String, String> kv(String[] w) {
        Map<String, String> m = new HashMap<>();
        for (int i = 1; i < w.length; i++) {
            int e = w[i].indexOf('=');
            if (e > 0) m.put(w[i].substring(0, e), w[i].substring(e + 1));
        }
        return m;
    }

    static long num(Map<String, String> m, String k, long d) {
        try {
            String v = m.get(k);
            return v == null ? d : Long.parseLong(v);
        } catch (NumberFormatException e) {
            return d;
        }
    }

    static long op(String s) {
        if (s == null) return 0;
        for (int i = 0; i < OPS.length; i++) if (OPS[i].equals(s)) return i;
        try {
            return Long.parseLong(s);
        } catch (NumberFormatException e) {
            return 0;
        }
    }

    public static void main(String[] args) throws Exception {
        for (String a : args) if (a.startsWith("--endpoint=")) endpoint = a.substring("--endpoint=".length());
        if (endpoint == null) {
            System.err.println("usage: scz-agent-java --endpoint=PATH");
            System.exit(2);
        }
        PrintStream out = new PrintStream(new FileOutputStream(FileDescriptor.out), false, StandardCharsets.UTF_8);
        int threads = 1;
        String kind = "platform", codec = "keyed", inputsPath = null;
        build(codec, null);
        // A JVM links its downcalls on first use, which takes tens of milliseconds a native agent
        // does not spend: let the client finish its first connection attempt (successful or not)
        // before the runner starts, so the scenarios see the same starting point.
        long until = System.nanoTime() + 2_000_000_000L;
        while (keyed.stats().connectAttempts() == 0 && System.nanoTime() < until) LockSupport.parkNanos(500_000);
        if (System.getenv("SCZ_JAVA_DEBUG") != null) System.err.println("agent: HELLO at " + System.nanoTime() / 1_000_000 + " ms, JVM up " + java.lang.management.ManagementFactory.getRuntimeMXBean().getUptime() + " ms");
        out.println("HELLO sdk=java version=0.1.0 caps=threads,per-call,virtual,frames,unwind,throws,nospin,cheap-clock,spawn"
                + (io.sidecarzero.Hooks.CRASHPOINTS ? ",crashpoints" : ""));
        out.flush();
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        for (String line; (line = in.readLine()) != null; ) {
            String[] w = line.trim().split("\\s+");
            if (w.length == 0 || w[0].isEmpty()) continue;
            Map<String, String> m = kv(w);
            String reply;
            switch (w[0]) {
                case "CONFIG" -> {
                    String k = m.getOrDefault("kind", kind);
                    if (!List.of("platform", "virtual", "per-call").contains(k)) {
                        reply = "ERR unsupported";
                        break;
                    }
                    kind = k;
                    if (kind.equals("virtual")) warmVirtual();
                    threads = (int) Math.max(1, num(m, "threads", threads));
                    fallbackUs = num(m, "fallback_us", fallbackUs);
                    if (m.containsKey("fallback")) throwing = m.get("fallback").equals("throws");
                    if (m.containsKey("spin")) Hooks.fixSpinCeiling(m.get("spin").equals("off") ? 0 : -1);
                    String c = m.getOrDefault("codec", codec);
                    String ip = m.getOrDefault("inputs", inputsPath);
                    reply = "OK";
                    if (!c.equals(codec) && open()) {
                        // Another codec is another client: the old session ends first.
                        closeClient();
                        try {
                            build(c, ip);
                        } catch (IOException e) {
                            reply = "ERR " + e.getMessage();
                        }
                    }
                    codec = c;
                    inputsPath = ip;
                }
                case "CODEC_FAULT" -> {
                    String mode = m.getOrDefault("mode", "off");
                    fault = mode.equals("encode") ? ENCODE : mode.equals("decode") ? DECODE : 0;
                    reply = "OK";
                }
                case "UNWIND" -> {
                    Hooks.armUnwind();
                    reply = "OK";
                }
                case "CLOCK_SKEW" -> {
                    Clock.skew(num(m, "ns", 0));
                    reply = "OK";
                }
                case "FORK" -> reply = "ERR unsupported";
                case "SPAWN" -> {
                    int pid = Native.spawn("sleep", "600");
                    reply = pid > 0 ? "SPAWNED pid=" + pid : "ERR spawn failed";
                }
                case "PROC" -> reply = proc();
                case "EXIT" -> {
                    out.println("BYE");
                    out.flush();
                    closeClient();
                    System.exit(0);
                    return;
                }
                case "BOUND" -> {
                    if (!open()) {
                        reply = "ERR closed";
                        break;
                    }
                    Duration d = Duration.ofMillis(num(m, "ms", 5000));
                    reply = "BOUND " + (dense != null ? dense.awaitBound(d) : keyed.awaitBound(d));
                }
                case "CALL" -> {
                    if (!open()) {
                        reply = "ERR closed";
                        break;
                    }
                    int cnt = (int) num(m, "count", 1);
                    Req q = new Req(op(m.get("op")), num(m, "arg", 0), num(m, "seed", 1), (int) num(m, "rows", 0), num(m, "budget_us", 20_000) * 1000);
                    String[][] lines = new String[threads][cnt];
                    Thread[] ts = new Thread[threads];
                    String kd = kind;
                    for (int t = 0; t < threads; t++) {
                        final int tt = t;
                        Runnable r = () -> {
                            for (int s = 0; s < cnt; s++) {
                                if (kd.equals("per-call")) {
                                    final int ss = s;
                                    Thread one = new Thread(() -> lines[tt][ss] = call(q, tt, ss));
                                    one.start();
                                    try {
                                        one.join();
                                    } catch (InterruptedException e) {
                                        throw new RuntimeException(e);
                                    }
                                } else {
                                    lines[tt][s] = call(q, tt, s);
                                }
                            }
                        };
                        ts[t] = kd.equals("virtual") ? Thread.ofVirtual().unstarted(r) : new Thread(r, "caller-" + t);
                    }
                    for (Thread t : ts) t.start();
                    for (Thread t : ts) t.join();
                    for (String[] ls : lines) for (String l : ls) out.println(l);
                    reply = "END calls=" + ((long) threads * cnt);
                }
                case "STATS" -> {
                    if (!open()) {
                        reply = "ERR closed";
                        break;
                    }
                    Stats s = dense != null ? dense.stats() : keyed.stats();
                    StringBuilder b = new StringBuilder("STATS model=" + s.model() + " sessions=" + s.sessions() + " retired=" + s.retired() + " late_wakes=" + s.lateWakes());
                    for (FallbackReason r : FallbackReason.values()) {
                        if (s.fallbacks(r) > 0) b.append(" fb.").append(r.label()).append('=').append(s.fallbacks(r));
                    }
                    reply = b.toString();
                }
                case "CLOSE" -> {
                    if (open()) closeClient();
                    reply = "CLOSED";
                }
                default -> reply = "ERR unknown command " + w[0];
            }
            out.println(reply);
            out.flush();
        }
        closeClient();
    }
}
