# 05 — Worker (`crates/scz-worker-core`, `crates/scz-worker-ort`, `crates/scz-worker-gguf`, Python runner)

[← High-level design](../high-level-design.md) · depends on [01 — SHM ABI](01-shm-abi.md), [02 — Signaling](02-signaling.md), [03 — Arrow data plane](03-arrow-data-plane.md)

The worker is the only process in SidecarZero that links or executes machine learning runtime code. It is a pure function over the shared-memory arena: Arrow IPC `RecordBatch` in, Arrow IPC `RecordBatch` out. It is isolated in a strict kernel sandbox and treated as disposable — if it crashes, leaks, or hangs, the supervisor replaces it without affecting host availability.

---

## 1. Contract

| Property | Meaning |
|---|---|
| **Stateless** | No state survives across requests except the loaded model. Killing the worker at any instruction loses at most the currently executing requests, which resolve to local fallbacks |
| **Socketless** | Cannot create network sockets, enforced by seccomp-BPF in the kernel |
| **Connectionless** | Makes no external calls to databases, caches, or feature stores; all features arrive in the request slot |
| **Disposable** | Process replacement is the universal remedy for memory growth, internal runtime corruption, or hangs |
| **Generation-bound** | Consumes requests only while `gate.worker_gen` equals its own generation |
| **Read-only view of supervisor state** | Maps `scz_ctl` read-only (`PROT_READ`), enforced by `F_SEAL_FUTURE_WRITE`; cannot alter the gate, directory geometry, or published Arrow templates |

---

## 2. Crate and binary structure

```
crates/scz-worker-core       shared worker protocol engine
├── runtime                  map segments, validate seals/CRC, populate page tables, spawn threads
├── consume                  per-ring loop: dequeue → validate → optional batch coalesce → infer → complete
├── control                  four-phase ctl_req / ctl_ack coordinator on ring 0
├── backend                  trait InferenceBackend
├── registry                 render Arrow capacity templates and stage candidate registry
├── heartbeat                dedicated 5 ms heartbeat thread + delayed-wake backstop
└── stats                    per-thread cache-line-striped latency histograms and counters

crates/scz-worker-ort        binary `sidecarzero-worker`: ONNX Runtime 1.20 (C API 1.31.1)
crates/scz-worker-gguf       binary `sidecarzero-worker-gguf`: GGUF embeddings via llama.cpp b11988
crates/scz-worker-echo       binary `scz-worker-echo`: zero-overhead transport loopback worker
crates/scz-worker-chaos      binary `scz-worker-chaos`: scriptable fault-injection worker for conformance
sdks/python/.../worker.py    `python -m sidecarzero.worker`: Python / NumPy / PyTorch worker runner
```

### `InferenceBackend` trait

```rust
pub trait InferenceBackend: Send + Sync {
    /// Input and output schemas, and whether the batch dimension is fixed.
    fn signature(&self) -> ModelSignature;

    /// Run inference. `input` and `output` are views over slot memory.
    /// Must not retain either beyond the call.
    fn infer(&self, input: &BatchView<'_>, output: &mut BatchSink<'_>, deadline: Deadline)
        -> Result<(), InferError>;

    /// Release whatever memory can be released cheaply.
    fn trim(&self);
}
```

`BatchView` exposes typed column slices located by verified template offsets, and `BatchSink` exposes writable response buffers inside the slot.

---

## 3. Startup sequence

The sandbox (`PR_SET_NO_NEW_PRIVS`, Landlock, seccomp-BPF, `RLIMIT_CORE = 0`, `oom_score_adj = 1000`) is already active before `execve`.

```
1. Parse args: --ctl-fd, --data-fd, --model-fd, --status-fd, --wrk-fd, --gen, --beat-slot
2. Map scz_ctl (PROT_READ), scz_data (PROT_READ | PROT_WRITE), and scz_wrk (PROT_READ | PROT_WRITE);
   verify file seals, sizes, magic, ABI version, layout CRC32C, and matching arena_id
3. Populate page tables (MADV_POPULATE_READ / MADV_POPULATE_WRITE); close the three memfd descriptors
   and mark --model-fd and --status-fd FD_CLOEXEC
4. Write pid and gen to beat[beat_slot]; set state = LOADING; start the 5 ms heartbeat thread
5. Map the model from --model-fd and initialize the backend runtime
6. Query backend signature; render and self-verify one capacity template per direction and slot class;
   compute schema_fingerprint; write candidate registry to staging[beat_slot] and set staged_len
7. Run warm-up inferences on every consume thread at 1 row and at max_rows for each slot class;
   then each consume thread acquires its robust death lock in scz_wrk and holds it until exit
8. Set state = WARM; write WARM byte to status pipe; park on standby_wait until gate.worker_gen == my_gen
9. Verify that the published registry in scz_ctl matches the staged bytes; read each ring's head;
   set state = ACTIVE; write ACTIVE byte to status pipe; begin consuming
```

Warm-up on every consume thread at `max_rows` serves two purposes: it pre-allocates thread-local runtime scratch buffers so the first live request pays no initialization penalty, and it establishes a realistic post-warmup private RSS baseline in `/proc/<pid>/statm` for the supervisor's leak detector ([supervisor §6](04-supervisor.md#6-memory-and-pressure-monitor)).

---

## 4. The consume loop

One thread per descriptor ring:

```
loop:
    if gate.worker_gen != my_gen: exit
    if ctl_req != my ctl_ack: handle control requests (below)

    d = ring.dequeue()  or  spin-then-park (02-signaling.md)
    now = monotonic_ns()

    // 0. Skip void cells left by unwound producers
    if d.flags & VOID: continue

    // 1. Bounds-check descriptor indices against control directory before touching slot memory
    if d.slab_class >= slab_class_count or d.slot_index >= slab_dir[d.slab_class].slot_count: continue
    slot = slot addresses computed from slab_dir[d.slab_class]

    // 2. Claim slot: CAS loop (d.slot_epoch, READY) → (d.slot_epoch, PROCESSING),
    //    preserving WAITER and setting OWNER = beat_slot + 1
    if !claim(slot, d.slot_epoch): continue

    // 3. Deadline feasibility check (using idle-decaying expected_run EWMA)
    if now + expected_run >= d.deadline_mono_ns:
        complete(slot, DEADLINE_EXCEEDED); continue

    // 4. Validate slot record fields from local copies
    stamp, req_len = slot.req_stamp, slot.req_len
    if epoch16(stamp) != my_registry_epoch: complete(slot, STALE_REGISTRY); continue
    if id(stamp) != req_template.id or req_len != req_template.frame_cap
       or rows not in 1..=req_template.max_rows: complete(slot, BAD_REQUEST); continue

    // 5. Execute inference over slot memory (or coalesce opportunistic batch if max_batch_rows > 1)
    status = backend.infer(view(slot.request), sink(slot.response), d.deadline_mono_ns)

    // 6. Stamp response region if needed, update length patches if rows changed, and complete
    slot.resp_len, slot.status, slot.model_gen, slot.t_done_ns = …
    complete(slot, status)   // CAS PROCESSING → DONE (or ABANDONED → FREE if caller timed out)
```

### Idle-decaying `expected_run` EWMA

Step 3 sheds requests that have less remaining budget than the thread's `expected_run` EWMA so a saturated worker does not waste capacity computing answers for callers that will time out before completion. Because a shed request is not timed, a naive EWMA could latch permanently if a single slow inference during startup exceeded callers' budgets. To prevent latching, `expected_run` decays by half for every **100 ms** of idle time since the last completed inference.

### Completing a slot: `DONE` vs `FAILED`

The worker always completes slots with state **`DONE`** and records the outcome in `slot.status`. State **`FAILED`** is written exclusively by the supervisor's sweep when a worker dies.

| `slot.status` | Meaning | SDK `FallbackReason` (when `status != OK`) |
|---|---|---|
| `OK` | Valid model output in response region | Returns `Outcome::Model` |
| `DEADLINE_EXCEEDED` | Shed at step 3 or cancelled mid-inference at deadline | `DEADLINE` |
| `BAD_REQUEST` | Invalid stamp, frame length, row count, or out-of-range token ID | `BAD_REQUEST` |
| `MODEL_ERROR` | Runtime execution error | `MODEL_ERROR` |
| `RESOURCE_EXHAUSTED` | Runtime hit memory allocation cap (`max_mem` or `RLIMIT_DATA`) | `RESOURCE_EXHAUSTED` |
| `STALE_REGISTRY` | Request encoded against a superseded `registry_epoch` | `REBINDING` (triggers client rebind) |
| `INTERNAL` / unknown | Internal worker fault | `WORKER_FAILED` |

### Control requests (`ctl_req` / `ctl_ack`)

Control requests are checked at the top of the consume loop where the thread holds no slot. Ring 0's consume thread coordinates multi-thread barriers:

| Bit | Action |
|---|---|
| `TRIM` | Ring 0 calls `backend.trim()` and `malloc_trim(0)`, then sets `ctl_ack |= TRIM` |
| `DRAIN` | Every consume thread stops at its loop boundary; once all have stopped, ring 0 reports `DRAINED` on the status pipe and all threads park on `standby_wait` until `DRAIN` clears, then re-read `ring.head` |
| `HANDOVER` | Every consume thread stops permanently at its loop boundary; once all have stopped, ring 0 sets `beat.state = RELEASED`, writes `RELEASED` to the status pipe, and the process exits cleanly |
| `DUMP_STATS` | Ring 0 flushes thread-local stats stripes to the status page and acknowledges |

### Opportunistic batching (`--max-batch-rows`)

Opportunistic batching is disabled by default (`--max-batch-rows=0`) so that each request is isolated and zero-copy. When `--max-batch-rows=M` (`M > 1`) is enabled on a model with a variable batch dimension and additional descriptors are already queued in the ring when the worker dequeues a single-row request, the consume thread coalesces up to `M` rows across ready slots into a contiguous scratch batch, executes a single `backend.infer` call, and scatters the outputs back into each participating slot before completing them. It never waits for a batch to fill, adding zero latency at low QPS while converting queue depth into batch throughput at high QPS.

---

## 5. Memory discipline

SidecarZero provides two memory profiles for ONNX Runtime (`--memory-profile=a|b`):

| Property | Profile A — Arena Off (default) | Profile B — Capped BFC Arena |
|---|---|---|
| Session options | `DisableCpuMemArena`, `DisableMemPattern` | Env allocator registered with `OrtArenaCfg` (`max_mem`, `arena_extend_strategy = kSameAsRequested`) + `session.use_env_allocators = "1"` |
| Where activations live | System allocator (`glibc` with `mallopt(M_MMAP_THRESHOLD, 256 KB)`) | ORT BFC arena hard-capped at `max_mem` |
| Steady-state RSS | Lowest; buffers ≥ 256 KB return immediately to the kernel on free, and smaller buffers return on `malloc_trim(0)` | Retains arena chunks up to `max_mem` |
| `TRIM` behavior | `malloc_trim(0)` returns RSS within 1 MB of post-load baseline | Sets `memory.enable_memory_arena_shrinkage = "cpu:0"` on the next `Run`, then calls `malloc_trim(0)` |
| Behavior on oversize allocation | Capped by `RLIMIT_DATA`; returns `RESOURCE_EXHAUSTED` | Refused by ORT's BFC arena cap; returns `RESOURCE_EXHAUSTED` without crashing |

Setting `mallopt(M_MMAP_THRESHOLD, 262144)` before creating the ORT session prevents `glibc` from dynamically raising its mmap threshold when large batch activations are freed; without it, freed activations remain trapped in per-thread heaps that `malloc_trim(0)` cannot release.

---

## 6. Sandboxing

Installed by the supervisor's pre-`exec` stub before any worker code runs:

| Layer | Mechanism | Details |
|---|---|---|
| No new privileges | `PR_SET_NO_NEW_PRIVS = 1` | Prevents privilege escalation on `execve` |
| Architecture verification | seccomp-BPF checks `seccomp_data.arch` | Kills the process immediately on non-native syscall ABIs (blocking 32-bit `int 0x80` / `socketcall` or `x32` bypasses on `x86_64`) |
| Network socket ban | seccomp-BPF denies `socket`, `socketpair`, `connect`, `bind`, `listen`, `accept`, `accept4`, `sendto`, `sendmsg`, and `io_uring_setup` | Enforces the socketless worker invariant in the kernel (with `--worker-socketpair` option for Python `asyncio` local self-pipes; see [§7.3](#73-python-worker-runner)) |
| Cross-process isolation | seccomp-BPF denies `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`, `mount`, `bpf`, `init_module`, `keyctl` + supervisor sets `PR_SET_DUMPABLE = 0` | Prevents a compromised worker from inspecting or writing supervisor memory even though both share a UID |
| File-descriptor scrubbing | `close_range(8, MAX, 0)` | Passes only fds `3..7` (`scz_ctl`, `scz_data`, `model`, `status_pipe`, `scz_wrk`) |
| Filesystem lockdown | Landlock (`ABI v1..v6`) | Read-only access to `/lib`, `/usr/lib`, `/proc/cpuinfo`, `/sys/devices/system/cpu`, and `--worker-read` paths; zero write access except optional scratch dir |
| Resource & core-dump limits | `RLIMIT_CORE = 0`, `RLIMIT_NOFILE = 64`, optional `RLIMIT_DATA` | Prevents core dumps from delaying death-lock wakeup or writing request payloads to disk |
| OOM & parent-death | `oom_score_adj = 1000`, `PR_SET_PDEATHSIG = SIGKILL` | Ensures the worker is always chosen before the supervisor on OOM and dies if the supervisor exits |

---

## 7. Backends

### 7.1 ONNX Runtime (`sidecarzero-worker`)

- **Zero-copy input and output:** Wraps `[base + col.values_off, ...)` directly as an `OrtValue` input tensor via `OrtCreateTensorWithDataAsOrtValue` and binds the output `OrtValue` directly to the slot's response region via `OrtBindOutput`. Fixed per-call binding overhead is **0.58 µs** at p50 ([`docs/benchmarks.md`](../benchmarks.md#41-onnx-runtime-120-c-api-1311--scz-worker-ort)).
- **Index-column copy protection:** Integer columns (`int32`, `int64`) used as embedding/vocabulary indices are copied into a worker-private buffer before inference so a concurrent host write to `scz_data` cannot trigger an out-of-bounds read inside `Gather` kernels.
- **Weight prepacking (`--prepack=on|off`):** By default (`on`), ORT prepacks matrix weights into private pages for maximum SIMD throughput. Setting `--prepack=off` sets `session.disable_prepacking = "1"` along with `session.use_ort_model_bytes_directly = "1"` and `session.use_ort_model_bytes_for_initializers = "1"`, keeping 100% of matrix weights in shared file-backed pages across active and standby workers when memory is tight.

### 7.2 GGUF via `llama.cpp` (`sidecarzero-worker-gguf`)

`sidecarzero-worker-gguf` (`crates/scz-worker-gguf`) statically links `llama.cpp` release `b11988` behind `scz-worker-core` to serve GGUF embedding models (such as `bge-small-en-v1.5` in `f16` or `Q4_0`):

- **Schema:** Request column `tokens: FixedSizeList<int32>[L]` (where `L` is `--max-tokens`, default `128`, padded with `-1`); response column `embedding: FixedSizeList<float32>[D]` (`D = 384` for `bge-small`).
- **Input validation & safety:** Token IDs are copied into private memory and range-checked (`0 ≤ id < vocab_size`, non-empty sequence) before building the `llama_batch`, returning `BAD_REQUEST` on invalid tokens so untrusted inputs can never trigger `GGML_ASSERT`.
- **Output normalization:** Pools embeddings using the model's native pooling strategy (e.g. `CLS`) and L2-normalizes the output vector by default (`--normalize=l2|none`), matching stock `llama.cpp` with cosine similarity **≥ 0.9999968**.
- **Shared weights & `--repack`:** With `f16` models (64.2 MiB), 100% of model weights remain in shared `mmap` pages (`Pss = 0.5 × Rss` across active and standby workers) and each worker uses only **12.2 MiB** of private RSS. For quantized models (`Q4_0`), `--repack=on` (default) uses repacked CPU buffers for **1.8× faster** inference (16.6 ms vs 29.8 ms at 128 tokens), while `--repack=off` shares 100% of weights across workers (`12.0 MiB` private RSS).
- **Cooperative deadline cancellation:** Wires `llama.cpp`'s `abort_callback` to `min(request_deadline, start + max_infer_ms)`. An inference that hits its deadline aborts at the next graph node boundary (within 0.3–1.5 ms), completes the slot with `DEADLINE_EXCEEDED`, and leaves the `llama_context` ready for the next call.
- **Landlock configuration:** Because `llama.cpp` opens `/proc/self/fd/5` by path to `mmap` the GGUF file, pass `--worker-read=<model_directory>` to the supervisor so Landlock permits opening newly swapped files in that directory across model reloads.

### 7.3 Python worker runner (`python -m sidecarzero.worker`)

The Python worker runner executes `scz-worker-core` inside `sidecarzero._core` and delegates inference to a user-provided Python module implementing `load(model_path)`, `signature(model)`, and `predict(model, batch)`:

- **NumPy zero-copy views:** Floating-point request columns are exposed to `predict` as zero-copy 2D NumPy `ndarray` views over slot memory (`int` columns are passed as private copies). Returned arrays or dicts of arrays are validated against the response template and copied into the slot's response region.
- **Sandbox flags:** Started by the supervisor via `--worker=/path/to/python3 --worker-arg=-B --worker-arg=-m --worker-arg=sidecarzero.worker --worker-arg=<MODULE> --worker-socketpair`. The `--worker-socketpair` flag allows local `AF_UNIX` `socketpair()` and `sendto(..., NULL, 0)` so Python's `asyncio` event loop can use its internal self-pipe wakeup while still blocking `socket()`, `connect()`, `bind()`, `listen()`, and `sendto()` with non-null addresses.
- **OOM resilience:** Any `MemoryError` raised inside Python under `RLIMIT_DATA` completes the request with `RESOURCE_EXHAUSTED`; three consecutive `MemoryError` exceptions cause the worker to exit so the supervisor recycles it cleanly.

### 7.4 Chaos worker (`scz-worker-chaos`)

`scz-worker-chaos` implements the worker protocol with deterministic fault injection driven either per-request (via opcodes in the first two elements of the input vector) or per-process (via its model script file), supporting crashes at every protocol step, hangs with or without heartbeats, corrupt response stamps/lengths, slow loads, memory leaks, and full data-segment scribbling (including setting `cell.seq` ahead of `tail` and continuously flipping `slot.word`'s `WAITER` bit).
