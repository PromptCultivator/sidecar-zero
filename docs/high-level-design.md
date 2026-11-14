# SidecarZero — High-Level Design & Architecture

SidecarZero is a fault-isolated, zero-copy machine-learning inference sidecar for Kubernetes pods. It decouples the host application container from native ML runtimes (ONNX Runtime, llama.cpp / GGUF, CPython) across a process and container boundary while preserving in-process latency and guaranteeing bounded, exception-free fallback when the model runtime crashes, stalls, reloads, or exhausts memory.

---

## 1. System Overview & Goals

### 1.1 The Problem

Running ML inference inside latency-critical services forces a trade-off between two flawed deployment models:

1. **In-process native runtimes (JNI, C++ FFI, cgo, embedded Python):** Fast, but a segmentation fault, native memory leak, allocator fragmentation, or CPU/GIL stall inside the ML runtime crashes or stalls the entire host application process, dropping all in-flight requests on that pod and triggering Kubernetes `EndpointSlice` churn.
2. **Out-of-process RPC sidecars or remote model servers (HTTP/JSON, gRPC, Unix sockets):** Process-isolated, but serialize payloads on every call, allocate on the heap, pay multiple kernel context switches, and suffer tens of milliseconds of failed requests or container restarts when the model server crashes.

SidecarZero eliminates this trade-off by combining **process and container isolation** in a Kubernetes native sidecar (KEP-753) with a **lock-free, kernel-sealed shared-memory transport** (`memfd_create` + `FUTEX_WAIT_BITSET`), **pre-stamped Apache Arrow IPC frames**, **sub-millisecond robust-futex death detection**, **warm-standby promotion**, and a **mandatory caller fallback contract**.

### 1.2 Design Goals

| Dimension | Goal | Measured Performance |
| :--- | :--- | :--- |
| **Transport overhead (spinning)** | `< 1 µs` p50, `< 10 µs` p99 round trip | `0.46 µs` p50 (`0.58 µs` p99) for 16 B; `0.88 µs` p50 (`1.12 µs` p99) for 1 KB |
| **Admission / disconnected fallback** | `< 100 ns` SDK overhead | `0.04 µs` (`40 ns`) p50–p99.9 |
| **Crash detection & gate close** | `< 1 ms` from `SIGKILL` or segfault to gate closed, independent of worker RSS | `0.05–0.12 ms` median (`< 0.5 ms` up to 2 GiB private RSS) via per-thread robust death locks |
| **Warm-standby promotion** | `< 5 ms` p99 from worker kill to `HEALTHY` | `0.23 ms` (`232.4 µs`) median, `1.14 ms` max across 59 kills at 5,000 RPS |
| **Kubernetes stability** | `0` `EndpointSlice` transitions and `0` container restarts across worker kills, hangs, leaks, and reloads | `0` flaps across 1,000-kill soaks and Kind v1.34–v1.37 clusters |
| **Live model reload** | `0` fallbacks on same-schema reload | `0` fallbacks over `314,191` continuous calls |
| **Hot-path heap allocation** | `0 B/call` in Rust, Java, C++, and Go; flat block count in Python | `0.0 B/call` verified on model, fallback, timeout, shed, and first-call paths |
| **Idle CPU footprint** | `< 2%` of one CPU core with no traffic | `0.38%–2.00%` of one core (supervisor 1 ms tick + worker 5 ms heartbeat) |
| **Supervisor footprint** | Static binary `< 5 MB`, no ML runtime, no async runtime | `0.86 MB` stripped static musl binary (`aarch64`) |

### 1.3 Explicit Non-Goals

- **Cross-node or network RPC:** SidecarZero is strictly intra-pod IPC over shared memory and Unix domain sockets.
- **Multi-second autoregressive token streaming:** The synchronous call path targets inference budgets up to `max_deadline_ns` (100 ms default): tabular scoring, ranking, fraud detection, embeddings, and small sequence classifiers.
- **Multi-process client sessions per arena in v1.0:** One client process (with any number of threads, goroutines, or virtual threads) attaches per arena; multi-session deployments provision one independent arena per session (`--sessions=N`).

---

## 2. Architecture & Process Topology

A SidecarZero deployment runs two containers inside a single Kubernetes pod:

```
┌────────────────────────────────────────────────────────────────────────────┐
│ Kubernetes Pod                                                             │
│                                                                            │
│  ┌──────────────────────────────┐     ┌─────────────────────────────────┐  │
│  │ App Container (Host)         │     │ Sidecar Container               │  │
│  │                              │     │                                 │  │
│  │  Host Service                │     │  sidecarzero-supervisor (PID 1) │  │
│  │  └── SidecarZero SDK         │     │  ├── epoll control loop         │  │
│  │      ├── Fast path (threads) │     │  ├── death-lock watcher threads │  │
│  │      └── Background conn mgr │◄───►│  ├── HTTP (:9464) & admin sock  │  │
│  └──────────────┬───────────────┘ UDS └───────┬───────────────┬─────────┘  │
│                 │             (SCM_RIGHTS)    │               │            │
│                 │                             ▼               ▼            │
│                 │                    ┌──────────────┐ ┌──────────────┐     │
│                 │                    │ Active       │ │ Warm Standby │     │
│                 │                    │ Worker (gen) │ │ Worker(gen+1)│     │
│                 │                    └──────┬───────┘ └──────┬───────┘     │
│                 ▼                           ▼                ▼             │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │ Shared Memory Arena (memfd pair)                                     │  │
│  │  • scz_ctl  (read-only to host & workers, F_SEAL_FUTURE_WRITE)       │  │
│  │      Control Header (gate, supervisor_hb, model seqlock, directory)  │  │
│  │      Registry Block (Arrow Schema messages, capacity templates)      │  │
│  │  • scz_data (read-write)                                             │  │
│  │      Status Page (BEAT[0..1], client_rejects, client_ring_faults)    │  │
│  │      Staging Regions, Vyukov MPSC Rings, Slot Table & Slot Payloads  │  │
│  └──────────────────────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 Three-Segment Memory Model

1. **Control segment (`scz_ctl`):** Created by the supervisor via `memfd_create`, pre-faulted, mapped writable by the supervisor alone, and then sealed with `F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_FUTURE_WRITE | F_SEAL_SEAL`. Because of `F_SEAL_FUTURE_WRITE`, the Linux kernel enforces that every subsequent `mmap` by the worker or host SDK is strictly `PROT_READ` and rejects `mprotect(..., PROT_WRITE)` with `EACCES`. It holds the 4 KB control header (including `gate`, `supervisor_hb_mono_ns`, the model seqlock, and the arena directory) followed by the verified Arrow template registry.
2. **Data segment (`scz_data`):** Sealed with `F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL` and mapped `PROT_READ | PROT_WRITE` by all participants. It contains the 4 KB status page (two worker beat slots, client fault counters, and striped worker histograms), two registry staging buffers, up to 8 Vyukov MPSC descriptor rings, and up to 8 slab classes of 64-byte slot records and 64-byte-aligned request/response regions.
3. **Per-worker death-lock segment (`scz_wrk`):** A 4 KB `memfd` shared exclusively between the supervisor and a single worker process (never sent to the host or to the standby worker). Each worker consume thread holds a process-shared robust mutex (`PTHREAD_MUTEX_ROBUST`) in its 64-byte cell for its entire lifetime.

---

## 3. End-to-End Request Lifecycle

### 3.1 Happy Path

1. **Admission (1 cache line, 0 syscalls):** The caller loads the current arena/binding snapshot, reads `CLOCK_MONOTONIC`, and loads `gate` and `supervisor_hb_mono_ns` from offset `0x040` of `scz_ctl`. If `state(gate) != HEALTHY` or `supervisor_hb + hb_stale_ns < now`, the SDK immediately invokes the caller's fallback.
2. **Slot Claim:** The caller claims a caller-state entry (setting its `in_call` marker) and probes up to 16 slots starting from its cached slot index via `CAS((epoch, FREE) → (epoch + 1, CLAIMED))`.
3. **Arrow Template Encode:** If the slot's `req_stamp` differs from `(registry_epoch16 << 16) | template_id`, the SDK copies the pre-rendered Arrow IPC metadata prefix into the slot. If the row count changed since the slot's last use, it patches the fixed-offset 8-byte length fields (`PatchDesc`), copies the feature values into the 64-byte-aligned column buffer, and stores `(epoch, READY)` with `Release` ordering.
4. **Ring Publish & Wake:** The caller claims a cell in the next MPSC ring (round-robin across rings, bounded at 64 CAS attempts), writes `slot_index`, `slot_epoch`, `slab_class`, and `deadline_mono_ns`, and publishes `cell.seq = pos + 1` with `SeqCst` ordering. It then checks `ring.consumer_wait` (`SeqCst`); if the worker thread is `PARKED` or `WAKING`, it transitions `PARKED → WAKING` and calls `futex_wake`.
5. **Gate Re-Check & Spin-Then-Park Wait:** Immediately after publishing, the caller reloads `gate` (`SeqCst`). If the gate is no longer `HEALTHY` and the slot is still `READY`, it cancels `(epoch, READY) → (epoch, FREE)` and falls back. Otherwise it spins up to its adaptive spin budget (`min(ceiling, 2 × EWMA, deadline)`) and, if still unresolved, sets `WAITER` via CAS and parks in `futex(FUTEX_WAIT_BITSET, expected, absolute_deadline)`.
6. **Worker Consume & Zero-Copy Inference:** The worker thread dequeues the cell, bounds-checks `slab_class` and `slot_index` against `scz_ctl`'s directory, and claims the slot via `CAS((epoch, READY) → (epoch, PROCESSING | OWNER))`, preserving `WAITER`. It checks that `now + expected_run < deadline_mono_ns`, verifies `req_stamp` and `req_len`, wraps the request and response column buffers directly as zero-copy tensors, runs inference, stamps the response region, writes `resp_len`, `status`, and `model_gen`, and transitions `(epoch, PROCESSING) → (epoch, DONE)`. If `WAITER` was set, it calls `futex_wake`.
7. **Decode & Release:** The caller wakes, reads `status`, `model_gen`, `resp_stamp`, and `resp_len` once into locals, verifies response integrity, decodes the output from the slot, releases the slot with `store((epoch, FREE), Release)`, clears its `in_call` marker, and returns `Outcome::Model`.

### 3.2 Deadline Expiry (Caller Abandonment)

If `wait_until` expires before completion:
- If the slot is still `READY`, the caller CASes `(epoch, READY) → (epoch, FREE)` and runs the fallback with `FallbackReason::Deadline`. When the worker later dequeues the stale descriptor, its claim fails on state/epoch mismatch and it skips the cell.
- If the slot is `PROCESSING`, the caller CASes `(epoch, PROCESSING, OWNER) → (epoch, ABANDONED, OWNER)` (preserving `OWNER`), forgets its cached slot index, and runs the fallback. When the worker finishes inference, its `PROCESSING → DONE` CAS fails, it observes `ABANDONED`, and it frees the slot via `CAS((epoch, ABANDONED) → (epoch, FREE))`.
- If the worker wins the race and transitions the slot to `DONE` before the abandon CAS, the caller accepts the valid model result.

### 3.3 Worker Crash & Warm-Standby Promotion

When the active worker crashes or is killed (`SIGKILL`):
1. **Early death signal (~50–120 µs):** As the kernel begins tearing down the dying worker's threads, it walks each thread's robust futex list *before* freeing the process's address space (`exit_mmap`). The supervisor's watcher threads wake on `scz_wrk.death_lock[i]` and signal an `eventfd`.
2. **Gate close & queue sweep (~40 µs):** On the first death-lock report, the supervisor takes the `DEAD` hold on `gate` (`SeqCst`), sends `SIGKILL` to ensure all worker threads terminate, fails every `READY` slot (`READY → FAILED`), and issues `futex_wake` on every slot with `WAITER` set and every `DONE` slot. New calls immediately fall back at admission in ~40 ns; queued calls wake and fall back within microseconds.
3. **Ring repair & standby activation (~95 µs):** Once each ring's consume thread has reported death, the supervisor repairs any mid-dequeue ring head (`cells[head & mask].seq` at `head + capacity` or `head + capacity + 1`), sets `gate.worker_gen` to the warm standby's generation, increments and wakes `standby_wait`, waits for `ACTIVE` on the standby's status pipe, and drops the `DEAD` hold—restoring `HEALTHY` in ~230 µs median.
4. **In-flight slot reclamation (on `pidfd`):** Once the kernel finishes freeing the dead worker's private memory and its `pidfd` becomes readable, the supervisor sweeps all `PROCESSING` and `ABANDONED` slots matching the dead worker's `OWNER` bits (`PROCESSING → FAILED`, `ABANDONED → FREE`), wakes any remaining waiters, and spawns a replacement warm standby into the freed beat slot.

---

## 4. Key Design Decisions & Engineering Rationale

### 4.1 Two-Segment Arena with `F_SEAL_FUTURE_WRITE`

**Decision:** Split the arena into a read-only control segment (`scz_ctl`) and a read-write data segment (`scz_data`), plus a private per-worker segment (`scz_wrk`).
**Rationale:** In a single writable segment, "only the supervisor writes the gate and directory" is merely a convention that a wild pointer in a native C++ worker or host library can violate. Sealing `scz_ctl` with `F_SEAL_FUTURE_WRITE` after the supervisor maps it enforces single-writer integrity in the kernel. Placing the robust death locks in `scz_wrk` prevents either the host or a standby worker from forging the active worker's death.

### 4.2 Pre-Faulted Arena Attribution

**Decision:** The supervisor faults every page of both `memfd` segments with `MADV_POPULATE_WRITE` (falling back to a byte-per-page write on kernels `< 5.14`) before sealing or sharing them, and clients/workers populate their own page tables with `MADV_POPULATE_READ` / `MADV_POPULATE_WRITE` at attach time.
**Rationale:** Linux cgroup v2 charges shared `memfd` pages to the cgroup of the task that *first touches* each page. Pre-faulting in the supervisor guarantees the entire arena is billed to the sidecar container (verified in [`spikes/s1-memfd-across-containers`](../spikes/s1-memfd-across-containers/FINDING.md): 64.0 MiB charged to the sidecar and 0 MiB to the host container), while client page-table population eliminates minor page faults on the first requests after attach or standby promotion.

### 4.3 Pre-Stamped Arrow IPC Capacity Templates

**Decision:** Render Arrow IPC `RecordBatch` frames once at model load time for `max_rows` with 64-byte buffer alignment, validate them in an isolated supervisor helper process, publish them in `scz_ctl`'s registry, and stamp slot regions once per `(registry_epoch, template_id)`.
**Rationale:** For fixed-width numeric schemas, an Arrow IPC `RecordBatch` message is byte-for-byte identical across calls except for the column value buffers and a small set of 8-byte length fields at fixed metadata offsets (`RecordBatch.length`, field node lengths, and buffer lengths). Patching those 8-byte fields takes ~2 ns versus ~85 ns and heap allocations for FlatBuffers serialization, while keeping every slot a 100% standard Arrow IPC message readable by Arrow C++, Rust, Go, Java, and PyArrow (verified across 162 reader/frame combinations in [`spikes/s4-arrow-templates`](../spikes/s4-arrow-templates/FINDING.md)).

### 4.4 Per-Thread Robust Death Locks & `RLIMIT_CORE = 0`

**Decision:** Detect worker crashes via per-consume-thread `PTHREAD_MUTEX_ROBUST` locks in `scz_wrk`, backed by `pidfd`, and spawn workers with `RLIMIT_CORE = 0` by default.
**Rationale:** Kernel `pidfd` exit notifications fire only after `exit_mmap` releases all private pages of the dying process, which scales at ~15 ms per GiB of private RSS (`29.9 ms` at 2 GiB). In contrast, the kernel walks a dying thread's robust futex list *before* tearing down memory, waking the supervisor in `0.05–0.12 ms` regardless of worker memory size ([`spikes/s6-standby-promotion`](../spikes/s6-standby-promotion/FINDING.md), [`spikes/s6b-death-signal`](../spikes/s6b-death-signal/FINDING.md)). Furthermore, if kernel core dumps are enabled (`RLIMIT_CORE > 0`), the kernel freezes all threads of a crashing process while writing the core dump before releasing robust locks (`289 ms` delay for a 512 MiB worker); setting `RLIMIT_CORE = 0` eliminates this delay and prevents request payloads from being written to disk.

### 4.5 Ranked Gate Holds Instead of Direct `HEALTHY` Stores

**Decision:** No recovery or lifecycle path directly writes `HEALTHY` to `gate`. Instead, subsystems acquire and drop typed holds (`DRAINING > DEAD > RELOADING > PSI_SHEDDING > INIT`), and a single function recomputes `gate` from the highest-ranked active hold.
**Rationale:** Multiple fault or lifecycle events can overlap (for example, a standby promotion completing while a session reset or memory-pressure shedding is active). Ranked holds guarantee the gate returns to `HEALTHY` if and only if every condition holding it closed has resolved.

### 4.6 Layered OOM Shielding & Proactive Pressure Relief

**Decision:** Combine a 10 ms cgroup v2 / PSI monitor (Layer 0) with configurable kernel OOM shields (`auto`, `singleProcess`, `nestedCgroup`, `oomScoreAdj`, `none`; Layer 1).
**Rationale:** By default on cgroup v2, the kubelet sets `memory.oom.group = 1` on every container cgroup, causing a kernel OOM in the worker to kill the PID 1 supervisor and flap pod readiness. Layer 0 prevents gradual leaks from reaching the kernel OOM killer by retiring or handing over to the warm standby at 85% utilization, shedding and issuing `TRIM` (`malloc_trim` with `M_MMAP_THRESHOLD = 256 KB`), and recycling the worker after `shed_max` (2 s). Layer 1 protects against sub-10 ms spikes using `singleProcessOOMKill` or per-worker child cgroups (`nestedCgroup`) so only the offending worker process is terminated ([`spikes/s2-oom-matrix`](../spikes/s2-oom-matrix/FINDING.md)).

### 4.7 ONNX Runtime Memory Profiles & Weight Prepacking

**Decision:** Provide two worker memory profiles—**Profile A** (default: `DisableCpuMemArena`, `DisableMemPattern`, `M_MMAP_THRESHOLD = 256 KB`, glibc `malloc_trim(0)`) and **Profile B** (environment-level BFC arena hard-capped via `max_mem` and `session.use_env_allocators` with `enable_memory_arena_shrinkage`)—and expose `--prepack` (`model.prepack`).
**Rationale:** ONNX Runtime's arena shrinkage option errors if `DisableCpuMemArena` is set, so they are separated into two clean profiles. Fixing glibc's mmap threshold at 256 KB prevents thread-arena retention after large batches, returning RSS within 1.1 MB of baseline after `TRIM`. By default, ONNX Runtime prepacks matrix weights into private memory (`27×` faster on MLP models than `disable_prepacking`), whereas tree ensembles always build small private structures ([`spikes/s5-ort-foreign-memory`](../spikes/s5-ort-foreign-memory/FINDING.md)).

### 4.8 In-Call Markers for Safe Arena Retirement

**Decision:** Track active calls in a per-client table of cache-line-aligned `CallerEntry` structs using a single 64-bit `in_call` word packing `(snapshot_id << 32) | tid`, and perform all `munmap` calls exclusively on the SDK's background connection thread.
**Rationale:** Reference-counting arenas on the request path would let the last finishing request thread run a slow `munmap` syscall inside `infer`, while relying on garbage-collector reachability in Java or Go is unsafe when raw addresses are passed to `futex` and can leak 8–16 MB arenas when allocation rates are zero. Packing `tid` into the same atomic word as `snapshot_id` allows the background thread in C++/Rust/Python to check `tgkill(pid, tid, 0) == ESRCH` and safely retire arenas even if a host thread was killed mid-call without unwinding ([`spikes/s7-platform-facts`](../spikes/s7-platform-facts/FINDING.md)).

### 4.9 JDK 22+ FFM Baseline & Carrier-Pinning Cap

**Decision:** Target JDK 22+ (tested on JDK 22 and JDK 25 LTS) using finalized `java.lang.foreign` (JEP 454), cap virtual threads parked in native `futex` downcalls at `carriers − 1`, and switch excess virtual callers to `LockSupport.parkNanos` polling.
**Rationale:** JEP 454 was finalized in JDK 22. A virtual thread blocked in a native `futex` downcall pins its carrier thread without triggering JFR's `VirtualThreadPinned` event or spawning spare carriers; capping native futex waits at `carriers − 1` and using `LockSupport.parkNanos` (never `Thread.yield()`) above the cap guarantees at least one free carrier so unrelated virtual threads never starve ([`spikes/s3-java-ffm`](../spikes/s3-java-ffm/FINDING.md)).

### 4.10 Formal Verification & Tagged Store Orderings

**Decision:** Model the slot/ring/sweep and consumer-handover protocols in PlusCal/TLA+ (`spec/SlotsRing.tla`, `spec/Consumers.tla`), model-check the Rust implementation with Loom, write store-then-load barriers as `store(Release) → fence(SeqCst) → load(Acquire)`, and tag every shared-memory store for deterministic crash-point injection (`SCZ_CRASH_AT`).
**Rationale:** TLA+ model checking uncovered two subtle multi-party races (a worker dying between `PROCESSING → DONE` CAS and `futex_wake`, solved by waking `DONE` slots during sweep; and an unwinding producer skipping the waker rule on a `VOID` cell). Writing store-load fences with an explicit `fence(SeqCst)` has identical hardware cost to `SeqCst` stores/loads while enabling exhaustive verification under Loom, which treats `Ordering::SeqCst` loads/stores as `Acquire`/`Release` ([`spikes/s7-platform-facts`](../spikes/s7-platform-facts/FINDING.md)).

---

## 5. Component Specifications & Repository Map

| # | Component Document | Implementation Path | Scope |
| :---: | :--- | :--- | :--- |
| 01 | [Shared-Memory ABI](components/01-shm-abi.md) | `abi/scz_abi.toml`, `crates/scz-shm`, `crates/scz-abi-gen` | Segment layout, control header, status page, Vyukov MPMC rings, slot state machine, supervisor sweep, and minor-version upgrade rules. |
| 02 | [Signaling: Adaptive Spin-Then-Wake](components/02-signaling.md) | `crates/scz-shm/src/wait.rs`, all SDKs | `consumer_wait` (`AWAKE`/`PARKED`/`WAKING`), `slot.word` `WAITER` bit, `standby_wait` counter, adaptive spin budgets, and CPU quota backoff. |
| 03 | [Arrow Data Plane](components/03-arrow-data-plane.md) | `crates/scz-shm/src/registry.rs`, `crates/scz-worker-core` | Pre-stamped Arrow IPC `RecordBatch` templates, `RegistryHeader`/`TemplateDesc`/`ColumnDesc`/`PatchDesc`, xxHash3-64 schema fingerprint, and live schema evolution. |
| 04 | [Supervisor](components/04-supervisor.md) | `crates/scz-supervisor` | PID 1 event loop, helper processes, `SOCK_SEQPACKET` handshake, robust death locks, warm standby, live model reload & rollback, cgroup/PSI monitor, HTTP probes, and `sczctl`. |
| 05 | [Worker](components/05-worker.md) | `crates/scz-worker-{core,ort,gguf,echo,chaos}` | Worker lifecycle, consume loop, deadline shedding, `TRIM`/`DRAIN`/`HANDOVER`, seccomp/Landlock sandboxing, ORT, GGUF, Python, and chaos backends. |
| 06 | [SDK Contract](components/06-sdk-contract.md) | `sdks/*` | Normative `infer` algorithm, mandatory `FallbackPolicy`, 19 `FallbackReason` codes, caller-side overload breaker, arena retirement, and unified Prometheus metrics. |
| 07 | [Java FFM SDK](components/07-sdk-java-ffm.md) | `sdks/java-ffm` | JDK 22+ `java.lang.foreign` client, `ScoreClient`/`ScoreOutcome`, `VarHandle` accessors, and virtual-thread carrier cap. |
| 08 | [C++20 SDK](components/08-sdk-cpp20.md) | `sdks/cpp20` | Header-only C++20 client, `FallbackPolicy`/`Codec` concepts, `__atomic_*` accessors, `-fno-exceptions` support, and CMake package. |
| 09 | [Go SDK](components/09-sdk-go.md) | `sdks/go` | Pure-Go 1.24 client (no `cgo`), `Client` and `Session` APIs, stack-striped sampling, and parked-caller cap. |
| 10 | [Rust SDK](components/10-sdk-rust.md) | `sdks/rust` | Reference `#![forbid(unsafe_code)]` client over `scz-shm`, typestate builder, `ShmCell<T>` volatile views, and `InProcessClient`. |
| 11 | [Python SDK & Worker Runner](components/11-sdk-python.md) | `sdks/python` | `abi3` PyO3 client with buffer-protocol inputs, native Rust fallbacks, and `python -m sidecarzero.worker` runner. |
| 12 | [Kubernetes Deployment](components/12-kubernetes-deploy.md) | `deploy/helm-kep753`, `deploy/docker`, `deploy/kind` | KEP-753 native sidecar topology, distroless images, Helm library chart, OOM shield modes, probes, and 16 Kind E2E tests. |
| 13 | [Chaos Benchmark](components/13-chaos-benchmark.md) | `bench/` | Open-loop benchmark harness, Systems A–E, fairness rules, and 70-run evaluation methodology. |
| 14 | [Testing and Verification](components/14-testing-verification.md) | `spec/`, `conformance/`, `fuzz/`, `scripts/check` | TLA+ specs, Loom models, crash-point injection, libFuzzer targets, 56 conformance scenarios, and allocation/performance gates. |
| 15 | [Failure Modes and Recovery](components/15-failure-modes.md) | Cross-cutting | 40-row FMEA matrix, step-by-step recovery timelines, residual risks, and operator runbook. |

---

## 6. Security & Trust Boundaries

| Participant | Trust Level | Capabilities & Restrictions |
| :--- | :--- | :--- |
| **Supervisor (`sidecarzero-supervisor`)** | Trusted control plane (PID 1 of sidecar) | Non-root, `PR_SET_DUMPABLE = 0` (blocks `ptrace`, `/proc/<pid>/mem`, and `pidfd_getfd` from same-UID workers), read-only root filesystem, all Linux capabilities dropped. Never parses model files or worker-authored FlatBuffers in-process; delegates SHA-256 hashing and FlatBuffers registry verification to short-lived sandboxed helper processes over a sealed copy of the staged bytes. |
| **Worker (`sidecarzero-worker*`)** | Untrusted (executes third-party model graphs and native kernels) | Maps `scz_ctl` read-only (`F_SEAL_FUTURE_WRITE`). Runs under `PR_SET_NO_NEW_PRIVS`, `PR_SET_PDEATHSIG(SIGKILL)`, `RLIMIT_CORE = 0`, `RLIMIT_NOFILE`, `oom_score_adj = 1000`, `close_range(8, MAX)` (receives only the 5 required descriptors), Landlock read-only filesystem allowlist, and a seccomp-BPF filter verifying syscall architecture and blocking all network socket syscalls (`socket`, `connect`, `bind`, `listen`, `accept*`, `sendto`, `sendmsg`, `io_uring_setup`) and cross-process inspection calls. |
| **Host Application (`sdks/*`)** | Mutually distrusting with worker | Authenticates over `/run/sidecarzero/scz.sock` (`mode 0660`, shared `fsGroup`, verified via `SO_PEERCRED`). Maps `scz_ctl` read-only. Never trusts lengths, offsets, or sequence numbers in `scz_data`: copies every shared field once via atomic/volatile load, bounds-checks against the immutable directory in `scz_ctl`, and caps every CAS/enqueue loop at 64 iterations so a corrupted data segment cannot cause an out-of-bounds access or infinite loop. |

---

## 7. Observability & Operational Interface

### 7.1 HTTP Endpoints (Supervisor, Default Port `:9464`)

| Endpoint | HTTP 200 Condition | Purpose |
| :--- | :--- | :--- |
| `/startupz` | Arena sealed and socket bound (or degraded start; or model warm when `--wait-for-model` is set) | Kubernetes `startupProbe`; gates host container startup. |
| `/readyz` | Supervisor event loop is running (**independent of worker state**) | Kubernetes `readinessProbe`; prevents `EndpointSlice` flaps during worker restarts. |
| `/livez` | Supervisor event loop has ticked within the last second | Kubernetes `livenessProbe`; restarts sidecar only if PID 1 wedges. |
| `/modelz` | `gate` state is `HEALTHY` (otherwise HTTP 503 with JSON diagnostic body) | Rollout gates (Argo Rollouts / Flagger), operator checks, or optional host readiness chaining. |
| `/metrics` | Always | Prometheus text exposition for supervisor and worker metrics. |

### 7.2 Exported Prometheus Metrics (27 Series Checked in CI)

**Supervisor & Worker Metrics (`/metrics`):**
- Gate & model state: `scz_gate_state`, `scz_gate_generation`, `scz_model_info{sha256,gen}`, `scz_degraded`
- Worker lifecycle & recovery: `scz_worker_spawns_total`, `scz_worker_deaths_total`, `scz_worker_restarts_total`, `scz_worker_condemned_total`, `scz_standby_promotions_total`, `scz_promotion_seconds`, `scz_worker_crashloop`
- Memory & OOM protection: `scz_memory_utilization_ratio`, `scz_shedding`, `scz_standby_suppressed`, `scz_model_fit`, `scz_oom_shield_mode{mode}`, `scz_kernel_oom_events_total{kind}`
- Reloads & handovers: `scz_reloads_total`, `scz_model_rollbacks_total`, `scz_models_rejected_total`, `scz_load_timeouts_total`, `scz_model_modified_total`, `scz_handoffs_total{result}`
- Client session & fault counters: `scz_session_resets_total{cause}`, `scz_session_reset_stuck`, `scz_client_rejects_total`, `scz_client_ring_faults_total`

**Host SDK Metrics (`MetricsSink` / `PrometheusText` across all 5 SDKs):**
- `scz_client_calls_total{source,reason}` (`source="model"` and `source="fallback"` across all 19 `FallbackReason` values)
- `scz_client_latency_seconds{source}` (sampled 16-bucket histogram)
- `scz_client_fallback_seconds` (sampled 16-bucket histogram)
- `scz_client_budget_clamped_total`
- `scz_client_breaker_open`
- `scz_client_late_wakes_total`
- `scz_client_reconnects_total`
- `scz_client_state` (`0` disconnected, `1` connecting, `2` bound, `3` rejected, `4` schema rejected)
- `scz_client_model_gen`

---

## 8. Platform Requirements & Configuration Reference

### 8.1 Platform & Language Baselines

- **Operating system:** Linux kernel `≥ 5.10` (`≥ 5.14` recommended for `MADV_POPULATE_WRITE` and Landlock), `x86_64` or `aarch64`, cgroup v2. (macOS and Windows compile every SDK into an always-fallback client with `InProcessClient` test doubles for local development.)
- **Kubernetes:** `≥ 1.29` for KEP-753 native sidecars (GA in `1.33+`); classic container mode supported on older clusters.
- **SDK toolchains:** Rust `1.85+`, JDK `22+` (tested on JDK 22 and JDK 25), C++20 (`GCC 11+` / `Clang 14+`), Go `1.24+`, CPython `3.11+` (`abi3`).

### 8.2 Default Operational Parameters

| Parameter | Default | Description |
| :--- | :--- | :--- |
| `hb_interval_ns` | `5 ms` | Supervisor and worker heartbeat write interval |
| `hb_stale_ns` | `15 ms` | Heartbeat staleness threshold (3 missed beats) |
| `spin_ns` | `50 µs` | Default adaptive spin ceiling before parking on `futex` |
| `default_deadline_ns` | `2 ms` | Default per-call inference budget |
| `max_deadline_ns` | `100 ms` | Hard ceiling on any call's inference budget |
| `ring_capacity` | `4,096` cells | Power-of-two capacity per MPSC descriptor ring |
| `small` slab class | `1,024 × 4 KB` | Default slot pool for single-row and small-batch requests |
| `batch` slab class | `64 × 128 KB` (opt-in) | Slot pool for multi-row batch requests |
| `pressure.shed` / `recover` / `hard` | `0.85` / `0.75` / `0.95` | Working-set memory utilization thresholds |
| `pressure.shed_max` | `2 s` | Maximum duration in `PSI_SHEDDING` before recycling the worker |
| `reload.rollback_window` | `60 s` | Serving time required before a newly reloaded model commits |
| `worker.load_timeout` | `300 s` | Maximum time allowed for a worker to reach `WARM` |
| `shutdown.session_grace` | `20 s` | Grace period to keep serving an attached host after `SIGTERM` |
