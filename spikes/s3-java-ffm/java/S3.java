// Spike S3 — Java FFM transport. Throwaway code: one file, clarity over polish.
// Uses only java.lang.foreign (no JNI, no JNA). Must compile with --release 22.
//
//   java --enable-native-access=ALL-UNNAMED S3 <socket> <mode>...
//   modes: handshake atomics futex alloc arena pin critical
//
// The helper (helper/helper.c) plays the supervisor on the other end of <socket>.

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.StructLayout;
import java.lang.foreign.SymbolLookup;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;
import static java.lang.foreign.ValueLayout.JAVA_SHORT;

public final class S3 {

    // ------------------------------------------------------------------ constants
    // Linux values; the same on x86-64 and aarch64 except the syscall numbers.
    static final int AF_UNIX = 1, SOCK_SEQPACKET = 5, SOCK_CLOEXEC = 0x80000;
    static final int SOL_SOCKET = 1, SCM_RIGHTS = 1;
    static final int MSG_NOSIGNAL = 0x4000, MSG_CMSG_CLOEXEC = 0x40000000, MSG_CTRUNC = 0x8;
    static final int F_GETFD = 1, FD_CLOEXEC = 1, F_DUPFD_CLOEXEC = 1030, F_ADD_SEALS = 1033, F_GET_SEALS = 1034;
    static final int F_SEAL_SEAL = 1, F_SEAL_SHRINK = 2, F_SEAL_GROW = 4, F_SEAL_WRITE = 8, F_SEAL_FUTURE_WRITE = 0x10;
    static final int PROT_READ = 1, PROT_WRITE = 2, MAP_SHARED = 1;
    static final int MADV_POPULATE_READ = 22, MADV_POPULATE_WRITE = 23;
    static final int SEEK_END = 2, CLOCK_MONOTONIC = 1;
    static final long FUTEX_WAIT = 0, FUTEX_WAKE = 1, FUTEX_WAIT_BITSET = 9, FUTEX_BITSET_MATCH_ANY = 0xFFFFFFFFL;

    static final String ARCH = System.getProperty("os.arch");
    static final boolean AARCH64 = ARCH.equals("aarch64");
    static final long SYS_FUTEX = AARCH64 ? 98 : 202;
    static final long SYS_GETPID = AARCH64 ? 172 : 39;
    static final long SYS_LSEEK = AARCH64 ? 62 : 8;

    // helper protocol
    static final int CMD_STORE_WAKE = 1, CMD_STORE_CTL = 2;
    static final long MAGIC = 0x3353504b495a4353L;

    // offsets inside the segments (all naturally aligned)
    static final long CTL_GATE = 64, CTL_HB = 72;
    static final long D_SLOT = 4096, D_STATE = 4100, D_CWAIT = 4104, D_STAMP = 4112, D_CELL = 4120, D_STATUS = 4128;
    static final long OFF_FUTEX = 8192, OFF_CAS = 12288, OFF_ARENA = 16384, OFF_PIN = 20480;

    // ------------------------------------------------------------------ linking
    static final Linker LINKER = Linker.nativeLinker();
    static final SymbolLookup LIBC = LINKER.defaultLookup();
    static final Linker.Option CAP = Linker.Option.captureCallState("errno");
    static final StructLayout CAPTURE_LAYOUT = Linker.Option.captureStateLayout();
    static final VarHandle ERRNO_VH = CAPTURE_LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("errno"));

    static MemorySegment sym(String name) {
        return LIBC.find(name).orElseThrow(() -> new UnsatisfiedLinkError("libc symbol not found: " + name));
    }

    static MethodHandle dc(String name, FunctionDescriptor fd, Linker.Option... opts) {
        return LINKER.downcallHandle(sym(name), fd, opts);
    }

    // Handshake calls: errno captured (first parameter of each handle is the capture segment).
    static final MethodHandle SOCKET = dc("socket", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT, JAVA_INT), CAP);
    static final MethodHandle CONNECT = dc("connect", FunctionDescriptor.of(JAVA_INT, JAVA_INT, ADDRESS, JAVA_INT), CAP);
    static final MethodHandle SENDMSG = dc("sendmsg", FunctionDescriptor.of(JAVA_LONG, JAVA_INT, ADDRESS, JAVA_INT), CAP);
    static final MethodHandle RECVMSG = dc("recvmsg", FunctionDescriptor.of(JAVA_LONG, JAVA_INT, ADDRESS, JAVA_INT), CAP);
    static final MethodHandle CLOSE = dc("close", FunctionDescriptor.of(JAVA_INT, JAVA_INT), CAP);
    static final MethodHandle LSEEK = dc("lseek", FunctionDescriptor.of(JAVA_LONG, JAVA_INT, JAVA_LONG, JAVA_INT), CAP);
    static final MethodHandle MMAP = dc("mmap",
            FunctionDescriptor.of(JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_INT, JAVA_INT, JAVA_INT, JAVA_LONG), CAP);
    static final MethodHandle MUNMAP = dc("munmap", FunctionDescriptor.of(JAVA_INT, JAVA_LONG, JAVA_LONG), CAP);
    static final MethodHandle MPROTECT = dc("mprotect", FunctionDescriptor.of(JAVA_INT, JAVA_LONG, JAVA_LONG, JAVA_INT), CAP);
    static final MethodHandle MADVISE = dc("madvise", FunctionDescriptor.of(JAVA_INT, JAVA_LONG, JAVA_LONG, JAVA_INT), CAP);
    static final MethodHandle CLOCK_GETTIME = dc("clock_gettime", FunctionDescriptor.of(JAVA_INT, JAVA_INT, ADDRESS));
    static final MethodHandle GETPID = dc("getpid", FunctionDescriptor.of(JAVA_INT));

    // Variadic in C: int fcntl(int fd, int cmd, ...);  long syscall(long number, ...);
    static final MethodHandle FCNTL2 = dc("fcntl", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT),
            CAP, Linker.Option.firstVariadicArg(2));
    static final MethodHandle FCNTL3 = dc("fcntl", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT, JAVA_INT),
            CAP, Linker.Option.firstVariadicArg(2));
    static final MethodHandle SYSCALL1 = dc("syscall", FunctionDescriptor.of(JAVA_LONG, JAVA_LONG),
            Linker.Option.firstVariadicArg(1));
    static final MethodHandle SYSCALL4 = dc("syscall",
            FunctionDescriptor.of(JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG), Linker.Option.firstVariadicArg(1));

    // syscall(SYS_futex, uaddr, op, val, timeout, uaddr2, val3): every argument passed as a long.
    static final FunctionDescriptor FUTEX_FD = FunctionDescriptor.of(JAVA_LONG,
            JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG);
    /** The request-path handle: no errno capture, as the design says. */
    static final MethodHandle FUTEX = dc("syscall", FUTEX_FD, Linker.Option.firstVariadicArg(1));
    /** Same call with errno captured, for the tests that want to see the reason. */
    static final MethodHandle FUTEX_ERRNO = dc("syscall", FUTEX_FD, CAP, Linker.Option.firstVariadicArg(1));
    /** Same call with uaddr as an ADDRESS, so FFM can see which segment the call uses (arena test only). */
    static final MethodHandle FUTEX_SEGARG = dc("syscall", FunctionDescriptor.of(JAVA_LONG,
            JAVA_LONG, ADDRESS, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG), Linker.Option.firstVariadicArg(1));

    // Atomic access to arena words, little-endian as the ABI fixes it. Coordinates: (MemorySegment, long offset).
    static final VarHandle LONG = JAVA_LONG.withOrder(ByteOrder.LITTLE_ENDIAN).varHandle();
    static final VarHandle INT = JAVA_INT.withOrder(ByteOrder.LITTLE_ENDIAN).varHandle();

    // ------------------------------------------------------------------ process-wide scratch
    static final Arena G = Arena.global();
    static final MemorySegment MSG = G.allocate(56, 8);     // struct msghdr
    static final MemorySegment IOV = G.allocate(16, 8);     // struct iovec
    static final MemorySegment CMSG = G.allocate(64, 8);    // control-message buffer
    static final MemorySegment BUF = G.allocate(64, 8);     // payload
    static final MemorySegment CAPSEG = G.allocate(CAPTURE_LAYOUT);   // main thread's errno capture
    static final MemorySegment CAP_CLEAN = G.allocate(CAPTURE_LAYOUT); // used by arena cleanup actions
    static final MemorySegment TS = G.allocate(16, 8);      // main thread's struct timespec

    /** CLOCK_MONOTONIC minus System.nanoTime(), from the tightest of several back-to-back samples (design §6). */
    static final long CLOCK_OFFSET = calibrate();

    static int sock, ctlFd, dataFd;
    static long ctlLen, dataLen;
    static final Arena CONN_ARENA = Arena.ofShared();
    static MemorySegment CTL, DATA;
    static String outDir = System.getProperty("s3.out", "/tmp");

    // ------------------------------------------------------------------ small helpers
    static void p(String fmt, Object... a) {
        System.out.printf(fmt + "%n", a);
        System.out.flush();
    }

    static int errno() {
        return (int) ERRNO_VH.get(CAPSEG, 0L);
    }

    static int errno(MemorySegment cap) {
        return (int) ERRNO_VH.get(cap, 0L);
    }

    static String en(int e) {
        return switch (e) {
            case 0 -> "0";
            case 1 -> "EPERM(1)";
            case 4 -> "EINTR(4)";
            case 9 -> "EBADF(9)";
            case 11 -> "EAGAIN(11)";
            case 13 -> "EACCES(13)";
            case 14 -> "EFAULT(14)";
            case 22 -> "EINVAL(22)";
            case 110 -> "ETIMEDOUT(110)";
            default -> "errno " + e;
        };
    }

    static void check(boolean ok, String what) {
        if (!ok) throw new IllegalStateException("FAILED: " + what + " (errno " + en(errno()) + ")");
    }

    static long monoNow(MemorySegment ts) throws Throwable {
        int rc = (int) CLOCK_GETTIME.invokeExact(CLOCK_MONOTONIC, ts);
        return ts.get(JAVA_LONG, 0) * 1_000_000_000L + ts.get(JAVA_LONG, 8);
    }

    static long calibrate() {
        try {
            long best = Long.MAX_VALUE, off = 0;
            for (int i = 0; i < 2000; i++) {
                long a = System.nanoTime();
                long m = monoNow(TS);
                long b = System.nanoTime();
                if (b - a < best) {
                    best = b - a;
                    off = m - (a + (b - a) / 2);
                }
            }
            return off;
        } catch (Throwable t) {
            throw new ExceptionInInitializerError(t);
        }
    }

    /** FUTEX_WAIT_BITSET, shared, absolute CLOCK_MONOTONIC deadline. Address is a plain number. No errno. */
    static long futexWait(long addr, int expected, long deadlineMonoNs, MemorySegment ts) throws Throwable {
        ts.set(JAVA_LONG, 0, deadlineMonoNs / 1_000_000_000L);
        ts.set(JAVA_LONG, 8, deadlineMonoNs % 1_000_000_000L);
        return (long) FUTEX.invokeExact(SYS_FUTEX, addr, FUTEX_WAIT_BITSET, (long) expected, ts.address(), 0L,
                FUTEX_BITSET_MATCH_ANY);
    }

    static long futexWaitErrno(MemorySegment cap, long addr, int expected, long deadlineMonoNs, MemorySegment ts)
            throws Throwable {
        ts.set(JAVA_LONG, 0, deadlineMonoNs / 1_000_000_000L);
        ts.set(JAVA_LONG, 8, deadlineMonoNs % 1_000_000_000L);
        return (long) FUTEX_ERRNO.invokeExact(cap, SYS_FUTEX, addr, FUTEX_WAIT_BITSET, (long) expected, ts.address(), 0L,
                FUTEX_BITSET_MATCH_ANY);
    }

    static long sendMsg(MemorySegment payload, long len) throws Throwable {
        MSG.fill((byte) 0);
        IOV.set(ADDRESS, 0, payload);
        IOV.set(JAVA_LONG, 8, len);
        MSG.set(ADDRESS, 16, IOV);      // msg_iov
        MSG.set(JAVA_LONG, 24, 1L);     // msg_iovlen
        return (long) SENDMSG.invokeExact(CAPSEG, sock, MSG, MSG_NOSIGNAL);
    }

    static long recvMsg(MemorySegment payload, long cap, boolean wantCmsg, int flags) throws Throwable {
        MSG.fill((byte) 0);
        IOV.set(ADDRESS, 0, payload);
        IOV.set(JAVA_LONG, 8, cap);
        MSG.set(ADDRESS, 16, IOV);
        MSG.set(JAVA_LONG, 24, 1L);
        if (wantCmsg) {
            CMSG.fill((byte) 0);
            MSG.set(ADDRESS, 32, CMSG);               // msg_control
            MSG.set(JAVA_LONG, 40, CMSG.byteSize());  // msg_controllen
        }
        return (long) RECVMSG.invokeExact(CAPSEG, sock, MSG, flags);
    }

    static void helperSend(int cmd, long off, int val, int nwake, int delayMs) throws Throwable {
        BUF.set(JAVA_INT, 0, cmd);
        BUF.set(JAVA_INT, 4, (int) off);
        BUF.set(JAVA_INT, 8, val);
        BUF.set(JAVA_INT, 12, nwake);
        BUF.set(JAVA_INT, 16, delayMs);
        BUF.set(JAVA_INT, 20, 0);
        check(sendMsg(BUF, 24) == 24, "send request to helper");
    }

    /** Returns the helper's rc (for CMD_STORE_WAKE: the number of waiters FUTEX_WAKE reported). */
    static int helperRecv() throws Throwable {
        check(recvMsg(BUF, 64, false, 0) == 8, "recv reply from helper");
        return BUF.get(JAVA_INT, 0);
    }

    static long mapsCount(String needle) throws Exception {
        return Files.readAllLines(Path.of("/proc/self/maps")).stream().filter(l -> l.contains(needle)).count();
    }

    // ------------------------------------------------------------------ (a)(b) handshake
    static void connect(String path, boolean v) throws Throwable {
        sock = (int) SOCKET.invokeExact(CAPSEG, AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
        check(sock >= 0, "socket");
        MemorySegment sa = G.allocate(110, 2);         // struct sockaddr_un
        sa.set(JAVA_SHORT, 0, (short) AF_UNIX);
        sa.setString(2, path);
        int rc = (int) CONNECT.invokeExact(CAPSEG, sock, sa, 110);
        check(rc == 0, "connect " + path);

        BUF.set(JAVA_LONG, 0, 0x4f4c4c4548L);          // "HELLO"
        check(sendMsg(BUF, 8) == 8, "sendmsg HELLO");
        long n = recvMsg(BUF, 64, true, MSG_CMSG_CLOEXEC);
        check(n == 24, "recvmsg WELCOME, got " + n);
        long magic = BUF.get(JAVA_LONG, 0);
        ctlLen = BUF.get(JAVA_LONG, 8);
        dataLen = BUF.get(JAVA_LONG, 16);
        long controllen = MSG.get(JAVA_LONG, 40);
        int msgFlags = MSG.get(JAVA_INT, 48);
        long cmsgLen = CMSG.get(JAVA_LONG, 0);
        int level = CMSG.get(JAVA_INT, 8), type = CMSG.get(JAVA_INT, 12);
        ctlFd = CMSG.get(JAVA_INT, 16);
        dataFd = CMSG.get(JAVA_INT, 20);
        if (v) {
            p("[a] socket fd=%d connected to %s (SOCK_SEQPACKET|SOCK_CLOEXEC)", sock, path);
            p("[a] recvmsg returned %d bytes; magic ok=%b ctl_len=%d data_len=%d", n, magic == MAGIC, ctlLen, dataLen);
            p("[a] msg_controllen=%d msg_flags=0x%x (MSG_CTRUNC set=%b) cmsg_len=%d level=%d type=%d",
                    controllen, msgFlags, (msgFlags & MSG_CTRUNC) != 0, cmsgLen, level, type);
            p("[a] SCM_RIGHTS descriptors: control fd=%d data fd=%d", ctlFd, dataFd);
        }
        if (magic != MAGIC || cmsgLen != 24 || level != SOL_SOCKET || type != SCM_RIGHTS || (msgFlags & MSG_CTRUNC) != 0)
            throw new IllegalStateException("bad WELCOME");

        // close-on-exec, sizes, seals. fcntl is variadic; here no variadic argument is passed.
        int fdfl1 = (int) FCNTL2.invokeExact(CAPSEG, ctlFd, F_GETFD);
        int fdfl2 = (int) FCNTL2.invokeExact(CAPSEG, dataFd, F_GETFD);
        int fdflS = (int) FCNTL2.invokeExact(CAPSEG, sock, F_GETFD);
        long sz1 = (long) LSEEK.invokeExact(CAPSEG, ctlFd, 0L, SEEK_END);
        long sz2 = (long) LSEEK.invokeExact(CAPSEG, dataFd, 0L, SEEK_END);
        int seals1 = (int) FCNTL2.invokeExact(CAPSEG, ctlFd, F_GET_SEALS);
        int seals2 = (int) FCNTL2.invokeExact(CAPSEG, dataFd, F_GET_SEALS);
        int wantCtl = F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_FUTURE_WRITE | F_SEAL_SEAL;
        int wantData = F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL;
        if (v) {
            p("[a] fcntl(F_GETFD): control=0x%x data=0x%x socket=0x%x  (FD_CLOEXEC=1 expected on all three)", fdfl1, fdfl2, fdflS);
            p("[a] lseek(fd,0,SEEK_END): control=%d (want %d) data=%d (want %d)", sz1, ctlLen, sz2, dataLen);
            p("[a] fcntl(F_GET_SEALS): control=0x%x (want superset of 0x%x: %b)  data=0x%x (want superset of 0x%x: %b)",
                    seals1, wantCtl, (seals1 & wantCtl) == wantCtl, seals2, wantData, (seals2 & wantData) == wantData);
        }
        if (sz1 != ctlLen || sz2 != dataLen || (seals1 & wantCtl) != wantCtl || (seals2 & wantData) != wantData
                || (fdfl1 & FD_CLOEXEC) == 0 || (fdfl2 & FD_CLOEXEC) == 0)
            throw new IllegalStateException("size / seal / cloexec check failed");

        // (b) mappings
        long ctlAddr = (long) MMAP.invokeExact(CAPSEG, 0L, ctlLen, PROT_READ, MAP_SHARED, ctlFd, 0L);
        check(ctlAddr != -1L, "mmap control PROT_READ");
        long bad = (long) MMAP.invokeExact(CAPSEG, 0L, ctlLen, PROT_READ | PROT_WRITE, MAP_SHARED, ctlFd, 0L);
        int badErr = errno();
        int mp = (int) MPROTECT.invokeExact(CAPSEG, ctlAddr, ctlLen, PROT_READ | PROT_WRITE);
        int mpErr = errno();
        long dataAddr = (long) MMAP.invokeExact(CAPSEG, 0L, dataLen, PROT_READ | PROT_WRITE, MAP_SHARED, dataFd, 0L);
        check(dataAddr != -1L, "mmap data PROT_READ|PROT_WRITE");
        int ma1 = (int) MADVISE.invokeExact(CAPSEG, ctlAddr, ctlLen, MADV_POPULATE_READ);
        int ma1e = errno();
        int ma2 = (int) MADVISE.invokeExact(CAPSEG, dataAddr, dataLen, MADV_POPULATE_WRITE);
        int ma2e = errno();
        if (v) {
            p("[b] mmap(control, PROT_READ, MAP_SHARED)            = 0x%x", ctlAddr);
            p("[b] mmap(control, PROT_READ|PROT_WRITE, MAP_SHARED) = %d errno=%s   (must fail)", bad, bad == -1L ? en(badErr) : "-");
            p("[b] mprotect(control mapping, PROT_READ|PROT_WRITE) = %d errno=%s   (must fail)", mp, mp == -1 ? en(mpErr) : "-");
            p("[b] mmap(data, PROT_READ|PROT_WRITE, MAP_SHARED)    = 0x%x", dataAddr);
            p("[b] madvise(control, MADV_POPULATE_READ)=%d%s  madvise(data, MADV_POPULATE_WRITE)=%d%s",
                    ma1, ma1 == 0 ? "" : " errno=" + en(ma1e), ma2, ma2 == 0 ? "" : " errno=" + en(ma2e));
        }
        if (bad != -1L || mp != -1) throw new IllegalStateException("control segment could be made writable");

        final long cl = ctlLen, dl = dataLen;
        CTL = MemorySegment.ofAddress(ctlAddr).reinterpret(cl, CONN_ARENA, s -> unmap(s.address(), cl, null)).asReadOnly();
        DATA = MemorySegment.ofAddress(dataAddr).reinterpret(dl, CONN_ARENA, s -> unmap(s.address(), dl, null));
        long cm = (long) LONG.getAcquire(CTL, 0L), dm = (long) LONG.getAcquire(DATA, 0L);
        if (v) p("[b] magic read through the segments: control ok=%b data ok=%b; control header data_len=%d",
                cm == MAGIC, dm == MAGIC, (long) LONG.getAcquire(CTL, 8L));
        if (cm != MAGIC || dm != MAGIC) throw new IllegalStateException("magic mismatch");
    }

    static void unmap(long addr, long len, String tag) {
        try {
            int rc = (int) MUNMAP.invokeExact(CAP_CLEAN, addr, len);
            if (tag != null) p("%s cleanup action ran on thread '%s': munmap(0x%x, %d) = %d", tag, Thread.currentThread().getName(), addr, len, rc);
        } catch (Throwable t) {
            throw new RuntimeException(t);
        }
    }

    static void variadicChecks() throws Throwable {
        p("");
        p("[a] --- variadic downcalls (os.arch=%s) ---", ARCH);
        // A variadic argument whose value we can see come back: F_DUPFD_CLOEXEC returns the lowest free fd >= arg.
        int dup = (int) FCNTL3.invokeExact(CAPSEG, dataFd, F_DUPFD_CLOEXEC, 100);
        p("[a] fcntl(data, F_DUPFD_CLOEXEC, 100) with firstVariadicArg(2) = %d  (>=100 expected: %b)", dup, dup >= 100);
        int cl = (int) CLOSE.invokeExact(CAPSEG, dup);
        int add = (int) FCNTL3.invokeExact(CAPSEG, ctlFd, F_ADD_SEALS, F_SEAL_WRITE);
        p("[a] fcntl(control, F_ADD_SEALS, F_SEAL_WRITE) = %d errno=%s  (EPERM expected: F_SEAL_SEAL is set)", add, en(errno()));

        int pid = (int) GETPID.invokeExact();
        long spid = (long) SYSCALL1.invokeExact(SYS_GETPID);
        long ssz = (long) SYSCALL4.invokeExact(SYS_LSEEK, (long) dataFd, 0L, (long) SEEK_END);
        p("[a] syscall(SYS_getpid=%d) with firstVariadicArg(1) = %d ; getpid() = %d ; equal=%b", SYS_GETPID, spid, pid, spid == pid);
        p("[a] syscall(SYS_lseek=%d, data, 0, SEEK_END) = %d ; equal to data_len=%b", SYS_LSEEK, ssz, ssz == dataLen);
        long wake = (long) FUTEX.invokeExact(SYS_FUTEX, DATA.address() + OFF_FUTEX, FUTEX_WAKE, 1L, 0L, 0L, 0L);
        p("[a] syscall(SYS_futex=%d, addr, FUTEX_WAKE, 1, 0, 0, 0) with no waiters = %d (0 expected)", SYS_FUTEX, wake);

        // For the record: the same two functions linked WITHOUT firstVariadicArg.
        try {
            MethodHandle f = dc("fcntl", FunctionDescriptor.of(JAVA_INT, JAVA_INT, JAVA_INT, JAVA_INT), CAP);
            int d2 = (int) f.invokeExact(CAPSEG, dataFd, F_DUPFD_CLOEXEC, 100);
            int c2 = (int) CLOSE.invokeExact(CAPSEG, d2);
            MethodHandle s = dc("syscall", FunctionDescriptor.of(JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG, JAVA_LONG));
            long z2 = (long) s.invokeExact(SYS_LSEEK, (long) dataFd, 0L, (long) SEEK_END);
            p("[a] linked WITHOUT firstVariadicArg (for the record): fcntl(F_DUPFD_CLOEXEC,100)=%d  syscall(SYS_lseek)=%d", d2, z2);
        } catch (Throwable t) {
            p("[a] linked WITHOUT firstVariadicArg: threw %s", t);
        }
        p("[a] nanoTime-to-CLOCK_MONOTONIC offset from calibration: %d ns", CLOCK_OFFSET);
    }

    // ------------------------------------------------------------------ (c) atomics
    static void atomics() throws Throwable {
        p("");
        p("[c] --- VarHandle access on MemorySegment ---");
        INT.setVolatile(DATA, OFF_CAS, 7);
        boolean c1 = INT.compareAndSet(DATA, OFF_CAS, 7, 8);
        boolean c2 = INT.compareAndSet(DATA, OFF_CAS, 7, 9);
        int v = (int) INT.getVolatile(DATA, OFF_CAS);
        p("[c] int word: setVolatile(7); compareAndSet(7->8)=%b; compareAndSet(7->9)=%b; getVolatile=%d", c1, c2, v);
        LONG.setRelease(DATA, OFF_CAS + 8, 0x1122334455667788L);
        long a = (long) LONG.getAcquire(DATA, OFF_CAS + 8);
        boolean c3 = LONG.compareAndSet(DATA, OFF_CAS + 8, 0x1122334455667788L, 42L);
        long o = (long) LONG.getOpaque(DATA, OFF_CAS + 8);
        long prev = (long) LONG.getAndAdd(DATA, OFF_CAS + 8, 8L);
        boolean w = LONG.weakCompareAndSet(DATA, OFF_CAS + 8, 50L, 51L);
        p("[c] long word: setRelease/getAcquire=0x%x; compareAndSet->42=%b; getOpaque=%d; getAndAdd(8) returned %d; weakCompareAndSet(50->51)=%b; now %d",
                a, c3, o, prev, w, (long) LONG.getVolatile(DATA, OFF_CAS + 8));

        // Contended increments: lost updates would show as a short count.
        INT.setVolatile(DATA, OFF_CAS + 16, 0);
        int threads = 4, per = 1_000_000;
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ts[i] = Thread.ofPlatform().start(() -> {
                for (int k = 0; k < per; k++) {
                    int cur;
                    do {
                        cur = (int) INT.getAcquire(DATA, OFF_CAS + 16);
                    } while (!INT.compareAndSet(DATA, OFF_CAS + 16, cur, cur + 1));
                }
            });
        }
        for (Thread t : ts) t.join();
        p("[c] %d threads x %d compareAndSet increments on one word: final=%d (want %d)", threads, per,
                (int) INT.getVolatile(DATA, OFF_CAS + 16), threads * per);

        // The control segment: readable, updated by the helper through its pre-seal mapping, never writable here.
        long g1 = (long) LONG.getAcquire(CTL, CTL_GATE);
        helperSend(CMD_STORE_CTL, CTL_GATE, 0x2222, 0, 0);
        helperRecv();
        long g2 = (long) LONG.getAcquire(CTL, CTL_GATE);
        p("[c] control word read-only here: 0x%x, then after the helper stores through its own mapping: 0x%x", g1, g2);
        try {
            LONG.setVolatile(CTL, CTL_GATE, 1L);
            p("[c] write to asReadOnly() control segment: NO EXCEPTION (unexpected)");
        } catch (Throwable t) {
            p("[c] write to asReadOnly() control segment threw: %s", t);
        }
        try {
            long x = (long) LONG.getVolatile(DATA, dataLen - 4);
            p("[c] out-of-bounds read: NO EXCEPTION (unexpected)");
        } catch (Throwable t) {
            p("[c] read 8 bytes at data_len-4 threw: %s", t);
        }
        try {
            long x = (long) LONG.getVolatile(DATA, OFF_CAS + 4);
            p("[c] misaligned long read: NO EXCEPTION");
        } catch (Throwable t) {
            p("[c] long access at a misaligned offset threw: %s", t);
        }
    }

    // ------------------------------------------------------------------ (d) futex
    static void futexTests() throws Throwable {
        p("");
        p("[d] --- futex through syscall() downcall: FUTEX_WAIT_BITSET, shared, absolute CLOCK_MONOTONIC deadline ---");
        long addr = DATA.address() + OFF_FUTEX;

        // 1. woken by the helper process
        INT.setVolatile(DATA, OFF_FUTEX, 0);
        helperSend(CMD_STORE_WAKE, OFF_FUTEX, 1, 1, 200);
        long t0 = System.nanoTime();
        long r = futexWaitErrno(CAPSEG, addr, 0, monoNow(TS) + 5_000_000_000L, TS);
        int e = errno();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        int woken = helperRecv();
        p("[d] wait(expected 0, deadline +5000 ms), helper stores 1 and wakes after 200 ms: returned %d errno=%s after %d ms; word=%d; helper's FUTEX_WAKE reported %d woken",
                r, r == 0 ? "-" : en(e), ms, (int) INT.getVolatile(DATA, OFF_FUTEX), woken);

        // 1b. the same with the request-path handle (no errno capture)
        INT.setVolatile(DATA, OFF_FUTEX, 0);
        helperSend(CMD_STORE_WAKE, OFF_FUTEX, 1, 1, 200);
        t0 = System.nanoTime();
        r = futexWait(addr, 0, System.nanoTime() + CLOCK_OFFSET + 5_000_000_000L, TS);
        ms = (System.nanoTime() - t0) / 1_000_000;
        woken = helperRecv();
        p("[d] same, handle without errno capture, deadline from nanoTime()+offset: returned %d after %d ms; helper reported %d woken", r, ms, woken);

        // 2. timeout
        INT.setVolatile(DATA, OFF_FUTEX, 0);
        t0 = System.nanoTime();
        r = futexWaitErrno(CAPSEG, addr, 0, monoNow(TS) + 300_000_000L, TS);
        e = errno();
        long us = (System.nanoTime() - t0) / 1000;
        p("[d] wait(deadline +300 ms), nobody wakes: returned %d errno=%s after %d us", r, en(e), us);

        // 3. value mismatch
        t0 = System.nanoTime();
        r = futexWaitErrno(CAPSEG, addr, 12345, monoNow(TS) + 5_000_000_000L, TS);
        e = errno();
        p("[d] wait with a stale expected value: returned %d errno=%s after %d us", r, en(e), (System.nanoTime() - t0) / 1000);

        // 4. deadline already in the past
        t0 = System.nanoTime();
        r = futexWaitErrno(CAPSEG, addr, 0, monoNow(TS) - 1_000_000L, TS);
        e = errno();
        p("[d] wait with a deadline 1 ms in the past: returned %d errno=%s after %d us", r, en(e), (System.nanoTime() - t0) / 1000);

        // 5. Java wakes Java: a platform thread parks, main wakes it with FUTEX_WAKE through the same downcall.
        INT.setVolatile(DATA, OFF_FUTEX, 0);
        MemorySegment ts2 = G.allocate(16, 8);
        long[] res = new long[2];
        Thread t = Thread.ofPlatform().start(() -> {
            try {
                long s = System.nanoTime();
                res[0] = futexWait(addr, 0, System.nanoTime() + CLOCK_OFFSET + 5_000_000_000L, ts2);
                res[1] = (System.nanoTime() - s) / 1_000_000;
            } catch (Throwable x) {
                x.printStackTrace();
            }
        });
        Thread.sleep(150);
        INT.setVolatile(DATA, OFF_FUTEX, 1);
        long nw = (long) FUTEX.invokeExact(SYS_FUTEX, addr, FUTEX_WAKE, 1L, 0L, 0L, 0L);
        t.join();
        p("[d] in-process: FUTEX_WAKE from main returned %d; waiter returned %d after %d ms", nw, res[0], res[1]);
    }

    // ------------------------------------------------------------------ (e) allocation on the call path
    static final com.sun.management.ThreadMXBean TMX =
            (com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean();
    static volatile Object SINK;
    static long[] carrierIds = new long[0];
    static boolean onVirtual;

    /** What one call does without a system call: clock, loads, a CAS, a plain store, a release store, a volatile store and load. */
    static long opCore(MemorySegment ctl, MemorySegment data, long i) {
        long now = System.nanoTime() + CLOCK_OFFSET;
        long gate = (long) LONG.getAcquire(ctl, CTL_GATE);          // gate
        long hb = (long) LONG.getAcquire(ctl, CTL_HB);              // supervisor heartbeat
        int slot = (int) INT.getAcquire(data, D_SLOT);              // slot word
        boolean won = INT.compareAndSet(data, D_SLOT, slot, slot + 1);   // claim
        LONG.set(data, D_STAMP, now);                               // plain field write before publish
        INT.setRelease(data, D_STATE, (int) i);                     // publish READY
        LONG.setVolatile(data, D_CELL, i);                          // descriptor cell publish ...
        int cw = (int) INT.getVolatile(data, D_CWAIT);              // ... then check consumer_wait
        long status = (long) LONG.getOpaque(data, D_STATUS);        // a field another process wrote
        return gate + hb + slot + cw + status + now + (won ? 1 : 0);
    }

    static long loopEmpty(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += i ^ (acc >>> 3);
        return acc;
    }

    static long loopCore(long n, MemorySegment ctl, MemorySegment data) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += opCore(ctl, data, i);
        return acc;
    }

    static long loopWake(long n, MemorySegment ctl, MemorySegment data) throws Throwable {
        long acc = 0;
        long cw = data.address() + D_CWAIT;
        for (long i = 0; i < n; i++) {
            acc += opCore(ctl, data, i);
            acc += (long) FUTEX.invokeExact(SYS_FUTEX, cw, FUTEX_WAKE, 1L, 0L, 0L, 0L);
        }
        return acc;
    }

    /** The wait call itself, made to return at once (stale expected value), with the timespec written each time. */
    static long loopWait(long n, MemorySegment ctl, MemorySegment data, MemorySegment ts) throws Throwable {
        long acc = 0;
        long cw = data.address() + D_CWAIT;
        for (long i = 0; i < n; i++) {
            acc += opCore(ctl, data, i);
            long deadline = System.nanoTime() + CLOCK_OFFSET + 1_000_000L;
            acc += futexWait(cw, 0x7fffffff, deadline, ts);
        }
        return acc;
    }

    static long loopWakeErrno(long n, MemorySegment ctl, MemorySegment data, MemorySegment cap) throws Throwable {
        long acc = 0;
        long cw = data.address() + D_CWAIT;
        for (long i = 0; i < n; i++) {
            acc += opCore(ctl, data, i);
            acc += (long) FUTEX_ERRNO.invokeExact(cap, SYS_FUTEX, cw, FUTEX_WAKE, 1L, 0L, 0L, 0L);
            acc += (int) ERRNO_VH.get(cap, 0L);
        }
        return acc;
    }

    /** Forbidden by the design: a slice per call. Shows what escape analysis does and does not save. */
    static long loopSlice(long n, MemorySegment ctl, MemorySegment data) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += opCore(ctl, data, i);
            acc += data.asSlice(D_STATUS, 8).get(JAVA_LONG, 0);
        }
        return acc;
    }

    /** Positive control: one boxed Long per iteration that escapes. Proves the meter reads non-zero. */
    static long loopBox(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            Long b = Long.valueOf(i + 1_000_000);
            SINK = b;
            acc += b;
        }
        return acc;
    }

    static long runVariant(int variant, long n, MemorySegment ts, MemorySegment cap) throws Throwable {
        return switch (variant) {
            case 0 -> loopEmpty(n);
            case 1 -> loopCore(n, CTL, DATA);
            case 2 -> loopWake(n, CTL, DATA);
            case 3 -> loopWait(n, CTL, DATA, ts);
            case 4 -> loopWakeErrno(n, CTL, DATA, cap);
            case 5 -> loopSlice(n, CTL, DATA);
            default -> loopBox(n);
        };
    }

    static final String[] VARIANT_NAMES = {
            "0 empty loop (meter baseline)",
            "1 core: nanoTime, 4 loads, CAS, plain/release/volatile stores",
            "2 core + futex WAKE downcall (no errno)",
            "3 core + timespec write + futex WAIT_BITSET downcall (returns EAGAIN at once)",
            "4 core + futex WAKE downcall with captureCallState(errno)",
            "5 core + asSlice per call (design forbids this)",
            "6 positive control: boxed Long escapes each iteration",
    };

    static long allocated() {
        if (!onVirtual) return TMX.getCurrentThreadAllocatedBytes();
        long s = 0;
        for (int i = 0; i < carrierIds.length; i++) s += TMX.getThreadAllocatedBytes(carrierIds[i]);
        return s;
    }

    static void measureAll(String who) throws Throwable {
        MemorySegment ts = G.allocate(16, 8), cap = G.allocate(CAPTURE_LAYOUT);
        long n = Long.getLong("s3.iters", 1_000_000L);
        long warm = Long.getLong("s3.warm", 200_000L);
        p("");
        p("[e] --- allocation on the call path, measured on %s; %d iterations per round after 8 x %d warm-up ---", who, n, warm);
        long blackhole = 0;
        for (int variant = 0; variant < VARIANT_NAMES.length; variant++) {
            // cold: the very first iterations, interpreter and linkage included
            long c0 = allocated();
            blackhole += runVariant(variant, 2_000, ts, cap);
            long c1 = allocated();
            // warm-up
            for (int r = 0; r < 6; r++) blackhole += runVariant(variant, warm, ts, cap);
            Thread.sleep(300);   // let queued compilations land
            for (int r = 0; r < 2; r++) blackhole += runVariant(variant, warm, ts, cap);
            p("[e] %s", VARIANT_NAMES[variant]);
            p("[e]     cold first 2000 iterations: %d B total (%.1f B/op)", c1 - c0, (c1 - c0) / 2000.0);
            for (int r = 1; r <= 3; r++) {
                long a0 = allocated();
                long t0 = System.nanoTime();
                blackhole += runVariant(variant, n, ts, cap);
                long t1 = System.nanoTime();
                long a1 = allocated();
                p("[e]     round %d: %d B allocated over %d ops = %.6f B/op   (%.1f ns/op, VM timing)",
                        r, a1 - a0, n, (a1 - a0) / (double) n, (t1 - t0) / (double) n);
            }
        }
        p("[e] (blackhole %d)", blackhole);
    }

    static List<Thread> carriers() {
        List<Thread> out = new ArrayList<>();
        for (Thread t : Thread.getAllStackTraces().keySet())
            if (t.getClass().getName().equals("jdk.internal.misc.CarrierThread")) out.add(t);
        return out;
    }

    static void alloc() throws Throwable {
        p("");
        p("[e] java.vm.version=%s  vm args=%s", System.getProperty("java.vm.version"),
                ManagementFactory.getRuntimeMXBean().getInputArguments());
        p("[e] compiler: %s   collectors: %s",
                ManagementFactory.getCompilationMXBean() == null ? "none (interpreter only)" : ManagementFactory.getCompilationMXBean().getName(),
                ManagementFactory.getGarbageCollectorMXBeans().stream().map(GarbageCollectorMXBean::getName).toList());
        p("[e] thread allocated-bytes metering supported=%b enabled=%b", TMX.isThreadAllocatedMemorySupported(),
                TMX.isThreadAllocatedMemoryEnabled());
        onVirtual = false;
        measureAll("a platform thread (getCurrentThreadAllocatedBytes)");

        // Virtual thread: ThreadMXBean does not meter virtual threads, so sum the carriers' counters instead.
        Thread.ofVirtual().start(() -> { }).join();
        List<Thread> cs = carriers();
        carrierIds = cs.stream().mapToLong(Thread::threadId).toArray();
        p("");
        p("[e] virtual-thread run: %d carrier thread(s) %s; allocation read as the sum of the carriers' counters",
                cs.size(), cs.stream().map(Thread::getName).toList());
        Thread vt = Thread.ofVirtual().name("alloc-vt").start(() -> {
            try {
                p("[e] inside virtual thread: isVirtual=%b getCurrentThreadAllocatedBytes()=%d", Thread.currentThread().isVirtual(),
                        TMX.getCurrentThreadAllocatedBytes());
                onVirtual = true;
                measureAll("a virtual thread (sum over carriers)");
            } catch (Throwable t) {
                t.printStackTrace(System.out);
            }
        });
        vt.join();
        onVirtual = false;
        p("[e] carriers after the virtual-thread run: %d", carriers().size());
    }

    // ------------------------------------------------------------------ (f) arena closed under a parked thread
    static void arenaCase(String tag, boolean passSegment, boolean helperWakes, boolean capture) throws Throwable {
        p("");
        p("%s --- parked thread: %s; errno capture: %b; after close: %s ---", tag,
                passSegment ? "segment passed to the downcall as ADDRESS" : "address passed as a plain long (the design)",
                capture, helperWakes ? "helper issues FUTEX_WAKE on the same file offset" : "nobody wakes, deadline +3000 ms");
        Arena arena = Arena.ofShared();
        final long len = dataLen;
        long addr = (long) MMAP.invokeExact(CAPSEG, 0L, len, PROT_READ | PROT_WRITE, MAP_SHARED, dataFd, 0L);
        check(addr != -1L, "mmap for arena test");
        MemorySegment seg = MemorySegment.ofAddress(addr).reinterpret(len, arena, s -> unmap(s.address(), len, tag));
        INT.setVolatile(seg, OFF_ARENA, 0);
        MemorySegment ts = G.allocate(16, 8), cap = G.allocate(CAPTURE_LAYOUT);
        long word = addr + OFF_ARENA;
        long base = System.nanoTime();
        Thread t = Thread.ofPlatform().name("parked").start(() -> {
            try {
                long deadline = System.nanoTime() + CLOCK_OFFSET + 3_000_000_000L;
                long r;
                String err = "(not captured)";
                if (passSegment) {
                    ts.set(JAVA_LONG, 0, deadline / 1_000_000_000L);
                    ts.set(JAVA_LONG, 8, deadline % 1_000_000_000L);
                    r = (long) FUTEX_SEGARG.invokeExact(SYS_FUTEX, seg.asSlice(OFF_ARENA, 4), FUTEX_WAIT_BITSET, 0L,
                            ts.address(), 0L, FUTEX_BITSET_MATCH_ANY);
                } else if (capture) {
                    r = futexWaitErrno(cap, word, 0, deadline, ts);
                    err = r == 0 ? "-" : en(errno(cap));
                } else {
                    r = futexWait(word, 0, deadline, ts);
                }
                p("%s parked thread: futex returned %d errno=%s at t=%d ms", tag, r, err, (System.nanoTime() - base) / 1_000_000);
                try {
                    int v = (int) INT.getVolatile(seg, OFF_ARENA);
                    p("%s parked thread then read the segment: OK, value %d", tag, v);
                } catch (Throwable x) {
                    p("%s parked thread then read the segment: threw %s", tag, x);
                }
                try {
                    INT.setVolatile(seg, OFF_ARENA, 5);
                    p("%s parked thread then wrote the segment: OK", tag);
                } catch (Throwable x) {
                    p("%s parked thread then wrote the segment: threw %s", tag, x);
                }
            } catch (Throwable x) {
                p("%s parked thread died: %s", tag, x);
            }
        });
        Thread.sleep(500);
        p("%s t=%d ms: parked thread state=%s alive=%b; mappings of memfd:scz-data in /proc/self/maps: %d", tag,
                (System.nanoTime() - base) / 1_000_000, t.getState(), t.isAlive(), mapsCount("memfd:scz-data"));
        long c0 = System.nanoTime();
        boolean closed = false;
        try {
            arena.close();
            closed = true;
            p("%s Arena.close() returned normally after %d us", tag, (System.nanoTime() - c0) / 1000);
        } catch (Throwable x) {
            p("%s Arena.close() threw after %d us: %s", tag, (System.nanoTime() - c0) / 1000, x);
        }
        p("%s after close: seg.scope().isAlive()=%b; mappings of memfd:scz-data: %d; parked thread alive=%b state=%s", tag,
                seg.scope().isAlive(), mapsCount("memfd:scz-data"), t.isAlive(), t.getState());
        if (closed) {
            // A late caller that had already computed the raw address and only now reaches the syscall.
            long r = futexWaitErrno(CAPSEG, word, 0, monoNow(TS) + 100_000_000L, TS);
            p("%s a NEW futex wait on the now-unmapped address returned %d errno=%s", tag, r, en(errno()));
        }
        if (helperWakes) {
            Thread.sleep(300);
            helperSend(CMD_STORE_WAKE, OFF_ARENA, 1, 1, 0);
            int woken = helperRecv();
            p("%s t=%d ms: helper stored 1 and called FUTEX_WAKE on its own mapping of that offset: %d woken", tag,
                    (System.nanoTime() - base) / 1_000_000, woken);
        }
        t.join(10_000);
        p("%s parked thread finished=%b at t=%d ms", tag, !t.isAlive(), (System.nanoTime() - base) / 1_000_000);
        if (!closed) {
            c0 = System.nanoTime();
            try {
                arena.close();
                p("%s second Arena.close(), after the call returned: ok after %d us; mappings of memfd:scz-data: %d", tag,
                        (System.nanoTime() - c0) / 1000, mapsCount("memfd:scz-data"));
            } catch (Throwable x) {
                p("%s second Arena.close() threw: %s", tag, x);
            }
        }
    }

    static void arenaTests() throws Throwable {
        p("");
        p("[f] --- a platform thread parked in futex on an address inside a shared Arena that another thread closes ---");
        p("[f] mappings of memfd:scz-data before the tests (the connection's own): %d", mapsCount("memfd:scz-data"));
        arenaCase("[f1]", false, false, false);
        arenaCase("[f2]", false, false, true);
        arenaCase("[f3]", false, true, false);
        arenaCase("[f4]", true, false, false);
        p("");
        p("[f] JVM still running after all four cases; connection's own data segment still readable: magic ok=%b",
                (long) LONG.getAcquire(DATA, 0L) == MAGIC);
    }

    // ------------------------------------------------------------------ (g) carriers pinned in futex
    static final AtomicLong TICKS = new AtomicLong(), YIELDS = new AtomicLong();

    static volatile boolean yielderStop;

    static void pinScenario(String name, int k, long parkMs, boolean helperWakesAt500, boolean extras, boolean withYielder)
            throws Throwable {
        p("");
        p("[g] scenario: %s — %d virtual thread(s) each parked in futex for up to %d ms%s%s", name, k, parkMs,
                helperWakesAt500 ? "; helper wakes all at ~500 ms" : "",
                withYielder ? "; plus one unrelated virtual thread that loops on Thread.yield()" : "");
        Thread yielder = null;
        if (withYielder) {
            yielderStop = false;
            yielder = Thread.ofVirtual().name("yielder").start(() -> {
                while (!yielderStop) {
                    YIELDS.incrementAndGet();
                    Thread.yield();
                }
            });
            Thread.sleep(100);
        }
        AtomicInteger inFutex = new AtomicInteger(), done = new AtomicInteger();
        long[] startAt = new long[k], endAt = new long[k], rc = new long[k];
        MemorySegment[] tsb = new MemorySegment[k];
        for (int i = 0; i < k; i++) tsb[i] = G.allocate(16, 8);
        INT.setVolatile(DATA, OFF_PIN, 0);
        long addr = DATA.address() + OFF_PIN;
        long base = System.nanoTime();
        Thread[] vts = new Thread[k];
        for (int i = 0; i < k; i++) {
            final int id = i;
            vts[i] = Thread.ofVirtual().name("parker-" + i).start(() -> {
                try {
                    startAt[id] = (System.nanoTime() - base) / 1_000_000;
                    inFutex.incrementAndGet();
                    long r = futexWait(addr, 0, System.nanoTime() + CLOCK_OFFSET + parkMs * 1_000_000L, tsb[id]);
                    inFutex.decrementAndGet();
                    endAt[id] = (System.nanoTime() - base) / 1_000_000;
                    rc[id] = r;
                } catch (Throwable x) {
                    x.printStackTrace(System.out);
                } finally {
                    done.incrementAndGet();
                }
            });
        }
        long lastTicks = TICKS.get(), lastYields = YIELDS.get();
        long[] lateRanAt = {-1};
        boolean didExtras = false, didWake = false;
        long quietUntil = -1;
        for (int s = 1; s <= 80; s++) {
            long target = base + s * 250_000_000L;
            long sl = (target - System.nanoTime()) / 1_000_000;
            if (sl > 0) Thread.sleep(sl);
            long tk = TICKS.get(), y = YIELDS.get();
            p("[g]   t=%5d ms  in-futex=%2d done=%2d/%d  ticker +%3d%s  carrier threads=%d",
                    (System.nanoTime() - base) / 1_000_000, inFutex.get(), done.get(), k, tk - lastTicks,
                    withYielder ? String.format("  yielder +%8d", y - lastYields) : "", carriers().size());
            lastTicks = tk;
            lastYields = y;
            if (extras && !didExtras && s == 2) {
                didExtras = true;
                long g0 = System.nanoTime();
                System.gc();
                p("[g]     while parked: System.gc() from a platform thread took %d ms", (System.nanoTime() - g0) / 1_000_000);
                long l0 = System.nanoTime();
                Thread.ofVirtual().name("late").start(() -> lateRanAt[0] = (System.nanoTime() - l0) / 1_000_000);
                p("[g]     while parked: started a new virtual thread 'late' at t=%d ms", (l0 - base) / 1_000_000);
                try {
                    Path dump = Path.of(outDir, "05-pinning-threaddump.txt");
                    Files.deleteIfExists(dump);
                    ManagementFactory.getPlatformMXBean(com.sun.management.HotSpotDiagnosticMXBean.class)
                            .dumpThreads(dump.toString(), com.sun.management.HotSpotDiagnosticMXBean.ThreadDumpFormat.TEXT_PLAIN);
                    p("[g]     while parked: thread dump written to %s", dump);
                } catch (Throwable x) {
                    p("[g]     thread dump failed: %s", x);
                }
            }
            if (helperWakesAt500 && !didWake && s == 2) {
                didWake = true;
                helperSend(CMD_STORE_WAKE, OFF_PIN, 1, 1000, 0);
                p("[g]     helper stored 1 and called FUTEX_WAKE(1000): %d woken", helperRecv());
            }
            if (done.get() == k) {
                if (quietUntil < 0) quietUntil = s + 2;      // two more samples to show recovery
                else if (s >= quietUntil) break;
            }
        }
        for (Thread t : vts) t.join();
        if (yielder != null) {
            yielderStop = true;
            yielder.join();
        }
        if (k > 0) {
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < k; i++) sb.append(String.format("%s[%d..%d ms rc=%d]", i == 0 ? "" : " ", startAt[i], endAt[i], rc[i]));
            p("[g]   parker entered..returned: %s", sb);
        }
        if (extras) p("[g]   virtual thread 'late' first ran %d ms after it was started", lateRanAt[0]);
    }

    static void sleepInUpcall() {
        try {
            Thread.sleep(100);
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    static int sleepyCompare(MemorySegment a, MemorySegment b) {
        sleepInUpcall();
        return 0;
    }

    static void pinTests() throws Throwable {
        int par = Integer.getInteger("jdk.virtualThreadScheduler.parallelism", Runtime.getRuntime().availableProcessors());
        p("");
        p("[g] --- virtual threads parked in the futex downcall ---");
        p("[g] availableProcessors=%d  scheduler parallelism=%d  jdk.virtualThreadScheduler.maxPoolSize=%s  jdk.tracePinnedThreads=%s",
                Runtime.getRuntime().availableProcessors(), par, System.getProperty("jdk.virtualThreadScheduler.maxPoolSize", "(default 256)"),
                System.getProperty("jdk.tracePinnedThreads", "(unset)"));

        jdk.jfr.Recording rec = new jdk.jfr.Recording();
        rec.enable("jdk.VirtualThreadPinned").withThreshold(Duration.ZERO).withStackTrace();
        rec.enable("jdk.VirtualThreadSubmitFailed");
        rec.start();

        // Unrelated work: one virtual thread that sleeps 5 ms per tick.
        Thread.ofVirtual().name("ticker").start(() -> {
            try {
                while (true) {
                    TICKS.incrementAndGet();
                    Thread.sleep(5);
                }
            } catch (InterruptedException e) { /* exit */ }
        });

        pinScenario("baseline, nothing parked", 0, 0, false, false, false);
        pinScenario("carriers-1 parked (the design's cap)", par - 1, 2000, false, false, false);
        pinScenario("every carrier parked", par, 2000, false, true, false);
        pinScenario("3x carriers parked", 3 * par, 1000, false, false, false);
        pinScenario("every carrier parked, then woken from the helper process", par, 5000, true, false, false);
        // Side observations about the one carrier the cap leaves free.
        pinScenario("nothing parked, with a yield-looping thread", 0, 0, false, false, true);
        pinScenario("carriers-1 parked, with a yield-looping thread on the free carrier", par - 1, 2000, false, false, true);

        // Controls for the diagnostics: cases where the JDK itself parks a pinned virtual thread.
        p("");
        p("[g] controls for pinning diagnostics (stdout lines from -Djdk.tracePinnedThreads, if any, appear here):");
        Object lock = new Object();
        Thread c1 = Thread.ofVirtual().name("control-synchronized-sleep").start(() -> {
            synchronized (lock) {
                try {
                    Thread.sleep(100);
                } catch (InterruptedException e) { /* ignore */ }
            }
        });
        c1.join();
        MethodHandle cmp = MethodHandles.lookup().findStatic(S3.class, "sleepyCompare",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        MemorySegment stub = LINKER.upcallStub(cmp, FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS), G);
        MethodHandle qsort = dc("qsort", FunctionDescriptor.ofVoid(ADDRESS, JAVA_LONG, JAVA_LONG, ADDRESS));
        MemorySegment arr = G.allocate(8, 4);
        Thread c2 = Thread.ofVirtual().name("control-sleep-inside-upcall").start(() -> {
            try {
                qsort.invokeExact(arr, 2L, 4L, stub);
            } catch (Throwable x) {
                x.printStackTrace(System.out);
            }
        });
        c2.join();
        Thread.sleep(200);

        rec.stop();
        Path jfr = Path.of(outDir, "05-pinning.jfr");
        Files.deleteIfExists(jfr);
        rec.dump(jfr);
        rec.close();
        List<jdk.jfr.consumer.RecordedEvent> evs = jdk.jfr.consumer.RecordingFile.readAllEvents(jfr);
        Files.deleteIfExists(jfr);
        p("");
        p("[g] JFR recording over the whole test (jdk.VirtualThreadPinned threshold 0, jdk.VirtualThreadSubmitFailed): %d event(s)", evs.size());
        for (jdk.jfr.consumer.RecordedEvent ev : evs) {
            String th = ev.getThread() == null ? "?" : ev.getThread().getJavaName();
            StringBuilder extra = new StringBuilder();
            for (String f : new String[]{"pinnedReason", "blockingOperation"})
                if (ev.hasField(f)) extra.append(' ').append(f).append("=\"").append(ev.getString(f)).append('"');
            String top = ev.getStackTrace() == null || ev.getStackTrace().getFrames().isEmpty() ? "" :
                    " top frame=" + ev.getStackTrace().getFrames().get(0).getMethod().getType().getName() + "."
                            + ev.getStackTrace().getFrames().get(0).getMethod().getName();
            p("[g]   %s thread=%s duration=%d ms%s%s", ev.getEventType().getName(), th, ev.getDuration().toMillis(), extra, top);
        }
        long fromParkers = evs.stream().filter(ev -> ev.getThread() != null && ev.getThread().getJavaName() != null
                && ev.getThread().getJavaName().startsWith("parker-")).count();
        long longOnes = evs.stream().filter(ev -> ev.getThread() != null && ev.getThread().getJavaName() != null
                && ev.getThread().getJavaName().startsWith("parker-") && ev.getDuration().toMillis() >= 500).count();
        p("[g] events on the futex-parked virtual threads (parker-*): %d in total; %d lasting >= 500 ms, i.e. reporting the futex block itself",
                fromParkers, longOnes);
    }

    // ------------------------------------------------------------------ (h) critical + captureCallState
    static void criticalTests() throws Throwable {
        p("");
        p("[h] --- Linker.Option.critical(...) together with captureCallState(\"errno\") ---");
        FunctionDescriptor closeFd = FunctionDescriptor.of(JAVA_INT, JAVA_INT);
        for (boolean heap : new boolean[]{false, true}) {
            try {
                MethodHandle h = LINKER.downcallHandle(sym("close"), closeFd, Linker.Option.critical(heap), CAP);
                p("[h] critical(%b) + captureCallState(\"errno\"): linked, type %s", heap, h.type());
                ERRNO_VH.set(CAPSEG, 0L, 0);
                int r = (int) h.invokeExact(CAPSEG, -1);
                p("[h]     close(-1) through it returned %d, captured errno=%s (EBADF expected)", r, en(errno()));
            } catch (Throwable t) {
                p("[h] critical(%b) + captureCallState(\"errno\"): threw %s", heap, t);
            }
        }
        try {
            MethodHandle h = LINKER.downcallHandle(sym("close"), closeFd, Linker.Option.critical(false));
            int r = (int) h.invokeExact(-1);
            p("[h] critical(false) alone: linked, close(-1) returned %d", r);
        } catch (Throwable t) {
            p("[h] critical(false) alone: threw %s", t);
        }
        try {
            MethodHandle h = LINKER.downcallHandle(sym("syscall"), FunctionDescriptor.of(JAVA_LONG, JAVA_LONG),
                    Linker.Option.critical(false), Linker.Option.firstVariadicArg(1));
            long r = (long) h.invokeExact(SYS_GETPID);
            p("[h] critical(false) + firstVariadicArg(1): linked, syscall(SYS_getpid) returned %d (getpid()=%d)", r, (int) GETPID.invokeExact());
        } catch (Throwable t) {
            p("[h] critical(false) + firstVariadicArg(1): threw %s", t);
        }

        // The design's reason for not using critical: the thread cannot reach a safepoint inside the call.
        p("");
        p("[h] --- a thread blocked 1500 ms in usleep(); another thread calls System.gc() 200 ms in ---");
        FunctionDescriptor usleepFd = FunctionDescriptor.of(JAVA_INT, JAVA_INT);
        MethodHandle normal = LINKER.downcallHandle(sym("usleep"), usleepFd);
        MethodHandle critical = LINKER.downcallHandle(sym("usleep"), usleepFd, Linker.Option.critical(false));
        for (int round = 0; round < 2; round++) {
            MethodHandle h = round == 0 ? normal : critical;
            Thread t = Thread.ofPlatform().start(() -> {
                try {
                    int r = (int) h.invokeExact(1_500_000);
                } catch (Throwable x) {
                    x.printStackTrace(System.out);
                }
            });
            Thread.sleep(200);
            long g0 = System.nanoTime();
            System.gc();
            long ms = (System.nanoTime() - g0) / 1_000_000;
            t.join();
            p("[h] %s downcall: System.gc() took %d ms", round == 0 ? "ordinary" : "critical(false)", ms);
        }
    }

    // ------------------------------------------------------------------ main
    public static void main(String[] args) throws Throwable {
        String path = args[0];
        List<String> modes = List.of(args).subList(1, args.length);
        p("S3 spike: java.version=%s vm=%s os.arch=%s os.version=%s cpus=%d nativeAccessEnabled=%b pid=%d",
                System.getProperty("java.version"), System.getProperty("java.vm.name"), ARCH, System.getProperty("os.version"),
                Runtime.getRuntime().availableProcessors(), S3.class.getModule().isNativeAccessEnabled(), (int) GETPID.invokeExact());
        p("modes=%s", modes);
        boolean verbose = modes.contains("handshake");
        connect(path, verbose);
        if (verbose) variadicChecks();
        for (String m : modes) {
            switch (m) {
                case "handshake" -> { }
                case "atomics" -> atomics();
                case "futex" -> futexTests();
                case "alloc" -> alloc();
                case "arena" -> arenaTests();
                case "pin" -> pinTests();
                case "critical" -> criticalTests();
                default -> throw new IllegalArgumentException("unknown mode " + m);
            }
        }
        p("");
        p("S3 spike: modes %s completed", modes);
        System.exit(0);   // the ticker virtual thread never ends
    }
}

/** Entry point that looks at the native-access flag BEFORE any restricted method has been called. */
final class Probe {
    public static void main(String[] args) throws Throwable {
        System.out.println("Probe: before any restricted call, Module.isNativeAccessEnabled() = "
                + Probe.class.getModule().isNativeAccessEnabled());
        System.out.flush();
        S3.main(args);
    }
}
