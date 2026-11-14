# 12 — Kubernetes Deployment (`deploy/helm-kep753`, `deploy/docker`)

[← High-level design](../high-level-design.md) · depends on [04 — Supervisor](04-supervisor.md), [05 — Worker](05-worker.md)

This specification defines how SidecarZero is packaged and run inside a Kubernetes pod so that worker failures remain invisible to the cluster control plane, along with the node and cluster properties required for zero `EndpointSlice` flaps.

---

## 1. Pod Topology

```yaml
apiVersion: v1
kind: Pod
spec:
  terminationGracePeriodSeconds: 30         # must exceed the sidecar's shutdown.sessionGraceSeconds (20 s)
  securityContext:
    fsGroup: 2000                           # both containers reach the socket through this group
  initContainers:
    # A model-download init container, if used, is listed here before the sidecar.
    - name: sidecarzero
      image: ghcr.io/sidecarzero/sidecarzero-ort:<version>
      restartPolicy: Always                 # KEP-753 native sidecar
      args: ["--socket=/run/sidecarzero/scz.sock", "--model=/models/fraud.ort", "--standby=warm"]
      ports: [{ name: scz-http, containerPort: 9464 }]
      startupProbe:   { httpGet: { path: /startupz, port: scz-http }, periodSeconds: 1, failureThreshold: 30 }
      readinessProbe: { httpGet: { path: /readyz,   port: scz-http }, periodSeconds: 2 }
      livenessProbe:  { httpGet: { path: /livez,    port: scz-http }, periodSeconds: 5, failureThreshold: 3 }
      resources:
        requests: { cpu: "2", memory: 1Gi }
        limits:   { memory: 1Gi }           # "shared" CPU profile; see §4
      securityContext:
        runAsNonRoot: true
        readOnlyRootFilesystem: true
        allowPrivilegeEscalation: false
        capabilities: { drop: ["ALL"] }
        seccompProfile: { type: RuntimeDefault }
      volumeMounts:
        - { name: scz-run,   mountPath: /run/sidecarzero }
        - { name: scz-admin, mountPath: /run/sidecarzero-admin }   # not mounted in the app
        - { name: models,    mountPath: /models, readOnly: true }
  containers:
    - name: app
      image: my-service:<version>
      env: [{ name: SIDECARZERO_SOCKET, value: /run/sidecarzero/scz.sock }]
      volumeMounts:
        - { name: scz-run, mountPath: /run/sidecarzero }
  volumes:
    - { name: scz-run,   emptyDir: {} }     # disk-backed; holds only a socket inode
    - { name: scz-admin, emptyDir: {} }     # supervisor admin socket; sidecar only
    - { name: models,    ... }              # image volume, PVC, or init-container download
```

Key operational invariants in the pod specification:

- **`fsGroup`:** The socket file is created with mode `0660`. Because the host and sidecar containers typically run as different non-root UIDs, they share access via the pod's `fsGroup` ([§7](#7-socket-sharing)).
- **Init-container ordering:** A KEP-753 native sidecar is an `initContainer` with `restartPolicy: Always`. Any init container that downloads or stages the model file must precede `sidecarzero` in `initContainers`.
- **Host `SIGTERM` handling:** If the host application runs as PID 1 inside its container, it must install a `SIGTERM` handler so that pod termination does not wait out the entire `terminationGracePeriodSeconds` before stopping the sidecar.
- **Startup probe threshold:** Thirty failures at 1 s intervals is ample for the default startup mode, where `/startupz` passes in milliseconds once the arena and socket are ready. When `startup.waitForModel: true` is enabled, the Helm chart automatically increases `failureThreshold` (`max(30, loadTimeoutSeconds + 10)`) to cover model loading and warm-up.

### Why a Native Sidecar (KEP-753)

| Property (KEP-753; beta and enabled by default in Kubernetes 1.29+, GA in 1.33+) | Benefit in SidecarZero |
|---|---|
| Starts before main containers; `startupProbe` gates main container launch | The shared-memory arena and UDS socket exist before the host process starts |
| Restarts independently of the pod's `restartPolicy` | A supervisor crash does not restart the host container |
| Terminated after main containers exit | The host application never outlives its sidecar during pod shutdown |
| Does not block `Job` completion | Usable directly in batch and cron workloads |

A native sidecar's `readinessProbe` feeds into the pod's `Ready` condition, which is why `/readyz` is decoupled from worker health ([04 — Supervisor §7](04-supervisor.md#7-probes-metrics-and-admin-socket)).

On clusters older than Kubernetes 1.29, the Helm chart supports rendering the sidecar as a regular container (`sidecar.mode: classic`), with the following behavioral differences:

- At startup, the host container may start before the arena exists; the SDK's non-blocking start-in-fallback behavior handles this seamlessly.
- At shutdown, both containers receive `SIGTERM` simultaneously; the supervisor continues serving while the host remains connected for up to `shutdown.sessionGraceSeconds` ([04 — Supervisor §8](04-supervisor.md#8-shutdown)).
- Classic mode is not suitable for Kubernetes `Job` workloads because regular sidecar containers do not exit automatically when the main container finishes.

### Supported Environments

| Environment | Support Status |
|---|---|
| Kubernetes 1.29+, cgroup v2, containerd or CRI-O with `runc` or `crun` | Supported and verified in Kind E2E across Kubernetes v1.34–v1.37 |
| Linux kernel 5.10+ (5.14+ recommended) | Supported. On kernels missing a required capability, the supervisor starts degraded (`scz_degraded = 1`) and names the missing facility |
| Amazon EKS and Google GKE | Supported on standard containerd/cgroup-v2 node images ([GCP setup](../setup/gcp.md), [AWS setup](../setup/aws.md)) |
| Kubernetes < 1.29 | Supported via `sidecar.mode: classic` |
| SELinux-enforcing nodes (e.g., OpenShift / RHEL) | Supported; containers within a pod share the same MCS category label ([empirical validation](../../spikes/FINDINGS.md#s1--sealed-memfd-over-scm_rights-across-containers)) |
| User-space / microVM sandboxes (gVisor, Kata) | Not supported; SidecarZero relies on host-kernel shared futexes, memfd seals, pidfds, Landlock, and cgroup v2 files |
| Windows nodes | Not supported; the chart sets `kubernetes.io/os: linux` |
| `hostNetwork: true` | Supported with `socket.kind: path` only ([§7](#7-socket-sharing)) |

---

## 2. Container Images (`deploy/docker`)

[`deploy/docker/build.sh`](../../deploy/docker/build.sh) builds the production and verification container images from [`deploy/docker/Dockerfile`](../../deploy/docker/Dockerfile):

| Image | Contents | Base |
|---|---|---|
| `sidecarzero-ort` | `sidecarzero-supervisor`, `sidecarzero-worker` (ONNX Runtime), `sczctl` | Distroless Debian 12 (`gcr.io/distroless/cc-debian12:nonroot`) |
| `sidecarzero-gguf` | `sidecarzero-supervisor`, `sidecarzero-worker-gguf` (statically linked `llama.cpp` `b11988`), `sczctl` | Distroless Debian 12 (`nonroot`) |
| `sidecarzero-python` | `sidecarzero-supervisor`, CPython 3.13, NumPy, `sidecarzero` package (`sidecarzero.worker` runner), `sczctl` | Distroless Debian 12 (`nonroot`) with staged `/usr` Python runtime (`deploy/docker/stage-python.sh`) |
| `sidecarzero-chaos` | `sidecarzero-supervisor`, `scz-worker-chaos`, `sczctl` (for conformance and Kind E2E testing) | Distroless Debian 12 (`nonroot`) |

All images share the same security and runtime properties:

- `sidecarzero-supervisor` is the container `ENTRYPOINT` and runs directly as PID 1 with no shell or init wrapper.
- Runs as a non-root user (`65532:65532` by default) with a read-only root filesystem (`readOnlyRootFilesystem: true`).
- The Python image requires no writable filesystem paths: its entrypoint invokes `--worker=/usr/bin/python3.13 --worker-arg=-B --worker-arg=-m --worker-arg=sidecarzero.worker --worker-socketpair` with `PYTHONDONTWRITEBYTECODE=1`. Users supply their model module via `--worker-arg=<module>` and grant read access to its directory via `--worker-read=<dir>`.

### Model Delivery

| Delivery Method | Notes |
|---|---|
| OCI image volume | Cleanest on clusters supporting Kubernetes `image` volumes: models are versioned and pulled like container images |
| Read-only PVC / CSI volume | Shared across pods, mounted `readOnly: true` |
| Init-container download to `emptyDir` | Works on all clusters; listed before `sidecarzero` in `initContainers` |
| Baked into a derived image | Simplest for small tabular and embedding models |

The chart accepts an optional `model.sha256` digest; the supervisor hashes the model in an isolated helper process and refuses any file that does not match.

**Atomic model updates:** Always publish a new model by writing to a temporary file and atomically renaming it into place, or by switching a symlink. Never overwrite a served model file in place: the supervisor verifies file identity (`(dev, ino, size, mtime)`) and rejects in-place modifications (`MODEL_MODIFIED`), and memory-mapped workers would otherwise observe torn weights. In environments where external tooling overwrites files in place, set `model.copy: memfd` so the supervisor copies the model into a sealed anonymous `memfd` before hashing and passes `/proc/self/fd/<fd>` to workers.

---

## 3. Helm Library Chart (`deploy/helm-kep753`)

`deploy/helm-kep753` is a Helm library chart accompanied by a complete runnable example in `deploy/helm-kep753/examples/fraud-scorer`.

```
deploy/helm-kep753/
├── Chart.yaml
├── values.yaml
├── templates/
│   ├── _sidecar.tpl          named templates: sidecarzero.sidecar, sidecarzero.fsGroup
│   ├── _volumes.tpl          named templates: sidecarzero.volumes, sidecarzero.appVolumeMounts
│   ├── _env.tpl              named template:  sidecarzero.appEnv
│   ├── _podmonitor.tpl       named template:  sidecarzero.podMonitor
│   └── _networkpolicy.tpl    named template:  sidecarzero.networkPolicy
└── examples/
    ├── fraud-scorer/         complete Deployment + Service + optional PodMonitor & NetworkPolicy
    └── monitoring/           alerts.yaml (PrometheusRule) + dashboard.json (Grafana)
```

Application charts include SidecarZero by adding `helm-kep753` as a chart dependency and invoking the named templates (`sidecarzero.sidecar`, `sidecarzero.volumes`, `sidecarzero.appVolumeMounts`, `sidecarzero.appEnv`, `sidecarzero.fsGroup`, and optionally `sidecarzero.podMonitor` and `sidecarzero.networkPolicy`).

### Configuration Values (`values.yaml`)

| Value | Default | Meaning |
|---|---|---|
| `sidecar.mode` | `native` | `native` (`initContainers` with `restartPolicy: Always`) or `classic` (`containers`) |
| `image.repository`, `image.tag` | `ghcr.io/sidecarzero/sidecarzero-ort`, `0.1.0` | Sidecar container image and tag |
| `model.path`, `model.sha256`, `model.copy` | `/models/model.ort`, `""`, `none` | Model path, optional SHA-256 pin, and copy mode (`none` or `memfd`, passing `--model-copy=memfd`) |
| `model.prepack` | `true` | ONNX Runtime weight prepacking (`false` sets `SCZ_ORT_PREPACK=0` so workers share mapped weight pages) |
| `arena.sizeMi` | `8` | Size of the data segment in MiB (`16` recommended when batch slots are enabled) |
| `arena.small.slots`, `arena.small.slotSizeKi` | `1024`, `4` | Slot count and per-direction byte capacity for class 0 (single-row requests) |
| `arena.batch.slots`, `arena.batch.slotSizeKi` | `0`, `128` | Slot count and per-direction byte capacity for class 1 (batches; `0` disables class 1) |
| `worker.threads`, `worker.standby` | `1`, `warm` | Worker consume threads (one MPSC ring per thread) and standby mode (`warm` or `none`) |
| `worker.spinMicros` | `50` | Worker adaptive spin budget in microseconds (`0` always parks on futex) |
| `worker.loadTimeoutSeconds` | `300` | Maximum allowed model load and warm-up duration before the supervisor kills the worker |
| `worker.coreDumps` | `false` | Allow crashing workers to write core dumps (`--worker-core-dumps`; debugging only) |
| `pressure.shed`, `pressure.recover`, `pressure.hard` | `0.85`, `0.75`, `0.95` | Working-set memory utilization thresholds (`--pressure-shed`, `--pressure-recover`, `--pressure-hard`) |
| `pressure.shedMaxSeconds` | `2` | Maximum duration in `PSI_SHEDDING` before recycling the worker (`--pressure-shed-max-ms`) |
| `oomShield.mode` | `auto` | Kernel OOM shield mode: `auto`, `singleProcess`, `nestedCgroup`, `oomScoreAdj`, or `none` (§5) |
| `oomShield.workerMemoryMaxMi` | `0` | Explicit per-worker `memory.max` in MiB under `nestedCgroup` (`0` computes automatically) |
| `socket.kind` | `path` | `path` (shared `emptyDir` with `0660` socket) or `abstract` (`@<name>`, no socket volume) |
| `startup.waitForModel` | `false` | Gate `/startupz` (and therefore host container launch) until the first worker is `WARM` (`--startup-wait-for-model`) |
| `startup.failOpen` | `true` | Start degraded on unsupported nodes so the host container can still start and serve fallbacks (`--fail-open`) |
| `reload.rollback`, `reload.rollbackWindowSeconds` | `true`, `60` | Roll back to the previous model if a new generation dies 3 times as ring consumer within this window |
| `shutdown.sessionGraceSeconds` | `20` | How long the supervisor continues serving an attached host after receiving `SIGTERM` |
| `hugepages.enabled` | `false` | Back the data segment with 2 MiB huge pages (`--hugepages`, mounting `hugepages-2Mi` and requesting `hugepages-2Mi` in `resources`) |
| `networkPolicy.enabled` | `false` | Emit a `NetworkPolicy` restricting ingress on the HTTP probe/metrics port (`9464`) |
| `podMonitor.enabled` | `false` | Emit a Prometheus Operator `PodMonitor` scraping `/metrics` |

---

## 4. Resource Sizing

### Memory Budget

```
sidecar memory limit ≥ arena
                     + worker private RSS × (2 if standby = warm, else 1)
                     + model file page cache (counted once)
                     + supervisor RSS (≈ 10 MiB)
                     + ≥ 20% headroom below the 85% shed threshold
```

- **Arena memory attribution:** Because the supervisor creates and pre-faults both `memfd` segments inside the sidecar container, 100% of the arena's pages are charged to the sidecar's cgroup and 0 bytes are charged to the host container ([empirical validation S1](../../spikes/FINDINGS.md#s1--sealed-memfd-over-scm_rights-across-containers)).
- **Private RSS and teardown latency:** Calls in `PROCESSING` at the moment of a worker `SIGKILL` wait for the kernel to reclaim the dead worker's address space before its `pidfd` becomes readable (≈ 15 ms per GiB of private anonymous memory), whereas queued (`READY`) calls and new calls resolve in ≈ 0.05 ms via the per-thread death lock ([04 — Supervisor §5.2](04-supervisor.md#52-detecting-death-and-hangs)). Keeping worker private memory compact bounds `PROCESSING` teardown latency.
- **Set `requests.memory == limits.memory`:** Guaranteed memory allocation prevents node-pressure eviction from squeezing the sidecar below its working set.
- **Automatic standby suppression:** If the container's `memory.max` fits one worker below the 85% shed threshold but cannot fit both an active worker and a warm standby, the supervisor automatically suppresses the standby, exports `scz_standby_suppressed = 1`, and operates with cold restarts rather than oscillating in and out of `PSI_SHEDDING`. If even a single worker exceeds 85% immediately after warm-up, the supervisor holds the gate closed with `DOES_NOT_FIT` (`scz_model_fit = 0`).

### CPU Profiles

Spinning workers and fractional CFS CPU quotas (`cpu.max`) do not mix: a spinning thread rapidly consumes its quota slice and causes the entire sidecar container to be throttled for tens of milliseconds. SidecarZero supports two CPU profiles:

| Property | Shared Profile (Default) | Dedicated Profile |
|---|---|---|
| Sidecar CPU spec | `requests.cpu` set, **`limits.cpu` omitted** | `requests.cpu == limits.cpu` in whole integer CPUs |
| Pod QoS class | `Burstable` | `Guaranteed` (all containers in the pod must set `requests == limits`) |
| Node requirement | None | Kubelet `cpuManagerPolicy: static` |
| Worker scheduling | Unthrottled share of shared node cores | Pinned exclusive physical cores |
| Tail latency | Low; minor scheduling jitter under node contention | Lowest possible |

Never combine a fractional `limits.cpu` with `worker.spinMicros > 0`. If organizational policy mandates CPU limits without static CPU manager, set `worker.spinMicros: 0` so workers park immediately on futexes instead of spinning.

General CPU sizing rules:

- Request at least 1 CPU core per worker consume thread.
- Size `worker.threads` so each consume thread stays under ≈ 70% utilization: `worker.threads ≥ (request_rate × mean_inference_time) / 0.7`.
- Match `worker.spinMicros` to request arrival rate: a worker picks up requests without a kernel wake-up whenever inter-arrival time is less than `worker.spinMicros` (default `50 µs` covers arrival rates above ≈ 20,000 RPS per ring).
- On the host container, budget roughly `request_rate × mean_service_time` of CPU for client-side spinning, or reduce the SDK's `spin_budget` when host CPU is constrained.
- **Autoscaling:** Do not configure HorizontalPodAutoscaler (HPA) on CPU utilization alone when spinning is enabled; scale on request rate or fallback ratio (`rate(scz_client_calls_total{source="fallback"}[1m]) / rate(scz_client_calls_total[1m])`).

---

## 5. OOM Shielding

On cgroup v2, the kubelet sets `memory.oom.group = 1` on every container cgroup by default. When the kernel OOM killer triggers inside a container with `memory.oom.group = 1`, it kills **every process in the container**, including the PID 1 supervisor, causing a container restart and a pod readiness flap. SidecarZero defends against worker memory exhaustion in two layers ([empirical validation S2](../../spikes/FINDINGS.md#s2--worker-oom-vs-container-pid-1-and-kubernetes-readiness)).

### Layer 0 — Proactive Userspace Relief (Always Active)

| Mechanism | Component |
|---|---|
| Poll `memory.current`, `memory.stat` (`inactive_file`), and `memory.pressure` every 10 ms | [04 — Supervisor §6](04-supervisor.md#6-memory-and-pressure-monitor) |
| At 85% working-set utilization: retire warm standby (if active worker is healthy) or hand over to warm standby (if active worker leaked); if no standby exists, close gate to `PSI_SHEDDING` and send `TRIM` | [04 — Supervisor §6](04-supervisor.md#6-memory-and-pressure-monitor) |
| Recycle worker if still shedding after `pressure.shedMaxSeconds` (`2 s`); at 95% hard threshold, shed immediately and replace by kill | [04 — Supervisor §6](04-supervisor.md#6-memory-and-pressure-monitor) |
| Per-worker memory ceiling (`RLIMIT_DATA` / capped arena) so runaway single-request allocations fail in-process | [05 — Worker §5](05-worker.md#5-memory-discipline) |

Layer 0 handles gradual memory growth and leaks invisibly on every stock Kubernetes cluster with zero container restarts and zero `EndpointSlice` changes.

### Layer 1 — Surviving a Sudden Kernel OOM Kill (`oomShield.mode`)

When a sudden unconstrained allocation spike crosses from 85% to 100% of `memory.max` inside a single 10 ms poll interval, Layer 1 determines whether PID 1 survives the kernel OOM killer:

| `oomShield.mode` | Mechanism | Cluster Requirement | Behavior on Sudden Spike |
|---|---|---|---|
| `singleProcess` | Kubelet sets `memory.oom.group = 0` (`singleProcessOOMKill: true`); kernel kills only the highest-RSS process (the worker with `oom_score_adj = 1000`) | Kubelet `singleProcessOOMKill: true` (Kubernetes 1.32+) | PID 1 and warm standby survive; zero container restarts, zero `EndpointSlice` flaps |
| `nestedCgroup` | Supervisor moves itself to `<cgroup>/scz-supervisor`, enables `+memory` on the parent, and places each worker in `<cgroup>/scz-worker-{0,1}` with its own `memory.max` | Writable, delegated cgroup v2 subtree in the container | Runaway worker hits its child `memory.max` and dies alone; PID 1 and warm standby survive; zero flaps |
| `oomScoreAdj` | Supervisor writes `-1000` to its own `/proc/self/oom_score_adj` before clearing `PR_SET_DUMPABLE`; kernel exempts PID 1 from the group OOM kill | `CAP_SYS_RESOURCE` in effective capability set (requires root user + `privileged` Pod Security Standard) | PID 1 survives (zero flaps), but `memory.oom.group = 1` kills both active and standby workers (cold recovery) |
| `none` | Layer 0 only | Stock cluster (`restricted` Pod Security Standard) | Gradual leaks handled with zero flaps; a sudden sub-10 ms spike past `memory.max` group-kills the sidecar container |
| `auto` (default) | Detects `singleProcess` if `memory.oom.group == 0`; else attempts `nestedCgroup`; else falls back to `none` (never selects `oomScoreAdj` automatically) | None | Automatically uses the strongest non-privileged shield available on the node and exports `scz_oom_shield_mode{mode}` |

Every worker process runs with `oom_score_adj = 1000` so that both within the container and under node-wide memory pressure, the worker is always selected first—the one process whose loss costs only clean fallbacks.

---

## 6. Probes

| Probe | Endpoint | Default Settings | Behavior |
|---|---|---|---|
| Startup | `/startupz` | `periodSeconds: 1`, `failureThreshold: 30` (raised automatically when `startup.waitForModel: true`) | Returns `200 OK` once the arena and socket are live (or when degraded under `startup.failOpen: true`). Gates main container startup |
| Readiness | `/readyz` | `periodSeconds: 2` | Returns `200 OK` whenever the supervisor's event loop is running, regardless of worker or gate state |
| Liveness | `/livez` | `periodSeconds: 5`, `failureThreshold: 3` | Restarts the sidecar container only if the supervisor's event loop is wedged |

**Host readiness probes:** Never couple the host application container's `readinessProbe` to model availability. The host always answers requests via its local fallback when the model is unavailable. Progressive delivery controllers (Argo Rollouts, Flagger) and deployment verification jobs should gate rollouts on the sidecar's `/modelz` endpoint (`200 OK` only when `gate == HEALTHY` and `scz_degraded == 0`) or on `scz_gate_state` and fallback-ratio metrics.

---

## 7. Socket Sharing

| `socket.kind` | Mechanism | When to Use |
|---|---|---|
| `path` (default) | Unix domain socket at `/run/sidecarzero/scz.sock` inside a shared `emptyDir` volume, `chmod 0660`, shared via `fsGroup` | Standard deployments. Works with all network configurations |
| `abstract` | Linux abstract-namespace socket (`@sidecarzero-<pod-uid>`), visible across containers sharing the pod network namespace without mounting a volume | Minimal pod specs. **Never use with `hostNetwork: true`**, which would expose the abstract socket to the entire node |

In both modes, the supervisor authenticates connecting peers via `SO_PEERCRED` ([04 — Supervisor §4](04-supervisor.md#4-handshake-and-session-management)). `shareProcessNamespace` is never required.

---

## 8. Security Posture

| Control | Configuration |
|---|---|
| User / Group | `runAsNonRoot: true`, fixed non-root UID/GID (`65532:65532`), shared `fsGroup` (`2000`) |
| Root filesystem | `readOnlyRootFilesystem: true` |
| Capabilities | `drop: ["ALL"]` |
| Privilege escalation | `allowPrivilegeEscalation: false` |
| Seccomp | Container runs under `RuntimeDefault`; supervisor installs a stricter BPF filter (`SCZ_SECCOMP`) and Landlock ruleset on every worker before `exec` |
| Network exposure | Supervisor listens on a single TCP port (`9464`) for probes and metrics; `sczctl` uses a sidecar-only Unix socket in `/run/sidecarzero-admin`; workers cannot create sockets |
| Pod Security Standards | Complies with `restricted` in all shield modes except `oomScoreAdj` |
| NetworkPolicy | Optional `sidecarzero.networkPolicy` template restricting port `9464` ingress |

---

## 9. Operations, Monitoring, and Administration

### Prometheus Alerts and Grafana Dashboard

`deploy/helm-kep753/examples/monitoring` provides `alerts.yaml` (a Prometheus Operator `PrometheusRule`) and `dashboard.json` (a Grafana dashboard). `scripts/check` verifies automatically that every metric referenced in `alerts.yaml` and `dashboard.json` is exported by the supervisor or SDKs.

| Alert | Trigger Condition |
|---|---|
| `SidecarZeroModelServingDegraded` | Fallback ratio > 5% for 10 minutes |
| `SidecarZeroWorkerCrashLoop` | `scz_worker_crashloop == 1` |
| `SidecarZeroMemoryShedding` | Time in `PSI_SHEDDING` above threshold |
| `SidecarZeroModelDoesNotFit` | `scz_model_fit == 0` |
| `SidecarZeroStandbySuppressed` | `scz_standby_suppressed == 1` |
| `SidecarZeroCallersShedding` | `scz_client_breaker_open > 0` for N minutes |
| `SidecarZeroHandoverFailed` | `increase(scz_handoffs_total{result!="ok"}[5m]) > 0` |
| `SidecarZeroDegraded` | `scz_degraded == 1` |
| `SidecarZeroModelRolledBack` | `increase(scz_model_rollbacks_total[15m]) > 0` |
| `SidecarZeroModelLoadTimedOut` | `increase(scz_load_timeouts_total[15m]) > 0` |
| `SidecarZeroSessionResetRequested` | `increase(scz_session_resets_total{cause!="host_exit"}[15m]) > 0` or `scz_session_reset_stuck == 1` |
| `SidecarZeroResponsesRejected` | `increase(scz_client_rejects_total[5m]) > 0` |
| `SidecarZeroRingFaultsReported` | `increase(scz_client_ring_faults_total[5m]) > 0` |
| `SidecarZeroOomShieldInactive` | `scz_oom_shield_mode{mode="none"} == 1` |
| `SidecarZeroKernelOomOccurred` | `increase(scz_kernel_oom_events_total[15m]) > 0` |

### Live Administration with `sczctl`

```bash
kubectl exec <pod> -c sidecarzero -- sczctl status
```

`sczctl` communicates with the supervisor over `/run/sidecarzero-admin/admin.sock`, mounted exclusively inside the sidecar container:

| Command | Description |
|---|---|
| `sczctl status` | Prints live gate state, worker generation, ring depths, slot-state counts, active holds, and recent transitions |
| `sczctl dump` | Receives read-only `memfd` descriptors over `SCM_RIGHTS` and writes an offline snapshot of both segments |
| `sczctl reload` | Triggers an immediate model reload |

---

## 10. Kind End-to-End Verification Suite (`deploy/kind/e2e.py`)

`scripts/kind-e2e` executes the 16-scenario Kubernetes end-to-end test suite in [`deploy/kind/e2e.py`](../../deploy/kind/e2e.py), verified across Kubernetes v1.34, v1.35, v1.36, and v1.37 ([empirical results](../benchmarks.md#6-kubernetes-e2e-verification-matrix)):

1. **Native sidecar startup ordering:** Sidecar starts before host; host starts and transitions cleanly to model predictions.
2. **Repeated worker `SIGKILL` under load:** Zero `EndpointSlice` condition changes (verified via Kubernetes Watch API) and zero host errors across repeated worker kills; warm standby promotes in sub-millisecond time.
3. **Gradual worker memory leak:** Supervisor hands over to warm standby at 85% utilization with zero fallbacks (and sheds/recycles cleanly when run without a standby), with zero container restarts.
4. **Sudden worker OOM spike across shield modes:** Verifies PID 1 survival, standby survival, and `EndpointSlice` stability across `none`, `nestedCgroup`, `oomScoreAdj`, and `singleProcess`.
5. **Supervisor kill (`SIGKILL` PID 1):** Kubelet restarts sidecar container; host SDK falls back during restart and reconnects to the new arena without restarting the host container.
6. **Host container restart:** Supervisor detects socket EOF, resets the session, and serves the reconnected host on the existing arena.
7. **Graceful pod deletion (`SIGTERM`):** In native mode, host terminates first while sidecar continues serving, then sidecar drains and exits cleanly; in classic mode, supervisor holds the session alive until the host disconnects.
8. **Live model reload in place:** Zero fallbacks during same-schema dual-worker reload; bounded `RELOADING` window on schema change and single-worker reload.
9. **ABI mismatch handling:** Host falls back with `ABI_MISMATCH` and recovers automatically once compatible versions are deployed.
10. **Pod Security Admission (`restricted`):** Verifies the Helm chart deploys cleanly into a namespace enforcing `pod-security.kubernetes.io/enforce: restricted`.
11. **Cross-UID socket sharing via `fsGroup`:** Host (`uid=10001`) and sidecar (`uid=65532`) connect via `0660` socket under `fsGroup: 2000`.
12. **Second client process rejection (`BUSY`):** A second process attempting to handshake receives `BUSY` and falls back cleanly without disturbing the active session.
13. **`sczctl` administration and isolation:** Verifies `sczctl status`, `dump`, and `reload` inside the sidecar container and confirms the admin socket is unreachable from the host container.
14. **Degraded startup vs specification fault:** Missing model file starts degraded (`startup.failOpen: true`, `/readyz` 200, `/modelz` 503); invalid configuration or insufficient memory limit fails fast and halts a rolling update at the first pod.
15. **Wedged supervisor event loop (`SIGSTOP`):** Readiness probe drops after ≈ 6 s, liveness probe restarts the sidecar after ≈ 15 s, and the host reconnects without restarting.
16. **Automatic model rollback:** A newly reloaded model that crashes on its first requests is automatically rolled back after 3 deaths to the previous file descriptor with zero container restarts.
