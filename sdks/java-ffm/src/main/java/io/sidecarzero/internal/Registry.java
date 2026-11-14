package io.sidecarzero.internal;

import io.sidecarzero.Column;
import io.sidecarzero.ElemType;
import io.sidecarzero.Schema;

import java.lang.foreign.MemorySegment;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;

import static io.sidecarzero.internal.Abi.ColumnDesc;
import static io.sidecarzero.internal.Abi.PatchDesc;
import static io.sidecarzero.internal.Abi.RegistryHeader;
import static io.sidecarzero.internal.Abi.TemplateDesc;

/**
 * The registry (arrow-data-plane §4), parsed from the copy read under the model seqlock, with
 * every check the fast path relies on (a port of {@code scz_shm::registry::parse}). Background
 * thread only; a malformed registry throws {@link IllegalArgumentException}.
 */
public final class Registry {
    public final Schema schema;
    public final List<Template> templates;
    /** By slot class: the request and the response template, or null. */
    public final Template[] req = new Template[(int) Abi.MAX_SLAB_CLASSES];
    public final Template[] resp = new Template[(int) Abi.MAX_SLAB_CLASSES];

    private Registry(Schema schema, List<Template> templates) {
        this.schema = schema;
        this.templates = templates;
        for (Template t : templates) {
            if (t.direction == Abi.Direction.REQUEST) req[t.cls] = t;
            else resp[t.cls] = t;
        }
    }

    public long fingerprint() {
        return templates.isEmpty() ? 0 : schema.fingerprint();
    }

    /** The smallest class whose request template holds {@code rows}, or -1. */
    public int classFor(int rows) {
        Template best = null;
        for (int i = 0; i < req.length; i++) {
            Template t = req[i];
            if (t != null && Integer.compareUnsigned(t.maxRows, rows) >= 0 && (best == null || Integer.compareUnsigned(t.maxRows, best.maxRows) < 0)) best = t;
        }
        return best == null ? -1 : best.cls;
    }

    /** One template: the frame's pre-rendered prefix, where its values sit, and its length fields. */
    public static final class Template {
        public final int id, direction, cls, maxRows, bodyOff, frameCap, rowsFieldOff;
        public final byte[] prefix;
        /** The prefix as a heap segment, copied into slot memory with no allocation. */
        public final MemorySegment prefixSeg;
        public final Column[] columns;
        public final int[] valuesOff, rowBytes;
        public final int[] patchOff, patchKind, patchScale;

        Template(int id, int direction, int cls, int maxRows, byte[] prefix, int bodyOff, int frameCap, int rowsFieldOff,
                 List<Column> columns, List<int[]> locs, List<int[]> patches) {
            this.id = id;
            this.direction = direction;
            this.cls = cls;
            this.maxRows = maxRows;
            this.prefix = prefix;
            this.prefixSeg = MemorySegment.ofArray(prefix);
            this.bodyOff = bodyOff;
            this.frameCap = frameCap;
            this.rowsFieldOff = rowsFieldOff;
            this.columns = columns.toArray(new Column[0]);
            this.valuesOff = locs.stream().mapToInt(l -> l[0]).toArray();
            this.rowBytes = locs.stream().mapToInt(l -> l[1]).toArray();
            this.patchOff = patches.stream().mapToInt(p -> p[0]).toArray();
            this.patchKind = patches.stream().mapToInt(p -> p[1]).toArray();
            this.patchScale = patches.stream().mapToInt(p -> p[2]).toArray();
        }

        /** {@code (registry_epoch & 0xFFFF) << 16 | template_id}: what a stamped region holds. */
        public int stamp(int epoch) {
            return (epoch & 0xFFFF) << 16 | id;
        }

        public long patchValue(int k, long rows) {
            return patchKind[k] == Abi.PatchKind.BITMAP_BYTES ? (rows + 7) / 8 : rows * (patchScale[k] & 0xFFFF_FFFFL);
        }
    }

    // ── reading ─────────────────────────────────────────────────────────────────────────────────

    private static final class Reader {
        final byte[] b;

        Reader(byte[] b) {
            this.b = b;
        }

        void need(long at, long n) {
            if (at < 0 || n < 0 || at + n > b.length) throw bad("read of " + n + " bytes at " + at + " past the registry");
        }

        int u8(long at) {
            need(at, 1);
            return b[(int) at] & 0xFF;
        }

        int u16(long at) {
            need(at, 2);
            return (b[(int) at] & 0xFF) | (b[(int) at + 1] & 0xFF) << 8;
        }

        long u32(long at) {
            need(at, 4);
            int i = (int) at;
            return (b[i] & 0xFFL) | (b[i + 1] & 0xFFL) << 8 | (b[i + 2] & 0xFFL) << 16 | (b[i + 3] & 0xFFL) << 24;
        }

        long u64(long at) {
            return u32(at) | u32(at + 4) << 32;
        }

        byte[] slice(long off, long len) {
            need(off, len);
            byte[] out = new byte[(int) len];
            System.arraycopy(b, (int) off, out, 0, (int) len);
            return out;
        }

        long array(long off, long count, long stride, long min, long align) {
            if (stride < min || stride % Math.min(align, 4) != 0) throw bad("stride " + stride + " below " + min);
            if (off % align != 0) throw bad("array at " + off + " is misaligned");
            if (off + count * stride > b.length) throw bad(count + " entries at " + off + " run past the registry");
            return off;
        }
    }

    private static IllegalArgumentException bad(String s) {
        return new IllegalArgumentException(s);
    }

    private static int i32(long v, String what) {
        if (v > Integer.MAX_VALUE) throw bad(what + " " + v + " out of range");
        return (int) v;
    }

    /**
     * Parse a registry copied out of shared memory. {@code reqCap}/{@code respCap} are the two
     * region capacities of each slot class. Null for an empty registry (raw mode).
     */
    public static Registry parse(byte[] bytes, long[] reqCap, long[] respCap) {
        if (bytes.length == 0) return null;
        Reader r = new Reader(bytes);
        if (r.u64(RegistryHeader.MAGIC) != Abi.REGISTRY_MAGIC) throw bad("bad magic");
        long len = r.u32(RegistryHeader.REGISTRY_LEN);
        if (len != bytes.length || len > Abi.CTL_REGISTRY_LEN) throw bad("registry_len " + len + " for " + bytes.length + " bytes");
        int nt = r.u16(RegistryHeader.TEMPLATE_COUNT);
        long ts = r.u16(RegistryHeader.TEMPLATE_STRIDE), cs = r.u16(RegistryHeader.COLUMN_STRIDE), ps = r.u16(RegistryHeader.PATCH_STRIDE);
        long nc = r.u32(RegistryHeader.COLUMN_TOTAL), np = r.u32(RegistryHeader.PATCH_TOTAL);
        if (nt == 0 || nt > Abi.MAX_TEMPLATES || nc > nt * Abi.MAX_COLUMNS || np > nt * Abi.MAX_PATCHES) throw bad(nt + " templates, " + nc + " columns, " + np + " patches");
        long toff = r.array(r.u32(RegistryHeader.TEMPLATES_OFF), nt, ts, TemplateDesc.SIZE, 8);
        long coff = r.array(r.u32(RegistryHeader.COLUMNS_OFF), nc, cs, ColumnDesc.SIZE, 4);
        long poff = r.array(r.u32(RegistryHeader.PATCHES_OFF), np, ps, PatchDesc.SIZE, 4);
        byte[] names = r.slice(r.u32(RegistryHeader.NAMES_OFF), r.u32(RegistryHeader.NAMES_LEN));
        r.slice(r.u32(RegistryHeader.IN_SCHEMA_OFF), r.u32(RegistryHeader.IN_SCHEMA_LEN));
        r.slice(r.u32(RegistryHeader.OUT_SCHEMA_OFF), r.u32(RegistryHeader.OUT_SCHEMA_LEN));
        List<Template> templates = new ArrayList<>();
        for (int k = 0; k < nt; k++) {
            long at = toff + k * ts;
            int id = r.u16(at + TemplateDesc.TEMPLATE_ID);
            int dir = r.u8(at + TemplateDesc.DIRECTION);
            int cls = r.u16(at + TemplateDesc.SLAB_CLASS);
            String what = "template " + id;
            if (id == 0 || templates.stream().anyMatch(t -> t.id == id)) throw bad(what + ": id 0 or repeated");
            if (cls >= reqCap.length) throw bad(what + ": no slot class " + cls);
            long cap = switch (dir) {
                case Abi.Direction.REQUEST -> reqCap[cls];
                case Abi.Direction.RESPONSE -> respCap[cls];
                default -> throw bad(what + ": direction " + dir);
            };
            if (templates.stream().anyMatch(t -> t.cls == cls && t.direction == dir)) throw bad(what + ": a second template for class " + cls);
            long maxRows = r.u32(at + TemplateDesc.MAX_ROWS);
            byte[] prefix = r.slice(r.u32(at + TemplateDesc.PREFIX_OFF), r.u32(at + TemplateDesc.PREFIX_LEN));
            long bodyOff = r.u32(at + TemplateDesc.BODY_OFF);
            long frameCap = r.u32(at + TemplateDesc.FRAME_CAP);
            long rowsFieldOff = r.u32(at + TemplateDesc.ROWS_FIELD_OFF);
            if (maxRows == 0 || prefix.length < 8 || bodyOff != prefix.length || bodyOff % 8 != 0 || frameCap < bodyOff || frameCap > cap) {
                throw bad(what + ": max_rows " + maxRows + ", prefix " + prefix.length + " bytes, body at " + bodyOff + ", frame " + frameCap + " in a " + cap + " byte region");
            }
            final int plen = prefix.length;
            java.util.function.LongPredicate inPrefix = off -> off % 8 == 0 && off + 8 <= plen;
            if (!inPrefix.test(rowsFieldOff)) throw bad(what + ": row count field outside the prefix");
            long c0 = r.u32(at + TemplateDesc.FIRST_COLUMN);
            int ncol = r.u16(at + TemplateDesc.COLUMN_COUNT);
            long p0 = r.u32(at + TemplateDesc.FIRST_PATCH);
            int npat = r.u16(at + TemplateDesc.PATCH_COUNT);
            if (c0 + ncol > nc || p0 + npat > np || ncol > Abi.MAX_COLUMNS || npat > Abi.MAX_PATCHES) throw bad(what + ": column or patch range out of bounds");
            List<Column> cols = new ArrayList<>();
            List<int[]> locs = new ArrayList<>();
            for (long i = c0; i < c0 + ncol; i++) {
                long ca = coff + i * cs;
                long nameOff = r.u32(ca + ColumnDesc.NAME_OFF);
                int nameLen = r.u16(ca + ColumnDesc.NAME_LEN);
                if (nameOff + nameLen > names.length) throw bad(what + ": a column name outside the names");
                String name;
                try {
                    name = StandardCharsets.UTF_8.newDecoder().onMalformedInput(CodingErrorAction.REPORT)
                            .decode(ByteBuffer.wrap(names, (int) nameOff, nameLen)).toString();
                } catch (CharacterCodingException e) {
                    throw bad(what + ": a column name that is not UTF-8");
                }
                int elem = r.u8(ca + ColumnDesc.ELEM_TYPE);
                long listSize = r.u32(ca + ColumnDesc.LIST_SIZE);
                boolean nullable = r.u8(ca + ColumnDesc.NULLABLE) != 0;
                long valuesOff = r.u32(ca + ColumnDesc.VALUES_OFF);
                long rowBytes = r.u32(ca + ColumnDesc.ROW_BYTES);
                if (nullable || r.u32(ca + ColumnDesc.VALIDITY_OFF) != 0 || r.u32(ca + ColumnDesc.NULL_COUNT_OFF) != 0) throw bad(what + ": column " + name + " is nullable");
                int w = ElemType.width(elem);
                if (listSize == 0 || w < 0 || w * listSize != rowBytes || rowBytes == 0) throw bad(what + ": column " + name + " has " + rowBytes + " bytes per row");
                long end = valuesOff + maxRows * rowBytes;
                if (valuesOff < bodyOff || valuesOff % 8 != 0 || end > frameCap) throw bad(what + ": column " + name + " values outside the frame");
                cols.add(new Column(name, elem, i32(listSize, "list_size"), false));
                locs.add(new int[] {(int) valuesOff, (int) rowBytes});
            }
            List<int[]> patches = new ArrayList<>();
            for (long i = p0; i < p0 + npat; i++) {
                long pa = poff + i * ps;
                long po = r.u32(pa + PatchDesc.PREFIX_OFF);
                long scale = r.u32(pa + PatchDesc.SCALE);
                int kind = r.u8(pa + PatchDesc.KIND);
                boolean okKind = (kind == Abi.PatchKind.ROWS_SCALED && scale > 0) || kind == Abi.PatchKind.BITMAP_BYTES;
                if (!okKind || !inPrefix.test(po) || po < 8) throw bad(what + ": a patch at " + po + " of kind " + kind + " outside the prefix");
                patches.add(new int[] {(int) po, kind, (int) scale});
            }
            if (patches.stream().noneMatch(p -> p[0] == rowsFieldOff && p[1] == Abi.PatchKind.ROWS_SCALED && p[2] == 1)) throw bad(what + ": no patch rewrites the row count");
            templates.add(new Template(id, dir, cls, i32(maxRows, "max_rows"), prefix, (int) bodyOff, (int) frameCap, (int) rowsFieldOff, cols, locs, patches));
        }
        List<Column> reqCols = null, respCols = null;
        for (Template t : templates) {
            int other = t.direction == Abi.Direction.REQUEST ? Abi.Direction.RESPONSE : Abi.Direction.REQUEST;
            Template u = templates.stream().filter(x -> x.cls == t.cls && x.direction == other).findFirst().orElse(null);
            if (u == null || u.maxRows != t.maxRows) throw bad("template " + t.id + ": class " + t.cls + " lacks a matching template of the other direction");
            List<Column> cols = List.of(t.columns);
            if (t.direction == Abi.Direction.REQUEST) {
                if (reqCols == null) reqCols = cols;
                else if (!reqCols.equals(cols)) throw bad("template " + t.id + ": columns differ from another template of its direction");
            } else {
                if (respCols == null) respCols = cols;
                else if (!respCols.equals(cols)) throw bad("template " + t.id + ": columns differ from another template of its direction");
            }
        }
        if (reqCols == null || respCols == null || reqCols.isEmpty() || respCols.isEmpty()) throw bad("a direction without columns");
        return new Registry(new Schema(reqCols, respCols), List.copyOf(templates));
    }
}
