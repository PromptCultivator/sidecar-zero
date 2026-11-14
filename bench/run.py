#!/usr/bin/env python3
"""The chaos benchmark's orchestrator (bench §6): run every system under every scenario, in
turn, and write the raw results that bench/report/report.py turns into the report.

    python3 bench/run.py [--quick] [--systems=A,B,C,D,E] [--scenarios=...] [--repeats=N]
                         [--kills=N] [--hosts=rust,java] [--modes=inproc,service] [--out=DIR]

Run it through `make -C bench chaos-bench-quick` (or `chaos-bench`), which runs it in the dev
image with the Docker socket. Results land in bench/results/<timestamp>/: one JSON file per run
(system, variant, scenario, repetition), each holding the host's or load generator's record, the
kill agent's kill times, and the supervisor's timestamped events.

In-process mode (bench §4.2) runs here: the sidecar (A) and the baseline servers (C, D) are
processes of this container, Triton (B) a container of its own whose IPC and network namespaces
the host joins, and the in-process floor (E) is the host itself. Service mode (bench §4.1) runs
in a Kind cluster (bench/service.py).
"""

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, ".."))
REL = os.path.join(ROOT, "target/release")
ARCH = os.uname().machine
MUSL = os.path.join(ROOT, f"target/{ARCH}-unknown-linux-musl/release")
MODELS = os.path.join(ROOT, "target/models")
ROWS = os.path.join(MODELS, "reference-features.f32")
JAVA_HOST = os.path.join(ROOT, "target/sdk/java-bench/scz-bench-host-java")
TRITON_IMAGE = "nvcr.io/nvidia/tritonserver@sha256:aa5fca20416939f243d587d56557d15f9d2bb81de30c6a4464707225250320fb"


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def sh(*args, check=True, timeout=900, **kw):
    r = subprocess.run(args, capture_output=True, text=True, timeout=timeout, **kw)
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(args)}: exit {r.returncode}: {(r.stderr or r.stdout).strip()[-600:]}")
    return r.stdout


def mono_ns():
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC)


def kill_agent(pid, sig="KILL", via=None):
    """Kill with the kill agent and return its CLOCK_MONOTONIC reading (bench §5)."""
    cmd = [f"{MUSL}/scz-kill-agent", f"--sig={sig}", str(pid)]
    if via:  # another container: the agent was copied into it
        cmd = ["docker", "exec", via, "/tmp/scz-kill-agent", f"--sig={sig}", str(pid)]
    out = sh(*cmd, check=False)
    m = re.search(r"KILL (\d+) \d+ \w+ ok", out)
    return int(m.group(1)) if m else None


def children(pid):
    try:
        return [int(x) for x in open(f"/proc/{pid}/task/{pid}/children").read().split()]
    except OSError:
        return []


def cmdline(pid):
    try:
        return open(f"/proc/{pid}/cmdline").read().replace("\0", " ")
    except OSError:
        return ""


def proc_cpu_ns(pids):
    """User + system CPU of these processes, from /proc/<pid>/stat."""
    tick = os.sysconf("SC_CLK_TCK")
    total = 0
    for p in pids:
        try:
            f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            total += (int(f[11]) + int(f[12])) * 1_000_000_000 // tick
        except (OSError, IndexError):
            pass
    return total


# ── the systems' servers ────────────────────────────────────────────────────────────────────

class Sidecar:
    """System A: the supervisor and the ONNX Runtime worker, with the variants of bench §3."""

    def __init__(self, d, variant, model="fraud.ort", threads=1):
        self.d, self.variant = d, variant
        self.sock, self.admin = f"{d}/scz.sock", f"{d}/admin.sock"
        self.model_path = f"{d}/model.ort"
        shutil.copy(f"{MODELS}/{model}", self.model_path)
        args = [f"{REL}/scz-supervisor", f"--socket={self.sock}", f"--admin={self.admin}", f"--worker={REL}/sidecarzero-worker",
                f"--model={self.model_path}", f"--rings={threads}", "--log-mono",
                "--standby=none" if variant == "nostandby" else "--standby=warm"]
        if variant == "nospin":
            args.append("--spin-us=0")
        self.log = open(f"{d}/supervisor.log", "w")
        self.p = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=self.log)
        self.wait_healthy(30)

    def status(self):
        return json.loads(sh(f"{REL}/sczctl", f"--admin={self.admin}", "status"))

    def wait_healthy(self, timeout, standby=None):
        end = time.time() + timeout
        while time.time() < end:
            try:
                s = self.status()
                warm = any(w["role"] == "Warm" for w in s["workers"])
                if s["state"] == "HEALTHY" and (standby is None or not standby or warm):
                    return s
            except (RuntimeError, json.JSONDecodeError):
                pass
            time.sleep(0.05)
        raise RuntimeError("the sidecar did not become healthy")

    def target(self):
        """The consuming worker's pid: what a kill of system A kills."""
        s = self.status()
        g = next((w["gen"] for w in s["workers"] if w["role"] == "Consumer"), None)
        for c in children(self.p.pid):
            if g is not None and f"--gen={g} " in cmdline(c) + " ":
                return c
        return None

    def pids(self):
        return [self.p.pid] + children(self.p.pid)

    def host_args(self):
        return [f"--socket={self.sock}"] + (["--no-spin"] if self.variant == "nospin" else [])

    def worker_stats(self):
        """The workers' own counters (sczctl dump): served requests and inference time."""
        out = sh(f"{REL}/sczctl", f"--admin={self.admin}", "dump", check=False)
        for line in out.splitlines():
            if line.startswith('{"worker_stats"'):
                return json.loads(line)["worker_stats"]
        return []

    def events(self):
        """The supervisor's timestamped events (--log-mono) that the recovery intervals need."""
        out = []
        for line in open(f"{self.d}/supervisor.log"):
            m = re.match(r"scz-supervisor: @(\d+) (.*)", line)
            if not m:
                continue
            t, msg = int(m.group(1)), m.group(2)
            for kind, pat in (("death_lock", r"death lock of gen (\d+) ring \d+ reported"), ("gate_closed", r"worker gen (\d+) died as the consumer"),
                              ("healthy", r"worker gen (\d+) ACTIVE; gate HEALTHY"), ("reopened", r"gate reopened"),
                              ("reaped", r"worker gen (\d+) exited and was reaped"), ("reload", r"handover: gen (\d+) released")):
                if re.search(pat, msg):
                    out.append([t, kind])
        return out

    def stop(self):
        self.p.send_signal(signal.SIGKILL)
        self.p.wait()
        self.log.close()


class Baseline:
    """Systems C and D: the native server under its restart loop (fairness rule 5)."""

    def __init__(self, d, kind, model="fraud.ort", threads=1):
        self.kind = kind
        self.addr = "127.0.0.1:18080" if kind == "C" else f"{d}/d.sock"
        flag = f"--http={self.addr}" if kind == "C" else f"--uds={self.addr}"
        self.log = open(f"{d}/server.log", "w")
        self.p = subprocess.Popen([f"{REL}/scz-bench-server", f"--model={MODELS}/{model}", flag, f"--threads={threads}", "--supervise"],
                                  stdout=subprocess.DEVNULL, stderr=self.log)
        end = time.time() + 30
        while time.time() < end and "inference thread" not in open(f"{d}/server.log").read():
            time.sleep(0.05)

    def target(self):
        c = children(self.p.pid)
        return c[0] if c else None

    def pids(self):
        return [self.p.pid] + children(self.p.pid)

    def host_args(self):
        return [f"--http={self.addr}"] if self.kind == "C" else [f"--uds={self.addr}"]

    def events(self):
        return []

    def stop(self):
        for p in children(self.p.pid):
            os.kill(p, signal.SIGKILL)
        self.p.send_signal(signal.SIGKILL)
        self.p.wait()
        self.log.close()


class Triton:
    """System B: Triton in a container of its own (bench/baselines/triton/run.sh), supervised."""

    NAME = "scz-bench-triton"

    def __init__(self, d, threads=1):
        # The script runs here, in the dev image; the daemon mounts the repository by its host path.
        env = dict(os.environ, TRITON_REPO_HOST=os.path.join(os.environ.get("SCZ_HOST_REPO", ROOT), "bench/baselines/triton/.repository"))
        sh("bash", f"{ROOT}/bench/baselines/triton/run.sh", "--supervised", "--instances", str(threads), "--name", self.NAME, MODELS, timeout=600, env=env)
        sh("docker", "cp", f"{MUSL}/scz-kill-agent", f"{self.NAME}:/tmp/scz-kill-agent")

    def target(self):
        out = sh("docker", "exec", self.NAME, "pgrep", "-x", "tritonserver", check=False).split()
        return int(out[0]) if out else None

    def pids(self):
        return []

    def host_args(self):
        return ["--triton=http://127.0.0.1:8001"]

    def events(self):
        return []

    def stop(self):
        sh("docker", "rm", "-f", self.NAME, check=False)


class Nothing:
    """System E: nothing outside the host."""

    def target(self):
        return None

    def pids(self):
        return []

    def host_args(self):
        return [f"--model={MODELS}/fraud.ort"]

    def events(self):
        return []

    def stop(self):
        pass


def start(system, variant, d, model="fraud.ort", threads=1):
    if system == "A":
        return Sidecar(d, variant, model, threads)
    if system in ("C", "D"):
        return Baseline(d, system, model, threads)
    if system == "B":
        return Triton(d, threads)
    return Nothing()


# ── scenarios ───────────────────────────────────────────────────────────────────────────────

def host_cmd(host, system, srv, spec, out):
    exe = f"{REL}/scz-bench-host" if host == "rust" else JAVA_HOST
    cmd = [exe, "inproc", f"--system={system}", f"--variant={spec['variant']}", f"--scenario={spec['scenario']}", f"--rate={spec['rate']}",
           f"--duration-s={spec['duration']}", f"--warmup-s={spec['warmup']}", f"--threads={spec['threads']}", f"--arrival={spec['arrival']}",
           f"--budget-us={spec['budget_us']}", f"--seed={spec['seed']}", f"--rows={ROWS}", f"--out={out}"] + srv.host_args()
    if system == "B":
        # The host joins Triton's IPC and network namespaces: its shared memory and 127.0.0.1:8001.
        dev = os.environ["SCZ_DEV_IMAGE"]
        repo = os.environ["SCZ_HOST_REPO"]
        cmd = ["docker", "run", "--rm", f"--ipc=container:{Triton.NAME}", f"--network=container:{Triton.NAME}", "-v", f"{repo}:/w",
               "-v", "scz-dev-target:/w/target", "-w", "/w", dev] + cmd
    return cmd


def inject(kind, srv, spec, t_start, t_end, kills, stop):
    """The chaos of one scenario, from its start to its end, recording each kill's time."""
    if kind == "kill":
        time.sleep(max(0.0, (t_start - mono_ns()) / 1e9) + 1.0)
        while mono_ns() + spec["kill_interval"] * 1e9 < t_end and not stop.is_set():
            # Every kill finds a warm standby, if the system has one (bench §4 "Kill loop").
            if isinstance(srv, Sidecar) and srv.variant != "nostandby":
                try:
                    srv.wait_healthy(30, standby=True)
                except RuntimeError:
                    pass
            pid = srv.target()
            if pid:
                t = kill_agent(pid, via=Triton.NAME if isinstance(srv, Triton) else None)
                if t:
                    kills.append(t)
            stop.wait(spec["kill_interval"])
    elif kind == "freeze":
        time.sleep(max(0.0, (t_start - mono_ns()) / 1e9) + spec["duration"] / 3)
        pid = srv.target()
        if pid:
            via = Triton.NAME if isinstance(srv, Triton) else None
            t = kill_agent(pid, "STOP", via)
            if t:
                kills.append(t)
            time.sleep(2)
            kill_agent(pid, "CONT", via)
    elif kind == "reload" and isinstance(srv, Sidecar):
        time.sleep(max(0.0, (t_start - mono_ns()) / 1e9) + 2)
        flip = 0
        while mono_ns() + spec["reload_interval"] * 1e9 < t_end and not stop.is_set():
            # A new file with the same schema, renamed into place (kubernetes-deploy §2).
            flip ^= 1
            tmp = srv.model_path + ".new"
            shutil.copy(f"{MODELS}/{'fraud-b.ort' if flip else 'fraud.ort'}", tmp)
            kills.append(mono_ns())
            os.rename(tmp, srv.model_path)
            stop.wait(spec["reload_interval"])


def run_one(host, system, spec, d, out):
    """One run: start the system, its injector, and the host; collect everything."""
    srv = start(system, spec["variant"], d, spec.get("model", "fraud.ort"), spec.get("model_threads", 1))
    try:
        kills, events_stop = [], threading.Event()
        if spec["scenario"] == "idle":
            pids = srv.pids()
            c0, t0 = proc_cpu_ns(pids), mono_ns()
            time.sleep(spec["duration"])
            c1, t1 = proc_cpu_ns(srv.pids()), mono_ns()
            res = {"host": host, "mode": "inproc", "system": system, "variant": spec["variant"], "scenario": "idle",
                   "duration_s": spec["duration"], "sidecar_cpu_ratio": (c1 - c0) / (t1 - t0)}
            json.dump(res, open(out, "w"))
            return res
        # The host's schedule begins 0.2 s after it starts, then warms up.
        t_start = mono_ns() + int((0.2 + spec["warmup"]) * 1e9)
        t_end = t_start + int(spec["duration"] * 1e9)
        inj = threading.Thread(target=inject, args=(spec["scenario"], srv, spec, t_start, t_end, kills, events_stop), daemon=True)
        inj.start()
        cpu0 = proc_cpu_ns(srv.pids())
        r = subprocess.run(host_cmd(host, system, srv, spec, out), capture_output=True, text=True, timeout=spec["duration"] + spec["warmup"] + 300)
        events_stop.set()
        inj.join(timeout=60)
        cpu1 = proc_cpu_ns(srv.pids())
        if r.returncode != 0:
            raise RuntimeError(f"the host failed: {r.stderr.strip()[-800:]}")
        log("   ", r.stderr.strip().splitlines()[-1] if r.stderr.strip() else "")
        res = json.load(open(out))
        res.update({"kills_ns": kills, "events": srv.events(), "server_cpu_ns": cpu1 - cpu0, "spec": spec})
        if isinstance(srv, Sidecar):
            res["worker_stats"] = srv.worker_stats()
        json.dump(res, open(out, "w"))
        return res
    finally:
        srv.stop()


def plan(quick, systems, scenarios, kills, infer_ns, mlp_ns):
    """The runs of bench §4.2's scenarios, as (system, spec) pairs. Quick: rates, durations,
    and the kill count cut down for a laptop (bench §6): the spinning regime at 30,000 a second
    from four caller threads, which a laptop VM can schedule, instead of 50,000 from eight.

    Model threads are derived, not assumed (bench §2): each at most 70% busy at the rate, from
    the inference time measured first; every system gets the same count (fairness rule 3)."""
    dur = 10 if quick else 60
    warm = 3 if quick else 30
    hi = 30000 if quick else 50000
    callers = 4 if quick else 8
    mthreads = lambda rate: max(1, -(-int(rate * infer_ns) // int(0.7e9)))  # noqa: E731
    base = {"variant": "default", "threads": callers, "arrival": "even", "budget_us": 2000, "seed": 1, "duration": dur, "warmup": warm, "model_threads": 1}
    out = []
    for s in systems:
        variants = ["default", "nospin", "nostandby"] if s == "A" else ["default"]
        for sc in scenarios:
            if sc == "steady":
                for rate in ([hi, 2000, 10] if s in ("A", "E") else [hi, 2000]):
                    for v in (variants if rate == hi else ["default"]):
                        if v == "nostandby":
                            continue  # the standby does not change steady state
                        out.append((s, dict(base, scenario="steady", rate=rate, variant=v, model_threads=mthreads(rate), duration=dur if rate >= 2000 else dur * 3)))
                out.append((s, dict(base, scenario="steady", rate=hi, arrival="poisson", model_threads=mthreads(hi))))
            elif sc == "kill" and s != "E":
                interval = 2.0
                for v in [v for v in variants if v != "nospin"]:
                    out.append((s, dict(base, scenario="kill", rate=5000, variant=v, kill_interval=interval, duration=kills * interval + 3)))
                # The control run of "no request is lost": the same rate and length, no kills.
                out.append((s, dict(base, scenario="control", rate=5000, duration=kills * interval + 3)))
            elif sc == "freeze" and s != "E":
                out.append((s, dict(base, scenario="freeze", rate=5000)))
            elif sc == "reload" and s == "A":
                out.append((s, dict(base, scenario="reload", rate=5000, reload_interval=5 if quick else 30, duration=dur * 3)))
            elif sc == "overload" and s == "A":
                # 1.5 times one worker thread's capacity (bench §4 "Overload"), with the MLP of
                # bench §2: the fraud model's capacity is more than a laptop can send.
                out.append((s, dict(base, scenario="overload", rate=int(1.5e9 / mlp_ns), model="mlp.ort", model_threads=1)))
            elif sc == "idle" and s == "A":
                out.append((s, dict(base, scenario="idle", rate=0)))
    return out


def measure_inference(model="fraud.ort"):
    """A model's inference time per call, before anything else runs (bench §2): system E,
    one thread, a gentle rate, the median."""
    d = tempfile.mkdtemp(prefix="scz-bench-")
    out = os.path.join(d, "e.json")
    sh(f"{REL}/scz-bench-host", "inproc", "--system=E", "--rate=2000", "--duration-s=3", "--warmup-s=1", "--threads=1", f"--rows={ROWS}",
       f"--out={out}", f"--model={MODELS}/{model}")
    ns = json.load(open(out))["record"]["model_latency_ns"]["p50"]
    shutil.rmtree(d, ignore_errors=True)
    return ns


def environment():
    def read(p):
        try:
            return open(p).read().strip()
        except OSError:
            return ""
    cpu = [l.split(":", 1)[1].strip() for l in read("/proc/cpuinfo").splitlines() if l.lower().startswith(("model name", "cpu part"))]
    gov = read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") or "unavailable"
    return {
        "kernel": os.uname().release, "arch": ARCH, "cpus": os.cpu_count(), "cpu": cpu[0] if cpu else "unknown",
        "governor": gov, "cgroup": "v2" if os.path.exists("/sys/fs/cgroup/cgroup.controllers") else "v1",
        "vm": "linuxkit" in os.uname().release or os.path.exists("/proc/xen"),
        "commit": sh("git", "-C", ROOT, "rev-parse", "--short", "HEAD", check=False).strip() or "uncommitted",
        "dirty": bool(sh("git", "-C", ROOT, "status", "--porcelain", check=False).strip()),
        "onnxruntime": "1.31.1", "triton": TRITON_IMAGE,
    }


def main(argv):
    opt = {"quick": False, "systems": "A,B,C,D,E", "scenarios": "steady,kill,freeze,reload,overload,idle", "repeats": None, "kills": None,
           "hosts": "rust", "modes": "inproc", "out": None}
    for a in argv:
        if a == "--quick":
            opt["quick"] = True
        elif a.startswith("--") and "=" in a:
            k, v = a[2:].split("=", 1)
            opt[k] = v
        else:
            print(f"unknown argument {a}", file=sys.stderr)
            return 2
    quick = opt["quick"]
    repeats = int(opt["repeats"] or (1 if quick else 5))
    kills = int(opt["kills"] or (10 if quick else 1000))
    out = opt["out"] or os.path.join(HERE, "results", time.strftime("%Y%m%d-%H%M%S"))
    os.makedirs(out, exist_ok=True)
    systems = opt["systems"].split(",")
    hosts = opt["hosts"].split(",")
    if "B" in systems and not sh("docker", "image", "inspect", TRITON_IMAGE, check=False).strip().startswith("["):
        log(f"system B skipped: {TRITON_IMAGE} is not present")
        systems.remove("B")
    log("building")
    sh("cargo", "build", "-q", "--release", "--workspace", "--bins", timeout=3600)
    sh("cargo", "build", "-q", "--release", "--target", f"{ARCH}-unknown-linux-musl", "-p", "scz-kill-agent", "-p", "scz-loadgen", timeout=3600)
    sh("python3", f"{ROOT}/models/gen.py", MODELS)
    if "java" in hosts:
        sh("bash", f"{ROOT}/bench/host-java/build.sh", timeout=1800)
    # The supervisor's size claim: the static binary, stripped (overview §1.3).
    sh("cargo", "build", "-q", "--release", "--target", f"{ARCH}-unknown-linux-musl", "-p", "scz-supervisor", timeout=3600)
    stripped = os.path.join(tempfile.mkdtemp(), "scz-supervisor")
    sh("strip", "-o", stripped, f"{MUSL}/scz-supervisor")
    meta = {"quick": quick, "repeats": repeats, "kills": kills, "systems": systems, "hosts": hosts, "environment": environment(),
            "supervisor_stripped_bytes": os.path.getsize(stripped),
            "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    json.dump(meta, open(os.path.join(out, "meta.json"), "w"), indent=2)
    infer_ns, mlp_ns = measure_inference(), measure_inference("mlp.ort")
    meta["inference_ns"] = {"fraud": infer_ns, "mlp": mlp_ns}
    log(f"inference here: fraud {infer_ns / 1e3:.1f} µs, mlp {mlp_ns / 1e3:.1f} µs a call")
    runs = plan(quick, systems, opt["scenarios"].split(","), kills, infer_ns, mlp_ns)
    failed = []
    if "inproc" in opt["modes"]:
        for host in hosts:
            for system, spec in runs:
                if host == "java" and system in ("B", "E"):
                    continue
                for rep in range(repeats):
                    spec = dict(spec, seed=rep + 1)
                    name = f"inproc-{host}-{system}-{spec['variant']}-{spec['scenario']}-{spec['rate']}-{spec['arrival']}-r{rep + 1}"
                    log(f"{name}")
                    d = tempfile.mkdtemp(prefix="scz-bench-")
                    try:
                        run_one(host, system, spec, d, os.path.join(out, name + ".json"))
                    except Exception as e:  # noqa: BLE001 - one failed run is recorded; the others go on
                        failed.append(name)
                        log(f"  FAILED: {e}")
                        json.dump({"name": name, "failed": str(e), "spec": spec, "system": system, "host": host}, open(os.path.join(out, name + ".json"), "w"))
                    finally:
                        shutil.rmtree(d, ignore_errors=True)
    if "service" in opt["modes"]:
        sys.path.insert(0, HERE)
        import service  # noqa: E402 - only with Kind
        failed += service.run(out, systems, quick, repeats, kills)
    meta["finished"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    meta["failed"] = failed
    json.dump(meta, open(os.path.join(out, "meta.json"), "w"), indent=2)
    sh("python3", f"{HERE}/report/report.py", out)
    log(f"report: {os.path.relpath(os.path.join(out, 'report.md'), ROOT)}" + (f"; {len(failed)} runs failed" if failed else ""))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
