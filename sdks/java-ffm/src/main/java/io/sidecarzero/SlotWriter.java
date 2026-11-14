package io.sidecarzero;

import java.lang.foreign.MemorySegment;

/**
 * Writes a request's opaque bytes into its slot's request region, bounds-checked against the
 * region. Valid only inside {@link Codec#encode}. A write past the region throws
 * {@link CodecException#OVERFLOW}.
 */
public final class SlotWriter {
    private MemorySegment seg;
    private long off;
    private int cap;
    private int len;

    SlotWriter() {}

    void reset(MemorySegment seg, long off, long cap) {
        this.seg = seg;
        this.off = off;
        this.cap = (int) Math.min(cap, Integer.MAX_VALUE);
        this.len = 0;
    }

    void clear() {
        seg = null;
    }

    public int capacity() {
        return cap;
    }

    /** One past the highest byte written. */
    public int length() {
        return len;
    }

    private long at(int at, int n) {
        if (at < 0 || n < 0 || at > cap - n) throw CodecException.OVERFLOW;
        len = Math.max(len, at + n);
        return off + at;
    }

    public void writeAt(int at, byte[] src, int from, int n) {
        long o = at(at, n);
        MemorySegment.copy(src, from, seg, Le.BYTE, o, n);
    }

    /** Append after the highest byte written so far. */
    public void write(byte[] src, int from, int n) {
        writeAt(len, src, from, n);
    }

    public void write(byte[] src) {
        writeAt(len, src, 0, src.length);
    }

    public void writeInt(int at, int v) {
        seg.set(Le.INT, at(at, 4), v);
    }

    public void writeLong(int at, long v) {
        seg.set(Le.LONG, at(at, 8), v);
    }

    public void writeFloat(int at, float v) {
        seg.set(Le.FLOAT, at(at, 4), v);
    }

    public void writeFloats(int at, float[] src, int from, int n) {
        if (n > (Integer.MAX_VALUE >> 2)) throw CodecException.OVERFLOW;
        long o = at(at, n * 4);
        MemorySegment.copy(src, from, seg, Le.FLOAT, o, n);
    }
}
