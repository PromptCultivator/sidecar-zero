package io.sidecarzero;

import io.sidecarzero.internal.Abi;
import io.sidecarzero.internal.Directory;
import io.sidecarzero.internal.Native;
import io.sidecarzero.internal.Registry;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.VarHandle;
import java.nio.charset.StandardCharsets;

import static io.sidecarzero.internal.Abi.ControlHeader;
import static io.sidecarzero.internal.Abi.MsgHello;
import static io.sidecarzero.internal.Abi.MsgReady;
import static io.sidecarzero.internal.Abi.MsgWelcome;
import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;
import static java.lang.foreign.ValueLayout.JAVA_SHORT;

/**
 * The background thread's side of a session (sdk-contract §6, "Handshake"): connect, HELLO,
 * WELCOME with the two memfds (close-on-exec), seal and size checks, the read-only control
 * mapping, the directory, the data mapping, page-table population, READY; then reading the
 * registry under the model seqlock and binding the codec to it. Everything here runs on the
 * background thread and may allocate.
 */
final class Session {
    static final int OK = 0, BUSY = 1, ABI = 2, OTHER = 3;
    static final int SDK_VERSION = 0x00_01_00;

    private final Arena scratch = Arena.ofShared();
    private final MemorySegment addr = scratch.allocate(112, 8);
    private final MemorySegment msg = scratch.allocate(64, 8);
    private final MemorySegment hdr = scratch.allocate(56, 8);
    private final MemorySegment iov = scratch.allocate(16, 8);
    private final MemorySegment cmsg = scratch.allocate(64, 8);
    private final MemorySegment tv = scratch.allocate(16, 8);
    private final MemorySegment pfd = scratch.allocate(8, 8);

    /** The outcome of the last {@link #connect}. */
    int code;
    /** Why the last connect failed, for diagnostics. */
    String why;

    void close() {
        scratch.close();
    }

    private Conn fail(int c, String w, int sock, int... fds) {
        code = c;
        why = w;
        for (int fd : fds) Native.close(fd);
        Native.close(sock);
        return null;
    }

    /** One connection attempt; null on failure, with {@link #code} saying why. */
    Conn connect(String endpoint, long timeoutMs) {
        int sock = -1;
        try {
            sock = Native.socket(Native.AF_UNIX, Native.SOCK_SEQPACKET | Native.SOCK_CLOEXEC, 0);
            if (sock < 0) return fail(OTHER, "socket", -1);
            tv.set(JAVA_LONG, 0, timeoutMs / 1000);
            tv.set(JAVA_LONG, 8, (timeoutMs % 1000) * 1000);
            Native.setsockopt(sock, Native.SOL_SOCKET, Native.SO_RCVTIMEO, tv, 16);
            Native.setsockopt(sock, Native.SOL_SOCKET, Native.SO_SNDTIMEO, tv, 16);
            int alen = sockaddr(endpoint);
            if (alen < 0) return fail(OTHER, "endpoint name too long", sock);
            if (Native.connect(sock, addr, alen) != 0) return fail(OTHER, "connect", sock);
            hello();
            if (Native.send(sock, msg, 64, Native.MSG_NOSIGNAL) != 64) return fail(OTHER, "send HELLO", sock);
            return welcome(sock);
        } catch (Throwable t) {
            return fail(OTHER, t.toString(), sock);
        }
    }

    private int sockaddr(String ep) {
        addr.fill((byte) 0);
        addr.set(JAVA_SHORT, 0, (short) Native.AF_UNIX);
        boolean abs = ep.startsWith("@");
        byte[] b = (abs ? ep.substring(1) : ep).getBytes(StandardCharsets.UTF_8);
        int start = abs ? 1 : 0;
        if (start + b.length >= 108) return -1;
        MemorySegment.copy(b, 0, addr, ValueLayout.JAVA_BYTE, 2 + start, b.length);
        return abs ? 2 + 1 + b.length : 2 + b.length + 1;
    }

    private void hello() {
        msg.fill((byte) 0);
        msg.set(JAVA_INT, MsgHello.MAGIC, (int) Abi.MSG_MAGIC);
        msg.set(JAVA_INT, MsgHello.KIND, Abi.MsgKind.HELLO);
        msg.set(JAVA_SHORT, MsgHello.ABI_MAJOR, (short) Abi.ABI_MAJOR);
        msg.set(JAVA_SHORT, MsgHello.ABI_MINOR_MIN, (short) Abi.ABI_MINOR_MIN);
        msg.set(JAVA_SHORT, MsgHello.ABI_MINOR_MAX, (short) Abi.ABI_MINOR);
        msg.set(JAVA_SHORT, MsgHello.SDK_LANG, (short) Abi.SdkLang.JAVA);
        msg.set(JAVA_INT, MsgHello.SDK_VERSION, SDK_VERSION);
        msg.set(JAVA_INT, MsgHello.PID, (int) ProcessHandle.current().pid());
        msg.set(JAVA_LONG, MsgHello.FLAGS, 0L);
    }

    private Conn welcome(int sock) throws Throwable {
        hdr.fill((byte) 0);
        cmsg.fill((byte) 0);
        msg.fill((byte) 0);
        iov.set(JAVA_LONG, 0, msg.address());
        iov.set(JAVA_LONG, 8, 64L);
        hdr.set(JAVA_LONG, 16, iov.address());
        hdr.set(JAVA_LONG, 24, 1L);
        hdr.set(JAVA_LONG, 32, cmsg.address());
        hdr.set(JAVA_LONG, 40, 64L);
        long n = Native.recvmsg(sock, hdr, Native.MSG_CMSG_CLOEXEC);
        if (n < 0) return fail(OTHER, "recvmsg WELCOME", sock);
        // Take every descriptor first, so that none leaks whatever happens next.
        int[] fds = new int[8];
        int nfd = 0;
        long clen = Math.min(hdr.get(JAVA_LONG, 40), 64);
        for (long off = 0; off + 16 <= clen; ) {
            long len = cmsg.get(JAVA_LONG, off);
            if (len < 16 || off + len > 64) break;
            if (cmsg.get(JAVA_INT, off + 8) == Native.SOL_SOCKET && cmsg.get(JAVA_INT, off + 12) == Native.SCM_RIGHTS) {
                for (long k = 0; k < (len - 16) / 4 && nfd < fds.length; k++) fds[nfd++] = cmsg.get(JAVA_INT, off + 16 + 4 * k);
            }
            off += (len + 7) & ~7L;
        }
        int[] got = java.util.Arrays.copyOf(fds, nfd);
        if (n == 0 && nfd == 0) return fail(OTHER, "end of stream before WELCOME", sock);
        int flags = hdr.get(JAVA_INT, 48);
        if ((flags & (Native.MSG_TRUNC | Native.MSG_CTRUNC)) != 0 || n != 64) return fail(OTHER, "malformed WELCOME", sock, got);
        if (msg.get(JAVA_INT, MsgWelcome.MAGIC) != (int) Abi.MSG_MAGIC || msg.get(JAVA_INT, MsgWelcome.KIND) != Abi.MsgKind.WELCOME) {
            return fail(OTHER, "not a WELCOME", sock, got);
        }
        int status = msg.get(JAVA_INT, MsgWelcome.STATUS);
        if (status == Abi.WelcomeStatus.BUSY) return fail(BUSY, "BUSY", sock, got);
        if (status == Abi.WelcomeStatus.ABI_MISMATCH) return fail(ABI, "ABI_MISMATCH", sock, got);
        if (status != Abi.WelcomeStatus.OK) return fail(OTHER, "WELCOME status " + status, sock, got);
        if (nfd != 2) return fail(OTHER, nfd + " descriptors with WELCOME", sock, got);
        long wCtl = msg.get(JAVA_INT, MsgWelcome.CTL_LEN) & 0xFFFF_FFFFL;
        long wData = msg.get(JAVA_LONG, MsgWelcome.DATA_LEN);
        long wArena = msg.get(JAVA_LONG, MsgWelcome.ARENA_ID);
        long session = msg.get(JAVA_LONG, MsgWelcome.SESSION_ID);
        int ctlFd = got[0], dataFd = got[1];
        int ctlSeals = Native.F_SEAL_GROW | Native.F_SEAL_SHRINK | Native.F_SEAL_FUTURE_WRITE | Native.F_SEAL_SEAL;
        int dataSeals = Native.F_SEAL_GROW | Native.F_SEAL_SHRINK | Native.F_SEAL_SEAL;
        int s1 = Native.fcntl(ctlFd, Native.F_GET_SEALS), s2 = Native.fcntl(dataFd, Native.F_GET_SEALS);
        if (s1 < 0 || (s1 & ctlSeals) != ctlSeals) return fail(OTHER, "control seals " + s1, sock, got);
        if (s2 < 0 || (s2 & dataSeals) != dataSeals) return fail(OTHER, "data seals " + s2, sock, got);
        long ctlLen = Native.lseek(ctlFd, 0, Native.SEEK_END), dataLen = Native.lseek(dataFd, 0, Native.SEEK_END);
        if (ctlLen < Abi.CTL_HEADER_LEN || ctlLen != wCtl || dataLen != wData) return fail(OTHER, "segment sizes", sock, got);
        Arena arena = Arena.ofShared();
        try {
            long ca = Native.mmap(ctlLen, Native.PROT_READ, Native.MAP_SHARED, ctlFd);
            if (ca == -1L) {
                arena.close();
                return fail(OTHER, "map control", sock, got);
            }
            final long cl = ctlLen;
            MemorySegment ctl = MemorySegment.ofAddress(ca).reinterpret(ctlLen, arena, s -> Native.munmap(ca, cl)).asReadOnly();
            Directory dir;
            try {
                dir = Directory.read(ctl, dataLen);
            } catch (Directory.LayoutException e) {
                arena.close();
                return fail(OTHER, "control header: " + e.getMessage(), sock, got);
            }
            if (dir.arenaId != wArena) {
                arena.close();
                return fail(OTHER, "arena_id differs from WELCOME", sock, got);
            }
            long da = Native.mmap(dataLen, Native.PROT_READ | Native.PROT_WRITE, Native.MAP_SHARED, dataFd);
            if (da == -1L) {
                arena.close();
                return fail(OTHER, "map data", sock, got);
            }
            final long dl = dataLen;
            MemorySegment data = MemorySegment.ofAddress(da).reinterpret(dataLen, arena, s -> Native.munmap(da, dl));
            if (Abi.StatusPage.magic(data, dir.statusOff) != Abi.DATA_MAGIC || Abi.StatusPage.arenaId(data, dir.statusOff) != dir.arenaId) {
                arena.close();
                return fail(OTHER, "the data segment's status page names another arena", sock, got);
            }
            Native.madvise(ca, ctlLen, Native.MADV_POPULATE_READ);
            Native.madvise(da, dataLen, Native.MADV_POPULATE_WRITE);
            Native.close(ctlFd); // the mappings stay
            Native.close(dataFd);
            msg.fill((byte) 0);
            msg.set(JAVA_INT, MsgReady.MAGIC, (int) Abi.MSG_MAGIC);
            msg.set(JAVA_INT, MsgReady.KIND, Abi.MsgKind.READY);
            msg.set(JAVA_LONG, MsgReady.ARENA_ID, wArena);
            if (Native.send(sock, msg, 64, Native.MSG_NOSIGNAL) != 64) {
                arena.close();
                return fail(OTHER, "send READY", sock);
            }
            code = OK;
            return new Conn(sock, arena, ctl, data, dir, session);
        } catch (Throwable t) {
            arena.close();
            throw t;
        }
    }

    /**
     * Wait up to {@code ms} for the session socket to carry something. Returns false when the
     * session has ended (end of stream, a hang-up, or an error); a message is read and dropped
     * (none is defined after READY).
     */
    boolean alive(int sock, int ms) {
        try {
            pfd.set(JAVA_INT, 0, sock);
            pfd.set(JAVA_SHORT, 4, (short) (Native.POLLIN | Native.POLLRDHUP));
            pfd.set(JAVA_SHORT, 6, (short) 0);
            int r = Native.poll(pfd, 1, ms);
            if (r <= 0) return true; // timeout, or EINTR
            short rev = pfd.get(JAVA_SHORT, 6);
            long k = Native.recv(sock, msg, 64, Native.MSG_PEEK | Native.MSG_DONTWAIT);
            if (k == 0) return false;
            if (k > 0) {
                Native.recv(sock, msg, 64, Native.MSG_DONTWAIT);
                return true;
            }
            return (rev & (Native.POLLHUP | Native.POLLERR | Native.POLLNVAL)) == 0;
        } catch (Throwable t) {
            return false;
        }
    }

    /** Whether the session socket shows end of stream now (a non-blocking peek; any thread). */
    static boolean peerClosed(int sock) {
        try (Arena a = Arena.ofConfined()) {
            return Native.recv(sock, a.allocate(64, 8), 64, Native.MSG_PEEK | Native.MSG_DONTWAIT) == 0;
        } catch (Throwable t) {
            return false;
        }
    }

    // ── the registry ────────────────────────────────────────────────────────────────────────────

    /** The model block and a copy of the registry, read under the seqlock. */
    record Block(int epoch, long fingerprint, byte[] registry) {}

    static Block readBlock(MemorySegment ctl, int attempts) {
        for (int a = 0; a < attempts; a++) {
            long s1 = ControlHeader.modelSeq(ctl, 0);
            if ((s1 & 1) == 1) {
                Thread.onSpinWait();
                continue;
            }
            int epoch = ControlHeader.registryEpoch(ctl, 0);
            long fp = ControlHeader.schemaFingerprint(ctl, 0);
            long off = ControlHeader.registryOff(ctl, 0) & 0xFFFF_FFFFL;
            long len = ControlHeader.registryLen(ctl, 0) & 0xFFFF_FFFFL;
            boolean fits = len == 0 || (off == Abi.CTL_HEADER_LEN && len <= Abi.CTL_REGISTRY_LEN && off + len <= ctl.byteSize());
            byte[] bytes = new byte[fits ? (int) len : 0];
            if (fits && len > 0) MemorySegment.copy(ctl, ValueLayout.JAVA_BYTE, off, bytes, 0, (int) len);
            VarHandle.acquireFence();
            if (ControlHeader.modelSeq(ctl, 0) == s1) return fits ? new Block(epoch, fp, bytes) : null;
        }
        return null;
    }

    /** The registry epoch alone, under the seqlock; -1 if the writer stayed busy. */
    static long epoch(MemorySegment ctl, int attempts) {
        for (int a = 0; a < attempts; a++) {
            long s1 = ControlHeader.modelSeq(ctl, 0);
            if ((s1 & 1) == 0) {
                int e = ControlHeader.registryEpoch(ctl, 0);
                VarHandle.acquireFence();
                if (ControlHeader.modelSeq(ctl, 0) == s1) return e & 0xFFFF_FFFFL;
            }
            Thread.onSpinWait();
        }
        return -1;
    }

    /**
     * Bind the codec ({@code want}: its fingerprint, null for opaque bytes) to the registry. The gate
     * generation is read first: the supervisor writes the model block before the gate shows a new
     * generation, so the block read after it is at least as new.
     */
    static Binding bind(Conn c, Long want) {
        int gen = (int) (ControlHeader.gate(c.ctl, 0) >>> 32);
        Block b = readBlock(c.ctl, 1000);
        if (b == null) return new Binding(Binding.UNBOUND, 0, null, 0);
        Registry reg = null;
        boolean err = false;
        try {
            reg = Registry.parse(b.registry(), c.dir.reqCap, c.dir.respCap);
        } catch (IllegalArgumentException e) {
            err = true;
        }
        int mode;
        if (want == null && reg == null && !err) mode = Binding.RAW;
        else if (want != null && reg != null && reg.fingerprint() == want && b.fingerprint() == want) mode = Binding.FRAMES;
        else if (err) mode = Binding.UNBOUND;
        else if (b.epoch() == 0) mode = Binding.UNBOUND; // nothing published yet
        else mode = Binding.MISMATCH;
        return new Binding(mode, b.epoch(), mode == Binding.FRAMES ? reg : null, gen);
    }
}
