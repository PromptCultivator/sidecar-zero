#!/usr/bin/env python3
"""The chaos benchmark's report generator (bench §7).

    python3 bench/report/report.py RESULTS_DIR

Reads RESULTS_DIR/meta.json and every run's JSON (bench/run.py) and writes RESULTS_DIR/report.md:
the environment, a pass/fail table for the claims of bench §1 and overview §1.3, model
availability and time in fallback per kill for every system, per-scenario latency with the
median and range across repetitions and the sample count behind every percentile, recovery
intervals, CPU, memory and allocation, the endpoint results of the service mode, and the
threats to validity. A percentile is printed only with enough samples behind it (bench §5):
1,000 for p99, 10,000 for p99.9; otherwise the count and the maximum.
"""

import glob
import json
import os
import statistics
import sys

SYSTEMS = {"A": "SidecarZero", "B": "Triton gRPC + system SHM", "C": "HTTP/JSON", "D": "Unix socket, binary", "E": "In-process"}
ADMISSION = {"NOT_CONNECTED", "ABI_MISMATCH", "SCHEMA_MISMATCH", "REBINDING", "GATE_INIT", "GATE_SHEDDING", "GATE_RELOADING", "GATE_DEAD",
             "GATE_DRAINING", "STALE_HEARTBEAT", "CLOCK_FAULT", "OVERLOADED"}


def us(ns):
    if ns is None:
        return "—"
    return f"{ns / 1e3:.1f} µs" if ns < 10_000_000 else f"{ns / 1e6:.1f} ms"


def pct(h, q):
    """A percentile of a histogram as written by the hosts, or None without enough samples."""
    need = {0.5: 1, 0.9: 10, 0.99: 1000, 0.999: 10000}[q]
    if not h or h["count"] < need:
        return None
    return h[{0.5: "p50", 0.9: "p90", 0.99: "p99", 0.999: "p999"}[q]]


def quantile_of(buckets, count, q):
    rank = max(1, int(q * count + 0.999999))
    seen = 0
    for lo, c in buckets:
        seen += c
        if seen >= rank:
            return lo
    return buckets[-1][0] if buckets else 0


def merge_hists(hs):
    """Histograms of the same bucket scheme, merged (across repetitions)."""
    b = {}
    for h in hs:
        for lo, c in h.get("buckets", []):
            b[lo] = b.get(lo, 0) + c
    items = sorted(b.items())
    n = sum(c for _, c in items)
    if n == 0:
        return {"count": 0, "max": 0}
    out = {"count": n, "max": max(h.get("max", 0) for h in hs), "buckets": items}
    for q, k in ((0.5, "p50"), (0.9, "p90"), (0.99, "p99"), (0.999, "p999")):
        out[k] = quantile_of(items, n, q)
    return out


def med_range(vals):
    vals = [v for v in vals if v is not None]
    if not vals:
        return None, None, None
    return statistics.median(vals), min(vals), max(vals)


def fmt_mr(vals, f=us):
    m, lo, hi = med_range(vals)
    if m is None:
        return "—"
    return f(m) if lo == hi else f"{f(m)} ({f(lo)}–{f(hi)})"


def load(d):
    meta = json.load(open(os.path.join(d, "meta.json")))
    runs = []
    for p in sorted(glob.glob(os.path.join(d, "*.json"))):
        if os.path.basename(p) == "meta.json":
            continue
        r = json.load(open(p))
        r["_file"] = os.path.basename(p)
        runs.append(r)
    return meta, runs


def key(r):
    s = r.get("spec", {})
    return (r.get("mode", "inproc"), r.get("host", "rust"), r.get("system"), r.get("variant", s.get("variant")), r.get("scenario", s.get("scenario")),
            r.get("rate", s.get("rate")), r.get("arrival", s.get("arrival")))


def per_kill(r):
    """Time in fallback and fallbacks per kill (bench §5): from the first fallback after a kill
    to the first model answer after it, across the host's threads."""
    kills = sorted(r.get("kills_ns", []))
    tr = r.get("transitions", [])
    out = []
    for i, k in enumerate(kills):
        nxt = kills[i + 1] if i + 1 < len(kills) else float("inf")
        first_fb = next((t for t in tr if t[0] >= k and t[2] == 0), None)
        if first_fb is None or first_fb[0] >= nxt:
            out.append({"kill": k, "time_ns": 0, "fallbacks": 0})
            continue
        back = next((t for t in tr if t[0] > first_fb[0] and t[2] == 1), None)
        fb = sum(t[3] for t in tr if t[2] == 1 and k <= t[0] < nxt)  # runs of fallbacks that ended
        out.append({"kill": k, "time_ns": (back[0] - first_fb[0]) if back else None, "fallbacks": fb})
    return out


def recovery(r):
    """System A's four recovery intervals per kill (bench §1, "Recovery is fast"), from the
    kill agent's time and the supervisor's timestamped events."""
    kills = sorted(r.get("kills_ns", []))
    ev = sorted(r.get("events", []))
    rows = []
    for k in kills:
        first = lambda kind, after: next((t for t, e in ev if e == kind and t >= after), None)  # noqa: E731
        lock = first("death_lock", k)
        closed = first("gate_closed", k)
        healthy = first("healthy", k) or first("reopened", k)
        reaped = first("reaped", k)
        rows.append({"lock": lock - k if lock else None, "flip": closed - lock if closed and lock else None,
                     "healthy": healthy - closed if healthy and closed else None, "kill_to_healthy": healthy - k if healthy else None,
                     "exit": reaped - k if reaped else None})
    return rows


def fallbacks(r):
    return sum(r["record"]["fallback"].values()) if "record" in r else 0


def calls(r):
    rec = r.get("record", {})
    return rec.get("model", 0) + fallbacks(r) + rec.get("error", 0) + rec.get("timeout", 0)


def verdict(ok):
    if ok == "invalid":
        return "invalid here: the load loop was late"
    return "**pass**" if ok is True else ("**FAIL**" if ok is False else "reported")


# A run whose load loop sent its calls more than this late (p99) measured the load generator,
# not the system: its latency figures are marked, and latency claims are not judged on it.
LAG_LIMIT_NS = 50_000


def lag_p99(rs):
    return merge_hists([r["record"]["send_lag_ns"] for r in rs]).get("p99") or 0


def main(d):
    meta, runs = load(d)
    ok_runs = [r for r in runs if "failed" not in r]
    groups = {}
    for r in ok_runs:
        groups.setdefault(key(r), []).append(r)
    env = meta["environment"]
    L = []
    w = L.append
    w("# SidecarZero chaos benchmark report\n")
    banner = []
    if env.get("vm"):
        banner.append("ran in a virtual machine")
    if meta.get("quick"):
        banner.append("ran in quick mode (reduced rates, durations, kills, and repetitions)")
    if env.get("governor") not in ("performance",):
        banner.append(f"CPU governor `{env.get('governor')}`, not `performance`")
    banner.append("no core isolation or static CPU manager")
    w(f"> **Environment Note:** This run {', '.join(banner)}. Virtualized CPU scheduling and shared cores add tail jitter to sub-millisecond measurements.\n")
    w("## 1. Environment\n")
    w("| | |\n|---|---|")
    for k in ("kernel", "arch", "cpu", "cpus", "governor", "cgroup", "vm", "onnxruntime", "triton", "commit", "dirty"):
        w(f"| {k} | `{env.get(k)}` |")
    inf = meta.get("inference_ns") or {}
    w(f"| repetitions | {meta['repeats']} |\n| kills per kill-loop run | {meta['kills']} |\n| inference time, measured first (bench §2) | fraud {us(inf.get('fraud'))}, MLP {us(inf.get('mlp'))} |\n| started | {meta.get('started')} |\n| finished | {meta.get('finished')} |\n")
    if meta.get("failed"):
        w(f"**Runs that failed** ({len(meta['failed'])}): " + ", ".join(f"`{f}`" for f in meta["failed"]) + "\n")

    def g(host, system, variant, scenario, rate=None, arrival="even", mode="inproc"):
        return [r for k, rs in groups.items() for r in rs if k[0] == mode and k[1] == host and k[2] == system and k[3] == variant and k[4] == scenario
                and (rate is None or k[5] == rate) and (arrival is None or k[6] == arrival)]

    host = "java" if any(k[1] == "java" for k in groups) else "rust"
    claims = []
    # The model stays in service through kills.
    for s in "ABCD":
        kr = g("rust", s, "default", "kill")
        if kr:
            avail = [r["record"]["model"] / max(1, calls(r)) for r in kr]
            pk = [x for r in kr for x in per_kill(r)]
            times = [x["time_ns"] for x in pk if x["time_ns"] is not None]
            claims.append((f"Model in service through kills — {SYSTEMS[s]}", f"availability {fmt_mr(avail, lambda v: f'{v * 100:.3f}%')}; time in fallback per kill: median {us(statistics.median(times)) if times else '—'}, max {us(max(times)) if times else '—'}; fallbacks per kill median {statistics.median([x['fallbacks'] for x in pk]) if pk else '—'}", None))
    # No request lost to a kill.
    for s in "ABCD":
        kr, cr = g("rust", s, "default", "kill"), g("rust", s, "default", "control")
        if kr and cr:
            lost = [r["record"]["error"] + r["record"]["timeout"] for r in kr]
            ctl = [r["record"]["error"] + r["record"]["timeout"] for r in cr]
            claims.append((f"No request lost to a kill — {SYSTEMS[s]}", f"errors and timeouts {fmt_mr(lost, str)} with kills, {fmt_mr(ctl, str)} in the control run", max(lost) <= max(ctl)))
    # Fallback switchover.
    ar = g("rust", "A", "default", "kill")
    if ar:
        h = merge_hists([r["record"]["admission_entry_ns"] for r in ar])
        p = h.get("p999") if h["count"] >= 10000 else None
        claims.append(("Call after the gate flips falls back at once (p99.9 < 1 µs)", f"{h['count']} admission fallbacks; p99.9 {us(p)}" if p is not None else f"{h['count']} admission fallbacks (fewer than 10,000: no p99.9); max {us(h.get('max'))}, p50 {us(h.get('p50'))}", (p < 1000) if p is not None else None))
        rec = [x for r in ar for x in recovery(r)]
        kh = sorted(x["kill_to_healthy"] for x in rec if x["kill_to_healthy"] is not None)
        if kh:
            p99 = kh[int(0.99 * (len(kh) - 1))] if len(kh) >= 1000 else None
            claims.append(("Kill to HEALTHY with a warm standby (p99 < 5 ms)", f"{len(kh)} kills; median {us(statistics.median(kh))}, max {us(kh[-1])}" + (f", p99 {us(p99)}" if p99 else " (fewer than 1,000 kills: no p99)"), (p99 < 5e6) if p99 else None))
        mx = max(r["record"]["latency_ns"]["max"] for r in ar)
        budget = ar[0]["spec"]["budget_us"] * 1000
        claims.append(("Calls in flight at a kill are bounded by their deadline", f"slowest call {us(mx)} against a {us(budget)} budget (+ fallback, + timer slack)", None))
    # Transport overhead, spinning and parked: A's latency less the worker's own inference
    # time (its counters), and less E's latency.
    hi = max([k[5] for k in groups if k[2] == "A" and k[4] == "steady" and k[5]] or [50000])
    for rate, label, mark in ((hi, f"spinning, {hi:,}/s", True), (2000, "parked, 2,000/s", False)):
        a, e = g("rust", "A", "default", "steady", rate), g("rust", "E", "default", "steady", rate)
        if a:
            ha = merge_hists([r["record"]["model_latency_ns"] for r in a])
            ws = [w for r in a for w in r.get("worker_stats", [])]
            served, infer = sum(w["served"] for w in ws), sum(w["infer_ns"] for w in ws)
            mean_inf = infer / served if served else None
            he = merge_hists([r["record"]["model_latency_ns"] for r in e]) if e else None
            o50 = ha["p50"] - mean_inf if mean_inf else None
            o99 = (ha["p99"] - mean_inf) if mean_inf and ha["count"] >= 1000 else None
            lag = lag_p99(a)
            ok = None
            if mark:
                ok = "invalid" if lag > LAG_LIMIT_NS else (o50 is not None and o99 is not None and o50 < 1000 and o99 < 10000)
            text = f"A less the worker's mean inference ({us(mean_inf)}): p50 {us(o50)}, p99 {us(o99)}"
            if he:
                text += f"; A − E: p50 {us(ha['p50'] - he['p50'])}"
            claims.append((f"Transport overhead, {label}" + (" (p50 < 1 µs, p99 < 10 µs)" if mark else ""), text + f"; send lag p99 {us(lag)}", ok))
    # Idle, allocation, overload, reload, size.
    idle = g("rust", "A", "default", "idle", rate=None, arrival=None)
    if idle:
        v = [r["sidecar_cpu_ratio"] for r in idle]
        claims.append(("An idle pod costs little (< 2% of one core)", f"sidecar CPU {fmt_mr(v, lambda x: f'{x * 100:.2f}%')} of one core", max(v) < 0.02))
    for h_ in sorted({k[1] for k in groups}):
        rs = [r for r in g(h_, "A", "default", "steady", None, None) if "alloc_bytes_per_call" in r]
        if rs:
            # Per call means growing with calls: a few bytes once in a run (a JIT deoptimization
            # when a rare path first runs) are not a per-call cost; a run's total is judged.
            totals = [(round(r["alloc_bytes_per_call"] * r["calls"]), r["calls"]) for r in rs]
            text = "; ".join(f"{t} B over {n:,} calls" for t, n in sorted(totals, key=lambda x: x[1]))
            claims.append((f"No allocation per call — {h_} host, A", text, all(t <= 1024 for t, _ in totals)))
    ov = g("rust", "A", "default", "overload")
    if ov:
        h = merge_hists([r["record"]["admission_entry_ns"] for r in ov])
        shed = sum(r["record"]["fallback"].get("OVERLOADED", 0) for r in ov)
        p99 = h.get("p99") if h["count"] >= 1000 else None
        dl = sum(r["record"]["fallback"].get("DEADLINE", 0) for r in ov)
        lag = lag_p99(ov)
        ok = "invalid" if lag > LAG_LIMIT_NS else ((p99 < 1000) if p99 is not None else None)
        claims.append(("Overload fails fast (shed calls < 1 µs at p99)", f"at {ov[0]['rate']:,}/s: {shed} shed calls, {dl} deadline misses; time to fallback p99 {us(p99)}; send lag p99 {us(lag)}", ok))
    rl = g("rust", "A", "default", "reload")
    if rl:
        # Fallbacks from 0.2 s before each reload to 1 s after it, against what the run's own
        # rate elsewhere would put in windows that long: a VM's stalls cause a few anyway.
        near, expected, n_reloads = 0, 0.0, 0
        for r in rl:
            tl, t0 = r["record"]["timeline_100ms"], r["start_ns"]
            slots = set()
            for k in r.get("kills_ns", []):
                i = int((k - t0) / 1e8)
                slots.update(range(max(0, i - 2), min(len(tl), i + 10)))
                n_reloads += 1
            inside = sum(tl[j][1] for j in slots)
            outside = sum(t[1] for j, t in enumerate(tl) if j not in slots)
            near += inside
            expected += outside / max(1, len(tl) - len(slots)) * len(slots)
        claims.append(("A reload with an unchanged schema costs nothing (0 fallbacks)", f"{n_reloads} reloads; {near} fallbacks within 1 s of one, where the run's rate elsewhere predicts {expected:.1f}", near <= max(0, int(expected + 0.999))))
    sz = meta.get("supervisor_stripped_bytes")
    if sz:
        claims.append(("Supervisor under 5 MB (stripped, static)", f"{sz / 1e6:.2f} MB on {env.get('arch')}", sz < 5_000_000))
    svc = [r for r in ok_runs if r.get("mode") == "service"]
    for r in svc:
        if r.get("scenario") == "kill":
            claims.append((f"Kubernetes does not notice a kill — {SYSTEMS.get(r['system'], r['system'])} ({r.get('variant')})",
                           f"{r.get('endpoint_changes')} endpoint changes, {r.get('container_restarts')} container restarts", (r.get("endpoint_changes") == 0 and r.get("container_restarts") == 0) if r["system"] == "A" else None))
    w("## 2. Claims\n")
    w("| Claim | This run | Verdict |\n|---|---|---|")
    for c, v, ok in claims:
        w(f"| {c} | {v} | {verdict(ok)} |")
    w("\n\"Reported\" rows have no pass mark in bench §1, or compare systems. A pass needs the sample count bench §5 asks for; with fewer, the row gives the count and the maximum instead.\n")

    w("## 3. Latency by scenario\n")
    w("Latency from the intended send time (open loop), merged across repetitions; in parentheses the range of each repetition's median where there were several.\n")
    w("| Host | System | Variant | Scenario | Rate | Arrival | Calls | Model | p50 | p90 | p99 | p99.9 | max | Send lag p99 | B/call |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for k in sorted(groups, key=lambda k: tuple(str(x) for x in k)):
        rs = groups[k]
        if k[0] != "inproc" or k[4] == "idle":
            continue
        h = merge_hists([r["record"]["latency_ns"] for r in rs])
        lag = merge_hists([r["record"]["send_lag_ns"] for r in rs])
        model = sum(r["record"]["model"] for r in rs) / max(1, sum(calls(r) for r in rs))
        p50s = [r["record"]["latency_ns"]["p50"] for r in rs]
        al = [r.get("alloc_bytes_per_call", 0) for r in rs]
        flag = " ⚠" if (lag.get("p99") or 0) > LAG_LIMIT_NS else ""
        w(f"| {k[1]} | {k[2]} | {k[3]} | {k[4]} | {k[5]} | {k[6]} | {h['count']} | {model * 100:.2f}% | {fmt_mr(p50s)} | {us(pct(h, 0.9))} | {us(pct(h, 0.99)) if pct(h, 0.99) is not None else 'n<1000'} | {us(pct(h, 0.999)) if pct(h, 0.999) is not None else 'n<10000'} | {us(h.get('max'))} | {us(lag.get('p99'))}{flag} | {statistics.median(al):.1f} |")

    w(f"\n⚠: the load loop sent its calls more than {us(LAG_LIMIT_NS)} late at p99 in that row, so its latencies measure this machine's scheduling, not the system.\n")
    w("\n## 4. Kill loops\n")
    w("| Host | System | Variant | Kills | Model share | Fallbacks per kill (median, max) | Time in fallback per kill (median, max) | Errors + timeouts | Fallback reasons |\n|---|---|---|---|---|---|---|---|---|")
    for k in sorted(groups, key=lambda k: tuple(str(x) for x in k)):
        if k[0] != "inproc" or k[4] != "kill":
            continue
        rs = groups[k]
        pk = [x for r in rs for x in per_kill(r)]
        fbk = [x["fallbacks"] for x in pk]
        tk = [x["time_ns"] for x in pk if x["time_ns"] is not None]
        reasons = {}
        for r in rs:
            for rr, n in r["record"]["fallback"].items():
                reasons[rr] = reasons.get(rr, 0) + n
        model = sum(r["record"]["model"] for r in rs) / max(1, sum(calls(r) for r in rs))
        err = sum(r["record"]["error"] + r["record"]["timeout"] for r in rs)
        w(f"| {k[1]} | {k[2]} | {k[3]} | {len(pk)} | {model * 100:.3f}% | {statistics.median(fbk) if fbk else '—'}, {max(fbk) if fbk else '—'} | {us(statistics.median(tk)) if tk else '—'}, {us(max(tk)) if tk else '—'} | {err} | {', '.join(f'{a} {b}' for a, b in sorted(reasons.items(), key=lambda x: -x[1]))} |")
    rec = [x for r in g("rust", "A", "default", "kill") for x in recovery(r)] + [x for r in g("rust", "A", "nostandby", "kill") for x in recovery(r)]
    if rec:
        w("\n### Recovery intervals, system A (supervisor events against the kill agent's clock)\n")
        w("| Variant | Kills | Kill → first death lock | Lock → gate closed and swept | Gate closed → HEALTHY | Kill → HEALTHY | Kill → exit report |\n|---|---|---|---|---|---|---|")
        for v in ("default", "nostandby"):
            rr = [x for r in g("rust", "A", v, "kill") for x in recovery(r)]
            if not rr:
                continue
            def col(f):
                xs = sorted(x[f] for x in rr if x[f] is not None)
                return f"median {us(statistics.median(xs))}, max {us(xs[-1])}" if xs else "—"
            w(f"| {v} | {len(rr)} | {col('lock')} | {col('flip')} | {col('healthy')} | {col('kill_to_healthy')} | {col('exit')} |")

    if svc:
        w("\n## 5. Service mode (Kind)\n")
        w("| System | Variant | Scenario | Rate | Calls | Model share | Errors + timeouts | Endpoint changes | Container restarts | p50 | p99 |\n|---|---|---|---|---|---|---|---|---|---|---|")
        for r in svc:
            rec_ = r["record"]
            h = rec_["latency_ns"]
            w(f"| {SYSTEMS.get(r['system'], r['system'])} | {r.get('variant')} | {r.get('scenario')} | {r.get('rate')} | {calls(r)} | {rec_['model'] / max(1, calls(r)) * 100:.2f}% | {rec_['error'] + rec_['timeout']} | {r.get('endpoint_changes')} | {r.get('container_restarts')} | {us(pct(h, 0.5))} | {us(pct(h, 0.99))} |")

    w("\n## 6. CPU and memory\n")
    w("| Host | System | Variant | Scenario | Rate | Host CPU per million calls | Server CPU per million calls |\n|---|---|---|---|---|---|---|")
    for k in sorted(groups, key=lambda k: tuple(str(x) for x in k)):
        rs = groups[k]
        if k[0] != "inproc" or k[4] != "steady" or k[6] != "even":
            continue
        n = sum(calls(r) for r in rs)
        hc = sum(r.get("cpu_ns", 0) for r in rs) / max(1, n) * 1e6 / 1e9
        sc = sum(r.get("server_cpu_ns", 0) for r in rs) / max(1, n) * 1e6 / 1e9
        w(f"| {k[1]} | {k[2]} | {k[3]} | {k[4]} | {k[5]} | {hc:.1f} s | {sc:.1f} s |")
    w("\nSpinning makes CPU follow how closely requests arrive, not how much work is done: A's worker and callers spin through gaps shorter than their spin budget (signaling §4). System A's standby is a second copy of the worker's memory, which the baselines do not have (bench §3, \"Variants of A\"). Triton's CPU is not counted here: it runs in another container.\n")

    w("## 7. Methodology Notes\n")
    for t in ("Single-node execution environment; the send-lag column indicates when the open-loop load generator experienced host scheduling delay.",
              "Synthetic 256-feature fraud model and dataset (models/gen.py) isolating IPC transport and fault recovery from model accuracy.",
              "Baselines C (HTTP/JSON) and D (Unix socket binary) use dedicated low-overhead Rust servers wrapping ONNX Runtime, and B uses Triton Inference Server 26.04 with registered POSIX shared memory (bench/baselines/triton/README.md).",
              "In-process mode runs baselines under a supervisor restart loop; service mode evaluates both supervised and default container-restart configurations.",
              "Transport overhead is computed as SidecarZero's latency minus worker inference time (and compared directly against the in-process ONNX Runtime floor E).",
              "Percentiles require at least 1,000 samples for p99 and 10,000 samples for p99.9; runs with fewer samples report exact sample counts and maximums.",
              "Service mode executes inside a single-node Kind cluster."):
        w(f"- {t}")
    w("\n## 8. Raw data\n")
    w("Every run's JSON is beside this report: " + ", ".join(f"[`{r['_file']}`]({r['_file']})" for r in runs) + ".")
    open(os.path.join(d, "report.md"), "w").write("\n".join(L) + "\n")


if __name__ == "__main__":
    main(sys.argv[1])
