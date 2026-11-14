package io.sidecarzero.internal;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;

import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

/**
 * The libc calls the SDK makes, through FFM downcalls (sdk-java §2). Linux on x86-64 and aarch64
 * only: elsewhere {@link #LINUX} is false, nothing here is linked, and the client never connects.
 *
 * <p>Every handle is created once, in {@link H}, and invoked with {@code invokeExact}. Pointers are
 * passed as plain {@code long} addresses, so no call creates a segment. No handle captures
 * {@code errno} (it would allocate without C2) and none is marked {@code critical}. {@code syscall}
 * and {@code fcntl} are variadic and linked with {@code firstVariadicArg}.
 */
public final class Native {
    private Native() {}

    public static final boolean LINUX;
    public static final boolean AARCH64;

    static {
        String os = System.getProperty("os.name", "");
        String arch = System.getProperty("os.arch", "");
        AARCH64 = arch.equals("aarch64");
        LINUX = os.startsWith("Linux") && (AARCH64 || arch.equals("amd64") || arch.equals("x86_64"));
    }

    // Linux values, the same on x86-64 and aarch64 apart from the syscall number.
    public static final int AF_UNIX = 1, SOCK_SEQPACKET = 5, SOCK_CLOEXEC = 0x80000;
    public static final int SOL_SOCKET = 1, SO_RCVTIMEO = 20, SO_SNDTIMEO = 21, SCM_RIGHTS = 1;
    public static final int MSG_PEEK = 0x2, MSG_CTRUNC = 0x8, MSG_TRUNC = 0x20, MSG_DONTWAIT = 0x40;
    public static final int MSG_NOSIGNAL = 0x4000, MSG_CMSG_CLOEXEC = 0x40000000;
    public static final short POLLIN = 0x1, POLLERR = 0x8, POLLHUP = 0x10, POLLNVAL = 0x20, POLLRDHUP = 0x2000;
    public static final int F_GET_SEALS = 1034;
    public static final int F_SEAL_SEAL = 1, F_SEAL_SHRINK = 2, F_SEAL_GROW = 4, F_SEAL_FUTURE_WRITE = 0x10;
    public static final int PROT_READ = 1, PROT_WRITE = 2, MAP_SHARED = 1;
    public static final int MADV_POPULATE_READ = 22, MADV_POPULATE_WRITE = 23;
    public static final int SEEK_END = 2, CLOCK_MONOTONIC = 1;
    static final long FUTEX_WAKE = 1, FUTEX_WAIT_BITSET = 9, FUTEX_BITSET_MATCH_ANY = 0xFFFF_FFFFL;
    static final long SYS_FUTEX;

    static {
        SYS_FUTEX = AARCH64 ? 98 : 202;
    }

    /** The handles. Initialized on first use, which only happens on Linux. */
    static final class H {
        static final Linker L = Linker.nativeLinker();
        static final SymbolLookup LIBC = L.defaultLookup();

        static MethodHandle dc(String name, FunctionDescriptor fd, Linker.Option... o) {
            MemorySegment s = LIBC.find(name).orElseThrow(() -> new UnsatisfiedLinkError("libc has no " + name));
            return L.downcallHandle(s, fd, o);
        }

        static final MethodHandle SOCKET = dc("socket", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT, JAVA_INT));
        static final MethodHandle CONNECT = dc("connect", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_LONG, JAVA_INT));
        static final MethodHandle SETSOCKOPT = dc("setsockopt", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT, JAVA_INT, JAVA_LONG, JAVA_INT));
        static final MethodHandle SEND = dc("send", FunctionDescriptor.of(JAVA_LONG, JAVA_INT, JAVA_LONG, JAVA_LONG, JAVA_INT));
        static final MethodHandle RECV = dc("recv", FunctionDescriptor.of(JAVA_LONG, JAVA_INT, JAVA_LONG, JAVA_LONG, JAVA_INT));
        static final MethodHandle RECVMSG = dc("recvmsg", FunctionDescriptor.of(JAVA_LONG, JAVA_INT, JAVA_LONG, JAVA_INT));
        static final MethodHandle POLL = dc("poll", FunctionDescriptor.of(JAVA_INT, JAVA_LONG, JAVA_LONG, JAVA_INT));
        static final MethodHandle CLOSE = dc("close", FunctionDescriptor.of(JAVA_INT, JAVA_INT));
        static final MethodHandle LSEEK = dc("lseek", FunctionDescriptor.of(JAVA_LONG, JAVA_INT, JAVA_LONG, JAVA_INT));
        static final MethodHandle FCNTL = dc("fcntl", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT), Linker.Option.firstVariadicArg(2));
        static final MethodHandle MMAP = dc("mmap", FunctionDescriptor.of(JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_INT, JAVA_INT, JAVA_INT, JAVA_LONG));
        static final MethodHandle MUNMAP = dc("munmap", FunctionDescriptor.of(JAVA_INT, JAVA_LONG, JAVA_LONG));
        static final MethodHandle MADVISE = dc("madvise", FunctionDescriptor.of(JAVA_INT, JAVA_LONG, JAVA_LONG, JAVA_INT));
        static final MethodHandle CLOCK_GETTIME = dc("clock_gettime", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_LONG));
        /** syscall(SYS_futex, uaddr, op, val, timeout, uaddr2, val3): every argument a long. */
        static final MethodHandle FUTEX = dc("syscall",
                FunctionDescriptor.of(JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG),
                Linker.Option.firstVariadicArg(1));
        static final MethodHandle POSIX_SPAWNP = dc("posix_spawnp",
                FunctionDescriptor.of(JAVA_INT, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG));
        static final MethodHandle RAISE = dc("raise", FunctionDescriptor.of(JAVA_INT, JAVA_INT));
    }

    /**
     * Link the handles and run each request-path call a few hundred times (on the background thread,
     * before the first request needs them). The first invocation of a downcall links its call site,
     * which can take milliseconds; and after 127 invocations the JDK customizes a method handle's
     * lambda form, generating and loading a class on whichever thread makes that call, which
     * allocates tens of kilobytes. Neither may happen on a request thread.
     */
    public static synchronized void link() {
        if (!LINUX || linked) return;
        try (Arena a = Arena.ofConfined()) {
            MemorySegment w = a.allocate(16, 8), ts = a.allocate(16, 8);
            w.set(JAVA_INT, 0, 1);
            for (int i = 0; i < 300; i++) {
                futexWait(w.address(), 0, 0, ts); // the word is not 0: returns at once
                futexWake(w.address(), 1);
                monotonicNs(ts);
            }
        }
        linked = true;
    }

    private static boolean linked;

    // ── request path: no errno, no allocation ─────────────────────────────────────────────────

    /**
     * FUTEX_WAIT_BITSET on the 32-bit word at {@code addr} (shared, absolute CLOCK_MONOTONIC
     * deadline). {@code ts} is the caller's own 16-byte timespec buffer. Returns the raw result;
     * callers re-read the word and the clock rather than trust it.
     */
    public static long futexWait(long addr, int expected, long deadlineNs, MemorySegment ts) {
        long tsAddr = 0L;
        if (deadlineNs > 0) {
            ts.set(JAVA_LONG, 0, deadlineNs / 1_000_000_000L);
            ts.set(JAVA_LONG, 8, deadlineNs % 1_000_000_000L);
            tsAddr = ts.address();
        }
        try {
            return (long) H.FUTEX.invokeExact(SYS_FUTEX, addr, FUTEX_WAIT_BITSET, expected & 0xFFFF_FFFFL, tsAddr, 0L, FUTEX_BITSET_MATCH_ANY);
        } catch (Throwable t) {
            return -1;
        }
    }

    public static void futexWake(long addr, int n) {
        try {
            long r = (long) H.FUTEX.invokeExact(SYS_FUTEX, addr, FUTEX_WAKE, (long) n, 0L, 0L, 0L);
        } catch (Throwable t) {
            // nothing to do: a waiter wakes at its deadline
        }
    }

    /** CLOCK_MONOTONIC in nanoseconds, through {@code ts} (16 bytes, owned by the caller). */
    public static long monotonicNs(MemorySegment ts) {
        try {
            int r = (int) H.CLOCK_GETTIME.invokeExact(CLOCK_MONOTONIC, ts.address());
        } catch (Throwable t) {
            return System.nanoTime();
        }
        return ts.get(JAVA_LONG, 0) * 1_000_000_000L + ts.get(JAVA_LONG, 8);
    }

    // ── background thread ─────────────────────────────────────────────────────────────────────

    public static int socket(int domain, int type, int proto) throws Throwable {
        return (int) H.SOCKET.invokeExact(domain, type, proto);
    }

    public static int connect(int fd, MemorySegment addr, int len) throws Throwable {
        return (int) H.CONNECT.invokeExact(fd, addr.address(), len);
    }

    public static int setsockopt(int fd, int level, int opt, MemorySegment val, int len) throws Throwable {
        return (int) H.SETSOCKOPT.invokeExact(fd, level, opt, val.address(), len);
    }

    public static long send(int fd, MemorySegment buf, long len, int flags) throws Throwable {
        return (long) H.SEND.invokeExact(fd, buf.address(), len, flags);
    }

    public static long recv(int fd, MemorySegment buf, long len, int flags) throws Throwable {
        return (long) H.RECV.invokeExact(fd, buf.address(), len, flags);
    }

    public static long recvmsg(int fd, MemorySegment msghdr, int flags) throws Throwable {
        return (long) H.RECVMSG.invokeExact(fd, msghdr.address(), flags);
    }

    public static int poll(MemorySegment fds, long n, int timeoutMs) throws Throwable {
        return (int) H.POLL.invokeExact(fds.address(), n, timeoutMs);
    }

    public static void close(int fd) {
        if (fd < 0) return;
        try {
            int r = (int) H.CLOSE.invokeExact(fd);
        } catch (Throwable t) {
            // the descriptor is gone either way
        }
    }

    public static long lseek(int fd, long off, int whence) throws Throwable {
        return (long) H.LSEEK.invokeExact(fd, off, whence);
    }

    public static int fcntl(int fd, int cmd) throws Throwable {
        return (int) H.FCNTL.invokeExact(fd, cmd);
    }

    public static long mmap(long len, int prot, int flags, int fd) throws Throwable {
        return (long) H.MMAP.invokeExact(0L, len, prot, flags, fd, 0L);
    }

    public static void munmap(long addr, long len) {
        try {
            int r = (int) H.MUNMAP.invokeExact(addr, len);
        } catch (Throwable t) {
            // nothing more can be done
        }
    }

    public static int madvise(long addr, long len, int advice) throws Throwable {
        return (int) H.MADVISE.invokeExact(addr, len, advice);
    }

    /** Terminates the current process immediately with SIGKILL for crash-point testing. */
    public static void killSelf() {
        if (LINUX) {
            try {
                int r = (int) H.RAISE.invokeExact(9);
            } catch (Throwable t) {
                // fall through to halt
            }
        }
        Runtime.getRuntime().halt(137);
    }

    /**
     * Start {@code argv[0]} (searched on PATH) with {@code argv}, inheriting the environment and
     * every descriptor that is not close-on-exec. Returns the pid, or -1. The conformance agent's
     * SPAWN, which must test what an exec'd child inherits: the JDK's own process launcher closes
     * every descriptor in the child, so it would hide a missing close-on-exec flag.
     */
    public static int spawn(String... argv) {
        if (!LINUX) return -1;
        try (Arena a = Arena.ofConfined()) {
            MemorySegment args = a.allocate(8L * (argv.length + 1), 8);
            for (int i = 0; i < argv.length; i++) {
                args.setAtIndex(ADDRESS, i, a.allocateFrom(argv[i]));
            }
            args.setAtIndex(ADDRESS, argv.length, MemorySegment.NULL);
            MemorySegment environ = H.LIBC.find("environ").orElseThrow().reinterpret(8).get(ADDRESS, 0);
            MemorySegment pid = a.allocate(ValueLayout.JAVA_INT);
            int r = (int) H.POSIX_SPAWNP.invokeExact(pid.address(), args.getAtIndex(ADDRESS, 0).address(), 0L, 0L, args.address(), environ.address());
            return r == 0 ? pid.get(ValueLayout.JAVA_INT, 0) : -1;
        } catch (Throwable t) {
            return -1;
        }
    }
}
