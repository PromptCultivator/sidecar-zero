# SidecarZero — Benchmarks & Empirical Results

[← High-level design](high-level-design.md) · [Chaos benchmark specification](components/13-chaos-benchmark.md)

This document consolidates the benchmark measurements, soak runs, backend evaluations, Kubernetes compatibility matrices, and kernel/runtime validation experiments across the repository.

---

## 1. Chaos Benchmark Suite (Systems A–E, 70 Runs)

Raw data and per-run artifacts live in [`bench/results/20261113-204426/report.md`](../bench/results/20261113-204426/report.md), produced by `make -C bench chaos-bench`.

### 1.1 Systems Compared

| System | Architecture | Transport | Failure Handling |
| :--- | :--- | :--- | :--- |
| **A — `sidecarzero`** | `sidecarzero-supervisor` + `sidecarzero-worker` (ONNX Runtime 1.31.1), warm standby | Sealed `memfd` arena, pre-stamped Arrow IPC frames, adaptive spin + shared `futex` | Per-thread robust death lock (`~50 µs`), gate flip, warm-standby promotion, SDK fallback |
| **A — `sidecarzero-nospin`** | System A with worker and client spin budgets set to `0` | Same shared memory, always waking via `futex` | Same as System A |
| **A — `sidecarzero-nostandby`** | System A with `--standby=none` (`1×` worker memory) | Same shared memory | Cold worker spawn + model load on crash |
| **B — `triton`** | NVIDIA Triton Inference Server 26.04 (`onnxruntime_onnx` backend, ORT 1.24.4), supervised restart loop | Tensors in registered POSIX `/dev/shm`, control over HTTP/2 gRPC (`127.0.0.1:8001`) | `timerfd` deadline + client fallback wrapper; auto re-registration of `/dev/shm` on restart |
| **C — `http-json`** | Native Rust HTTP/1.1 keep-alive server wrapping ONNX Runtime 1.31.1 | TCP loopback + JSON encode/decode | Client timeout + fallback wrapper |
| **D — `uds-binary`** | Native Rust server wrapping ONNX Runtime 1.31.1 | Unix domain stream socket + length-prefixed raw `float32` binary frames | Client timeout + fallback wrapper |
| **E — `in-process`** | ONNX Runtime 1.31.1 linked directly into the host process | Direct C API function call | None (host process dies when the model crashes) |

**Workload:** `fraud.onnx` / `fraud.ort` (gradient-boosted tree ensemble, 300 trees of depth 6, 256 `float32` features → 1 `float32` score), 2 ms per-call budget, open-loop scheduling (recording latency from intended send time to capture coordinated omission), 5 repetitions per scenario.
**Environment:** Linux `6.12.76-linuxkit` (`aarch64`, Apple Silicon virtualized host, 10 vCPUs, 7.75 GiB RAM, cgroup v2, Docker `29.5.3`, Kind `v0.33.0` / Kubernetes `v1.37.0`).

---

### 1.2 Kill Loop Availability & Fallback Window (In-Process Mode, 5,000 RPS)

`SIGKILL` delivered to the model-serving process every 2 s over 120 s (59 kills per run, 600,000 calls per run, 5 repetitions; values are `median (min–max)` across runs):

| System | Host | Model Share (%) | Fallbacks / Kill (median) | Fallbacks / Kill (max) | Time in Fallback / Kill (median) | Time in Fallback / Kill (max) | Errors / Timeouts |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **A — `sidecarzero`** | Rust | **99.964%** (99.956–99.974) | **3.0** (3.0–3.0) | **6** (6–8) | **739.4 µs** (698.6–765.8 µs) | **1,222.5 µs** (1,126.2–1,488.5 µs) | **0 / 0** |
| **A — `sidecarzero`** | Java | **99.877%** (99.873–99.890) | **2.0** (2.0–2.0) | **4** (4–260) | **714.4 µs** (702.3–741.8 µs) | **1,093.8 µs** (1,073.5–52,188.6 µs) | **0 / 0** |
| **A — `sidecarzero-nospin`** | Rust | **99.955%** (99.953–99.965) | **4.0** (3.0–4.0) | **6** (6–7) | **870.2 µs** (851.5–926.2 µs) | **1,264.6 µs** (1,210.8–1,354.5 µs) | **0 / 0** |
| **A — `sidecarzero-nostandby`** | Rust | 68.423% (68.381–68.423) | 3,147.0 (3,146.0–3,147.0) | 3,167 (3,163–3,190) | 630.4 ms (630.4–630.4 ms) | 634.4 ms (633.7–639.1 ms) | **0 / 0** |
| **B — `triton` (supervised)** | Rust | 98.240% (98.234–98.240) | 199.5 (199.0–200.0) | 219 (212–233) | 38.0 ms (37.8–38.0 ms) | 41.8 ms (40.3–44.6 ms) | **0 / 0** |
| **C — `http-json` (supervised)** | Rust | 98.753% (98.749–98.765) | 135.0 (135.0–137.0) | 159 (149–162) | 25.5 ms (25.4–25.7 ms) | 30.1 ms (28.1–30.7 ms) | **0 / 0** |
| **D — `uds-binary` (supervised)** | Rust | 98.590% (98.567–98.613) | 147.5 (145.0–150.5) | 267 (259–268) | 28.0 ms (27.4–28.6 ms) | 51.8 ms (50.2–51.8 ms) | **0 / 0** |

### 1.3 Recovery Sub-Intervals (`sidecarzero`, Warm Standby)

Measured on the host's `CLOCK_MONOTONIC` from the exact instant the kill agent issued `kill(pid, SIGKILL)`:

| Sub-Interval | Rust Host (median / max) | Java Host (median / max) |
| :--- | :---: | :---: |
| **Kill → first consume-thread death lock** | `122.8 µs` / `1,032.2 µs` | `122.5 µs` / `964.6 µs` |
| **First death lock → gate closed & queue swept** | `43.5 µs` / `456.8 µs` | `46.5 µs` / `284.4 µs` |
| **Gate closed → standby `ACTIVE` & gate `HEALTHY`** | `95.8 µs` / `542.6 µs` | `88.2 µs` / `326.8 µs` |
| **Total: Kill → gate `HEALTHY`** | **`232.4 µs`** / **`1,136.8 µs`** | **`226.8 µs`** / **`1,089.5 µs`** |
| **Kill → kernel `pidfd` exit report (background)** | `2,005.5 µs` / `3,718.6 µs` | `1,962.4 µs` / `3,512.0 µs` |

### 1.4 Caller Experience Around a Worker Kill

- **Calls arriving after the gate has closed (`gate_closed_entry_to_fallback`):**
  - Rust host: `p50 0.04 µs` (`40 ns`), `p99 0.04 µs`, `p99.9 0.08 µs`, `max 21.0 µs`.
  - Java host: `p50 0.08 µs` (`80 ns`), `p99 0.25 µs`, `p99.9 0.62 µs`, `max 41.2 µs`.
- **Calls in flight at the instant of `SIGKILL` (`inflight_at_kill`):**
  - 100% of in-flight calls resolved within their 2 ms deadline (`0` deadline overruns across all 295 kills in Rust and 295 kills in Java).
  - Queued (`READY`) calls resolved at the death-lock sweep (`median 165.4 µs`, `max 1,088.2 µs`).
  - In-inference (`PROCESSING`) calls resolved either at the `pidfd` sweep or at their 2 ms deadline (`max 2,041.0 µs`, within the 1 ms shared-runner timer-slack tolerance).

---

### 1.5 Kubernetes Service Mode (Kind Cluster, 1,000 RPS, Kill Loop)

4 kills over 60 s (`60,000` requests through a Kubernetes `Service`, watched via the Kubernetes `EndpointSlice` watch API):

| System | Supervision | Model Share (%) | Errors / Timeouts | `EndpointSlice` Changes | Container Restarts | Latency p50 / p99 / max |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **A — `sidecarzero`** | Native sidecar (`sidecarzero-supervisor`) | **100.000%** | **0 / 0** | **0** | **0** | `78.8 µs` / `446.5 µs` / `1,236.2 µs` |
| C — `http-json` | Unsupervised (container PID 1) | 91.500% | 0 / 0 | **10** | **4** | `118.4 µs` / `1,842.0 µs` / `4,120.5 µs` |
| D — `uds-binary` | Unsupervised (container PID 1) | 91.855% | 0 / 0 | **9** | **4** | `102.1 µs` / `1,790.4 µs` / `3,980.0 µs` |
| E — `in-process` | None (model in app container) | 94.698% | **3,181 / 0** | **9** | **4** | `72.4 µs` / `1,650.2 µs` / `12,450.0 µs` |

---

### 1.6 Steady-State Latency & CPU Efficiency (In-Process Mode)

Medians across 5 repetitions (`fraud.ort`, single-row inference time `~9.5 µs` in ORT):

| System | Host | Rate & Pattern | p50 (µs) | p90 (µs) | p99 (µs) | p99.9 (µs) | max (µs) | Host + Sidecar CPU (s / 1M calls) |
| :--- | :---: | :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **A — `sidecarzero`** | Rust | 30,000 RPS, even | `11.2` | `15.4` | `26.8` | `68.4` | `412.5` | `14.8 + 18.2` |
| **A — `sidecarzero`** | Java | 30,000 RPS, even | `12.1` | `16.8` | `31.2` | `84.6` | `520.4` | `16.4 + 18.5` |
| **A — `sidecarzero`** | Rust | 30,000 RPS, Poisson | `14.6` | `28.4` | `64.2` | `162.0` | `690.2` | `15.1 + 18.4` |
| **A — `sidecarzero-nospin`** | Rust | 30,000 RPS, even | `24.8` | `36.2` | `62.4` | `148.5` | `580.0` | `8.2 + 11.4` |
| **A — `sidecarzero`** | Rust | 2,000 RPS, even | `22.4` | `31.6` | `54.8` | `124.2` | `460.8` | `9.4 + 12.6` |
| **A — `sidecarzero`** | Java | 2,000 RPS, even | `24.1` | `34.2` | `61.0` | `142.8` | `510.2` | `11.2 + 12.8` |
| **A — `sidecarzero-nospin`** | Rust | 2,000 RPS, even | `25.2` | `35.8` | `60.4` | `138.0` | `495.4` | `7.8 + 10.2` |
| **A — `sidecarzero`** | Rust | 10 RPS, even | `31.4` | `44.2` | `78.6` | `112.0` | `148.0` | — |
| **B — `triton` (gRPC + SHM)** | Rust | 2,000 RPS, even | `88.4` | `114.2` | `178.6` | `342.0` | `890.4` | `24.6 + 38.4` |
| **C — `http-json`** | Rust | 30,000 RPS, even | `38.6` | `56.4` | `112.8` | `284.0` | `1,120.0` | `28.4 + 31.2` |
| **D — `uds-binary`** | Rust | 30,000 RPS, even | `26.4` | `38.8` | `74.2` | `186.4` | `840.2` | `18.6 + 21.4` |
| **E — `in-process` ORT** | Rust | 30,000 RPS, even | `9.8` | `12.6` | `19.4` | `46.2` | `280.4` | `11.2 + 0.0` |

### 1.7 Overload, Live Reload, Idle CPU, & Footprint Gates

| Benchmark Scenario / Gate | Target | Measured Result |
| :--- | :---: | :--- |
| **Overload shedding (`OVERLOADED` call latency)** | `p99 < 1 µs` | `p50 0.04 µs`, `p99 0.08 µs`, `max 12.4 µs` (claims no slot, publishes no ring cell) |
| **Same-schema live model reload (5,000 RPS)** | `0` fallbacks; gap `< 1 ms` | **`0` fallbacks** across all 5 runs (`314,191` calls); handover gap `median 142.0 µs` (`max 486.2 µs`) |
| **Idle sidecar CPU (60 s, 0 RPS)** | `< 2.00%` of one core | **`0.38%–2.00%`** of one core (`0.00%` worker consume threads; supervisor tick + 5 ms heartbeat only) |
| **Hot-path heap allocation (Rust, Java, C++, Go)** | `0.0 B/call` | **`0.0 B/call`** (`0` GC collections in Java JMH/ThreadMXBean; `0` mallocs in Rust/C++/Go) |
| **Supervisor binary size (`aarch64` static musl)** | `< 5.0 MB` | **`0.86 MB`** stripped |
| **Container image sizes (`linux/arm64`)** | Minimal distroless | `sidecarzero-ort`: **`22.8 MB`** · `sidecarzero-gguf`: **`14.5 MB`** (`75.6 MB` with `bge-small-f16`) · `sidecarzero-python`: **`35.5 MB`** · Python `abi3` wheel: **`~850 KB`** |

---

## 2. Transport Loopback Microbenchmarks (`scz-worker-echo`)

Measured by `./scripts/phase1-latency` (300,000 synchronous calls per payload size after warmup, Rust SDK → `scz-worker-echo`, spinning regime, CPU-pinned cores):

| Payload Size (Request + Response) | p50 (µs) | p90 (µs) | p99 (µs) | p99.9 (µs) | Allocations / Call |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **16 B each way** | `0.46 µs` | `0.54 µs` | `0.58–0.62 µs` | `0.75–0.83 µs` | `0 B` |
| **256 B each way** | `0.62 µs` | `0.71 µs` | `0.83 µs` | `1.12 µs` | `0 B` |
| **1,024 B (1 KB) each way** | `0.88–0.92 µs` | `1.00 µs` | `1.12–1.21 µs` | `1.50–1.54 µs` | `0 B` |
| **Disconnected fallback (`NOT_CONNECTED`)** | `0.04 µs` | `0.04 µs` | `0.04 µs` | `0.04 µs` | `0 B` |

---

## 3. High-Frequency Kill Soak & Zero-Loss Reload

- **1-Hour Continuous `SIGKILL` Soak (`SCZ_SOAK_SECS=3600`, 4 caller threads):**
  - **Total worker kills:** `36,835` `kill -9` signals (one kill every `~98 ms` for 60 minutes).
  - **Total client calls:** `29,912,408,116` (`29.9 billion` calls: `9.49 billion` model responses + `20.42 billion` deterministic fallbacks during the sub-100 ms kill intervals).
  - **Invariants:** `0` hung calls, `0` deadline overruns, `0` leaked or stuck slots (`1,024 / 1,024` slots `FREE` at completion), flat supervisor and client RSS and file-descriptor counts.
  - **Standby promotion time across soak:** `0.03–0.23 ms` with echo worker; `0.07–1.60 ms` (`median 0.15 ms`) with `fraud.ort`.
- **Consecutive Model Handovers Under Load:**
  - `261,139` back-to-back calls across 3 consecutive same-schema model reloads completed with **`0` fallbacks**.

---

## 4. Backend Benchmarks (ONNX Runtime, GGUF / `llama.cpp`, Python Runner)

### 4.1 ONNX Runtime 1.31.1 (`sidecarzero-worker`)

| Benchmark / Property | Measured Result |
| :--- | :--- |
| **C API per-call binding overhead (`tree1`, 1 node)** | `0.63 µs` median (`0.83 µs` p99) with per-call IO binding vs `0.42 µs` static binding |
| **`fraud.ort` (300 trees, depth 6, 256 features) — 1 row** | `8.6–9.5 µs` median inference (`15.0 µs` p99); `15–17 µs` end-to-end via SDK |
| **`fraud.ort` — 32-row batch** | `227 µs` median (`7.1 µs` per row, `269 µs` p99) |
| **Zero-copy verification** | `0` copies across 5,000 1-row calls and 32-row batches; `libonnxruntime` reads directly from `scz_data` request addresses and writes directly into `scz_data` response addresses; scores match Python `onnxruntime` within `1.2 × 10⁻⁶` |
| **Memory Profile A (`DisableCpuMemArena` + `M_MMAP_THRESHOLD=256 KB`)** | MLP (`36.9 MB` weights) under mixed 1–2,048 row batches peaks at `78 MB` private RSS and returns to **`46 MB` (within `1.1 MB` of post-load baseline)** on every `TRIM` (`malloc_trim(0)`). Without `M_MMAP_THRESHOLD=256 KB`, thread arenas retained `+25 MB` |
| **Memory Profile B (Capped environment arena + shrinkage)** | Hard-capped at `64 MB` / `128 MB`; oversize 4,096-row batch is cleanly rejected with `RESOURCE_EXHAUSTED` while subsequent 1-row calls succeed; `TRIM` shrinks arena + purges glibc back to `46 MB` |
| **Weight prepacking (`--prepack`)** | Prepacking on (default): `0.52 ms` median 1-row MLP latency, `+38 MB` private RSS per worker (`148 MB` PSS for active + standby). Prepacking off (`--prepack=false`): weights shared via page cache (`+1.4 MB` private RSS, `74 MB` PSS for 2 workers), `14.0 ms` median latency (`27×` slower) |

### 4.2 GGUF / `llama.cpp` `b11988` (`sidecarzero-worker-gguf`)

Evaluated with `BAAI/bge-small-en-v1.5` (`f16`, `64.2 MiB`, 384-dimensional embeddings, CLS pooling + L2 normalization) and `Q4_0` (`24.6 MiB`):

| Metric | Measured Result |
| :--- | :--- |
| **Numerical parity vs stock `llama.cpp`** | Minimum cosine similarity **`0.9999968`** across 3–128 token inputs (single-row and packed multi-row) |
| **Weight sharing between active and warm standby (`f16`)** | **`63.5 MiB` shared (`0 B` private weight pages)**; each worker's total private RSS is **`12.2 MiB`** (compute buffer, context, thread pool) |
| **Quantized weight repacking (`Q4_0`, 128 tokens, 2 threads)** | `--repack=on` (default): `16.6 ms` p50, `23.4 MiB` private RSS/worker (`11.4 MiB` repacked kernels). `--repack=off`: `29.8 ms` p50, `12.0 MiB` private RSS/worker (`100%` of weights shared) |
| **Cooperative deadline cancellation (`abort_callback`)** | Stops at next `ggml` graph node within **`0.05–5.6 ms`** (`0.3–1.5 ms` typical on a 20 ms budget against a 200 ms 512-token input); context serves the next request normally in `~5 ms` |
| **Kill recovery under load (`kill -9` with warm standby)** | `1` fallback out of `58` calls (`0.16 ms` standby promotion); standby embeddings match within cosine `> 0.99999` |

**End-to-End p50 Latency (`bge-small-f16`, 1 row, Rust SDK):**

| Thread Count | 8 Tokens | 32 Tokens | 128 Tokens | 512 Tokens |
| :--- | :---: | :---: | :---: | :---: |
| **1 thread** | `3.1 ms` | `15.8 ms` | `65.6 ms` | `370 ms` |
| **2 threads (default)** | `2.2 ms` | `9.1 ms` | `35.2 ms` | `196 ms` |
| **4 threads** | `1.7 ms` | `5.7 ms` | `22.1 ms` | `114 ms` |

### 4.3 Python Client SDK & Worker Runner (`sdks/python`)

| Path | Measured Performance |
| :--- | :--- |
| **Python SDK → native worker (spinning model answer)** | `0.7 µs` median in container |
| **Python SDK → `python -m sidecarzero.worker` (NumPy 256-feature dot-product model, 1 row)** | **`6.0 µs` median (`14.0 µs` p99)** end-to-end round trip |
| **Python SDK native fallback (`fallbacks.constant` / `threshold`)** | **`0.3 µs` (`300 ns`)** median |
| **Python allocation stability** | `sys.getallocatedblocks()` delta = `0` across `1,000,000` model and fallback calls |

---

## 5. Kubernetes Kind Matrix & OOM Shield Evaluation

### 5.1 Multi-Version Kubernetes Matrix

20 worker `SIGKILL`s under continuous load per cluster version (`./scripts/kind-e2e`):

| Kubernetes Version | Node Image (`containerd 2.3.x`, cgroup v2) | `EndpointSlice` Flaps | Container Restarts | Standby Promotion Range | All 16 E2E Scenarios |
| :--- | :--- | :---: | :---: | :---: | :---: |
| **v1.34.11** | `kindest/node:v1.34.11` | `0` | `0` | `0.24–0.88 ms` | **16 / 16 Pass** |
| **v1.35.8** | `kindest/node:v1.35.8` | `0` | `0` | `0.22–0.91 ms` | **16 / 16 Pass** |
| **v1.36.4** | `kindest/node:v1.36.4` | `0` | `0` | `0.25–0.95 ms` | **16 / 16 Pass** |
| **v1.37.0** | `kindest/node:v1.37.0` | `0` | `0` | `0.22–0.84 ms` | **16 / 16 Pass** |

### 5.2 Kernel OOM Shield Matrix (Layer 1, with Layer 0 Monitor Disabled)

Measured by running a worker that allocates abruptly past a 96 MiB container limit across 3 consecutive OOM spikes ([`spikes/s2-oom-matrix`](../spikes/s2-oom-matrix/FINDING.md) and Kind test 4):

| Shield Mode | Pod Security | `memory.oom.group` | PID 1 Supervisor Survives | Warm Standby Survives | Recovery Path |
| :--- | :--- | :---: | :---: | :---: | :--- |
| **`singleProcess`** (kubelet `singleProcessOOMKill: true`) | `restricted` | `0` | **Yes** | **Yes** | Sub-millisecond standby promotion; `0` flaps |
| **`nestedCgroup`** (per-worker child cgroups `scz-worker-0/1`) | Writable cgroup / `privileged` | `1` (parent), per-worker `memory.max` | **Yes** | **Yes** | Worker hits child `memory.max`; standby promoted; `0` flaps |
| **`oomScoreAdj`** (supervisor `oom_score_adj = -1000` as root + `CAP_SYS_RESOURCE`) | `privileged` | `1` | **Yes** | **No** | Group kill spares PID 1 and kills both workers; cold restart; `0` flaps |
| **`none`** (stock cluster, Layer 0 disabled) | `restricted` | `1` | **No** | **No** | Kernel kills container (`OOMKilled`); kubelet restarts sidecar; host serves fallback throughout |

---

## 6. Kernel & Runtime Empirical Validation (`spikes/s1`–`s7`)

### 6.1 Death Detection & Promotion Scaling with Worker Private Memory (`s6`, `s6b`)

20 `SIGKILL` runs per row (`median / p90 / max` in milliseconds after `kill -9`):

| Worker Private Memory | First Robust Death Lock | Last of 4 Consume-Thread Locks | Kernel `pidfd` Readable | Total Time to Standby `HEALTHY` (via Death Locks) |
| :--- | :---: | :---: | :---: | :---: |
| **0 MiB (minimal)** | `0.056 / 0.092 / 0.410 ms` | `0.099 / 0.240 / 0.450 ms` | `0.164 / 0.257 / 0.453 ms` | `0.22 / 0.51 / 0.52 ms` |
| **128 MiB private** | `0.050 / 0.090 / 0.160 ms` | — | `3.88 / 4.77 / 5.80 ms` | `< 0.30 ms` (`3.92 ms` on `pidfd` alone) |
| **512 MiB private (`RLIMIT_CORE=0`)** | `0.053 / 0.090 / 0.166 ms` | `0.093 / 0.119 / 0.186 ms` | `0.58–9.44 ms` | `< 0.30 ms` (`9.48 ms` on `pidfd` alone) |
| **512 MiB private (core dumps enabled)** | **`289.0 ms`** | — | `322.0 ms` | Delayed by kernel core dump write |
| **1,024 MiB private** | `0.050 / 0.060 / 0.060 ms` | — | `15.68 / 16.92 / 17.07 ms` | `< 0.30 ms` (`15.71 ms` on `pidfd` alone) |
| **2,048 MiB private** | `0.064 / 0.115 / 0.233 ms` | `0.132 / 0.342 / 0.367 ms` | `25.88–29.89 ms` (`60.1 ms` max) | `< 0.50 ms` (`29.93 ms` on `pidfd` alone) |
| **1,024 MiB shared file mapping** | `0.050 / 0.080 / 0.110 ms` | — | `8.03 / 8.65 / 8.74 ms` | `< 0.30 ms` |

### 6.2 Formal Verification Coverage (`spec/`, `crates/scz-shm`)

- **PlusCal / TLA+ (`SlotsRing.tla` & `Consumers.tla`):** Verified across **14 TLC configurations** (`3.8M` to `42.5M` distinct states per full model) and **13 seeded bug mutants** (100% caught by TLC with minimal counterexample traces).
- **Loom Concurrency Models (`crates/scz-shm` & `sdks/rust`):** **10 exhaustive thread-interleaving models** covering MPSC ring enqueue/dequeue/void/repair, slot claim/abandon/complete/sweep, worker park (`WAKING` + heartbeat backstop), client park, `standby_wait`, gate re-check, handoff, control wake, caller entry claim, and arena retirement.
- **Cross-Language Conformance Suite (`conformance/`):** **56 scenarios** (46 functional + 10 `SCZ_CRASH_AT` crash-point scenarios) verified across Rust, Java FFM, C++20, Go, and Python agents.
