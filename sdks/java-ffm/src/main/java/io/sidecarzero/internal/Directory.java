package io.sidecarzero.internal;

import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.zip.CRC32C;

import static io.sidecarzero.internal.Abi.ControlHeader;
import static io.sidecarzero.internal.Abi.DataMap;
import static io.sidecarzero.internal.Abi.RingDirEntry;
import static io.sidecarzero.internal.Abi.SlabDirEntry;

/**
 * Everything a client needs to find its way around the data segment, read once from the
 * read-only control segment and checked (a port of {@code scz_shm::layout::Directory::read}).
 * Nothing in the data segment is trusted to describe its own layout.
 */
public final class Directory {
    public final long arenaId, dataLen, statusOff;
    public final long hbIntervalNs, hbStaleNs, spinNs, defaultDeadlineNs, maxDeadlineNs;
    public final int rings, classes;
    public final long[] ringOff, ringCap;
    public final long[] tableOff, slotDataOff, slotSize, reqCap, respCap;
    public final int[] slotCount;

    private Directory(MemorySegment ctl, long dataLen, int nr, int nc) {
        this.arenaId = ControlHeader.arenaId(ctl, 0);
        this.dataLen = dataLen;
        this.hbIntervalNs = ControlHeader.hbIntervalNs(ctl, 0);
        this.hbStaleNs = ControlHeader.hbStaleNs(ctl, 0);
        this.spinNs = ControlHeader.spinNs(ctl, 0);
        this.defaultDeadlineNs = ControlHeader.defaultDeadlineNs(ctl, 0);
        this.maxDeadlineNs = ControlHeader.maxDeadlineNs(ctl, 0);
        this.rings = nr;
        this.classes = nc;
        ringOff = new long[nr];
        ringCap = new long[nr];
        tableOff = new long[nc];
        slotDataOff = new long[nc];
        slotSize = new long[nc];
        reqCap = new long[nc];
        respCap = new long[nc];
        slotCount = new int[nc];
        statusOff = DataMap.statusOff(ctl, ControlHeader.DATA_MAP_OFF);
    }

    /** Why a control segment was refused. */
    public static final class LayoutException extends Exception {
        public LayoutException(String s) {
            super(s, null, false, false);
        }
    }

    /** CRC-32C over IDENT, CONFIG, and DIRECTORY, with the checksum field read as zero. */
    public static int layoutCrc(MemorySegment ctl) {
        byte[] h = new byte[(int) Abi.CTL_HEADER_LEN];
        MemorySegment.copy(ctl, ValueLayout.JAVA_BYTE, 0, h, 0, h.length);
        for (int i = 0; i < 4; i++) h[(int) ControlHeader.LAYOUT_CRC32C + i] = 0;
        CRC32C c = new CRC32C();
        for (long[] r : ControlHeader.CRC_RANGES) c.update(h, (int) r[0], (int) (r[1] - r[0]));
        return (int) c.getValue();
    }

    private static boolean within(long off, long len, long dataLen) {
        return off >= 0 && len >= 0 && off + len >= off && off + len <= dataLen && off % 64 == 0;
    }

    public static Directory read(MemorySegment ctl, long dataLenActual) throws LayoutException {
        if (ControlHeader.magic(ctl, 0) != Abi.CTL_MAGIC) throw new LayoutException("bad magic");
        if (ControlHeader.abiMajor(ctl, 0) != Abi.ABI_MAJOR) throw new LayoutException("ABI major " + ControlHeader.abiMajor(ctl, 0));
        if (ControlHeader.abiMinor(ctl, 0) < Abi.ABI_MINOR_MIN) throw new LayoutException("ABI minor " + ControlHeader.abiMinor(ctl, 0));
        if (ControlHeader.layoutCrc32c(ctl, 0) != layoutCrc(ctl)) throw new LayoutException("bad layout checksum");
        long dataLen = ControlHeader.dataLen(ctl, 0);
        if (dataLen != dataLenActual) throw new LayoutException("data segment size");
        if ((ControlHeader.ctlLen(ctl, 0) & 0xFFFF_FFFFL) != ctl.byteSize()) throw new LayoutException("control segment size");
        int nr = ControlHeader.ringCount(ctl, 0), nc = ControlHeader.slabClassCount(ctl, 0);
        if (nr == 0 || nr > Abi.MAX_RINGS || nc == 0 || nc > Abi.MAX_SLAB_CLASSES) throw new LayoutException("ring or slab class count");
        Directory d = new Directory(ctl, dataLen, nr, nc);
        for (int i = 0; i < nr; i++) {
            long b = ControlHeader.RING_DIR_OFF + i * ControlHeader.RING_DIR_STRIDE;
            long off = RingDirEntry.ringOff(ctl, b), cap = RingDirEntry.capacity(ctl, b) & 0xFFFF_FFFFL;
            if (RingDirEntry.kind(ctl, b) != Abi.RingKind.MPSC || (RingDirEntry.cellStride(ctl, b) & 0xFFFF_FFFFL) != Abi.CELL_LEN
                    || cap == 0 || Long.bitCount(cap) != 1 || !within(off, Abi.RING_HEADER_LEN + cap * Abi.CELL_LEN, dataLen)) {
                throw new LayoutException("ring " + i);
            }
            d.ringOff[i] = off;
            d.ringCap[i] = cap;
        }
        for (int i = 0; i < nc; i++) {
            long b = ControlHeader.SLAB_DIR_OFF + i * ControlHeader.SLAB_DIR_STRIDE;
            long size = SlabDirEntry.slotSize(ctl, b) & 0xFFFF_FFFFL, count = SlabDirEntry.slotCount(ctl, b) & 0xFFFF_FFFFL;
            long rq = SlabDirEntry.reqCap(ctl, b) & 0xFFFF_FFFFL, rs = SlabDirEntry.respCap(ctl, b) & 0xFFFF_FFFFL;
            long table = SlabDirEntry.tableOff(ctl, b), data = SlabDirEntry.dataOff(ctl, b);
            if (SlabDirEntry.recordStride(ctl, b) != Abi.SLOT_RECORD_LEN || rq + rs > size || count == 0 || count > Integer.MAX_VALUE
                    || !within(table, count * Abi.SLOT_RECORD_LEN, dataLen) || !within(data, count * size, dataLen)) {
                throw new LayoutException("slab class " + i);
            }
            d.tableOff[i] = table;
            d.slotDataOff[i] = data;
            d.slotSize[i] = size;
            d.slotCount[i] = (int) count;
            d.reqCap[i] = rq;
            d.respCap[i] = rs;
        }
        if (!within(d.statusOff, Abi.STATUS_PAGE_LEN, dataLen)) throw new LayoutException("status page");
        return d;
    }

    /** Offset of slot {@code i}'s record in class {@code c}. */
    public long rec(int c, int i) {
        return tableOff[c] + (long) i * Abi.SLOT_RECORD_LEN;
    }

    /** Offset of slot {@code i}'s request region; its response region follows at {@code reqCap}. */
    public long req(int c, int i) {
        return slotDataOff[c] + (long) i * slotSize[c];
    }
}
