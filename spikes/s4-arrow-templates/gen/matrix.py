"""Spike S4: compare every reader's output with the generator's expectations
and print the acceptance matrix.

usage: matrix.py RESULTS_DIR      (reads frames/expected.json, frames/patch-offsets.json, reader-*.jsonl)
Exit status 1 if any frame that must be accepted was rejected or read wrongly.
"""
import glob
import json
import os
import sys
from collections import defaultdict

from s4common import VARIANTS

res = sys.argv[1]
expected = json.load(open(os.path.join(res, "frames", "expected.json")))
patches = json.load(open(os.path.join(res, "frames", "patch-offsets.json")))

rows = []
for path in sorted(glob.glob(os.path.join(res, "reader-*.jsonl"))):
    for line in open(path):
        line = line.strip()
        if line:
            rows.append(json.loads(line))

ORDER = ["pyarrow", "rust", "go", "java"]
readers = sorted({(r["reader"], r["mode"]) for r in rows}, key=lambda k: (ORDER.index(k[0]) if k[0] in ORDER else 9, k[1]))
libs = {r["reader"]: r["lib"] for r in rows}
by = {(r["reader"], r["mode"], r["frame"]): r for r in rows}
missing = [n for n in ORDER if n not in libs]


def values_ok(r, e):
    return (r.get("rows") == e["rows"]
            and [(c["name"], c["nulls"], c["fnv"]) for c in r.get("cols", [])]
            == [(c["name"], c["nulls"], c["fnv"]) for c in e["cols"]])


def validation_ok(r):
    return all(str(r.get(k, "ok")).startswith(("ok", "n/a")) for k in ("validate", "validate_full"))


def cell(reader, mode, frame):
    r = by.get((reader, mode, frame))
    if r is None:
        return "not run"
    if not r["ok"]:
        return "REJECTED"
    if not values_ok(r, expected[frame]):
        return "WRONG"
    if not validation_ok(r):
        return "INVALID"
    return "ok"


out = []


def say(s=""):
    out.append(s)


def table(header, body):
    w = [max(len(str(x[i])) for x in [header] + body) for i in range(len(header))]
    say("  ".join(str(h).ljust(w[i]) for i, h in enumerate(header)))
    say("  ".join("-" * w[i] for i in range(len(header))))
    for b in body:
        say("  ".join(str(x).ljust(w[i]) for i, x in enumerate(b)))


say("SPIKE S4 - ARROW TEMPLATE INTEROP: RESULTS")
say("=" * 100)
say("Libraries: " + "; ".join(libs[n] for n in ORDER if n in libs))
if missing:
    say("NOT RUN: " + ", ".join(missing))
say()
say("Cell legend: ok = accepted by the library's stock IPC reader, row count, null counts and per-column")
say("checksums equal the generator's, and the library's validation (where it has one) passed.")
say("REJECTED = the reader returned an error. WRONG = accepted but values differ. INVALID = validation failed.")
say()

failures = 0
for group, title in (("matrix", "1. Capacity template, metadata rendered by PyArrow (Arrow C++), patched in place"),
                     ("matrix-rs", "2. Capacity template, metadata rendered by arrow-rs StreamWriter at alignment 64, patched in place")):
    frames = [f for f, e in expected.items() if e["group"] == group]
    if not frames:
        continue
    say(title)
    cols = [(v.name, n) for v in VARIANTS.values() for n in v.row_counts]
    header = ["reader", "API"] + [f"{v}/{n}" for v, n in cols]
    body = []
    tag = "template" if group == "matrix" else "rs-template"
    for reader, mode in readers:
        line = [reader, mode]
        for v, n in cols:
            c = cell(reader, mode, f"{v}.r{n}.{tag}")
            failures += c != "ok"
            line.append(c)
        body.append(line)
    table(header, body)
    say("(columns are schema variant / row count; launch has max_rows 32, prims and nullable have max_rows 100)")
    say()

say("3. The two layout variations in isolation (9 frames each: 3 variants x 3 row counts), frames accepted with correct values")
layouts = [("packed", "stock PyArrow frame (control)"),
           ("longbody", "packed buffers, body declared longer than the buffers need"),
           ("spaced", "buffers at capacity positions on 64-byte boundaries, body cut after the last one"),
           ("template", "both (the design's frame; same cells as table 1)")]
header = ["reader", "API"] + [k for k, _ in layouts]
body = []
for reader, mode in readers:
    line = [reader, mode]
    for lay, _ in layouts:
        fs = [f for f, e in expected.items() if e["layout"] == lay and e["group"] in ("matrix", "isolation")]
        good = sum(cell(reader, mode, f) == "ok" for f in fs)
        if lay != "template":
            failures += len(fs) - good
        line.append(f"{good}/{len(fs)}")
    body.append(line)
table(header, body)
for k, d in layouts:
    say(f"  {k:9s} {d}")
say()

say("4. Cross-reader agreement on the frames of tables 1-3")
agree = disagree = 0
for f, e in expected.items():
    if e["group"] == "extra":
        continue
    sigs = {json.dumps([r.get("rows"), r.get("cols")], sort_keys=True)
            for (rd, md, fr), r in by.items() if fr == f and r["ok"]}
    if len(sigs) == 1:
        agree += 1
    else:
        disagree += 1
        say(f"  DISAGREE on {f}")
say(f"  {agree} frames: every reader and API returned the same row count, null counts and checksums; {disagree} frames disagreed")
say()

say("5. Validation each reader ran on the accepted frames of tables 1-3")
for reader, mode in readers:
    rs = [r for (rd, md, fr), r in by.items() if rd == reader and md == mode and r["ok"] and expected[fr]["group"] != "extra"]
    kinds = defaultdict(int)
    for r in rs:
        v = r.get("validate_full", "absent")
        if r.get("validate") not in (None, "ok"):
            v = "validate(): " + r["validate"]
        kinds[v] += 1
    say(f"  {reader:8s} {mode:27s} " + "; ".join(f"{v} x{n}" for v, n in sorted(kinds.items())))
say()

say("6. Did the reader hand back views into the region (zero copy)? (frames of tables 1-3, slot-style APIs only)")
for reader, mode in readers:
    rs = [r for (rd, md, fr), r in by.items() if rd == reader and md == mode and r["ok"]
          and expected[fr]["group"] != "extra" and "borrowed" in r]
    if rs:
        say(f"  {reader:8s} {mode:27s} values buffers inside the region: {sum(r['borrowed'] for r in rs)}/{len(rs)}")
    else:
        say(f"  {reader:8s} {mode:27s} not measured (the stream API reads through its own allocator)")
say()

say("7. Extras: negative controls and edge cases (NOT part of the acceptance matrix)")
for f, e in expected.items():
    if e["group"] != "extra":
        continue
    say(f"  {f}  [{e['layout']}]  expectation: {e['expect']}")
    for reader, mode in readers:
        r = by.get((reader, mode, f))
        if r is None:
            continue
        if not r["ok"]:
            s = "REJECTED: " + r.get("error", "")
        else:
            got_nulls = [c["nulls"] for c in r["cols"]]
            exp_nulls = [c["nulls"] for c in e["cols"]]
            fnv_ok = [c["fnv"] == x["fnv"] for c, x in zip(r["cols"], e["cols"])]
            s = "accepted; " + ("values correct" if values_ok(r, e) else
                                f"null counts {got_nulls} (true {exp_nulls}), checksums match per column: {fnv_ok}")
            for k in ("validate", "validate_full"):
                if k in r and not str(r[k]).startswith(("ok", "n/a")):
                    s += f"; {k}: {r[k]}"
        if e["expect"] == "accept" and cell(reader, mode, f) != "ok":
            failures += 1
        if e["expect"] == "reject" and r["ok"]:
            failures += 1
            s += "   <-- expected a rejection"
        say(f"    {reader:8s} {mode:27s} {s}")
say()

say("8. Patch offsets (byte offset from the start of the region) and the values stored there per row count")
for key, t in patches.items():
    variant, source = key.split("/")
    rc = [str(n) for n in VARIANTS[variant].row_counts]
    say(f"  {variant}, metadata rendered by {source}: prefix_len={t['prefix_len']} body_len={t['body_len']} "
        f"frame_cap={t['frame_cap']} rows_field_off={t['rows_field_off']}")
    header = ["prefix_off", "kind", "scale", "field"] + [f"rows={n}" for n in rc]
    body = [[p["prefix_off"], p["kind"], p["scale"], p["field"]] + [p["values"][n] for n in rc] for p in t["patches"]]
    body += [[p["prefix_off"], "NULL_COUNT", "-", f"null count ({p['column']}), written per request"] +
             [p["values"][n] for n in rc] for p in t["null_counts"]]
    out_before = len(out)
    table(header, body)
    for i in range(out_before, len(out)):
        out[i] = "    " + out[i]
    say()
say("  The offsets are properties of the template, not of the row count: one list per template served every")
say("  row count, and gen.txt shows that between writes the only prefix bytes that changed lie in these fields.")
say()

total = sum(1 for (rd, md, fr) in by if expected[fr]["group"] in ("matrix", "matrix-rs"))
say("=" * 100)
if failures == 0 and not missing:
    say(f"S4 RESULT: every reader accepted every template frame with correct values "
        f"({total} reads across {len(readers)} reader APIs). Kill criterion NOT met.")
elif failures == 0:
    say(f"S4 RESULT: no rejection among the readers that ran ({total} reads), but these did not run: {', '.join(missing)}.")
else:
    say(f"S4 RESULT: {failures} cells failed. See REJECTED / WRONG / INVALID above.")

text = "\n".join(out) + "\n"
sys.stdout.write(text)
with open(os.path.join(res, "matrix.txt"), "w") as f:
    f.write(text)
sys.exit(1 if failures else 0)
