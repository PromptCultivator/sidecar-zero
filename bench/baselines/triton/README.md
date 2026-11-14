# System B Baseline: Triton Inference Server (gRPC + System Shared Memory)

[← 13 — Chaos Benchmark §3](../../../docs/components/13-chaos-benchmark.md#3-systems-compared)

Triton Inference Server 26.04 (`nvcr.io/nvidia/tritonserver:26.04-py3`, Triton 2.68.0 with ONNX Runtime backend) serving the 256-feature `fraud.onnx` model over gRPC with input and output tensors placed in registered POSIX shared memory (`/dev/shm`). Paired with the Rust gRPC + SHM client in [`bench/triton-client`](../../triton-client/README.md).

| Path | Description |
|---|---|
| `repository/fraud/config.pbtxt` | Triton model configuration template (`onnxruntime_onnx` platform, `intra_op = 1`, `inter_op = 1`, sequential execution, model warmup enabled) |
| `run.sh` | Stages the model repository, starts the Triton container (either standalone or under a supervised restart loop via `--supervised`), and waits for gRPC readiness on port `8001` |
| `smoke.sh` | Self-contained smoke test verifying numerical score agreement (`≤ 1.2e-6` max difference), steady-state latency, `SIGSTOP` freeze handling, `SIGKILL` recovery, and shared-memory cleanup |

---

## Server Configuration

`run.sh` launches `tritonserver` with best-practice CPU low-latency flags:

```bash
tritonserver \
  --model-repository=/models \
  --disable-auto-complete-config \
  --allow-grpc=true --grpc-port=8001 \
  --allow-http=false --allow-metrics=false \
  --allow-client-shm=true \
  --pinned-memory-pool-byte-size=0
```

- `--allow-client-shm=true` enables `SystemSharedMemoryRegister` so tensor payloads bypass gRPC protobuf serialization and travel via `/dev/shm`.
- `--disable-auto-complete-config` and `max_batch_size: 0` disable dynamic batching queues so single-row requests are dispatched immediately.
- `intra_op_thread_count: 1` and `inter_op_thread_count: 1` match SidecarZero's worker thread configuration.
- `--supervised` wraps `tritonserver` in an in-container PID 1 restart loop so `SIGKILL` of `tritonserver` respawns the server without destroying the container's `/dev/shm` and network namespaces.

---

## Smoke Verification

```bash
bench/baselines/triton/smoke.sh
```

Verifies:

1. **Numerical equivalence:** All 64 reference rows match SidecarZero's ONNX Runtime reference scores within `1.2e-6`.
2. **Steady-state sequential and 4-thread concurrent inference** over registered POSIX shared memory.
3. **Fault injection (`SIGSTOP` and `SIGKILL`):** Verifies client `timerfd` deadline enforcement, automatic gRPC reconnection, and shared-memory region re-registration after `tritonserver` restarts.
4. **Resource cleanup:** Verifies that dropping the client unregisters both shared-memory regions from Triton and unlinks them from `/dev/shm`.
