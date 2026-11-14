package io.sidecarzero;

import io.sidecarzero.internal.Native;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * Test hooks for the conformance agent (testing §3, "Agent protocol"). Not part of the API.
 */
public final class Hooks {
    private Hooks() {}

    /** Thrown by the call {@link #armUnwind} armed, between claiming a ring cell and publishing it. */
    public static final class Unwound extends Error {
        Unwound() {
            super("sidecarzero test hook: unwinding between claiming a ring cell and publishing it", null, false, false);
        }
    }

    static final Unwound UNWOUND = new Unwound();
    static volatile boolean unwind;
    private static final VarHandle UNWIND;

    static {
        try {
            UNWIND = MethodHandles.lookup().findStaticVarHandle(Hooks.class, "unwind", boolean.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /** UNWIND: the next call, on any thread, unwinds between claiming a ring cell and publishing it. */
    public static void armUnwind() {
        unwind = true;
    }

    static boolean takeUnwind() {
        return (boolean) UNWIND.compareAndSet(true, false);
    }

    /** The process-wide spin ceiling fixed at {@code ns} (0: never spin), or adaptive with -1. */
    public static void fixSpinCeiling(long ns) {
        Engine.spinFixed = ns;
    }

    /** True when the JVM was started with {@code -Dscz.crashpoints=true} (testing §2.4). */
    public static final boolean CRASHPOINTS = Boolean.getBoolean("scz.crashpoints");
    private static final String CRASH_AT = CRASHPOINTS ? System.getenv("SCZ_CRASH_AT") : null;

    /** Kills the process with SIGKILL when crashpoints are enabled and {@code SCZ_CRASH_AT} equals {@code tag}. */
    public static void crashPoint(String tag) {
        if (CRASHPOINTS && tag.equals(CRASH_AT)) {
            Native.killSelf();
        }
    }
}
