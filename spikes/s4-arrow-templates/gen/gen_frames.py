"""Spike S4 generator.

Builds capacity templates as docs/components/03-arrow-data-plane.md describes
them, then plays the client fast path (section 7.1) into ONE region per
template: stamp the prefix once, then for each row count rewrite the fixed
8-byte length fields in place and memcpy the column bytes. A snapshot of the
region is written to a file after every write.

The metadata is rendered once by a real Arrow writer (PyArrow / Arrow C++; and,
if --rust-render is given, arrow-rs with alignment 64). It is never
re-serialized: every later change is a store into bytes that already exist.

usage: gen_frames.py OUT_DIR [--rust-render DIR]
"""
import json
import os
import struct
import sys
from dataclasses import dataclass, field

import pyarrow as pa

from s4common import (ELEM, VARIANTS, expected_for, load_i64, logical_rows, raw_values,
                      round_up, split_message, validity_bitmap, walk_record_batch_message)

ALIGN = 64
GARBAGE = 0xA5  # a fresh region is filled with this, never zeros: slot memory is reused

PA_TYPE = {"f32": pa.float32(), "i64": pa.int64(), "f64": pa.float64(), "i32": pa.int32(),
           "u8": pa.uint8()}

report_lines = []


def say(s=""):
    print(s)
    report_lines.append(s)


# ---------------------------------------------------------------- stock rendering


def pa_schema(variant):
    fields = []
    for c in variant.cols:
        t = PA_TYPE[c.elem]
        if c.list_size > 1:
            t = pa.list_(pa.field("item", t, nullable=False), c.list_size)
        fields.append(pa.field(c.name, t, nullable=c.nullable))
    return pa.schema(fields)


def pa_batch(variant, rows):
    """An ordinary RecordBatch holding the test values, built with PyArrow."""
    arrays = []
    for ci, c in enumerate(variant.cols):
        lr = logical_rows(variant, ci, rows)
        if c.list_size > 1:
            flat = pa.array([v for row in lr for v in row], PA_TYPE[c.elem])
            arrays.append(pa.FixedSizeListArray.from_arrays(
                flat, type=pa.list_(pa.field("item", PA_TYPE[c.elem], nullable=False), c.list_size)))
        else:
            arrays.append(pa.array([None if row is None else row[0] for row in lr], PA_TYPE[c.elem]))
    return pa.RecordBatch.from_arrays(arrays, schema=pa_schema(variant))


def stock_message(variant, rows):
    """The encapsulated RecordBatch message a stock PyArrow writer produces."""
    return pa_batch(variant, rows).serialize().to_pybytes()


# ---------------------------------------------------------------- template


@dataclass
class Patch:  # mirrors PatchDesc
    prefix_off: int
    kind: str  # ROWS_SCALED | BITMAP_BYTES
    scale: int
    what: str

    def value(self, rows):
        return rows * self.scale if self.kind == "ROWS_SCALED" else (rows + 7) // 8


@dataclass
class ColumnDesc:  # mirrors ColumnDesc
    name: str
    values_off: int
    row_bytes: int
    validity_off: int  # 0 if non-nullable
    null_count_off: int  # 0 if non-nullable
    nullable: bool


@dataclass
class Template:
    source: str
    variant: object
    prefix: bytes
    body_off: int
    body_len: int
    frame_cap: int
    rows_field_off: int
    body_length_off: int
    patches: list
    columns: list
    buffers: list = field(default_factory=list)  # (index, offset_in_body, capacity, kind, column)
    rewrites: list = field(default_factory=list)  # fields changed relative to the stock render


def plan(variant):
    """Arrow's flattened node and buffer order for the supported column shapes."""
    nodes, bufs = [], []  # nodes: (col, length scale, nullable); bufs: (col, kind, scale)
    for ci, c in enumerate(variant.cols):
        w = ELEM[c.elem][1]
        if c.list_size > 1:
            assert not c.nullable
            nodes.append((ci, 1, False))
            bufs.append((ci, "EMPTY", 0))  # list validity
            nodes.append((ci, c.list_size, False))
            bufs.append((ci, "EMPTY", 0))  # child validity
            bufs.append((ci, "VALUES", c.list_size * w))
        else:
            nodes.append((ci, 1, c.nullable))
            bufs.append((ci, "BITMAP" if c.nullable else "EMPTY", 1))
            bufs.append((ci, "VALUES", w))
    return nodes, bufs


def build_template(source, variant, rendered):
    """Turn a stock-rendered max_rows message into a capacity template.

    One-time work, done where the design has the worker do it at model load:
    find the fixed positions of the length fields, move every buffer to its
    capacity position on a 64-byte boundary, declare the body at capacity, and
    pad the metadata so the body starts on a 64-byte boundary of the region.
    """
    max_rows = variant.max_rows
    msize, fb, _ = split_message(rendered)
    fb = bytearray(fb)
    pos = walk_record_batch_message(fb)
    assert pos.header_type == 3 and not pos.has_compression
    assert pos.rows is not None and pos.body_length is not None
    nodes, bufs = plan(variant)
    assert len(nodes) == len(pos.nodes), (len(nodes), len(pos.nodes))
    assert len(bufs) == len(pos.buffers), (len(bufs), len(pos.buffers))
    assert load_i64(fb, pos.rows) == max_rows

    prefix_len = round_up(8 + msize, ALIGN)
    t = Template(source, variant, b"", prefix_len, 0, 0, 8 + pos.rows, 8 + pos.body_length, [], [])
    t.patches.append(Patch(8 + pos.rows, "ROWS_SCALED", 1, "RecordBatch.length"))
    cols = [ColumnDesc(c.name, 0, c.row_bytes, 0, 0, c.nullable) for c in variant.cols]

    for i, (ci, scale, nullable) in enumerate(nodes):
        lpos, npos = pos.nodes[i]
        assert load_i64(fb, lpos) == max_rows * scale
        t.patches.append(Patch(8 + lpos, "ROWS_SCALED", scale, f"nodes[{i}].length ({variant.cols[ci].name})"))
        if nullable:
            cols[ci].null_count_off = 8 + npos
        else:
            assert load_i64(fb, npos) == 0

    def rewrite(p, new, what):
        old = load_i64(fb, p)
        if old != new:
            t.rewrites.append((8 + p, what, old, new))
            struct.pack_into("<q", fb, p, new)

    cursor = 0
    for i, (ci, kind, scale) in enumerate(bufs):
        opos, lpos = pos.buffers[i]
        cap = {"EMPTY": 0, "BITMAP": (max_rows + 7) // 8, "VALUES": max_rows * scale}[kind]
        rewrite(opos, cursor, f"buffers[{i}].offset")
        rewrite(lpos, cap, f"buffers[{i}].length")
        t.buffers.append((i, cursor, cap, kind, variant.cols[ci].name))
        if kind == "VALUES":
            cols[ci].values_off = prefix_len + cursor
            t.patches.append(Patch(8 + lpos, "ROWS_SCALED", scale, f"buffers[{i}].length ({variant.cols[ci].name} values)"))
        elif kind == "BITMAP":
            cols[ci].validity_off = prefix_len + cursor
            t.patches.append(Patch(8 + lpos, "BITMAP_BYTES", 0, f"buffers[{i}].length ({variant.cols[ci].name} validity)"))
        cursor += round_up(cap, ALIGN)
    t.body_len = cursor
    rewrite(pos.body_length, t.body_len, "Message.bodyLength")

    prefix = bytearray(prefix_len)  # zero padding after the flatbuffer
    struct.pack_into("<Ii", prefix, 0, 0xFFFFFFFF, prefix_len - 8)
    if prefix_len - 8 != msize:
        t.rewrites.append((4, "metadata_size (int32)", msize, prefix_len - 8))
    prefix[8:8 + len(fb)] = fb
    t.prefix = bytes(prefix)
    t.columns = cols
    t.frame_cap = prefix_len + t.body_len
    return t


# ---------------------------------------------------------------- client fast path (7.1)


class Region:
    """A slot's request region. Every store is logged so the run can show
    exactly which bytes each write touched."""

    def __init__(self, cap):
        self.b = bytearray([GARBAGE]) * cap
        self.stamp = None
        self.log = []

    def store(self, off, data, what):
        self.b[off:off + len(data)] = data
        self.log.append((off, len(data), what))


def sdk_write(region, tmpl, rows):
    region.log = []
    if region.stamp != id(tmpl):  # first use of this region with this template
        region.store(0, tmpl.prefix, "stamp prefix")
        region.stamp = id(tmpl)
    if load_i64(region.b, tmpl.rows_field_off) != rows:
        for p in tmpl.patches:
            region.store(p.prefix_off, struct.pack("<q", p.value(rows)), "patch " + p.what)
    for ci, col in enumerate(tmpl.columns):
        region.store(col.values_off, raw_values(tmpl.variant, ci, rows), f"values {col.name}")
        if col.nullable:
            bm, nulls = validity_bitmap(tmpl.variant, ci, rows)
            region.store(col.validity_off, bm, f"validity {col.name}")
            region.store(col.null_count_off, struct.pack("<q", nulls), f"null count {col.name}")
    return bytes(region.b)


def referenced_bytes(frame, tmpl):
    """Sum of buffer lengths the metadata references, and the end of the last one."""
    total, end = 0, 0
    for (i, off, cap, kind, _), (opos, lpos) in zip(tmpl.buffers, tmpl._fbpos.buffers):
        o, l = load_i64(frame, 8 + opos), load_i64(frame, 8 + lpos)
        total += l
        end = max(end, o + l)
    return total, end


# ---------------------------------------------------------------- main


def main():
    out = sys.argv[1]
    rust_dir = sys.argv[sys.argv.index("--rust-render") + 1] if "--rust-render" in sys.argv else None
    os.makedirs(out, exist_ok=True)
    manifest, expected, patch_table = [], {}, {}

    def emit(frame_id, group, variant, rows, layout, schema_file, data, expect, exp=None):
        fn = frame_id + ".frame"
        with open(os.path.join(out, fn), "wb") as f:
            f.write(data)
        manifest.append((frame_id, group, variant.name, rows, layout, schema_file, fn))
        e = dict(exp if exp is not None else expected_for(variant, rows))
        e.update(expect=expect, group=group, variant=variant.name, layout=layout, bytes=len(data))
        expected[frame_id] = e

    say(f"pyarrow {pa.__version__} (Arrow C++ {pa.cpp_build_info.version})")
    say()

    for vname, variant in VARIANTS.items():
        say("=" * 78)
        say(f"variant {vname}: max_rows={variant.max_rows} row_counts={variant.row_counts}")
        schema = pa_schema(variant)
        say("schema: " + "; ".join(f"{f.name}: {f.type}{'' if f.nullable else ' not null'}" for f in schema))
        schema_file = f"{vname}.schema.msg"
        with open(os.path.join(out, schema_file), "wb") as f:
            f.write(schema.serialize().to_pybytes())

        sources = [("pyarrow", stock_message(variant, variant.max_rows), schema_file)]
        if rust_dir:
            with open(os.path.join(rust_dir, f"{vname}.maxrows.align64.stream"), "rb") as f:
                stream = f.read()
            _, _, batch_off = split_message(stream)  # the first message is the Schema; it has no body
            rs_schema_file = f"{vname}.rs.schema.msg"
            with open(os.path.join(out, rs_schema_file), "wb") as f:
                f.write(stream[:batch_off])
            ms, fb, body_off = split_message(stream, batch_off)
            body_len = load_i64(fb, walk_record_batch_message(fb).body_length)
            sources.append(("arrow-rs", stream[batch_off:body_off + body_len], rs_schema_file))

        for source, rendered, sfile in sources:
            tag = "template" if source == "pyarrow" else "rs-template"
            group = "matrix" if source == "pyarrow" else "matrix-rs"
            say("-" * 78)
            say(f"[{vname}] template rendered by {source}")
            ms, fb, _ = split_message(rendered)
            stock_pos = walk_record_batch_message(fb)
            say(f"  stock render at max_rows: metadata_size={ms} total={len(rendered)} "
                f"body starts at byte {8 + ms} of the message")
            say("  stock buffers (offset,length): " + " ".join(
                f"({load_i64(fb, o)},{load_i64(fb, l)})" for o, l in stock_pos.buffers))
            stock_aligned = all(load_i64(fb, o) % 64 == 0 for o, _ in stock_pos.buffers) and (8 + ms) % 64 == 0
            say(f"  stock render already on 64-byte boundaries (body start and every buffer): {stock_aligned}")

            tmpl = build_template(source, variant, rendered)
            tmpl._fbpos = stock_pos
            say(f"  template: prefix_len=body_off={tmpl.body_off} body_len={tmpl.body_len} "
                f"frame_cap={tmpl.frame_cap} rows_field_off={tmpl.rows_field_off}")
            say("  capacity buffers (index, offset in body, capacity bytes, kind, column):")
            for b in tmpl.buffers:
                say(f"    {b}")
            say("  one-time rewrites of the stock render (prefix_off, field, stock -> template):")
            for r in tmpl.rewrites:
                say(f"    {r[0]:5d}  {r[1]:28s} {r[2]} -> {r[3]}")
            if not tmpl.rewrites:
                assert tmpl.prefix == rendered[:tmpl.body_off] and len(rendered) == tmpl.frame_cap
                say("    (none: the stock render IS the capacity template; its metadata bytes are the prefix, unchanged)")
            say("  patches (prefix_off, kind, scale, field):")
            for p in tmpl.patches:
                say(f"    {p.prefix_off:5d}  {p.kind:12s} {p.scale:5d}  {p.what}")
            for c in tmpl.columns:
                say(f"  column {c.name}: values_off={c.values_off} row_bytes={c.row_bytes} "
                    f"validity_off={c.validity_off} null_count_off={c.null_count_off}")
            assert all(p.prefix_off % 8 == 0 for p in tmpl.patches), "a patched field is not 8-byte aligned"
            assert all(c.null_count_off % 8 == 0 for c in tmpl.columns)
            assert tmpl.body_off % 64 == 0 and all(c.values_off % 64 == 0 for c in tmpl.columns)
            say("  every patched field is 8-byte aligned within the region; every buffer starts on a 64-byte boundary")

            patch_field_bytes = set()
            for p in tmpl.patches:
                patch_field_bytes.update(range(p.prefix_off, p.prefix_off + 8))
            for c in tmpl.columns:
                if c.nullable:
                    patch_field_bytes.update(range(c.null_count_off, c.null_count_off + 8))

            # ---- play the fast path: ONE region, patched in place between row counts
            lo, mid, hi = variant.row_counts
            region = Region(tmpl.frame_cap)
            snaps, prev, per_rows = {}, None, {}
            for step, rows in enumerate([hi, lo, mid, hi]):
                snap = sdk_write(region, tmpl, rows)
                prefix_stores = sorted((o, n, w) for o, n, w in region.log if o < tmpl.body_off and w != "stamp prefix")
                ref, end = referenced_bytes(snap, tmpl)
                say(f"  write #{step + 1} rows={rows}: {len(region.log)} stores "
                    f"({sum(1 for _, _, w in region.log if w.startswith('patch'))} length patches); "
                    f"buffers referenced={ref} bytes, last ends at body+{end}, declared body={tmpl.body_len}")
                if prev is not None:
                    diff = {i for i in range(tmpl.body_off) if prev[i] != snap[i]}
                    assert diff <= patch_field_bytes, sorted(diff - patch_field_bytes)
                    say(f"    prefix bytes that changed since the previous write: {len(diff)}, "
                        f"all inside the {len(patch_field_bytes) // 8} patchable 8-byte fields")
                per_rows.setdefault(rows, []).append([o for o, _, _ in prefix_stores])
                if step < 3:
                    snaps[rows] = snap
                    emit(f"{vname}.r{rows}.{tag}", group, variant, rows, tag, sfile, snap, "accept")
                else:
                    assert snap == snaps[hi], "returning to max_rows did not reproduce the first frame"
                    say("    back at max_rows: region is byte-identical to write #1")
                prev = snap
            assert all(len(s) == tmpl.frame_cap for s in snaps.values())

            patch_table[f"{vname}/{source}"] = {
                "prefix_len": tmpl.body_off, "body_len": tmpl.body_len, "frame_cap": tmpl.frame_cap,
                "rows_field_off": tmpl.rows_field_off,
                "patches": [{"prefix_off": p.prefix_off, "kind": p.kind, "scale": p.scale, "field": p.what,
                             "values": {str(r): load_i64(snaps[r], p.prefix_off) for r in variant.row_counts}}
                            for p in tmpl.patches],
                "null_counts": [{"prefix_off": c.null_count_off, "column": c.name,
                                 "values": {str(r): load_i64(snaps[r], c.null_count_off) for r in variant.row_counts}}
                                for c in tmpl.columns if c.nullable],
                "columns": [vars(c) for c in tmpl.columns],
            }

            # ---- cross-check against a stock writer at every row count (pyarrow source only)
            if source == "pyarrow":
                same_positions = True
                for rows in variant.row_counts:
                    sm = stock_message(variant, rows)
                    sms, sfb, sbody = split_message(sm)
                    sp = walk_record_batch_message(sfb)
                    same_positions &= (sms == ms and sp == stock_pos)
                    tfb = snaps[rows][8:8 + sms]
                    diffs = [i for i in range(0, sms, 8) if tfb[i:i + 8] != sfb[i:i + 8]]
                    offset_fields = {o for o, _ in sp.buffers} | {sp.body_length}
                    len_fields = {l for _, l in sp.buffers}
                    kinds = ["buffer offset/bodyLength" if d in offset_fields else
                             "buffer length" if d in len_fields else f"OTHER@{d}" for d in diffs]
                    say(f"  vs stock PyArrow render at rows={rows}: metadata_size equal={sms == ms}, "
                        f"field positions equal={sp == stock_pos}, differing 8-byte words={len(diffs)} "
                        f"({', '.join(sorted(set(kinds))) or 'none'})")
                    for d in diffs:
                        if d in len_fields:
                            say(f"      buffer length at prefix_off {8 + d}: stock={load_i64(sfb, d)} "
                                f"template={load_i64(tfb, d)}")
                    assert not any(k.startswith("OTHER") for k in kinds)

                    # ---- isolation frames for the two layout variations
                    cap_body = tmpl.body_len
                    emit(f"{vname}.r{rows}.packed", "isolation", variant, rows, "packed", sfile, sm, "accept")
                    # (1) body longer than the buffers it references, buffers still packed
                    lb = bytearray(sm) + bytes([GARBAGE]) * (cap_body + 256 - (len(sm) - sbody))
                    struct.pack_into("<q", lb, 8 + sp.body_length, len(lb) - sbody)
                    emit(f"{vname}.r{rows}.longbody", "isolation", variant, rows, "longbody", sfile, bytes(lb), "accept")
                    # (2) buffers at capacity positions on 64-byte boundaries, body cut right after the last one
                    _, end = referenced_bytes(snaps[rows], tmpl)
                    cut = round_up(end, 8)
                    sp_ = bytearray(snaps[rows][:tmpl.body_off + cut])
                    struct.pack_into("<q", sp_, tmpl.body_length_off, cut)
                    emit(f"{vname}.r{rows}.spaced", "isolation", variant, rows, "spaced", sfile, bytes(sp_), "accept")
                say(f"  a stock writer puts every patched field at the same position for every row count: {same_positions}")

                # ---- design 03 section 10 proposes finding patch positions by diffing two stock renders
                a, b = split_message(stock_message(variant, hi))[1], split_message(stock_message(variant, mid))[1]
                words = [i for i in range(0, len(a), 8) if a[i:i + 8] != b[i:i + 8]]
                patchable = {p.prefix_off - 8 for p in tmpl.patches} | {c.null_count_off - 8 for c in tmpl.columns if c.nullable}
                offs = {o for o, _ in stock_pos.buffers}
                say(f"  diff of two stock PyArrow renders (rows={hi} vs rows={mid}): {len(words)} differing 8-byte words: "
                    f"{sum(w in patchable for w in words)} patchable fields, "
                    f"{sum(w in offs for w in words)} buffer offsets, "
                    f"{sum(w == stock_pos.body_length for w in words)} bodyLength, "
                    f"{sum(w not in patchable and w not in offs and w != stock_pos.body_length for w in words)} other; "
                    f"patchable fields the diff did NOT flag: {len(patchable - set(words))} of {len(patchable)}")

                # ---- what a zero-row render looks like (design 03 section 10, "Finding the patch positions")
                zp = walk_record_batch_message(split_message(stock_message(variant, 0))[1])
                say(f"  stock render with 0 rows: RecordBatch.length field present={zp.rows is not None}, "
                    f"Message.bodyLength field present={zp.body_length is not None}")

                # ---- extras: negative controls, not part of the acceptance matrix
                if vname == "nullable":
                    base = snaps[mid]
                    exp = expected_for(variant, mid)
                    nc = [c.null_count_off for c in tmpl.columns]
                    x = bytearray(base)
                    for o in nc:
                        struct.pack_into("<q", x, o, 0)
                    emit(f"{vname}.r{mid}.x-nullcount-zero", "extra", variant, mid, "null counts left at 0",
                         sfile, bytes(x), "any", exp)
                    x = bytearray(base)
                    struct.pack_into("<q", x, nc[0], load_i64(base, nc[0]) + 1)
                    emit(f"{vname}.r{mid}.x-nullcount-off-by-one", "extra", variant, mid, "null count of col 0 one too high",
                         sfile, bytes(x), "any", exp)
                    x = bytearray(base)
                    for c in tmpl.columns:  # set the bits past `rows` in the last bitmap byte
                        last = c.validity_off + (mid + 7) // 8 - 1
                        x[last] |= (0xFF << (mid % 8)) & 0xFF
                    emit(f"{vname}.r{mid}.x-dirty-trailing-bits", "extra", variant, mid, "bits past rows set in bitmap",
                         sfile, bytes(x), "accept", exp)
                if vname == "launch":
                    x = bytearray(snaps[mid])  # a values buffer that claims to run past the body
                    lpos = tmpl.patches[-1].prefix_off
                    struct.pack_into("<q", x, lpos, tmpl.body_len + 4096)
                    emit(f"{vname}.r{mid}.x-buffer-past-body", "extra", variant, mid, "values length > body",
                         sfile, bytes(x), "reject")
                    x = bytearray(snaps[mid])  # the same frame with every unreferenced body byte zeroed
                    emit(f"{vname}.r{mid}.x-zero-gaps", "extra", variant, mid, "template with gaps zeroed",
                         sfile, bytes(zero_unreferenced(x, tmpl)), "accept")
        say()

    if rust_dir:
        say("=" * 78)
        say("arrow-rs StreamWriter probe: where does the stock writer put the body and the buffers?")
        for fn in sorted(os.listdir(rust_dir)):
            with open(os.path.join(rust_dir, fn), "rb") as f:
                stream = f.read()
            _, _, batch_off = split_message(stream)
            ms, fb, _ = split_message(stream, batch_off)
            p = walk_record_batch_message(fb)
            bufs = [(load_i64(fb, o), load_i64(fb, l)) for o, l in p.buffers]
            say(f"  {fn}")
            say(f"    metadata_size={ms}; body starts {8 + ms} bytes into the message "
                f"({'on' if (8 + ms) % 64 == 0 else 'NOT on'} a 64-byte boundary); "
                f"bodyLength={load_i64(fb, p.body_length)}")
            say(f"    buffers (offset,length): {' '.join(str(b) for b in bufs)}")
            say(f"    every buffer offset a multiple of 64: {all(o % 64 == 0 for o, _ in bufs)}")
        say()

    with open(os.path.join(out, "manifest.tsv"), "w") as f:
        for row in manifest:
            f.write("\t".join(str(x) for x in row) + "\n")
    with open(os.path.join(out, "expected.json"), "w") as f:
        json.dump(expected, f, indent=1)
    with open(os.path.join(out, "patch-offsets.json"), "w") as f:
        json.dump(patch_table, f, indent=1)
    say(f"wrote {len(manifest)} frames to {out}")
    with open(os.path.join(out, "..", "gen.txt"), "w") as f:
        f.write("\n".join(report_lines) + "\n")


def zero_unreferenced(frame, tmpl):
    """Zero every body byte no buffer references (what a stock writer's padding looks like)."""
    keep = bytearray(len(frame))
    keep[:tmpl.body_off] = b"\x01" * tmpl.body_off
    for (_, off, _, _, _), (opos, lpos) in zip(tmpl.buffers, tmpl._fbpos.buffers):
        l = load_i64(frame, 8 + lpos)
        keep[tmpl.body_off + off: tmpl.body_off + off + l] = b"\x01" * l
    return bytes(b if k else 0 for b, k in zip(frame, keep))


if __name__ == "__main__":
    main()
