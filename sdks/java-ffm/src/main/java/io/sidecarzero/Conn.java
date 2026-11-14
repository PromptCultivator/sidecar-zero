package io.sidecarzero;

import io.sidecarzero.internal.Directory;
import io.sidecarzero.internal.Native;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;

/** One session: the socket, the two mappings (tied to one shared Arena), and the geometry. */
final class Conn {
    final int sock;
    final Arena arena;
    final MemorySegment ctl, data;
    /** The data segment's base address, for futex calls (sdk-java §2: addresses as numbers). */
    final long dataAddr;
    final Directory dir;
    final long sessionId;
    /** The ids of this session's snapshots: retirement waits for markers in this range. */
    volatile long firstSnap, lastSnap;

    Conn(int sock, Arena arena, MemorySegment ctl, MemorySegment data, Directory dir, long sessionId) {
        this.sock = sock;
        this.arena = arena;
        this.ctl = ctl;
        this.data = data;
        this.dataAddr = data.address();
        this.dir = dir;
        this.sessionId = sessionId;
    }

    /** Unmap (the Arena's cleanups munmap both segments), then close the socket: last. */
    void close() {
        try {
            arena.close();
        } catch (RuntimeException e) {
            System.err.println("sidecarzero: closing an arena failed: " + e);
        }
        Native.close(sock);
    }
}
