# S1 — Sealed `memfd` Across Containers

[← Findings Summary](../FINDINGS.md)

## Objective

Verify that:

1. A sealed `memfd` created in the sidecar container and passed via `SCM_RIGHTS` over a Unix domain socket to the host application container remains charged to the sidecar container's cgroup v2 memory budget after the host maps and writes every page.
2. The `F_SEAL_FUTURE_WRITE` control segment maps read-only (`PROT_READ`) in the host and worker while rejecting `PROT_WRITE` mappings (`EPERM`) and `mprotect(..., PROT_WRITE)` escalations (`EACCES`).
3. Cross-container `futex(FUTEX_WAIT_BITSET)` and `futex(FUTEX_WAKE_BITSET)` operations succeed on words inside the read-only control segment under Kubernetes `restricted` Pod Security Standards.

---

## Experimental Setup

Two validation programs (`supervisor.c` and `client.c`): the supervisor creates a `64 KiB` control segment (`F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_FUTURE_WRITE`) and a `64 MiB` data segment (`F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW`), and the client receives both over `SOCK_SEQPACKET` (`MSG_CMSG_CLOEXEC`), verifies seal enforcement, writes every page of the `64 MiB` data segment, and parks on a futex word inside its read-only control mapping.

| Environment | Command | Configuration |
|---|---|---|
| Multi-container Docker | `./run.sh` | Linux `6.12.76` (`aarch64`), cgroup v2, separate non-root container UIDs sharing a supplementary group |
| Kubernetes native sidecar pod (Kind) | `./run.sh kind` | Kind `v0.33.0`, Kubernetes `v1.37.0`, `containerd 2.3.4`, KEP-753 native sidecar (`uid=1000`) + app container (`uid=1001`), `fsGroup: 2000`, `readOnlyRootFilesystem: true`, `capabilities.drop: ["ALL"]`, `seccompProfile: RuntimeDefault` |

Each environment was tested both **with** supervisor pre-faulting (`MADV_POPULATE_WRITE` / page-stride write before sending descriptors) and **without** supervisor pre-faulting.

---

## Measured Results

### 1. Cgroup v2 Memory Charge Attribution

After the client container mapped and wrote all `64 MiB` of the data segment:

| Environment | Pre-Faulted by Supervisor | Sidecar Container Cgroup (`memory.current`) | Host App Container Cgroup Delta |
|---|---|---|---|
| Docker | **Yes** | **`64.0 MiB`** (`shmem`) | `-0.6 MiB` (noise) |
| Docker | No | `0 MiB` | **`+63.7 MiB`** |
| Kind Pod | **Yes** | **`64.0 MiB`** (`shmem`) | `-0.3 MiB` (noise) |
| Kind Pod | No | `0 MiB` | **`+63.7 MiB`** |

In Linux cgroup v2, anonymous shared-memory pages (`memfd` / `tmpfs`) are permanently charged to the memory cgroup of the process that first faults each page into existence. When the supervisor pre-faults the segments before sending `WELCOME`, 100% of the arena remains charged to the sidecar container regardless of how heavily the host writes to it. Conversely, if pre-faulting is omitted, the host container's `mmap(..., MAP_POPULATE)` or `MADV_POPULATE_READ` faults the pages first and transfers the entire `64 MiB` charge to the host container before a single request is sent.

### 2. Seal Enforcement and Cross-Container `futex` (`results/*-client.txt`)

| Verification Check | Result |
|---|---|
| `mmap(..., PROT_READ \| PROT_WRITE, MAP_SHARED)` on control segment | Rejected by kernel with `EPERM` |
| `mmap(..., PROT_READ, MAP_SHARED)` on control segment | Succeeds and observes live supervisor writes |
| `mprotect(..., PROT_READ \| PROT_WRITE)` on mapped control segment | Rejected by kernel with `EACCES` |
| `ftruncate` shrink/grow on data segment | Rejected by kernel with `EPERM` |
| `futex(FUTEX_WAIT_BITSET)` on read-only control segment woken by supervisor container | Succeeds |
| `MSG_CMSG_CLOEXEC` sets `FD_CLOEXEC` on received descriptors | Succeeds |
| Cross-UID socket connection (`uid=1001` → `uid=1000`, mode `0660`, `fsGroup: 2000`) | Succeeds |
| `SO_PEERCRED` across separate container PID namespaces | Returns peer UID and GID accurately; returns `pid = 0` because containers have disjoint PID namespaces |
| `clone3` vs `clone(SIGCHLD \| CLONE_PIDFD)` under `containerd` `RuntimeDefault` seccomp | `clone3` returns `ENOSYS`; fallback to `clone` with `CLONE_PIDFD` succeeds |

---

## Architectural Conclusions

1. **Mandatory supervisor pre-faulting ([01 — Shared-Memory ABI §7](../../docs/components/01-shm-abi.md#7-file-backing-and-sealing)):** The supervisor pre-faults every page of both `memfd` segments before applying `F_SEAL_FUTURE_WRITE` and before passing descriptors to any peer, guaranteeing that arena memory is always accounted against the sidecar container's cgroup limit.
2. **Read-only control plane ([01 — Shared-Memory ABI §2](../../docs/components/01-shm-abi.md#2-two-segment-model)):** `F_SEAL_FUTURE_WRITE` enforces at the kernel level that neither the host process nor any worker process can corrupt the gate word, directory, or model seqlock, while still permitting shared `futex` wait/wake operations (`standby_wait`) on the read-only mapping.
3. **Peer credential verification ([04 — Supervisor §4](../../docs/components/04-supervisor.md#4-handshake-and-session-management)):** The supervisor authenticates connecting peers using `SO_PEERCRED` UID/GID checks and treats the client PID reported in `HELLO` as informational metadata.
