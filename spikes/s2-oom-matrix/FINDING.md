# S2 — Container OOM Behavior Matrix

[← Findings Summary](../FINDINGS.md)

## Objective

Measure what survives a sudden worker out-of-memory kill inside a Kubernetes cgroup v2 container across each OOM shield configuration ([12 — Kubernetes Deployment §5](../../docs/components/12-kubernetes-deploy.md#5-oom-shielding)): stock cluster (`none`), `oomScoreAdj` (`-1000`), `nestedCgroup`, and kubelet `singleProcessOOMKill: true` (`singleProcess`).

---

## Experimental Setup

Executed via `./run.sh` on Kind `v0.33.0` (Kubernetes `v1.37.0`, `containerd 2.3.4`, Linux `6.12.76` `aarch64`, cgroup v2).

A supervisor test binary (`oomtest`) runs as PID 1 inside a sidecar container with a `96 MiB` memory limit. It spawns a warm standby worker holding `8 MiB` RSS and then repeatedly spawns an active worker that allocates anonymous memory rapidly until the kernel OOM killer fires. After each OOM event, the harness records whether PID 1 survived, whether the warm standby survived, and whether Kubernetes restarted the container.

---

## Measured Results

| # | Shield Mode | Pod Security Posture | PID 1 Supervisor Survives | Warm Standby Survives | Kernel & Kubelet Behavior |
|---|---|---|---|---|---|
| 1 | `none` (stock cluster default) | `restricted` | **No** | No | Kubelet sets `memory.oom.group = 1` on the container cgroup. Kernel kills all processes in the container; kubelet restarts the container (`OOMKilled`). |
| 2 | `oomScoreAdj` on non-root user (`capabilities.add: ["SYS_RESOURCE"]`, `allowPrivilegeEscalation: false`) | Non-root | **No** | No | Effective capability set is `0x0`; writing `-1000` to `/proc/self/oom_score_adj` fails with `EACCES`. Behaves identically to row 1. |
| 3 | `oomScoreAdj` as root (`CAP_SYS_RESOURCE` effective) | `privileged` | **Yes** | **No** | Writing `-1000` succeeds. Kernel exempts PID 1 from the group kill, but `memory.oom.group = 1` still kills both the active worker and the warm standby. Zero container restarts; recovery is a cold respawn. |
| 4 | `nestedCgroup` in unprivileged container without cgroup delegation | `restricted` | **No** | No | `/sys/fs/cgroup` is mounted read-only (`EROFS`); supervisor automatically falls back to `none` (`scz_oom_shield_mode{mode="none"}`). |
| 5 | `nestedCgroup` with writable cgroup v2 subtree | Writable cgroup | **Yes** | **Yes** | Supervisor moves itself to `scz-supervisor` and places each worker in `scz-worker-{0,1}` with a `56 MiB` child `memory.max`. The runaway worker hits its child `memory.max` and dies alone without triggering the parent container's `memory.oom.group`. Both PID 1 and the warm standby survive. |
| 6 | `singleProcess` (kubelet `singleProcessOOMKill: true`) | `restricted` | **Yes** | **Yes** | Kubelet sets `memory.oom.group = 0` on the container cgroup. Kernel kills only the highest-scoring process (the active worker with `oom_score_adj = 1000`). Both PID 1 and the warm standby survive. |

---

## Architectural Conclusions

1. **Two-layer OOM defense ([12 — Kubernetes Deployment §5](../../docs/components/12-kubernetes-deploy.md#5-oom-shielding)):** Layer 0 (10 ms cgroup working-set polling, standby retirement/handover at 85%, and in-worker memory caps) prevents gradual leaks from ever reaching the kernel OOM killer on stock clusters. Layer 1 (`singleProcess`, `nestedCgroup`, or `oomScoreAdj`) protects PID 1 if an unconstrained allocation spike crosses from 85% to 100% inside a single 10 ms poll window.
2. **Automatic shield detection (`--oom-shield=auto`):** At startup, the supervisor inspects `memory.oom.group` (selecting `singleProcess` if `0`), otherwise attempts `nestedCgroup`, and otherwise falls back to `none`, exporting the active shield in `scz_oom_shield_mode{mode}`.
3. **Resolving container cgroup path from `/proc/self/cgroup`:** When a container runs with writable `/sys/fs/cgroup` access on `containerd`, `/sys/fs/cgroup` exposes the node's root cgroup tree rather than the container's leaf cgroup. The supervisor therefore always resolves its own cgroup directory from `/proc/self/cgroup` before creating `scz-supervisor` and `scz-worker-{0,1}`.
