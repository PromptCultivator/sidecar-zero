package io.sidecarzero.bench;

import io.sidecarzero.FallbackReason;
import io.sidecarzero.Options;
import io.sidecarzero.ScoreClient;
import io.sidecarzero.ScoreOutcome;
import io.sidecarzero.SidecarZero;

import java.io.BufferedInputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.time.Duration;

/**
 * The systems of bench §3 the Java host runs, each behind one caller per thread
 * ({@code bench/host/src/systems.rs}): A through the Java SDK's allocation-free
 * {@link ScoreClient}, C and D through the baselines' wrapper of fairness rule 1.
 */
final class Systems {
    private Systems() {}

    /** One call's result, kept in the caller: the answer, the reason (-1: the model), and the fallback entry time (-1: none). */
    abstract static class Caller {
        float value;
        int reason;
        long entryNs;

        abstract void call(float[] x, long budgetNs);
    }

    // ── A ───────────────────────────────────────────────────────────────────────────────────────

    /** When the current thread's fallback was last entered: the reference fallback notes it. */
    static final ThreadLocal<long[]> FALLBACK_AT = ThreadLocal.withInitial(() -> new long[1]);

    static ScoreClient sidecarzero(String socket, boolean noSpin) {
        Options o = Options.defaults();
        if (noSpin) o.spinCeiling(Duration.ZERO);
        ScoreClient c = SidecarZero.connect(socket).score(Ref.FEATURES).fallback((x, r) -> {
            FALLBACK_AT.get()[0] = Ref.now();
            return Ref.fallback(x);
        }).options(o).build();
        if (!c.awaitBound(Duration.ofSeconds(30))) {
            System.err.println("scz-bench-host-java: the sidecar at " + socket + " did not bind within 30 s; measuring anyway");
        }
        return c;
    }

    static final class Scz extends Caller {
        final ScoreClient c;
        final long[] at;

        /** Made on its caller thread, so that the thread-local exists before the first call. */
        Scz(ScoreClient c) {
            this.c = c;
            this.at = FALLBACK_AT.get();
        }

        @Override
        void call(float[] x, long budgetNs) {
            long t0 = Ref.now();
            long p = c.scoreOutcome(x, budgetNs);
            value = ScoreOutcome.score(p);
            FallbackReason r = ScoreOutcome.reason(p);
            if (r == null) {
                reason = -1;
                entryNs = -1;
            } else {
                reason = r.ordinal(); // the SDK's reasons are REASONS[0..21) in the same order
                entryNs = Math.max(0, at[0] - t0);
            }
        }
    }

    // ── the baselines' wrapper ──────────────────────────────────────────────────────────────────

    /** A transport's timeout (the Rust host's Err(true)); any other IOException is a failure. */
    static final class Timeout extends IOException {
        Timeout() {
            super("timed out", null);
        }
    }

    interface Transport {
        float score(float[] x) throws IOException;
    }

    /** Fairness rule 1: an error or a missed deadline calls the same reference fallback. */
    static final class Wrapped extends Caller {
        final Transport t;

        Wrapped(Transport t) {
            this.t = t;
        }

        @Override
        void call(float[] x, long budgetNs) {
            long t0 = Ref.now();
            int kind; // 0 ok, 1 timeout, 2 failure
            float v = 0;
            try {
                v = t.score(x);
                kind = 0;
            } catch (SocketTimeoutException | Timeout e) {
                kind = 1;
            } catch (IOException | RuntimeException e) {
                kind = 2;
            }
            boolean late = Ref.now() - t0 > budgetNs;
            if (kind == 0 && !late) {
                value = v;
                reason = -1;
                entryNs = -1;
            } else {
                entryNs = Ref.now() - t0;
                value = Ref.fallback(x);
                reason = kind == 2 ? Ref.TRANSPORT_ERROR : Ref.TRANSPORT_TIMEOUT;
            }
        }
    }

    static int timeoutMs(long budgetNs) {
        return (int) Math.max(1, (budgetNs + 999_999) / 1_000_000);
    }

    // ── C: HTTP/1.1 keep-alive, JSON ────────────────────────────────────────────────────────────

    static final class HttpJson implements Transport {
        final String host;
        final int port;
        final int timeoutMs;
        /** The request, built in place each call; then its bytes (ASCII) and the answer's. */
        final StringBuilder body = new StringBuilder(4096);
        final StringBuilder head = new StringBuilder(128);
        byte[] wire = new byte[8192];
        byte[] line = new byte[1024];
        byte[] answer = new byte[256];
        int lineLen;
        Socket s;
        InputStream in;
        OutputStream out;

        HttpJson(String addr, long budgetNs) {
            int c = addr.lastIndexOf(':');
            host = addr.substring(0, c);
            port = Integer.parseInt(addr.substring(c + 1));
            timeoutMs = timeoutMs(budgetNs);
        }

        void connect() throws IOException {
            Socket k = new Socket();
            try {
                k.setTcpNoDelay(true);
                k.setSoTimeout(timeoutMs);
                k.connect(new InetSocketAddress(host, port));
            } catch (IOException e) {
                k.close();
                throw e;
            }
            s = k;
            in = new BufferedInputStream(k.getInputStream(), 8192);
            out = k.getOutputStream();
        }

        void drop() {
            try {
                if (s != null) s.close();
            } catch (IOException ignored) {
                // gone either way
            }
            s = null;
        }

        /** Read one line, with its CRLF, into {@code line}; false at end of stream before any byte. */
        boolean readLine() throws IOException {
            lineLen = 0;
            for (;;) {
                int c = in.read();
                if (c < 0) return lineLen > 0;
                if (lineLen < line.length) line[lineLen++] = (byte) c;
                if (c == '\n') return true;
            }
        }

        boolean lineStarts(String p, boolean ignoreCase) {
            if (lineLen < p.length()) return false;
            for (int i = 0; i < p.length(); i++) {
                int a = line[i], b = p.charAt(i);
                if (ignoreCase && a >= 'A' && a <= 'Z') a += 'a' - 'A';
                if (a != b) return false;
            }
            return true;
        }

        static void ascii(StringBuilder b, byte[] dst, int at) {
            for (int i = 0; i < b.length(); i++) dst[at + i] = (byte) b.charAt(i);
        }

        @Override
        public float score(float[] x) throws IOException {
            if (s == null) connect();
            try {
                return exchange(x);
            } catch (IOException e) {
                drop();
                throw e;
            }
        }

        float exchange(float[] x) throws IOException {
            // The body's floats are written by StringBuilder.append(float), which allocates
            // nothing: Java's shortest form ("1.0E-4" where Rust writes "0.0001"); the server
            // parses either.
            body.setLength(0);
            body.append("{\"features\":[");
            for (int i = 0; i < x.length; i++) {
                if (i > 0) body.append(',');
                body.append(x[i]);
            }
            body.append("]}");
            head.setLength(0);
            head.append("POST /v1/score HTTP/1.1\r\nHost: b\r\nContent-Type: application/json\r\nContent-Length: ").append(body.length()).append("\r\n\r\n");
            int n = head.length() + body.length();
            if (wire.length < n) wire = new byte[n];
            ascii(head, wire, 0);
            ascii(body, wire, head.length());
            out.write(wire, 0, n);
            out.flush();
            if (!readLine()) throw new EOFException();
            boolean ok = lineStarts("HTTP/1.1 200", false);
            int len = 0;
            for (;;) {
                if (!readLine()) throw new EOFException();
                if (lineLen == 2 && line[0] == '\r' && line[1] == '\n') break;
                if (lineStarts("content-length:", true)) {
                    len = 0;
                    for (int i = 15; i < lineLen; i++) {
                        int d = line[i] - '0';
                        if (d >= 0 && d <= 9) len = len * 10 + d;
                    }
                }
            }
            if (answer.length < len) answer = new byte[len];
            if (in.readNBytes(answer, 0, len) != len) throw new EOFException();
            int c = 0;
            while (c < len && answer[c] != ':') c++;
            int e = len;
            while (e > c + 1 && (answer[e - 1] == '}' || answer[e - 1] == ' ')) e--;
            if (!ok || c >= len) throw new IOException("invalid answer");
            try {
                return Float.parseFloat(new String(answer, c + 1, e - c - 1, StandardCharsets.US_ASCII).trim());
            } catch (NumberFormatException x2) {
                throw new IOException("invalid answer");
            }
        }
    }

    // ── D: Unix socket, binary frames ──────────────────────────────────────────────────────────

    static final java.util.function.Consumer<SelectionKey> IGNORE = k -> {};

    static final class UdsBinary implements Transport {
        final String path;
        final long budgetNs;
        final ByteBuffer req = ByteBuffer.allocateDirect(4 + Ref.FEATURES * 4).order(ByteOrder.LITTLE_ENDIAN);
        final ByteBuffer resp = ByteBuffer.allocateDirect(8).order(ByteOrder.LITTLE_ENDIAN);
        SocketChannel ch;
        Selector sel;

        UdsBinary(String path, long budgetNs) {
            this.path = path;
            this.budgetNs = budgetNs;
        }

        void drop() {
            try {
                if (sel != null) sel.close();
                if (ch != null) ch.close();
            } catch (IOException ignored) {
                // gone either way
            }
            ch = null;
            sel = null;
        }

        /** Wait for the channel, at most the budget per operation (the Rust host's socket timeouts). */
        void await(int op) throws IOException {
            SelectionKey k = ch.keyFor(sel);
            k.interestOps(op);
            long until = Ref.now() + budgetNs;
            for (;;) {
                long left = until - Ref.now();
                if (left <= 0) throw new Timeout();
                if (sel.select(IGNORE, timeoutMs(left)) > 0) return; // the consumer form keeps no selected-key set
            }
        }

        void readFully(ByteBuffer b) throws IOException {
            while (b.hasRemaining()) {
                int n = ch.read(b);
                if (n < 0) throw new EOFException();
                if (n == 0) await(SelectionKey.OP_READ);
            }
        }

        @Override
        public float score(float[] x) throws IOException {
            if (ch == null) {
                SocketChannel c = SocketChannel.open(StandardProtocolFamily.UNIX);
                try {
                    c.connect(UnixDomainSocketAddress.of(path));
                    c.configureBlocking(false);
                    sel = Selector.open();
                    c.register(sel, SelectionKey.OP_READ);
                } catch (IOException e) {
                    c.close();
                    if (sel != null) sel.close();
                    sel = null;
                    throw e;
                }
                ch = c;
            }
            try {
                req.clear();
                req.putInt(Ref.FEATURES * 4);
                for (int j = 0; j < Ref.FEATURES; j++) req.putFloat(x[j]);
                req.flip();
                while (req.hasRemaining()) {
                    if (ch.write(req) == 0) await(SelectionKey.OP_WRITE);
                }
                resp.clear().limit(4);
                readFully(resp);
                if (resp.getInt(0) != 4) throw new IOException("invalid answer");
                resp.limit(8);
                readFully(resp);
                return resp.getFloat(4);
            } catch (IOException e) {
                drop();
                throw e;
            }
        }
    }
}
