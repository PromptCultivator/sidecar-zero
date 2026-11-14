package io.sidecarzero;

import io.sidecarzero.internal.Registry.Template;

import java.lang.foreign.MemorySegment;

/**
 * The response frame's columns (arrow-data-plane §7.3), after the SDK has checked its stamp,
 * length, and row count. Valid only inside {@link Codec#decodeFrame}.
 */
public final class FrameReader {
    private MemorySegment seg;
    private long region;
    private Template t;
    private int rows;

    FrameReader() {}

    void reset(MemorySegment seg, long region, Template t, int rows) {
        this.seg = seg;
        this.region = region;
        this.t = t;
        this.rows = rows;
    }

    void clear() {
        seg = null;
        t = null;
    }

    public int rows() {
        return rows;
    }

    private void f32(int col) {
        if (col < 0 || col >= t.columns.length || t.columns[col].elemType() != ElemType.F32) throw CodecException.SHAPE;
    }

    /** Column {@code col}'s values for every row into {@code dst[from..]}: {@code rows × list_size} floats. */
    public void readF32(int col, float[] dst, int from) {
        f32(col);
        int n = rows * t.columns[col].listSize();
        if (from < 0 || dst.length - from < n) throw CodecException.SHAPE;
        MemorySegment.copy(seg, Le.FLOAT, region + t.valuesOff[col], dst, from, n);
    }

    /** Element {@code i} of column {@code col} (row-major). */
    public float f32(int col, int i) {
        f32(col);
        if (i < 0 || i >= rows * t.columns[col].listSize()) throw CodecException.SHAPE;
        return seg.get(Le.FLOAT, region + t.valuesOff[col] + 4L * i);
    }

    public void readBytes(int col, byte[] dst, int from) {
        if (col < 0 || col >= t.columns.length) throw CodecException.SHAPE;
        long n = (long) rows * t.rowBytes[col];
        if (from < 0 || dst.length - from < n) throw CodecException.SHAPE;
        MemorySegment.copy(seg, Le.BYTE, region + t.valuesOff[col], dst, from, (int) n);
    }
}
