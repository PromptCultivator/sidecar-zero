"""Shared definitions for spike S4. No Arrow import here: the layout rules, the
test values, the checksum and the flatbuffer walker are all plain Python, so the
expected results do not depend on the library under test.
"""
import struct
from dataclasses import dataclass, field

# ---------------------------------------------------------------- schemas

ELEM = {  # elem code -> (struct format, byte width)
    "f32": ("<f", 4),
    "i64": ("<q", 8),
    "f64": ("<d", 8),
    "i32": ("<i", 4),
    "u8": ("<B", 1),
}


@dataclass
class Col:
    name: str
    elem: str
    list_size: int = 1  # 1 = scalar column, N = FixedSizeList<elem>[N]
    nullable: bool = False

    @property
    def row_bytes(self):
        return ELEM[self.elem][1] * self.list_size


@dataclass
class Variant:
    name: str
    max_rows: int
    row_counts: list
    cols: list


_PRIMS = [("id", "i64"), ("amount", "f64"), ("count", "i32"), ("flag", "u8")]

VARIANTS = {
    # (a) the launch schema
    "launch": Variant("launch", 32, [1, 17, 32], [Col("features", "f32", 256, False)]),
    # (b) primitive columns of different widths, non-nullable
    "prims": Variant("prims", 100, [1, 37, 100], [Col(n, e, 1, False) for n, e in _PRIMS]),
    # (c) the same columns, nullable
    "nullable": Variant("nullable", 100, [1, 37, 100], [Col(n, e, 1, True) for n, e in _PRIMS]),
}

# ---------------------------------------------------------------- test values
# Values depend on (row count, element index) so a reader that picks up stale
# bytes left by an earlier, larger write produces a different checksum.


def elem_value(elem, salt, i):
    if elem == "f32":
        return float(salt * 100000 + i)  # < 2^24, exact in float32
    if elem == "i64":
        x = (salt << 40) + i * 1000003
        return -x if i & 1 else x
    if elem == "f64":
        return salt * 1000.0 + i + 0.25
    if elem == "i32":
        return -(salt * 100000 + i * 7)
    if elem == "u8":
        return (salt * 7 + i * 3) & 0xFF
    raise ValueError(elem)


def is_null(variant, ci, r, rows):
    """Null pattern for the nullable variant. Column 2 is nullable but never
    null (count 0, bitmap present); column 3 is entirely null when rows == 1."""
    if variant.name != "nullable":
        return False
    return [(r + rows) % 3 == 0, r % 4 == 1, False, r % 2 == 0][ci]


POISON = 0xEE  # written under null slots; must never reach a checksum


def logical_rows(variant, ci, rows):
    """What a correct reader must return: per row, None or a list of elements."""
    c = variant.cols[ci]
    out = []
    for r in range(rows):
        if is_null(variant, ci, r, rows):
            out.append(None)
        else:
            out.append([elem_value(c.elem, rows, r * c.list_size + j) for j in range(c.list_size)])
    return out


def raw_values(variant, ci, rows):
    """The bytes an SDK would memcpy into the values buffer."""
    c = variant.cols[ci]
    fmt, w = ELEM[c.elem]
    out = bytearray()
    for r, row in enumerate(logical_rows(variant, ci, rows)):
        if row is None:
            out += bytes([POISON]) * (w * c.list_size)
        else:
            for v in row:
                out += struct.pack(fmt, v)
    return bytes(out)


def validity_bitmap(variant, ci, rows):
    """(bitmap bytes of length ceil(rows/8), null count). Bits past `rows` are 0."""
    bm = bytearray((rows + 7) // 8)
    nulls = 0
    for r in range(rows):
        if is_null(variant, ci, r, rows):
            nulls += 1
        else:
            bm[r >> 3] |= 1 << (r & 7)
    return bytes(bm), nulls


# ---------------------------------------------------------------- checksum
# FNV-1a 64 per column over: for each row, 0x00 if null, else 0x01 followed by
# the little-endian bytes of every element in the row. Every reader computes
# this from values it obtained through its Arrow library's array accessors.

FNV_OFFSET = 0xCBF29CE484222325
FNV_PRIME = 0x100000001B3
_M = (1 << 64) - 1


def fnv1a(data, h=FNV_OFFSET):
    for b in data:
        h = ((h ^ b) * FNV_PRIME) & _M
    return h


def column_checksum(elem, rows_list):
    fmt = ELEM[elem][0]
    buf = bytearray()
    for row in rows_list:
        if row is None:
            buf.append(0)
        else:
            buf.append(1)
            for v in row:
                buf += struct.pack(fmt, v)
    return "%016x" % fnv1a(buf)


def expected_for(variant, rows):
    cols = []
    for ci, c in enumerate(variant.cols):
        lr = logical_rows(variant, ci, rows)
        cols.append({"name": c.name, "nulls": sum(1 for x in lr if x is None),
                     "fnv": column_checksum(c.elem, lr)})
    return {"rows": rows, "cols": cols}


# ---------------------------------------------------------------- flatbuffer walker
# Just enough to find where the fields the design patches live inside an
# org.apache.arrow.flatbuf.Message whose header is a RecordBatch. Field slots
# come from Arrow's format/Message.fbs:
#   Message:     0 version, 1 header_type, 2 header, 3 bodyLength, 4 custom_metadata
#   RecordBatch: 0 length, 1 nodes, 2 buffers, 3 compression, 4 variadicBufferCounts
#   FieldNode (struct): length i64, null_count i64
#   Buffer    (struct): offset i64, length i64


def _u16(b, o):
    return struct.unpack_from("<H", b, o)[0]


def _i32(b, o):
    return struct.unpack_from("<i", b, o)[0]


def _u32(b, o):
    return struct.unpack_from("<I", b, o)[0]


def _table_field(b, table, idx):
    vt = table - _i32(b, table)
    slot = 4 + 2 * idx
    if slot + 2 > _u16(b, vt):
        return None
    off = _u16(b, vt + slot)
    return table + off if off else None


@dataclass
class FbPositions:  # all positions are byte offsets from the start of the flatbuffer
    header_type: int
    body_length: int  # position of Message.bodyLength, or None if omitted
    rows: int  # position of RecordBatch.length, or None if omitted
    nodes: list = field(default_factory=list)  # [(length_pos, null_count_pos)]
    buffers: list = field(default_factory=list)  # [(offset_pos, length_pos)]
    has_compression: bool = False


def walk_record_batch_message(fb):
    msg = _u32(fb, 0)
    ht = _table_field(fb, msg, 1)
    header_type = fb[ht] if ht is not None else 0
    out = FbPositions(header_type, _table_field(fb, msg, 3), None)
    if header_type != 3:  # MessageHeader.RecordBatch
        return out
    h = _table_field(fb, msg, 2)
    rb = h + _u32(fb, h)
    out.rows = _table_field(fb, rb, 0)
    for idx, dst in ((1, out.nodes), (2, out.buffers)):
        f = _table_field(fb, rb, idx)
        if f is None:
            continue
        vec = f + _u32(fb, f)
        n = _u32(fb, vec)
        for i in range(n):
            dst.append((vec + 4 + 16 * i, vec + 4 + 16 * i + 8))
    out.has_compression = _table_field(fb, rb, 3) is not None
    return out


def split_message(buf, off=0):
    """Split one encapsulated IPC message at `off`: returns (metadata_size,
    flatbuffer bytes, offset of the body). Requires the continuation marker."""
    assert _u32(buf, off) == 0xFFFFFFFF, "no continuation marker"
    msize = _i32(buf, off + 4)
    return msize, bytes(buf[off + 8: off + 8 + msize]), off + 8 + msize


def load_i64(b, o):
    return struct.unpack_from("<q", b, o)[0]


def round_up(n, a):
    return (n + a - 1) // a * a
