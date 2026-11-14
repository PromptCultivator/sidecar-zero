#!/usr/bin/env python3
"""The Kind end-to-end tests of kubernetes-deploy §10.

    python3 deploy/kind/e2e.py [--k8s=1.37] [--matrix] [--keep] [--list] [TEST...]

Run through scripts/kind-e2e, which builds the images and runs this in the dev image with the
Docker socket. Each run makes a Kind cluster named scz-e2e-<version>, loads the images, runs
the tests (each in a namespace of its own, with the example chart deploy/helm-kep753/
examples/fraud-scorer), and deletes the cluster unless --keep. With --matrix the kill test runs
on every Kubernetes version Kind v0.33.0 publishes a node image for, then the other tests run
on the newest; the OOM test's singleProcess row runs on a cluster whose kubelet has
singleProcessOOMKill. Results are written to deploy/kind/results/<version>.json.

A test passes or fails on its assertions. The OOM test records what happened under each
shield mode; it fails only when a mode does not do what kubernetes-deploy §5 says it does.
"""

import json
import os
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "../.."))
CHART = os.path.join(ROOT, "deploy/helm-kep753/examples/fraud-scorer")
RESULTS = os.path.join(HERE, "results")

# The node images Kind v0.33.0 publishes (its release notes), pinned by digest.
NODE_IMAGES = {
    "1.37": "kindest/node:v1.37.0@sha256:a1ed56cfb0e7b93589bdf97c8cd566405a265939e3620fc4f5de89adff580ae5",
    "1.36": "kindest/node:v1.36.4@sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed",
    "1.35": "kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0",
    "1.34": "kindest/node:v1.34.11@sha256:44e222ee2132dab25ff87301682f89eb82c7880ea3a1bf543bfe9708fd08d67d",
}
IMAGES = ["sidecarzero-ort-fraud:dev", "sidecarzero-chaos:dev", "scz-demo-host:dev"]
ADMIN = "/run/sidecarzero-admin/admin.sock"


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


class Fail(Exception):
    pass


def check(cond, msg):
    if not cond:
        raise Fail(msg)


def sh(*args, check=True, timeout=300, input=None):
    r = subprocess.run(args, capture_output=True, text=True, timeout=timeout, input=input)
    if check and r.returncode != 0:
        raise Fail(f"{' '.join(args)}: exit {r.returncode}: {(r.stderr or r.stdout).strip()[-800:]}")
    return r.stdout


def kubectl(*args, **kw):
    return sh("kubectl", *args, **kw)


def kjson(*args):
    return json.loads(kubectl(*args, "-o", "json"))


def wait_for(what, fn, timeout=60, every=0.5):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except (Fail, json.JSONDecodeError, KeyError, IndexError, subprocess.TimeoutExpired) as e:
            last = e
        time.sleep(every)
    raise Fail(f"timed out after {timeout} s waiting for {what}" + (f" ({last})" if last else ""))


# ── the cluster ───────────────────────────────────────────────────────────────────────────

class Cluster:
    def __init__(self, version, single_process=False):
        self.version = version
        self.name = f"scz-e2e-{version.replace('.', '-')}" + ("-sp" if single_process else "")
        self.node = f"{self.name}-control-plane"
        self.single_process = single_process
        self.kubeconfig = os.path.join(tempfile.gettempdir(), f"kubeconfig-{self.name}")

    def create(self):
        if self.name not in sh("kind", "get", "clusters").split():
            cfg = "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n"
            if self.single_process:
                cfg += "kubeadmConfigPatches:\n  - |\n    kind: KubeletConfiguration\n    singleProcessOOMKill: true\n"
            log(f"creating {self.name} ({NODE_IMAGES[self.version].split('@')[0]}{', singleProcessOOMKill' if self.single_process else ''})")
            sh("kind", "create", "cluster", "--name", self.name, "--image", NODE_IMAGES[self.version], "--config", "-", "--wait", "180s", input=cfg, timeout=900)
        # This process runs in a container: join Kind's network and use the internal address.
        me = os.uname().nodename
        if me not in sh("docker", "network", "inspect", "kind", "-f", "{{range .Containers}}{{.Name}} {{end}}"):
            sh("docker", "network", "connect", "kind", me, check=False)
        with open(self.kubeconfig, "w") as f:
            f.write(sh("kind", "get", "kubeconfig", "--internal", "--name", self.name))
        os.environ["KUBECONFIG"] = self.kubeconfig
        wait_for("the API server", lambda: kubectl("get", "--raw", "/readyz", check=False).strip() == "ok", 120)
        log(f"loading {', '.join(IMAGES)}")
        sh("kind", "load", "docker-image", "--name", self.name, *IMAGES, timeout=900)
        self.server = kubectl("version", "-o", "json", check=False)

    def delete(self):
        sh("kind", "delete", "cluster", "--name", self.name, check=False, timeout=300)

    def node_sh(self, script, check=True):
        return sh("docker", "exec", self.node, "sh", "-c", script, check=check)

    def pids(self, pattern):
        """Pids of processes on the node whose command line matches `pattern` (pgrep -f)."""
        out = self.node_sh(f"pgrep -f -- '{pattern}' || true")
        return [int(p) for p in out.split()]

    def kill(self, pid, sig="KILL"):
        self.node_sh(f"kill -{sig} {pid}")


# ── one release of the example chart ──────────────────────────────────────────────────────

ORT = {"sidecarzero": {"image": {"repository": "sidecarzero-ort-fraud", "tag": "dev"}, "model": {"path": "/models/fraud.ort"}}}


def chaos(release, script="", extra=None):
    """Values for the chaos sidecar serving `script` from a ConfigMap, and a keyed host."""
    v = {
        "sidecarzero": {
            "image": {"repository": "sidecarzero-chaos", "tag": "dev"},
            "model": {"path": "/models/model", "volume": {"configMap": {"name": f"{release}-model"}}},
        },
        "app": {"args": ["--codec=keyed", "--threads=2", "--budget-us=2000", "--interval-us=500", "--http=0.0.0.0:8080"]},
    }
    return merge(v, extra or {})


def merge(a, b):
    out = dict(a)
    for k, v in b.items():
        out[k] = merge(out[k], v) if isinstance(v, dict) and isinstance(out.get(k), dict) else v
    return out


class Release:
    def __init__(self, cluster, name, values, script=None, ns=None, labels=None):
        self.c, self.name, self.ns = cluster, name, ns or name
        self.values, self.script = values, script
        # A namespace of the same name from an earlier run may still be terminating.
        wait_for(f"namespace {self.ns} gone or active", lambda: kubectl("get", "namespace", self.ns, "-o", "jsonpath={.status.phase}", check=False) != "Terminating", 180, 1)
        kubectl("create", "namespace", self.ns, check=False)
        for k, v in (labels or {}).items():
            kubectl("label", "namespace", self.ns, f"{k}={v}", "--overwrite")
        if script is not None:
            self.set_script(script)

    def set_script(self, script):
        with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
            f.write("SCZCHAOS-SCRIPT\n" + script)
        y = kubectl("create", "configmap", f"{self.name}-model", "-n", self.ns, f"--from-file=model={f.name}", "--dry-run=client", "-o", "yaml")
        kubectl("apply", "-f", "-", input=y)
        os.unlink(f.name)

    def install(self, values=None, wait=True):
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(merge(self.values, values or {}), f)
        sh("helm", "upgrade", "--install", self.name, CHART, "-n", self.ns, "-f", f.name, timeout=120)
        os.unlink(f.name)
        if wait:
            self.wait_ready()

    def uninstall(self):
        # Called from each test's `finally`: if a test is failing, keep what explains it first.
        if sys.exc_info()[1] is not None:
            self.diagnose()
        sh("helm", "uninstall", self.name, "-n", self.ns, "--wait", check=False, timeout=120)
        kubectl("delete", "namespace", self.ns, "--wait=false", check=False)

    def diagnose(self):
        """The pod's status and events, both containers' logs, and `sczctl status`, written to
        deploy/kind/results/<release>-failure.log for a failed test."""
        os.makedirs(RESULTS, exist_ok=True)
        path = os.path.join(RESULTS, f"{self.c.version}-{self.name}-failure.log")
        parts = [("pods", ["kubectl", "get", "pods", "-n", self.ns, "-o", "wide"]),
                 ("describe", ["kubectl", "describe", "pods", "-n", self.ns, "-l", f"app={self.name}"])]
        for p in kjson("get", "pods", "-n", self.ns, "-l", f"app={self.name}").get("items", []):
            n = p["metadata"]["name"]
            parts += [(f"{n} sidecarzero", ["kubectl", "logs", "-n", self.ns, n, "-c", "sidecarzero", "--tail=200"]),
                      (f"{n} sidecarzero (previous)", ["kubectl", "logs", "-n", self.ns, n, "-c", "sidecarzero", "--previous", "--tail=100"]),
                      (f"{n} app", ["kubectl", "logs", "-n", self.ns, n, "-c", "app", "--tail=40"]),
                      (f"{n} sczctl status", ["kubectl", "exec", "-n", self.ns, n, "-c", "sidecarzero", "--", "/usr/bin/sczctl", f"--admin={ADMIN}", "status"])]
        with open(path, "w") as f:
            for title, cmd in parts:
                r = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
                f.write(f"===== {title}\n{r.stdout}{r.stderr}\n")
        log(f"  diagnostics in {os.path.relpath(path, ROOT)}")

    def pods(self):
        return [p for p in kjson("get", "pods", "-n", self.ns, "-l", f"app={self.name}")["items"] if not p["metadata"].get("deletionTimestamp")]

    def pod(self):
        ps = self.pods()
        check(len(ps) >= 1, "no pod")
        return sorted(ps, key=lambda p: p["metadata"]["creationTimestamp"])[-1]

    def pod_name(self):
        return self.pod()["metadata"]["name"]

    def ready(self, pod=None):
        p = pod or self.pod()
        return any(c["type"] == "Ready" and c["status"] == "True" for c in p["status"].get("conditions", []))

    def wait_ready(self, timeout=180):
        wait_for(f"{self.ns}/{self.name} ready", lambda: self.ready(), timeout)

    def restarts(self):
        p = self.pod()
        st = p["status"].get("initContainerStatuses", []) + p["status"].get("containerStatuses", [])
        return {c["name"]: c.get("restartCount", 0) for c in st}

    def container(self, name):
        p = self.pod()
        for c in p["status"].get("initContainerStatuses", []) + p["status"].get("containerStatuses", []):
            if c["name"] == name:
                return c
        raise Fail(f"no container {name}")

    def stats(self, pod=None):
        """The demo host's counts so far, from its last log line (it prints one a second)."""
        def last():
            lines = kubectl("logs", "-n", self.ns, pod or self.pod_name(), "-c", "app", "--tail=5", check=False).strip().splitlines()
            js = [json.loads(l) for l in lines if l.startswith("{")]
            return js[-1] if js else None
        return wait_for("the host's counts", last, 30)

    def sczctl(self, *cmd, check=True):
        return kubectl("exec", "-n", self.ns, self.pod_name(), "-c", "sidecarzero", "--", "/usr/bin/sczctl", f"--admin={ADMIN}", *cmd, check=check)

    def status(self):
        return json.loads(self.sczctl("status"))

    def consumer_gen(self):
        s = self.status()
        g = [w["gen"] for w in s["workers"] if w["role"] == "Consumer"]
        return g[0] if g and s["state"] == "HEALTHY" else None

    def http(self, port, path):
        """A GET to the pod, from the node (the API server's proxy refuses to relay a 503)."""
        ip = self.pod()["status"]["podIP"]
        return self.c.node_sh(f"curl -s -m 5 http://{ip}:{port}{path}", check=False)

    def wait_model(self, timeout=60):
        """Until the host's model answers rise."""
        m0 = wait_for("the host's first counts", self.stats, 30)["model"]
        return wait_for("model answers", lambda: self.stats()["model"] > m0 + 100, timeout)

    def sidecar_pid(self):
        """The supervisor's pid on the node: the container's PID 1."""
        cid = self.container("sidecarzero")["containerID"].split("//")[-1]
        info = json.loads(self.c.node_sh(f"crictl inspect {cid}"))
        return info["info"]["pid"]

    def worker_pid(self, gen):
        """A worker of this release, by generation: a child of the supervisor."""
        sup = self.sidecar_pid()
        kids = self.c.node_sh(f"cat /proc/{sup}/task/{sup}/children").split()
        for k in kids:
            cmd = self.c.node_sh(f"tr '\\0' ' ' < /proc/{k}/cmdline", check=False)
            if f"--gen={gen} " in cmd + " ":
                return int(k)
        raise Fail(f"no worker of generation {gen} among {kids}")


class SliceWatch(threading.Thread):
    """Every change of the release's EndpointSlice endpoints' conditions, through the API's
    watch (kubernetes-deploy §10 test 2). The first state seen is the baseline."""

    def __init__(self, rel):
        super().__init__(daemon=True)
        self.rel, self.changes, self.state, self.events = rel, [], None, 0
        self.proc = subprocess.Popen(
            ["kubectl", "get", "endpointslices", "-n", rel.ns, "-l", f"kubernetes.io/service-name={rel.name}", "--watch", "--output-watch-events", "-o", "json"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)

    def run(self):
        dec, buf, slices = json.JSONDecoder(), "", {}
        for chunk in iter(lambda: self.proc.stdout.read(1), ""):
            buf += chunk
            if chunk != "}":
                continue
            try:
                ev, end = dec.raw_decode(buf.lstrip())
            except json.JSONDecodeError:
                continue
            buf = buf.lstrip()[end:]
            self.events += 1
            obj = ev.get("object", {})
            slices[obj.get("metadata", {}).get("name")] = obj if ev.get("type") != "DELETED" else {}
            state = tuple(sorted(
                (e.get("targetRef", {}).get("name", "?"), str(e.get("conditions", {}).get("ready")), str(e.get("conditions", {}).get("serving")), str(e.get("conditions", {}).get("terminating")))
                for s in slices.values() for e in (s.get("endpoints") or [])))
            if self.state is not None and state != self.state:
                self.changes.append((round(time.time(), 3), state))
            self.state = state

    def stop(self):
        self.proc.terminate()


# ── the tests ─────────────────────────────────────────────────────────────────────────────

TESTS = {}


def test(name, number):
    def deco(f):
        TESTS[name] = (number, f)
        return f
    return deco


@test("start", 1)
def t_start(c):
    """The sidecar starts before the host; the host starts on its fallback and moves to model answers."""
    r = Release(c, "start", ORT)
    try:
        r.install()
        side = r.container("sidecarzero")["state"].get("running", {}).get("startedAt")
        app = r.container("app")["state"].get("running", {}).get("startedAt")
        check(side and app and side <= app, f"the host started at {app}, the sidecar at {side}")
        r.wait_model()
        s = r.stats()
        check(s["wrong"] == 0, f"wrong answers: {s}")
        return {"sidecar_started": side, "host_started": app, "host": s}
    finally:
        r.uninstall()


@test("kill", 2)
def t_kill(c, kills=20):
    """kill -9 the active worker again and again: no EndpointSlice change, no restart, no host error."""
    r = Release(c, "kill", ORT)
    try:
        r.install()
        r.wait_model()
        w = SliceWatch(r)
        w.start()
        wait_for("the slice watch", lambda: w.state is not None, 30)
        before = r.stats()
        times = []
        for i in range(kills):
            # Each kill finds a warm standby, so that each is a promotion (the supervisor's
            # restart backoff can delay the replacement standby after many deaths).
            wait_for("a warm standby", lambda: any(x["role"] == "Warm" for x in r.status()["workers"]), 60, 0.2)
            g = wait_for("a HEALTHY gate", r.consumer_gen, 30, 0.2)
            pid = r.worker_pid(g)
            t0 = time.time()
            c.kill(pid)
            wait_for(f"promotion after kill {i + 1}", lambda: (r.consumer_gen() or g) != g, 30, 0.05)
            times.append(round(time.time() - t0, 3))
            time.sleep(1)
        time.sleep(5)
        after = r.stats()
        w.stop()
        restarts = r.restarts()
        s = r.status()
        result = {"kills": kills, "endpoint_changes": len(w.changes), "slice_events": w.events, "container_restarts": restarts,
                  "promotions": s["promotions"], "cold_restarts": s["restarts"], "max_promotion_ms": s["max_promotion_ms"],
                  "host_before": before, "host_after": after, "seconds_to_new_consumer": times}
        log(f"  {result}")
        check(not w.changes, f"EndpointSlice conditions changed: {w.changes}")
        check(all(v == 0 for v in restarts.values()), f"container restarts: {restarts}")
        check(after["wrong"] == 0, f"wrong answers: {after}")
        check(after["model"] > before["model"] + 1000, f"the model stopped answering: {before} → {after}")
        check(s["promotions"] >= kills, f"{s['promotions']} promotions for {kills} kills")
        return result
    finally:
        r.uninstall()


@test("leak", 3)
def t_leak(c):
    """A leaking worker: the supervisor hands over to the standby before the kernel acts. No restart."""
    r = Release(c, "leak", chaos("leak", extra={"sidecarzero": {"resources": {"limits": {"memory": "256Mi"}, "requests": {"memory": "256Mi"}}}}),
                script="gen=1\nleak_mb=8\n")
    try:
        r.install()
        before = r.stats()
        wait_for("the leaking worker replaced", lambda: (r.consumer_gen() or 1) > 1, 120, 1)
        time.sleep(5)
        after, s, restarts = r.stats(), r.status(), r.restarts()
        metrics = r.http(9464, "/metrics")
        fb = {k: after["fallback"].get(k, 0) - before["fallback"].get(k, 0) for k in after["fallback"]}
        result = {"status": {k: s[k] for k in ("gen", "spawns", "deaths", "reloads", "promotions", "last_cause")}, "restarts": restarts,
                  "fallbacks_during": fb, "calls_during": after["calls"] - before["calls"], "kernel_oom": [l for l in metrics.splitlines() if l.startswith("scz_kernel_oom")]}
        check(all(v == 0 for v in restarts.values()), f"container restarts: {restarts}")
        check(all(l.endswith(" 0") for l in result["kernel_oom"]), f"the kernel's OOM killer ran: {result['kernel_oom']}")
        return result
    finally:
        r.uninstall()


SPIKE_LIMIT = {"resources": {"limits": {"memory": "256Mi"}, "requests": {"memory": "256Mi"}}}
# The spike rows turn the memory monitor off: in this VM it catches even a 1 GiB populated
# mapping before the kernel does (the kernel reclaims for tens of milliseconds at the limit),
# so with it on, Layer 1 would never be reached. The last row keeps it on, and is recorded only.
OFF = ["--memory-monitor=off"]
SHIELDS = {
    # row: (values beyond the chaos defaults, what kubernetes-deploy §5 says survives:
    #       (PID 1, standby), or None to record only)
    "none": ({"sidecarzero": dict(SPIKE_LIMIT, oomShield={"mode": "none"}, extraArgs=OFF)}, (False, False)),
    "nestedCgroup": ({"sidecarzero": dict(SPIKE_LIMIT, oomShield={"mode": "nestedCgroup"}, privileged=True, extraArgs=OFF)}, (True, True)),
    "oomScoreAdj": ({"sidecarzero": dict(SPIKE_LIMIT, oomShield={"mode": "oomScoreAdj"}, extraArgs=OFF)}, (True, False)),
    "singleProcess": ({"sidecarzero": dict(SPIKE_LIMIT, oomShield={"mode": "singleProcess"}, extraArgs=OFF)}, (True, True)),
    "none+monitor": ({"sidecarzero": dict(SPIKE_LIMIT, oomShield={"mode": "none"})}, None),
}


@test("oom", 4)
def t_oom(c, modes=None):
    """A memory spike under each shield mode: does PID 1 survive, does the standby, do endpoints change?"""
    modes = modes or [m for m in SHIELDS if (m == "singleProcess") == c.single_process]
    out = {}
    for mode in modes:
        values, expect = SHIELDS[mode]
        name = f"oom-{mode.lower().replace('+', '-')}"
        r = Release(c, name, chaos(name, extra=values), script="gen=1\nspike_after_ms=3000\n")
        try:
            r.install()
            w = SliceWatch(r)
            w.start()
            wait_for("the slice watch", lambda: w.state is not None, 30)
            s0 = r.status()
            standby0 = [x["gen"] for x in s0["workers"] if x["role"] == "Warm"]
            time.sleep(15)  # the spike comes 3 s after activation; then recovery
            sup_restarts = r.restarts().get("sidecarzero", 0)
            pid1 = sup_restarts == 0
            st = r.status() if pid1 else None
            # The standby survived if it was promoted: generation 2 serves, and no cold start
            # of a third worker was needed in its place.
            standby = pid1 and bool(standby0) and st is not None and r.consumer_gen() == standby0[0]
            metrics = r.http(9464, "/metrics")
            row = {"mode_in_effect": next((l.split('"')[1] for l in metrics.splitlines() if l.startswith("scz_oom_shield_mode")), "?"),
                   "pid1_survived": pid1, "standby_survived": standby, "sidecar_restarts": sup_restarts,
                   "last_state": r.container("sidecarzero").get("lastState", {}).get("terminated", {}).get("reason"),
                   "endpoint_changes": len(w.changes),
                   "kernel_oom": [l for l in metrics.splitlines() if l.startswith("scz_kernel_oom")],
                   "status": st and {k: st[k] for k in ("gen", "spawns", "deaths", "promotions", "restarts", "last_cause")}}
            w.stop()
            out[mode] = row
            log(f"  {mode}: {row}")
            if expect is None:
                continue
            pid1_should, standby_should = expect
            check(row["mode_in_effect"] == mode or not pid1_should, f"{mode}: the supervisor settled on {row['mode_in_effect']}")
            check(pid1 == pid1_should, f"{mode}: PID 1 {'died' if pid1_should else 'survived'}, kubernetes-deploy §5 says otherwise: {row}")
            check(standby == standby_should, f"{mode}: the standby {'died' if standby_should else 'survived'}, kubernetes-deploy §5 says otherwise: {row}")
        finally:
            r.uninstall()
    return out


@test("supervisor-killed", 5)
def t_supervisor_killed(c):
    """Kill the supervisor: the sidecar restarts, and the host reconnects to the new arena without restarting."""
    r = Release(c, "supkill", ORT)
    try:
        r.install()
        r.wait_model()
        c.kill(r.sidecar_pid())
        wait_for("the sidecar restarted", lambda: r.restarts().get("sidecarzero", 0) >= 1, 60)
        r.wait_ready()
        r.wait_model(90)
        restarts = r.restarts()
        check(restarts.get("app", 0) == 0, f"the host restarted: {restarts}")
        return {"restarts": restarts, "host": r.stats()}
    finally:
        r.uninstall()


@test("host-killed", 6)
def t_host_killed(c):
    """Kill the host: the session is reset, and the restarted host attaches cleanly."""
    r = Release(c, "hostkill", ORT)
    try:
        r.install()
        r.wait_model()
        cid = r.container("app")["containerID"].split("//")[-1]
        pid = json.loads(c.node_sh(f"crictl inspect {cid}"))["info"]["pid"]
        c.kill(pid)
        wait_for("the host restarted", lambda: r.restarts().get("app", 0) >= 1, 60)
        r.wait_model(90)
        s = r.status()
        check(s["resets"] >= 1, f"no session reset: {s}")
        check(r.restarts().get("sidecarzero", 0) == 0, "the sidecar restarted")
        return {"resets": s["resets"], "host": r.stats()}
    finally:
        r.uninstall()


@test("delete", 7)
def t_delete(c):
    """Pod deletion: the host stops first, the sidecar then exits, all within the grace period."""
    r = Release(c, "delete", ORT)
    try:
        r.install()
        r.wait_model()
        name = r.pod_name()
        t0 = time.time()
        kubectl("delete", "pod", "-n", r.ns, name, "--wait=true", timeout=120)
        took = time.time() - t0
        # A sidecar that waited for SIGKILL would take the whole 30 s grace period.
        check(took < 25, f"the pod took {took:.1f} s to go")
        return {"seconds": round(took, 1)}
    finally:
        r.uninstall()


@test("reload", 8)
def t_reload(c):
    """A model reload in place with the same schema: no fallback attributable to it, no restart."""
    # The shared-runner allowances of the conformance suite (testing §3): a 20 ms budget and a
    # 100 ms stale-heartbeat limit, so that the VM's own stalls are not counted as the reload's.
    r = Release(c, "reload", chaos("reload", extra={"app": {"args": ["--codec=keyed", "--threads=2", "--budget-us=20000", "--interval-us=500", "--http=0.0.0.0:8080"]},
                                                     "sidecarzero": {"extraArgs": ["--hb-stale-ms=100"]}}), script="delay_us=0\n")
    try:
        r.install()
        r.wait_model()
        s0 = r.status()
        r.set_script("delay_us=1\n")  # another file, the same (raw) schema
        # The ConfigMap reaches the pod within a minute or so. Sample the host's counts each
        # second meanwhile, so that the fallbacks around the reload can be told from the rest.
        hist, end = [], time.time() + 180
        while time.time() < end:
            hist.append((r.status()["reloads"], r.stats()))
            if hist[-1][0] > s0["reloads"] and len(hist) >= 3 and hist[-3][0] > s0["reloads"]:
                break
            time.sleep(1)
        k = next((i for i, (n, _) in enumerate(hist) if n > s0["reloads"]), None)
        check(k is not None, "no reload within 180 s")
        fb = lambda a, b: {x: b["fallback"].get(x, 0) - a["fallback"].get(x, 0) for x in b["fallback"] if b["fallback"].get(x, 0) - a["fallback"].get(x, 0)}
        lo, hi = hist[max(0, k - 2)][1], hist[-1][1]
        around = fb(lo, hi)
        ambient = fb(hist[0][1], lo)
        check(not around, f"fallbacks around the reload: {around}")
        check(all(v == 0 for v in r.restarts().values()), "a container restarted")
        return {"reloads": hist[-1][0], "calls_around": hi["calls"] - lo["calls"], "fallbacks_around": around,
                "ambient_before": ambient, "ambient_calls": lo["calls"] - hist[0][1]["calls"]}
    finally:
        r.uninstall()


@test("abi", 9)
def t_abi(c):
    """A sidecar that speaks another ABI major: the host falls back with ABI_MISMATCH; the corrected sidecar serves."""
    r = Release(c, "abi", merge(ORT, {"sidecarzero": {"extraArgs": ["--test-abi-major=9"]}}))
    try:
        r.install()
        wait_for("ABI_MISMATCH fallbacks", lambda: r.stats()["fallback"].get("ABI_MISMATCH", 0) > 100, 60)
        check(r.stats()["model"] == 0, "model answers across an ABI mismatch")
        r.install({"sidecarzero": {"extraArgs": []}})  # the corrected image: a new pod
        wait_for("the new pod", lambda: len(r.pods()) == 1 and r.ready(), 120)
        r.wait_model(90)
        return {"host": r.stats()}
    finally:
        r.uninstall()


@test("restricted", 10)
def t_restricted(c):
    """The example pod is admitted to a namespace that enforces the restricted Pod Security Standard."""
    r = Release(c, "restricted", ORT, labels={"pod-security.kubernetes.io/enforce": "restricted", "pod-security.kubernetes.io/enforce-version": "latest"})
    try:
        r.install()
        r.wait_model()
        return {"admitted": True}
    finally:
        r.uninstall()


@test("users", 11)
def t_users(c):
    """The host and the sidecar run as different users and meet through the pod's fsGroup."""
    r = Release(c, "users", ORT)
    try:
        r.install()
        r.wait_model()
        spec = r.pod()["spec"]
        app = next(x for x in spec["containers"] if x["name"] == "app")["securityContext"]["runAsUser"]
        side = next(x for x in spec["initContainers"] if x["name"] == "sidecarzero")["securityContext"]["runAsUser"]
        check(app != side, f"the same user {app}")
        return {"host_uid": app, "sidecar_uid": side, "fsGroup": spec["securityContext"]["fsGroup"]}
    finally:
        r.uninstall()


@test("second-client", 12)
def t_second_client(c):
    """A second client process in the pod is refused with BUSY and falls back."""
    r = Release(c, "second", ORT)
    try:
        r.install()
        r.wait_model()
        out = kubectl("exec", "-n", r.ns, r.pod_name(), "-c", "app", "--", "/usr/bin/scz-demo-host", "--once=20", "--threads=1", "--interval-us=0")
        s = json.loads(out.strip().splitlines()[-1])
        check(s["model"] == 0 and s["fallback"].get("NOT_CONNECTED", 0) == 20, f"the second client: {s}")
        r.wait_model()  # and the first is untouched
        return {"second": s}
    finally:
        r.uninstall()


@test("admin", 13)
def t_admin(c):
    """sczctl status, dump, and reload work in the sidecar; the admin socket is not in the host's container."""
    r = Release(c, "admin", ORT)
    try:
        r.install()
        r.wait_model()
        s = r.status()
        d = r.sczctl("dump")
        check('"ok":' in d, f"dump: {d[:200]}")
        r.sczctl("reload")
        out = kubectl("exec", "-n", r.ns, r.pod_name(), "-c", "app", "--", "/usr/bin/scz-demo-host", f"--socket={ADMIN}", "--once=3", "--threads=1")
        a = json.loads(out.strip().splitlines()[-1])
        check(a["fallback"].get("NOT_CONNECTED", 0) == 3, f"the admin socket from the host's container: {a}")
        return {"state": s["state"], "dump_ok": '"ok":true' in d}
    finally:
        r.uninstall()


@test("degraded", 14)
def t_degraded(c):
    """No model file: the sidecar starts degraded, the host serves fallbacks, /modelz says why.
    An invalid setting: the pod never starts, and a rolling update stops at its first pod."""
    r = Release(c, "degraded", merge(ORT, {"sidecarzero": {"model": {"path": "/models/none.ort"}}}))
    try:
        r.install()
        time.sleep(5)
        s = r.stats()
        check(s["model"] == 0 and s["calls"] > 0, f"the host: {s}")
        modelz = r.http(9464, "/modelz")
        check("none.ort" in modelz or "model" in modelz.lower(), f"/modelz: {modelz}")
    finally:
        r.uninstall()
    r = Release(c, "badspec", merge(ORT, {"replicas": 2}))
    try:
        r.install()
        r.install({"sidecarzero": {"extraArgs": ["--no-such-setting"]}}, wait=False)
        time.sleep(30)
        d = kjson("get", "deployment", "-n", r.ns, r.name)
        ready = d["status"].get("readyReplicas", 0)
        check(ready == 2, f"the rolling update replaced ready pods: {d['status']}")
        new = [p for p in r.pods() if not r.ready(p)]
        check(len(new) == 1, f"{len(new)} new pods not ready, expected the first only")
        return {"modelz": modelz.strip()[:200], "ready_during_bad_update": ready, "stuck_new_pods": len(new)}
    finally:
        r.uninstall()


@test("frozen", 15)
def t_frozen(c):
    """Stop the supervisor's loop: the pod goes un-ready, the liveness probe restarts the sidecar,
    the host reconnects without restarting."""
    r = Release(c, "frozen", ORT)
    try:
        r.install()
        r.wait_model()
        c.kill(r.sidecar_pid(), "STOP")
        wait_for("the pod un-ready", lambda: not r.ready(), 30)
        wait_for("the sidecar restarted", lambda: r.restarts().get("sidecarzero", 0) >= 1, 60)
        r.wait_ready()
        r.wait_model(90)
        restarts = r.restarts()
        check(restarts.get("app", 0) == 0, f"the host restarted: {restarts}")
        return {"restarts": restarts}
    finally:
        r.uninstall()


@test("rollback", 16)
def t_rollback(c):
    """A model that crashes on its first requests is rolled back; the previous one serves again,
    with no container restart."""
    r = Release(c, "rollback", chaos("rollback"), script="delay_us=0\n")
    try:
        r.install()
        r.wait_model()
        s0 = r.status()
        r.set_script("crash_after=1\n")
        wait_for("the rollback", lambda: r.status()["rollbacks"] >= 1, 240, 2)
        time.sleep(3)
        r.wait_model(60)
        s = r.status()
        check(all(v == 0 for v in r.restarts().values()), f"a container restarted: {r.restarts()}")
        return {"rollbacks": s["rollbacks"], "deaths": s["deaths"] - s0["deaths"]}
    finally:
        r.uninstall()


# ── running ───────────────────────────────────────────────────────────────────────────────

def run_tests(c, names):
    results, failed = {}, []
    for n in names:
        num, f = TESTS[n]
        log(f"── {num}. {n}: {f.__doc__.splitlines()[0]}")
        t0 = time.time()
        try:
            results[n] = {"ok": True, "seconds": round(time.time() - t0, 1), "result": f(c)}
            results[n]["seconds"] = round(time.time() - t0, 1)
            log(f"PASS {n} ({results[n]['seconds']} s)")
        except Exception as e:  # noqa: BLE001 - a failed test is recorded and the run goes on
            results[n] = {"ok": False, "seconds": round(time.time() - t0, 1), "error": str(e)}
            failed.append(n)
            log(f"FAIL {n}: {e}")
    return results, failed


def save(version, results, extra=None):
    os.makedirs(RESULTS, exist_ok=True)
    path = os.path.join(RESULTS, f"{version}.json")
    old = json.load(open(path)) if os.path.exists(path) else {}
    old.update(results)
    if extra:
        old.update(extra)
    json.dump(old, open(path, "w"), indent=2, sort_keys=True)


def with_cluster(version, single_process, keep, fn):
    c = Cluster(version, single_process)
    try:
        c.create()
        return fn(c)
    finally:
        if not keep:
            log(f"deleting {c.name}")
            c.delete()


def main(argv):
    version, keep, matrix, names = "1.37", False, False, []
    for a in argv:
        if a.startswith("--k8s="):
            version = a.split("=", 1)[1]
        elif a == "--keep":
            keep = True
        elif a == "--matrix":
            matrix = True
        elif a == "--list":
            for n, (num, f) in sorted(TESTS.items(), key=lambda x: x[1][0]):
                print(f"{num:2}. {n:16} {f.__doc__.splitlines()[0]}")
            return 0
        elif a in TESTS:
            names.append(a)
        else:
            print(f"unknown argument {a}; --list shows the tests", file=sys.stderr)
            return 2
    check(version in NODE_IMAGES, f"no node image for {version}: {sorted(NODE_IMAGES)}")
    sh("helm", "dependency", "build", CHART)
    all_names = [n for n, _ in sorted(TESTS.items(), key=lambda x: x[1][0])]
    names = names or all_names
    failed = []
    if matrix:
        # The exit criterion first: the kill test on every version.
        for v in sorted(NODE_IMAGES, key=lambda s: tuple(map(int, s.split(".")))):
            res, f = with_cluster(v, False, keep, lambda c: run_tests(c, ["kill"]))
            save(v, res)
            failed += [f"{n}@{v}" for n in f]
        rest = [n for n in names if n != "kill"]
    else:
        rest = names
    if rest:
        res, f = with_cluster(version, False, keep, lambda c: run_tests(c, rest))
        save(version, res)
        failed += [f"{n}@{version}" for n in f]
    if "oom" in rest:
        res, f = with_cluster(version, True, keep, lambda c: run_tests(c, ["oom"]))
        save(version, {"oom-singleProcess": res["oom"]})
        failed += [f"oom-singleProcess@{version}" for n in f]
    log("all passed" if not failed else f"FAILED: {', '.join(failed)}")
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except Fail as e:
        print(f"e2e: {e}", file=sys.stderr)
        sys.exit(1)
