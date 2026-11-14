# Chaos & Performance Benchmark Suite (`bench/`)

[← 13 — Chaos Benchmark Spec](../docs/components/13-chaos-benchmark.md) · [Canonical Benchmark Results →](../docs/benchmarks.md)

The reproducible chaos and performance benchmark suite comparing SidecarZero (`System A`) against Triton Inference Server over gRPC + POSIX shared memory (`System B`), localhost HTTP/JSON (`System C`), Unix domain socket binary framing (`System D`), and in-process ONNX Runtime (`System E`).

```bash
make -C bench chaos-bench-quick
```

Executes the suite and writes raw JSON records plus a compiled Markdown report to `bench/results/<timestamp>/report.md`.

| Make Target | Description |
|---|---|
| `make -C bench chaos-bench` | Full campaign across all systems, scenarios, host languages, and modes (5 repetitions, 1,000 kills per kill-loop run) |
| `make -C bench chaos-bench-quick` | Fast campaign across all systems and scenarios (10 kills per kill-loop run, 30,000 RPS spinning regime) |
| `make -C bench chaos-bench-docker` | In-process campaign in isolated containers without requiring a Kind cluster |
| `make -C bench report RESULTS=bench/results/<timestamp>` | Regenerates `report.md` from existing raw run artifacts |

Configurable Make variables: `SYSTEMS` (`A,B,C,D,E`), `SCENARIOS` (`steady,kill,freeze,reload,overload,idle`), `HOST_LANG` (`rust,java`), `MODES` (`inproc,service`), `REPEATS`, and `KILLS`.

---

## Systems Under Test

| ID | Architecture | Implementation |
|---|---|---|
| **A** | **SidecarZero** (`sidecarzero-supervisor` + `sidecarzero-worker` + warm standby, plus `nospin` and `nostandby` variants) | Rust (`sdks/rust`) and Java FFM (`sdks/java-ffm`) SDKs |
| **B** | **Triton Inference Server 26.04** (ONNX Runtime backend, tensors in registered POSIX shared memory, control plane over gRPC) | [`baselines/triton`](baselines/triton/README.md) and [`triton-client`](triton-client/README.md) |
| **C** | **Localhost HTTP/JSON** server wrapping ONNX Runtime | [`server`](server/src/main.rs) (`--http`) |
| **D** | **Unix Domain Socket binary** server wrapping ONNX Runtime (length-prefixed little-endian `f32`) | [`server`](server/src/main.rs) (`--uds`) |
| **E** | **In-Process ONNX Runtime** linked directly into the host process | [`host`](host/src/systems.rs) |

Every baseline client (`B`, `C`, `D`) is wrapped with the identical fallback policy ([`common`](common/src/lib.rs)) and executes the same 256-feature fraud model (`fraud.onnx` / `fraud.ort`) with matching ONNX Runtime thread counts (`intra_op = 1`, `inter_op = 1`).

---

## Directory Layout

| Path | Description |
|---|---|
| [`run.py`](run.py) | Campaign orchestrator: measures baseline inference time, runs each system and fault scenario in isolation, collects telemetry, and invokes `report.py` |
| [`service.py`](service.py) | Kubernetes service-mode runner on Kind, tracking `EndpointSlice` transitions and container restarts |
| [`host/`](host/src/main.rs) | `scz-bench-host` (Rust): in-process open-loop benchmark driver (`inproc`) and service-mode HTTP server (`serve`) |
| [`host-java/`](host-java/) | Java 25 FFM in-process open-loop benchmark driver |
| [`loadgen/`](loadgen/src/main.rs) | `scz-loadgen`: open-loop HTTP load generator for service mode |
| [`chaos/`](chaos/src/main.rs) | `scz-kill-agent`: records `CLOCK_MONOTONIC` timestamps and injects `SIGKILL` / `SIGSTOP` directly into target worker/server PIDs |
| [`server/`](server/src/main.rs) | Native HTTP/JSON (`System C`) and UDS binary (`System D`) ONNX Runtime server, with `--supervise` support |
| [`triton-client/`](triton-client/), [`baselines/triton/`](baselines/triton/) | `System B` Triton server harness and gRPC + POSIX shared-memory client |
| [`common/`](common/src/lib.rs) | Shared fraud scorer, reference fallback, sub-bucketed latency histogram, and JSON result schema |
| [`report/`](report/report.py) | Markdown report generator (`report.py`) |
| [`results/`](results/) | Raw campaign JSON files and generated `report.md` summaries |
