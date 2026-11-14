# S5 — ONNX Runtime Zero-Copy Foreign Memory & Memory Profiles

[← Findings Summary](../FINDINGS.md)

## Objective

Verify with ONNX Runtime (`1.31.1`) over shared-memory `memfd` slots:

1. Whether `CreateTensorWithDataAsOrtValue` binds input and output tensors directly to 64-byte-aligned slot addresses with zero intermediate copies.
2. What fixed per-call dispatch overhead ONNX Runtime adds relative to model inference time.
3. Whether Memory Profile A (`DisableCpuMemArena` + `malloc_trim(0)`) and Memory Profile B (capped environment arena + `shrink_memory_arena`) strictly bound worker RSS and survive oversize requests.
4. How ONNX Runtime handles weight sharing between active and warm-standby workers with weight prepacking enabled vs disabled.

---

## Experimental Setup

Executed via `./run.sh` using ONNX Runtime `1.31.1` (`linux/arm64`, Linux `6.12`, `glibc 2.36`) across three models:

| Model | Architecture | Size |
|---|---|---|
| `tree` | 256-feature GBDT (`TreeEnsembleRegressor`, 300 trees, depth 6) | `3.1 MB` |
| `tree1` | Single depth-1 tree (isolates ORT dispatch overhead) | `4 KB` |
| `mlp` | `256 → 4096 → 2048 → 1` dense matrix network | `36.9 MB` |

---

## Measured Results

### 1. Zero-Copy Request and Response Verification (`results/02-zero-copy.txt`)

Using `mprotect` page-access traps and hardware instruction disassembly (`results/02-zero-copy-disasm.txt`) across batch sizes `1`, `2`, `32`, and `64`:

- The input tensor's data pointer remained the exact slot request address, first read directly by the tree/GEMM kernel inside `libonnxruntime` and never copied or modified.
- The output tensor's data pointer remained the exact slot response address, written in place by the inference kernel with bit-for-bit identical scores.

### 2. Per-Call Runtime Dispatch Cost (`results/03-fixed-cost.txt`)

Measured over 100,000 single-threaded calls after 10,000 warm-up iterations:

| Model & Binding Mode | Median (`p50`) | `p99` | `p99.9` |
|---|---|---|---|
| `tree1` (reused `IoBinding`) | `0.42 µs` | `0.54 µs` | `5.0 µs` |
| `tree1` (per-call binding, SidecarZero path) | **`0.63 µs`** | **`0.83 µs`** | `6.4 µs` |
| `tree` (300 trees, 1 row, per-call binding) | **`9.5 µs`** | `15.0 µs` | `47.8 µs` |
| `tree` (300 trees, 32 rows, per-call binding) | `227 µs` (`7.1 µs/row`) | `269 µs` | `388 µs` |
| `mlp` (1 row, per-call binding) | `518 µs` | `1,329 µs` | `3,216 µs` |

ONNX Runtime's per-call tensor-wrapping and `IoBinding` overhead is `0.63 µs`—roughly 6% of the 300-tree fraud model's `9.5 µs` execution time.

### 3. Memory Profiles and `TRIM` Behavior (`results/04-memory-profiles.txt`)

Measured on the `36.9 MB` `mlp` model under mixed batch load (`1–32` rows with periodic `256–2,048`-row bursts) followed by a `4,096`-row oversize request:

| Profile | Peak Private RSS Before `TRIM` | Private RSS After `TRIM` | `4,096`-Row Oversize Request Behavior |
|---|---|---|---|
| **Profile A** (arena off, `glibc` + `malloc_trim(0)`) | `78 MB` | **`46 MB` (every cycle)** | Executed (`207 MB` peak), then returned to `46 MB` on `TRIM`; capped when paired with `RLIMIT_DATA` |
| **Profile B** (env arena capped at `128 MB`) | `138 MB` | **`46 MB`** (after `enable_memory_arena_shrinkage` + `malloc_trim(0)`) | **Rejected cleanly (`RESOURCE_EXHAUSTED`)**; worker survived and served subsequent requests |

Setting `RLIMIT_DATA` (`--worker-rlimit-data-mb`) similarly caused the oversize `4,096`-row allocation to fail cleanly inside `Run` (`std::bad_alloc` caught by ORT and returned as `OrtErrorCode 6`), leaving the worker healthy for subsequent requests.

### 4. Weight Prepacking vs Shared Page Cache (`results/05-model-load.txt`)

| Metric | `mlp` (Default Prepacking On) | `mlp` (`session.disable_prepacking = "1"`) | `tree` (Either Setting) |
|---|---|---|---|
| Private RSS added per worker on load | `+38.0 MB` | **`+1.4 MB`** | `+2.7 MB` |
| Combined PSS of active + warm standby workers | `148 MB` | **`74 MB`** | `43 MB` |
| Single-row inference latency (`p50`) | **`0.52 ms`** | `14.0 ms` | `9.4 µs` |

---

## Architectural Conclusions

1. **Zero-copy slot binding ([05 — Worker §7.1](../../docs/components/05-worker.md#71-onnx-runtime-backend-sidecarzero-worker)):** `sidecarzero-worker` wraps slot request and response pointers directly via `CreateTensorWithDataAsOrtValue` without copying tensor payloads.
2. **Configurable weight prepacking (`model.prepack` / `SCZ_ORT_PREPACK`):** By default, weight prepacking remains enabled for maximum GEMM performance (`0.52 ms` vs `14.0 ms`), and memory sizing budgets `2 × worker_private_rss` when `standby: warm` is active. Setting `model.prepack: false` (`SCZ_ORT_PREPACK=0`) disables prepacking so large matrix weights remain shared in page cache across active and standby workers.
3. **Dual memory discipline ([05 — Worker §5](../../docs/components/05-worker.md#5-memory-discipline)):** Profile A (`DisableCpuMemArena` + `malloc_trim(0)` + `RLIMIT_DATA`) is the default for fixed-shape models, returning RSS to `46 MB` baseline on every `TRIM` command.
