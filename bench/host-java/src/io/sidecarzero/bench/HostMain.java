package io.sidecarzero.bench;

import io.sidecarzero.ScoreClient;

import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

/**
 * scz-bench-host-java: the benchmark's host in Java, in-process mode only (bench §4.2), a port of
 * {@code bench/host/src/main.rs} {@code inproc}: the same flags, the same open-loop schedule, the
 * same per-call recording, and the same JSON with {@code "host":"java"}.
 *
 * <pre>
 *   scz-bench-host-java inproc --system=A|C|D [--variant=NAME] [--scenario=NAME] [--rate=50000]
 *       [--duration-s=30] [--warmup-s=5] [--threads=8] [--arrival=even|poisson] [--budget-us=2000]
 *       [--seed=1] --rows=PATH --out=PATH [--socket=PATH] [--http=ADDR:PORT] [--uds=PATH] [--no-spin]
 * </pre>
 *
 * <p>Allocation is read with {@code ThreadMXBean} around each measured call on its caller thread
 * (the Rust host counts every call's allocations, warm-up included; here the warm-up is the
 * JIT's, so only measured calls count).
 */
public final class HostMain {
    static final com.sun.management.ThreadMXBean MX = (com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean();
    static volatile int sink;

    static final class Opts {
        String mode, system = "A", variant = "default", scenario = "steady", rows = "", out = "";
        double rate = 50_000, durationS = 30, warmupS = 5;
        int threads = 8;
        boolean poisson, noSpin;
        long budgetUs = 2000, seed = 1;
        String socket = "", http = "", uds = "";
    }

    static double num(String k, String v) {
        try {
            return Double.parseDouble(v);
        } catch (NumberFormatException e) {
            throw new IllegalArgumentException(k + ": not a number: " + v);
        }
    }

    static String system(String v) {
        return switch (v) {
            case "A", "sidecarzero" -> "A";
            case "B", "triton" -> "B";
            case "C", "http-json" -> "C";
            case "D", "uds-binary" -> "D";
            case "E", "in-process" -> "E";
            default -> throw new IllegalArgumentException("--system: " + v);
        };
    }

    static Opts opts(String[] args) {
        if (args.length == 0) throw new IllegalArgumentException("usage: scz-bench-host-java inproc --system=… (see the source)");
        Opts o = new Opts();
        o.mode = args[0];
        for (int i = 1; i < args.length; i++) {
            String a = args[i];
            int e = a.indexOf('=');
            String k = e < 0 ? a : a.substring(0, e), v = e < 0 ? "" : a.substring(e + 1);
            switch (k) {
                case "--system" -> o.system = system(v);
                case "--variant" -> o.variant = v;
                case "--scenario" -> o.scenario = v;
                case "--rate" -> o.rate = num(k, v);
                case "--duration-s" -> o.durationS = num(k, v);
                case "--warmup-s" -> o.warmupS = num(k, v);
                case "--threads" -> o.threads = (int) num(k, v);
                case "--arrival" -> o.poisson = v.equals("poisson");
                case "--budget-us" -> o.budgetUs = (long) num(k, v);
                case "--seed" -> o.seed = (long) num(k, v);
                case "--rows" -> o.rows = v;
                case "--out" -> o.out = v;
                case "--socket" -> o.socket = v;
                case "--http" -> o.http = v;
                case "--uds" -> o.uds = v;
                case "--no-spin" -> o.noSpin = true;
                case "--listen", "--triton", "--model" -> {} // the Rust host's flags for modes and systems this host does not run
                default -> throw new IllegalArgumentException("unknown argument " + a);
            }
        }
        return o;
    }

    /** One thread's share of the run. */
    static final class ThreadOut {
        final Record rec = new Record();
        long bytes, calls;
        /** (t1, to_model, calls in the run before it, the reason the run of fallbacks started with). */
        final List<long[]> transitions = new ArrayList<>();
    }

    static long secsToNs(double s) {
        return Math.round(s * 1e9);
    }

    static void inproc(Opts o) throws Exception {
        if (o.system.equals("B") || o.system.equals("E")) {
            System.err.println("scz-bench-host-java: the Java host does not run system " + o.system + " (Triton, in-process ONNX Runtime): use the Rust host");
            System.exit(2);
        }
        float[][] rows = Ref.readRows(o.rows);
        ScoreClient shared = o.system.equals("A") ? Systems.sidecarzero(o.socket, o.noSpin) : null;
        long budgetNs = o.budgetUs * 1000;
        long t0 = Ref.now() + 200_000_000L; // every thread has started by then
        long measureFrom = t0 + secsToNs(o.warmupS);
        long end = measureFrom + secsToNs(o.durationS);
        int threads = o.threads;
        double rate = o.rate;
        long period = (long) (threads * 1e9 / rate); // each thread's mean gap
        com.sun.management.OperatingSystemMXBean os = (com.sun.management.OperatingSystemMXBean) ManagementFactory.getOperatingSystemMXBean();
        long cpu0 = os.getProcessCpuTime();
        ThreadOut[] outs = new ThreadOut[threads];
        Thread[] ts = new Thread[threads];
        Throwable[] failed = new Throwable[1];
        for (int t = 0; t < threads; t++) {
            final int i = t;
            ts[t] = new Thread(() -> {
                Systems.Caller c = switch (o.system) {
                    case "A" -> new Systems.Scz(shared);
                    case "C" -> new Systems.Wrapped(new Systems.HttpJson(o.http, budgetNs));
                    default -> new Systems.Wrapped(new Systems.UdsBinary(o.uds, budgetNs));
                };
                ThreadOut out = new ThreadOut();
                long[] seed = {o.seed ^ (i * 0x9E37L)};
                // Evenly spaced: thread i takes slots i, i + T, i + 2T, … of the whole schedule.
                long next = t0 + (long) (i * 1e9 / rate);
                long k = 0, run = 0;
                boolean lastModel = true;
                int runReason = 0;
                while (next < end) {
                    float[] row = rows[(int) Long.remainderUnsigned(k * threads + i, rows.length)];
                    Ref.waitUntil(next);
                    boolean measured = next >= measureFrom;
                    long a0 = measured ? MX.getCurrentThreadAllocatedBytes() : 0;
                    long sent = Ref.now();
                    c.call(row, budgetNs);
                    long t1 = Ref.now();
                    if (measured) out.bytes += MX.getCurrentThreadAllocatedBytes() - a0;
                    sink += Float.floatToRawIntBits(c.value);
                    if (measured) {
                        out.rec.add(c.reason, t1 - next, c.entryNs, next - measureFrom);
                        out.rec.sendLag.record(sent - next);
                        out.calls++;
                        boolean model = c.reason < 0;
                        if (model != lastModel) {
                            int reason = model ? runReason : c.reason;
                            out.transitions.add(new long[] {t1, model ? 1 : 0, run, runReason});
                            run = 0;
                            runReason = reason;
                            lastModel = model;
                        }
                        run++;
                    }
                    k++;
                    if (o.poisson) {
                        // Exponential gaps with mean `period`, from a uniform in (0, 1].
                        double u = ((Ref.splitmix(seed) >>> 11) + 1.0) / (double) (1L << 53);
                        next = next + (long) (-Math.log(u) * period);
                    } else {
                        next = t0 + (long) ((double) (k * threads + i) * 1e9 / rate);
                    }
                }
                outs[i] = out;
            }, "caller-" + t);
            ts[t].setUncaughtExceptionHandler((th, e) -> failed[0] = e);
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        if (failed[0] != null) throw new IllegalStateException("a caller thread failed", failed[0]);
        Record rec = new Record();
        long bytes = 0, calls = 0;
        List<long[]> transitions = new ArrayList<>();
        for (int i = 0; i < threads; i++) {
            rec.merge(outs[i].rec);
            bytes += outs[i].bytes;
            calls += outs[i].calls;
            for (long[] x : outs[i].transitions) transitions.add(new long[] {x[0], i, x[1], x[2], x[3]});
        }
        long cpu = os.getProcessCpuTime() - cpu0;
        transitions.sort(Arrays::compareUnsigned);
        StringBuilder tr = new StringBuilder();
        for (long[] x : transitions) {
            if (tr.length() > 0) tr.append(',');
            tr.append('[').append(x[0]).append(',').append(x[1]).append(',').append(x[2]).append(',').append(x[3]).append(",\"").append(Ref.REASONS[(int) x[4]]).append("\"]");
        }
        double perCall = bytes / (double) Math.max(1, calls);
        String json = "{\"host\":\"java\",\"mode\":\"inproc\",\"system\":\"" + o.system + "\",\"variant\":\"" + o.variant + "\",\"scenario\":\"" + o.scenario
                + "\",\"rate\":" + Ref.f64(o.rate) + ",\"arrival\":\"" + (o.poisson ? "poisson" : "even") + "\",\"threads\":" + threads + ",\"duration_s\":"
                + Ref.f64(secsToNs(o.durationS) / 1e9) + ",\"warmup_s\":" + Ref.f64(secsToNs(o.warmupS) / 1e9) + ",\"budget_us\":" + o.budgetUs + ",\"start_ns\":"
                + measureFrom + ",\"end_ns\":" + end + ",\"calls\":" + calls + ",\"cpu_ns\":" + cpu + ",\"alloc_bytes_per_call\":"
                + String.format(Locale.ROOT, "%.3f", perCall) + ",\"record\":" + rec.json() + ",\"transitions\":[" + tr + "]}\n";
        Files.writeString(Path.of(o.out), json, StandardCharsets.UTF_8);
        System.err.println(String.format(Locale.ROOT,
                "scz-bench-host-java: %s/%s %s: %d calls, model %d, fallback %d, error %d; p50 %.1f µs, p99 %.1f µs, p99.9 %.1f µs; send lag p99 %.1f µs; %.2f B/call",
                o.system, o.variant, o.scenario, calls, rec.model, rec.fallbacks(), rec.error + rec.timeout, rec.latency.quantile(0.5) / 1e3,
                rec.latency.quantile(0.99) / 1e3, rec.latency.quantile(0.999) / 1e3, rec.sendLag.quantile(0.99) / 1e3, perCall));
        if (shared != null) shared.close();
    }

    public static void main(String[] args) {
        System.setErr(new java.io.PrintStream(new java.io.FileOutputStream(java.io.FileDescriptor.err), true, StandardCharsets.UTF_8));
        try {
            Opts o = opts(args);
            if (!o.mode.equals("inproc")) {
                System.err.println("scz-bench-host-java: mode " + o.mode + ": the Java host runs only inproc (bench §4.2)");
                System.exit(o.mode.equals("serve") ? 2 : 1);
            }
            inproc(o);
            System.exit(0);
        } catch (Exception e) {
            System.err.println("scz-bench-host-java: " + e.getMessage());
            System.exit(1);
        }
    }
}
