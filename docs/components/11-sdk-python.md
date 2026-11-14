# 11 — Python SDK (`sdks/python`)

[← High-level design](../high-level-design.md) · implements [06 — SDK contract](06-sdk-contract.md) · built on [10 — Rust SDK](10-sdk-rust.md)

The SidecarZero Python package (`sidecarzero`) serves two roles in a single PyO3 (`abi3-py311`) native extension (`sidecarzero._core`):
1. **Host Client SDK:** Allows Python web services, notebooks, and batch jobs to call the sidecar over shared memory with the GIL released.
2. **Worker Runner (`python -m sidecarzero.worker`):** Wraps `scz-worker-core` around a user-provided Python model module (NumPy, PyTorch, scikit-learn) so Python models can serve behind the lock-free shared-memory arena inside the sidecar sandbox.

---

## 1. Package layout and `abi3` wheel packaging

```
sdks/python/
├── Cargo.toml                   PyO3 0.30 cdylib (abi3-py311) wrapping sidecarzero and scz-worker-core
├── pyproject.toml               PEP 517 / wheel metadata (Python >= 3.11)
├── build-wheel.sh               builds portable cp311-abi3-{linux,macosx}_*.whl packages
├── src/
│   ├── lib.rs                   sidecarzero._core PyO3 module definition
│   ├── client.rs                PyClient wrapper, GIL management, ref-counted close(), metrics export
│   ├── input.rs                 Python buffer-protocol zero-copy export and dtype conversion
│   ├── codecs.rs                DenseF32ToScore and Bytes codecs
│   ├── fallbacks.rs             Rust-native fallbacks (constant, threshold, linear, lookup)
│   ├── outcome.rs               Outcome, Source, FallbackReason Python classes
│   └── worker.rs                scz-worker-core InferenceBackend calling Python model modules
├── python/sidecarzero/
│   ├── __init__.py              Client, Outcome, Source, FallbackReason, MetricsSink, PrometheusText
│   ├── codecs.py                codec constructors
│   ├── fallbacks.py             native fallback constructors
│   ├── testing.py               InProcessClient and Fault injection
│   ├── worker/                  __main__.py, _runner.py, Signature, tensor, model_path, model_bytes
│   ├── _core.pyi                full PEP 484 type stubs
│   └── py.typed                 PEP 561 marker
├── agent/scz_agent.py           scz-agent-python conformance agent
└── tests/                       unit tests, supervisor E2E tests, worker runner tests, allocation gate
```

`sdks/python/build-wheel.sh` packages `_core.abi3.so` and the pure-Python modules into a standard `cp311-abi3` wheel installable on any CPython 3.11+ interpreter without requiring a Rust toolchain at install time.

---

## 2. Client API

```python
from sidecarzero import Client, Outcome, Source, FallbackReason, fallbacks
from sidecarzero.codecs import DenseF32ToScore

with Client(
    "/run/sidecarzero/scz.sock",
    codec=DenseF32ToScore(256),
    fallback=fallbacks.threshold(feature=17, gt=0.8, then=0.9, otherwise=0.1),  # or any Python callable
    sample_every=64,
) as client:
    out: Outcome = client.infer(features, budget_ms=2.0)
    # out.value, out.source, out.reason, out.model_gen
    metrics_body: str = client.prometheus_text()
```

### Mandatory fallback and native Rust fallbacks

- `fallback` is a required keyword-only argument on `Client`. Omitting it raises `TypeError` at construction; passing `None` raises `ValueError`.
- Callers can supply either an arbitrary Python callable `fn(input, reason) -> out` or a **native Rust fallback** from `sidecarzero.fallbacks` (`constant`, `threshold`, `linear`, `lookup`). Native fallbacks execute entirely inside Rust without invoking the Python interpreter, completing a full fallback call in **~300 ns** (and a spinning model round-trip in **~700 ns**).

### Buffer-protocol inputs and GIL release

1. On `client.infer(features, budget_ms=...)`, the extension exports `features` via the CPython buffer protocol (`PyBuffer`) while holding the GIL (pinning the underlying NumPy `ndarray`, `array.array`, `bytes`, or `memoryview` buffer).
2. The extension then calls `py.allow_threads(...)`, **releasing the GIL across the entire Rust `infer` execution** (admission, slot claim, dtype conversion/copy into shared memory, ring enqueue, spin, `futex` wait, and response decode).
3. If the call succeeds on the model path or uses a native fallback, it re-acquires the GIL only to return the `Outcome` object to Python. If a Python `on_fallback` hook or Python fallback callable must run, the GIL is re-acquired after the slot and caller entry have already been released.

### Multi-process Python servers and fork safety

Because `scz-shm` registers a `pthread_atfork` handler ([sdk-rust §4](10-sdk-rust.md#4-caller-state-table-arena-retirement-and-fork-safety)), forking after constructing a `Client` safely resets the child process's client to `DISCONNECTED` and closes its inherited session socket descriptor. When running multi-worker Python servers (such as `gunicorn -w N` or `uvicorn --workers N`), configure the supervisor with `--sessions=N` so each worker process receives its own isolated arena session ([shm-abi §11.1](01-shm-abi.md#111-multi-session-layout)), or run one multi-threaded worker process per pod (`gunicorn -w 1 --threads T`).

---

## 3. Python worker runner (`python -m sidecarzero.worker`)

The Python worker runner lets a Python model run inside the sidecar's worker sandbox behind the exact same shared-memory rings, heartbeats, death locks, and watchdog rules as the native ONNX and GGUF workers.

### Defining a Python model module

```python
# my_model.py
import numpy as np
from sidecarzero.worker import Signature, tensor

def load(model_path: str):
    # model_path is /proc/self/fd/5 (the exact file descriptor hashed by the supervisor)
    return np.load(model_path)

def signature(weights) -> Signature:
    return Signature(
        inputs={"features": tensor("float32", [256])},
        outputs={"score": tensor("float32", [])},
    )

def predict(weights, batch: dict[str, np.ndarray]):
    # batch["features"] is a read-only (rows, 256) zero-copy NumPy view over slot memory
    return (batch["features"] @ weights).astype(np.float32)
```

### Launching via the supervisor

```bash
sidecarzero-supervisor \
  --socket=/run/sidecarzero/scz.sock \
  --model=/models/weights.npy \
  --worker=/opt/py/bin/python3 \
  --worker-arg=-B --worker-arg=-m --worker-arg=sidecarzero.worker --worker-arg=my_model \
  --worker-read=/opt/py \
  --worker-read=/app/models_code \
  --worker-read=/models \
  --worker-socketpair \
  --worker-rlimit-data-mb=2048
```

### Runner execution model and sandboxing

| Aspect | Implementation |
|---|---|
| Main thread & GIL | At startup, `sidecarzero.worker` imports the user module, invokes `load()` and `signature()`, warms up `predict()` at `1` row and `max_rows`, and then permanently releases the GIL on the main thread while entering `scz-worker-core`. Each consume thread attaches to the GIL only around `predict()` |
| Heartbeat independence | The 5 ms heartbeat thread runs in pure Rust without acquiring the GIL, so a multi-millisecond Python `predict()` or garbage-collection pause never delays worker heartbeats |
| Request views | Floating-point input columns are exposed as read-only zero-copy NumPy `ndarray` views directly over the slot's request region (`valid` until `predict()` returns). Integer columns (`int8`..`uint64`) are copied into private memory before `predict()` sees them |
| Response validation | The returned array (or `dict` of arrays) is checked for exact dtype, row count, and column dimensions; mismatches or raised Python exceptions complete the slot with `MODEL_ERROR`. Valid outputs are copied into the slot's response region |
| Memory & `TRIM` | On `TRIM`, the runner calls `gc.collect()` followed by `malloc_trim(0)` with the GIL released. A Python `MemoryError` under `--worker-rlimit-data-mb` completes the slot with `RESOURCE_EXHAUSTED`; 3 consecutive `MemoryError` exceptions exit the worker (code `3`) so the supervisor recycles it |
| Sandbox (`--worker-socketpair`) | Enables local `AF_UNIX` `socketpair()` and `sendto(..., NULL, 0)` so Python's `asyncio` self-pipe wakeup and `multiprocessing.Pipe()` work inside the worker, while seccomp-BPF still denies `socket()`, `connect()`, `bind()`, `listen()`, and addressed `sendto()` |

---

## 4. Verification

`sdks/python/test.sh` executes:
1. Unit tests covering `Client`, native and Python fallbacks, dtype conversions (`float64 → float32`), `CodecError` handling on malformed shapes, `MetricsSink` and `PrometheusText` parity, and `InProcessClient` fault injection.
2. Live supervisor integration tests and `python -m sidecarzero.worker` sandbox tests (verifying NumPy inference, warm standby promotion on `SIGKILL`, `MemoryError` recycling under `RLIMIT_DATA`, `asyncio` self-pipe operation under `--worker-socketpair`, and `PermissionError` when user code attempts `socket.socket(AF_INET, ...)`).
3. **No-growth allocation gate:** Verifies that `sys.getallocatedblocks()` remains flat across 1,000,000 `infer` calls on both the model and fallback paths.
4. Full 55/56 cross-language conformance suite (`scz-agent-python`, including `SCZ_CRASH_AT` crash-point injection).
