# Cross-Language Conformance Suite (`conformance/`)

[← 14 — Testing and Verification §3](../docs/components/14-testing-verification.md#3-cross-language-conformance-suite-conformance)

The declarative cross-language conformance and crash-point verification suite for SidecarZero. Every SDK builds a conformance agent speaking the line-oriented v2 agent protocol (`scz-agent-rust`, `scz-agent-java`, `scz-agent-cpp20`, `scz-agent-go`, `scz-agent-python`) and passes the identical 56-scenario suite against `sidecarzero-supervisor` and `scz-worker-chaos`.

| Path | Description |
|---|---|
| [`proto/`](proto/src/lib.rs) | Wire format for keyed conformance requests (`opcode`, `arg`, `nonce`, `seed`, `filler`) and deterministic FNV-1a response verification |
| [`runner/`](runner/src/lib.rs) | `scz-conformance` orchestrator binary and `cargo test` integration harness |
| [`scenarios/`](scenarios/) | Declarative `.toml` conformance scenarios covering happy path, admission, deadlines, overload, worker faults, hostile scribblers, wake-ups, standby promotion, live reload, rollback, supervisor faults, client faults, ABI/schema compatibility, and byte equality |
| [`scenarios/crash/`](scenarios/crash/) | Per-tag atomic crash-point injection scenarios (`SCZ_CRASH_AT`) across worker and client stores |
| [`coverage.toml`](coverage.toml) | Explicit per-SDK fallback-reason coverage exemptions (such as `CLOCK_FAULT` on SDKs that read `CLOCK_MONOTONIC` directly) |
| [`skips/`](skips/) | Explicit capability-based scenario skip lists (`<sdk>.toml`), verified by CI so no scenario is ever skipped silently |

---

## Running the Suite

`scripts/check` builds and runs the full conformance and crash-point suite across all five SDKs. To run the suite directly:

```bash
# Run against the reference Rust agent
scripts/dev cargo test -p scz-conformance

# Run a single scenario filter with output
scripts/dev bash -c 'SCZ_SCENARIO=reload cargo test -p scz-conformance --test scenarios -- --nocapture'

# Run against another language's agent (e.g., Go)
scripts/dev bash -c 'sdks/go/build.sh && target/release/scz-conformance \
    --bins=target/release --crash-bins=target/crash/release \
    --agent=target/sdk/go/scz-agent-go --crash-agent=target/sdk/go/crash/scz-agent-go \
    conformance/scenarios conformance/scenarios/crash'
```

---

## Scenario Format

```toml
name = "standby promotion mid-load"
seed = 1
allow = ["WORKER_FAILED", "GATE_DEAD", "DEADLINE"]
covers = []
needs = []

[sidecar]
worker = "scz-worker-chaos"
args = ["--standby=warm"]
env = ["SCZ_CHAOS_LOAD_MS=300"]
start = true

[agent]
start = true
threads = 4
budget_us = 20000
fallback_us = 0

[[step]]
do = "bound"

[[step]]
do = "call"
count = 50000
background = true

[[step]]
do = "signal"
target = "active"
```

### Universal Post-Scenario Assertions

On every call and after the final step of every scenario, `scz-conformance` asserts:

1. Every call returned within `budget_us + fallback_us + tolerance_ms`.
2. Every `model` answer matches the deterministic FNV-1a digest of its own request's `(thread, seq, seed)` nonce at a valid non-zero `model_gen` (or matches the ONNX Runtime reference fraud scores within `1e-5` when `codec = "features"`).
3. Every `fallback` outcome carries a `FallbackReason` permitted by the step's `reasons` or scenario's `allow` list.
4. The supervisor remains alive (unless killed by the scenario) and the gate returns to `HEALTHY`.
5. The shared-memory arena passes a full structural invariant check via `sczctl dump` (all slots `FREE` with `WAITER == 0` and `OWNER == 0`, all rings empty with consistent sequence numbers, and all wake words at rest).
6. The agent's open file descriptors, `/memfd:scz-*` mappings, and thread count match its initial post-bind baseline (`PROC`).
7. The agent's `STATS` counters match the call outcomes recorded by the runner.
8. All `FallbackReason` variants, chaos opcodes, and `scz_shm::crash::TAGS` crash points are covered across the suite.

---

## Conformance Agent Line Protocol (v2)

Each conformance agent accepts `--endpoint=<path>`, constructs a single client, and processes line-delimited commands on `stdin` with line-delimited responses on `stdout`:

| Command | Response |
|---|---|
| *(startup)* | `HELLO sdk=<sdk> version=<V> caps=<c1,c2,...>` |
| `CONFIG threads=N fallback_us=N kind=K fallback=F spin=S codec=C inputs=PATH` | `OK` or `ERR unsupported` |
| `BOUND ms=N` | `BOUND true\|false` |
| `CALL count=N budget_us=N rows=N op=O arg=N seed=N` | One `R ...` line per call, followed by `END calls=<total>` |
| `CODEC_FAULT mode=off\|encode\|decode` | `OK` |
| `UNWIND` | `OK` (arms next call to unwind between claiming a ring cell and publishing it) |
| `CLOCK_SKEW ns=N` | `OK` or `ERR unsupported` |
| `FORK` | `FORK src=<src> reason=<R> status=<exit_code>` or `ERR unsupported` |
| `SPAWN` | `SPAWNED pid=<pid>` |
| `STATS` | `STATS model=N sessions=N retired=N late_wakes=N fb.<REASON>=N ...` |
| `PROC` | `PROC fds=N maps=N threads=N` |
| `CLOSE` | `CLOSED` |
| `EXIT` | `BYE` |
