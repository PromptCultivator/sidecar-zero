"""The benchmark's service mode (bench §4.1), called by bench/run.py with --modes=service.

Each system runs as host pods behind a Kubernetes Service in a Kind cluster (the phase 5 harness,
deploy/kind/e2e.py). The host is `scz-bench-host serve`; a load-generator pod (`scz-loadgen`)
sends to the Service, open loop; the kill agent on the node kills one pod's model process every
interval and records `CLOCK_MONOTONIC`; a watcher counts the Service's EndpointSlice changes.

| System | Pod | A kill kills |
|---|---|---|
| A | the host and the SidecarZero sidecar (the library chart) | the consuming worker |
| C, D | the host and the baseline server; `supervised`: the server under its restart loop | the server: unsupervised it is the container's PID 1, so the container restarts |
| E | the host alone, with ONNX Runtime in it | the host, which is the model |

Triton (B) is not run here: its image is 24 GB, and loading it into the node is the cost of a
second copy. bench/results' report says so.
"""

import json
import os
import random
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, ".."))
sys.path.insert(0, os.path.join(ROOT, "deploy/kind"))
import e2e  # noqa: E402 - the phase 5 harness: Cluster, Release, SliceWatch, kubectl

ARCH = os.uname().machine
IMAGES = ["scz-bench-host:dev", "scz-loadgen:dev"]


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def build_images():
    """The bench images, from binaries the dev image built (this process runs in it)."""
    st = os.path.join(HERE, "docker/.stage")
    subprocess.run(["rm", "-rf", st], check=True)
    os.makedirs(os.path.join(st, "ort"))
    os.makedirs(os.path.join(st, "models"))
    rel, musl = os.path.join(ROOT, "target/release"), os.path.join(ROOT, f"target/{ARCH}-unknown-linux-musl/release")
    for b in ("scz-bench-host", "scz-bench-server"):
        subprocess.run(["strip", "-o", os.path.join(st, b), os.path.join(rel, b)], check=True)
    subprocess.run(["cp", os.path.join(musl, "scz-loadgen"), st], check=True)
    so = subprocess.run(["sh", "-c", f"readelf -d {rel}/scz-bench-host | sed -n 's/.*NEEDED.*\\[\\(libonnxruntime[^]]*\\)\\]/\\1/p'"], capture_output=True, text=True).stdout.strip()
    subprocess.run(["cp", "-L", f"{os.environ.get('ORT_DIR', '/opt/ort')}/lib/{so}", os.path.join(st, "ort", so)], check=True)
    for m in ("fraud.ort", "reference-features.f32"):
        subprocess.run(["cp", os.path.join(ROOT, "target/models", m), os.path.join(st, "models")], check=True)
    for target, tag in (("host", "scz-bench-host:dev"), ("loadgen", "scz-loadgen:dev")):
        subprocess.run(["docker", "build", "-q", "--target", target, "-t", tag, os.path.join(HERE, "docker")], check=True, capture_output=True)
    # The sidecar's images, as the deployment tests build them.
    subprocess.run(["bash", os.path.join(ROOT, "deploy/docker/build.sh"), "dev"], check=True, capture_output=True)


def host_args(system):
    t = {"A": ["--socket=/run/sidecarzero/scz.sock"], "C": ["--http=127.0.0.1:18080"], "D": ["--uds=/run/d/d.sock"], "E": ["--model=/models/fraud.ort"]}[system]
    return ["serve", f"--system={system}", "--listen=0.0.0.0:8080", "--budget-us=2000", "--rows=/models/reference-features.f32"] + t


def manifest(name, system, variant, replicas):
    """A Deployment and its Service for systems C, D, and E."""
    containers = [{
        "name": "app", "image": "scz-bench-host:dev", "imagePullPolicy": "IfNotPresent", "args": host_args(system),
        "ports": [{"name": "http", "containerPort": 8080}],
        "readinessProbe": {"httpGet": {"path": "/healthz", "port": "http"}, "periodSeconds": 1},
        "volumeMounts": [{"name": "run", "mountPath": "/run/d"}],
        "resources": {"requests": {"cpu": "500m", "memory": "256Mi"}, "limits": {"memory": "256Mi"}},
    }]
    if system in ("C", "D"):
        flag = "--http=127.0.0.1:18080" if system == "C" else "--uds=/run/d/d.sock"
        containers.append({
            "name": "server", "image": "scz-bench-host:dev", "imagePullPolicy": "IfNotPresent", "command": ["/usr/bin/scz-bench-server"],
            "args": ["--model=/models/fraud.ort", flag, "--threads=1"] + (["--supervise"] if variant == "supervised" else []),
            "volumeMounts": [{"name": "run", "mountPath": "/run/d"}],
            "resources": {"requests": {"cpu": "1", "memory": "512Mi"}, "limits": {"memory": "512Mi"}},
        })
    dep = {
        "apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name},
        "spec": {"replicas": replicas, "selector": {"matchLabels": {"app": name}},
                 "template": {"metadata": {"labels": {"app": name}},
                              "spec": {"terminationGracePeriodSeconds": 5, "securityContext": {"fsGroup": 65532},
                                       "containers": containers, "volumes": [{"name": "run", "emptyDir": {}}]}}},
    }
    svc = {"apiVersion": "v1", "kind": "Service", "metadata": {"name": name},
           "spec": {"selector": {"app": name}, "ports": [{"name": "http", "port": 8080, "targetPort": "http"}]}}
    return json.dumps({"apiVersion": "v1", "kind": "List", "items": [dep, svc]})


def pods(ns, name):
    return [p for p in e2e.kjson("get", "pods", "-n", ns, "-l", f"app={name}")["items"] if not p["metadata"].get("deletionTimestamp")]


def container_pid(c, pod, cname):
    st = pod["status"].get("initContainerStatuses", []) + pod["status"].get("containerStatuses", [])
    cid = next((s.get("containerID", "") for s in st if s["name"] == cname), "").split("//")[-1]
    if not cid:
        return None
    return json.loads(c.node_sh(f"crictl inspect {cid}"))["info"]["pid"]


def target(c, ns, system, variant, pod):
    """The model process of `pod`: the pid the kill agent kills."""
    if system == "A":
        name = pod["metadata"]["name"]
        s = json.loads(e2e.kubectl("exec", "-n", ns, name, "-c", "sidecarzero", "--", "/usr/bin/sczctl", f"--admin={e2e.ADMIN}", "status"))
        g = next((w["gen"] for w in s["workers"] if w["role"] == "Consumer"), None)
        sup = container_pid(c, pod, "sidecarzero")
        for k in c.node_sh(f"cat /proc/{sup}/task/{sup}/children").split():
            if f"--gen={g} " in c.node_sh(f"tr '\\0' ' ' < /proc/{k}/cmdline", check=False) + " ":
                return int(k)
        return None
    if system in ("C", "D"):
        p1 = container_pid(c, pod, "server")
        if variant == "supervised":
            kids = c.node_sh(f"cat /proc/{p1}/task/{p1}/children", check=False).split()
            return int(kids[0]) if kids else None
        return p1
    return container_pid(c, pod, "app")


def restarts(ns, name):
    n = 0
    for p in pods(ns, name):
        for s in p["status"].get("initContainerStatuses", []) + p["status"].get("containerStatuses", []):
            n += s.get("restartCount", 0)
    return n


def run_one(c, out, system, variant, scenario, rate, duration, warmup, kills_wanted, interval, replicas, rep):
    name = f"bench-{system.lower()}-{variant}"
    ns = f"svc-{system.lower()}-{variant}"
    e2e.wait_for(f"namespace {ns}", lambda: e2e.kubectl("get", "namespace", ns, "-o", "jsonpath={.status.phase}", check=False) != "Terminating", 180, 1)
    e2e.kubectl("create", "namespace", ns, check=False)
    rel = None
    try:
        if system == "A":
            values = {"replicas": replicas, "app": {"image": {"repository": "scz-bench-host", "tag": "dev"}, "args": host_args("A"),
                                                     "resources": {"requests": {"cpu": "500m", "memory": "256Mi"}, "limits": {"memory": "256Mi"}}},
                      "sidecarzero": {"image": {"repository": "sidecarzero-ort-fraud", "tag": "dev"}, "model": {"path": "/models/fraud.ort"}}}
            rel = e2e.Release(c, name, values, ns=ns)
            rel.install()
        else:
            e2e.kubectl("apply", "-n", ns, "-f", "-", input=manifest(name, system, variant, replicas))
        e2e.wait_for(f"{name} ready", lambda: len([p for p in pods(ns, name) if any(x["type"] == "Ready" and x["status"] == "True" for x in p["status"].get("conditions", []))]) == replicas, 180, 1)
        time.sleep(3)
        r0 = restarts(ns, name)
        w = e2e.SliceWatch(type("R", (), {"ns": ns, "name": name})())
        w.start()
        e2e.wait_for("the slice watch", lambda: w.state is not None, 30)
        lg = f"loadgen-{rep}"
        e2e.kubectl("run", lg, "-n", ns, "--image=scz-loadgen:dev", "--image-pull-policy=IfNotPresent", "--restart=Never", "--",
                    f"--target={name}:8080", f"--rate={rate}", f"--duration-s={duration}", f"--warmup-s={warmup}", "--conns=16", "--out=/dev/stdout")
        t_start = time.time() + warmup + 1
        kills = []
        if scenario == "kill":
            time.sleep(max(0, t_start - time.time()))
            while len(kills) < kills_wanted and time.time() + interval < t_start + duration:
                ps = pods(ns, name)
                if ps:
                    pod = random.choice(ps)
                    try:
                        pid = target(c, ns, system, variant, pod)
                    except (e2e.Fail, json.JSONDecodeError, KeyError) as e:
                        log(f"    finding the model process: {e}")
                        pid = None
                    if pid:
                        o = c.node_sh(f"/usr/local/bin/scz-kill-agent {pid}", check=False)
                        if o.startswith("KILL") and o.split()[-1] == "ok":
                            kills.append(int(o.split()[1]))
                        else:
                            log(f"    the kill agent: {o.strip() or 'no output'}")
                    else:
                        log(f"    no model process found in {pod['metadata']['name']}")
                time.sleep(interval)
        e2e.wait_for("the load generator", lambda: e2e.kjson("get", "pod", "-n", ns, lg)["status"].get("phase") in ("Succeeded", "Failed"), duration + warmup + 120, 1)
        time.sleep(2)
        w.stop()
        logs = e2e.kubectl("logs", "-n", ns, lg)
        res = json.loads(next(l for l in logs.splitlines() if l.startswith("{")))
        res.update({"mode": "service", "host": "rust", "system": system, "variant": variant, "scenario": scenario, "rate": rate, "arrival": "even",
                    "replicas": replicas, "kills_ns": kills, "endpoint_changes": len(w.changes), "endpoint_timeline": w.changes,
                    "container_restarts": restarts(ns, name) - r0, "spec": {"variant": variant, "scenario": scenario, "rate": rate}})
        path = os.path.join(out, f"service-{system}-{variant}-{scenario}-{rate}-r{rep}.json")
        json.dump(res, open(path, "w"))
        log(f"  {system}/{variant} {scenario}: {res['record']['model']} model, {sum(res['record']['fallback'].values())} fallback, "
            f"{res['record']['error'] + res['record']['timeout']} failed; {len(kills)} kills, {len(w.changes)} endpoint changes, {res['container_restarts']} restarts")
    finally:
        if rel:
            rel.uninstall()
        e2e.kubectl("delete", "namespace", ns, "--wait=false", check=False)


def run(out, systems, quick, repeats, kills):
    """Every system's service-mode runs; returns the names of those that failed."""
    log("service mode: building images and the Kind cluster")
    build_images()
    e2e.IMAGES = e2e.IMAGES + IMAGES
    c = e2e.Cluster("1.37")
    failed = []
    try:
        c.create()
        e2e.sh("helm", "dependency", "build", e2e.CHART)
        e2e.sh("docker", "cp", os.path.join(ROOT, f"target/{ARCH}-unknown-linux-musl/release/scz-kill-agent"), f"{c.node}:/usr/local/bin/scz-kill-agent")
        rate = 1000 if quick else 5000
        n = min(kills, 10) if quick else kills
        interval = 2.0
        duration = n * interval + 5
        variants = {"A": ["default"], "C": ["default", "supervised"], "D": ["default", "supervised"], "E": ["default"]}
        for s in systems:
            if s == "B":
                log("  B: not run in service mode (a 24 GB image); see the report")
                continue
            for v in variants.get(s, []):
                for rep in range(repeats):
                    for sc in ("control", "kill"):
                        try:
                            run_one(c, out, s, v, sc, rate, duration, 5, n, interval, 2, rep + 1)
                        except Exception as e:  # noqa: BLE001 - recorded; the others go on
                            failed.append(f"service-{s}-{v}-{sc}-r{rep + 1}")
                            log(f"  FAILED {s}/{v} {sc}: {e}")
    finally:
        c.delete()
    return failed
