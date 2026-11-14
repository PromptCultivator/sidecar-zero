# SidecarZero Python SDK & Worker Runner (`sdks/python`)

[← 11 — Python SDK & Worker Runner Spec](../../docs/components/11-sdk-python.md) · [06 — Shared SDK Contract](../../docs/components/06-sdk-contract.md)

The `sidecarzero` Python package provides two components in a single `PyO3` `abi3` extension (`sidecarzero._core`, compatible with CPython 3.11+):

1. **Host Client SDK (`sidecarzero.Client`):** Wraps the reference Rust client (`sdks/rust`), exporting inputs via the CPython buffer protocol (`PyObject_GetBuffer`) and releasing the GIL (`py.allow_threads`) across slot claim, payload copy, ring enqueue, spin, and futex wait.
2. **Python Worker Runner (`python -m sidecarzero.worker`):** Embeds the Rust worker core (`crates/scz-worker-core`) around a Python model module (`load`, `signature`, `predict`), running under the same shared-memory arena, heartbeats, death locks, and seccomp/Landlock sandbox as native workers.

```python
from sidecarzero import Client, Source, FallbackReason, fallbacks
from sidecarzero.codecs import DenseF32ToScore

with Client(
    "/run/sidecarzero/scz.sock",
    codec=DenseF32ToScore(256),
    fallback=lambda x, reason: 0.1,   # mandatory keyword-only argument
) as client:
    out = client.infer(features, budget_ms=2.0)
    out.value, out.source, out.reason, out.model_gen
```

---

## Directory Layout

| Path | Description |
|---|---|
| `src/lib.rs` | `sidecarzero._core` `PyO3` `abi3` module definition |
| `src/client.rs` | `Client` implementation delegating the shared-memory protocol to `sdks/rust` (`sidecarzero` crate) on Linux and providing the non-Linux fallback stub elsewhere |
| `src/input.rs` | Buffer-protocol (`PyBuffer`) input pinning and conversion (exported with GIL held, copied into shared memory with GIL released) |
| `src/codecs.rs` | `Bytes` (opaque bytes) and `DenseF32ToScore` (`float32[rows, n]` → `float32[rows, 1]`) codecs |
| `src/fallbacks.rs` | Native Rust-evaluated fallbacks (`constant`, `threshold`, `linear`, `lookup`) that execute without re-acquiring the GIL |
| `src/outcome.rs`, `src/fingerprint.rs` | `Outcome` type and XXH3-64 schema fingerprint helper |
| `src/worker.rs` | Native `scz-worker-core` backend bridging shared-memory Arrow IPC slots to Python NumPy arrays |
| `python/sidecarzero/` | Public Python package (`Client`, `Outcome`, `Source`, `FallbackReason`, `FallbackPolicy`, `codecs`, `fallbacks`, `testing.InProcessClient`, `_core.pyi`, `py.typed`) |
| `python/sidecarzero/worker/` | Python worker runner (`__main__.py`, `_runner.py`, `Signature`, `tensor`, `model_path`, `model_bytes`) |
| `agent/scz_agent.py` | Conformance agent (`scz-agent-python`) implementing conformance protocol v2 |
| `build.sh`, `test.sh`, `build-wheel.sh` | Build, test, allocation-gate, and PEP 517 `.whl` packaging scripts |
| `tests/` | Client unit tests, live supervisor integration tests, worker runner tests (`test_runner.py`), and `sys.getallocatedblocks()` flat-memory gate |

---

## Building, Testing, and Packaging

Inside the development container (`scripts/dev`):

```bash
# Build and run unit, runner, and memory-growth gate tests
scripts/dev bash -c 'sdks/python/build.sh && sdks/python/test.sh'

# Run the 56-scenario cross-language conformance and crash-point suite
scripts/dev bash -c 'sdks/python/build.sh && target/release/scz-conformance --bins=target/release \
    --crash-bins=target/crash/release --agent=target/sdk/python/scz-agent-python \
    conformance/scenarios conformance/scenarios/crash'

# Build a standalone abi3 wheel (sidecarzero-0.1.0-cp311-abi3-linux_<arch>.whl)
scripts/dev bash -c 'sdks/python/build-wheel.sh'
```

---

## Python Worker Runner Usage

Define a Python module exposing `load`, `signature`, and `predict`:

```python
# my_model.py
import numpy as np
from sidecarzero.worker import Signature, tensor

def load(model_path: str):
    return np.load(model_path)

def signature(w) -> Signature:
    return Signature(
        inputs={"features": tensor("float32", [256])},
        outputs={"score": tensor("float32", [])},
    )

def predict(w, batch: dict[str, np.ndarray]):
    # batch["features"] is a zero-copy read-only (rows, 256) NumPy view over slot memory
    return (batch["features"] @ w).astype(np.float32)
```

Launch under `sidecarzero-supervisor`:

```bash
sidecarzero-supervisor \
  --socket=/run/sidecarzero/scz.sock \
  --model=/models/w.npy \
  --worker=/usr/bin/python3 \
  --worker-arg=-B --worker-arg=-m --worker-arg=sidecarzero.worker --worker-arg=my_model \
  --worker-read=/usr --worker-read=/app --worker-read=/models/w.npy \
  --worker-socketpair --worker-rlimit-data-mb=2048
```
