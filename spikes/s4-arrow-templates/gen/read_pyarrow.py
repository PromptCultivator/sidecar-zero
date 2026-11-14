"""Spike S4 reader: PyArrow (Arrow C++ underneath).

Mode `read_record_batch` is the one docs/components/03-arrow-data-plane.md
section 7.4 names for Python: the region is wrapped with pa.py_buffer and read
with pa.ipc.read_record_batch against the schema from the registry's Schema
message. The region is an anonymous mmap, so it is page-aligned like a slot.

Mode `open_stream` feeds the Schema message followed by the frame to the stock
stream reader.

Both run RecordBatch.validate() and RecordBatch.validate(full=True).

usage: read_pyarrow.py FRAMES_DIR > jsonl
"""
import json
import mmap
import os
import struct
import sys

import pyarrow as pa

from s4common import ELEM, column_checksum

LIB = f"pyarrow {pa.__version__} / Arrow C++ {pa.cpp_build_info.version}"
ELEM_OF = {pa.float32(): "f32", pa.int64(): "i64", pa.float64(): "f64", pa.int32(): "i32", pa.uint8(): "u8"}


def summarize(batch):
    cols = []
    for name, col in zip(batch.schema.names, batch.columns):
        t = col.type
        if pa.types.is_fixed_size_list(t):
            elem = ELEM_OF[t.value_type]
            rows = [None if r is None else r for r in col.to_pylist()]
        else:
            elem = ELEM_OF[t]
            rows = [None if v is None else [v] for v in col.to_pylist()]
        cols.append({"name": name, "nulls": col.null_count, "fnv": column_checksum(elem, rows)})
    return cols


def values_address(col):
    if pa.types.is_fixed_size_list(col.type):
        col = col.values
    return col.buffers()[1].address


def check(batch, out, region_addr=None, region_len=0):
    out["rows"] = batch.num_rows
    out["schema"] = "; ".join(f"{f.name}: {f.type}{'' if f.nullable else ' not null'}" for f in batch.schema)
    for key, full in (("validate", False), ("validate_full", True)):
        try:
            batch.validate(full=full)
            out[key] = "ok"
        except Exception as e:  # noqa: BLE001
            out[key] = f"{type(e).__name__}: {e}"
    out["cols"] = summarize(batch)
    if region_addr is not None:
        out["borrowed"] = all(region_addr <= values_address(c) < region_addr + region_len for c in batch.columns)


def run(frame_id, schema_bytes, frame_bytes):
    results = []

    # --- mode 1: read_record_batch over the region, with the registry's schema
    out = {"reader": "pyarrow", "lib": LIB, "mode": "read_record_batch", "frame": frame_id}
    mm = mmap.mmap(-1, len(frame_bytes))  # anonymous shared mapping, page-aligned
    try:
        mm[:] = frame_bytes
        region = pa.py_buffer(mm)
        schema = pa.ipc.read_schema(pa.py_buffer(schema_bytes))
        msg = pa.ipc.read_message(region)
        out["detail"] = (f"message type={msg.type} metadata={msg.metadata.size}B body={msg.body.size}B "
                         f"region@{region.address % 4096}mod4096")
        batch = pa.ipc.read_record_batch(region, schema)
        check(batch, out, region.address, region.size)
        out["ok"] = True
    except Exception as e:  # noqa: BLE001
        out["ok"] = False
        out["error"] = f"{type(e).__name__}: {e}"
    results.append(out)
    batch = msg = region = None  # drop borrowed views; the mmap is left to the GC

    # --- mode 2: the stock stream reader over Schema message + frame
    out = {"reader": "pyarrow", "lib": LIB, "mode": "open_stream", "frame": frame_id}
    try:
        rdr = pa.ipc.open_stream(pa.py_buffer(schema_bytes + frame_bytes))
        batch = rdr.read_next_batch()
        check(batch, out)
        try:
            rdr.read_next_batch()
            out["detail"] = "a second batch followed"
        except StopIteration:
            out["detail"] = "stream ended cleanly after the frame"
        out["ok"] = True
    except Exception as e:  # noqa: BLE001
        out["ok"] = False
        out["error"] = f"{type(e).__name__}: {e}"
    results.append(out)
    return results


def main():
    d = sys.argv[1]
    with open(os.path.join(d, "manifest.tsv")) as f:
        manifest = f.read().splitlines()
    for line in manifest:
        frame_id, _group, _variant, _rows, _layout, schema_file, frame_file = line.rstrip("\n").split("\t")
        with open(os.path.join(d, schema_file), "rb") as f:
            schema_bytes = f.read()
        with open(os.path.join(d, frame_file), "rb") as f:
            frame_bytes = f.read()
        for r in run(frame_id, schema_bytes, frame_bytes):
            print(json.dumps(r), flush=True)


if __name__ == "__main__":
    main()
