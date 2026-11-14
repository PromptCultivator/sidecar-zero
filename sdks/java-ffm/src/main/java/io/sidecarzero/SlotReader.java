package io.sidecarzero;

import java.lang.foreign.MemorySegment;

/**
 * Reads a response's opaque bytes, whose length was read once from the slot record and checked
 * against the region. Valid only inside {@link Codec#decode}. A read past the length throws
 * {@link CodecException#OVERFLOW}.
 */
public final class SlotReader {
    private MemorySegment seg;
    private long off;
    private int len;

    SlotReader() {}

    void reset(MemorySegment seg, long off, int len) {
        this.seg = seg;
        this.off = off;
        this.len = len;
    }

    void clear() {
        seg = null;
    }

    public int length() {
        return len;
    }

    private long at(int at, int n) {
        if (at < 0 || n < 0 || at > len - n) throw CodecException.OVERFLOW;
        return off + at;
    }

    public void readBytes(int at, byte[] dst, int from, int n) {
        MemorySegment.copy(seg, Le.BYTE, at(at, n), dst, from, n);
    }

    public int readInt(int at) {
        return seg.get(Le.INT, at(at, 4));
    }

    public long readLong(int at) {
        return seg.get(Le.LONG, at(at, 8));
    }

    public float readFloat(int at) {
        return seg.get(Le.FLOAT, at(at, 4));
    }

    public void readFloats(int at, float[] dst, int from, int n) {
        if (n > (Integer.MAX_VALUE >> 2)) throw CodecException.OVERFLOW;
        MemorySegment.copy(seg, Le.FLOAT, at(at, n * 4), dst, from, n);
    }
}
