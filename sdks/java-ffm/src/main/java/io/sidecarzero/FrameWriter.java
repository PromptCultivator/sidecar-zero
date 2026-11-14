package io.sidecarzero;

import io.sidecarzero.internal.Registry.Template;

import java.lang.foreign.MemorySegment;

/**
 * The request frame's columns, writable (arrow-data-plane §7.1). The frame's metadata (prefix,
 * row count, length fields) is already in place; a codec writes only values. Each write is
 * checked against the column's type and the frame's row count. Valid only inside
 * {@link Codec#encodeFrame}.
 */
public final class FrameWriter {
    private MemorySegment seg;
    private long region;
    private Template t;
    private int rows;

    FrameWriter() {}

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

    public int columns() {
        return t.columns.length;
    }

    private void f32(int col) {
        if (col < 0 || col >= t.columns.length || t.columns[col].elemType() != ElemType.F32) throw CodecException.SHAPE;
    }

    /** Column {@code col}'s values for every row, row-major: {@code rows × list_size} floats from {@code src[from]}. */
    public void writeF32(int col, float[] src, int from) {
        f32(col);
        int n = rows * t.columns[col].listSize();
        if (from < 0 || src.length - from < n) throw CodecException.SHAPE;
        MemorySegment.copy(src, from, seg, Le.FLOAT, region + t.valuesOff[col], n);
    }

    /** The values of row {@code row} alone: {@code list_size} floats from {@code src[from]}. */
    public void writeF32Row(int col, int row, float[] src, int from) {
        f32(col);
        int n = t.columns[col].listSize();
        if (row < 0 || row >= rows || from < 0 || src.length - from < n) throw CodecException.SHAPE;
        MemorySegment.copy(src, from, seg, Le.FLOAT, region + t.valuesOff[col] + (long) row * t.rowBytes[col], n);
    }

    /** Raw bytes for every row, for any fixed-width type: {@code rows × row_bytes} from {@code src[from]}. */
    public void writeBytes(int col, byte[] src, int from) {
        if (col < 0 || col >= t.columns.length) throw CodecException.SHAPE;
        long n = (long) rows * t.rowBytes[col];
        if (from < 0 || src.length - from < n) throw CodecException.SHAPE;
        MemorySegment.copy(src, from, seg, Le.BYTE, region + t.valuesOff[col], (int) n);
    }
}
