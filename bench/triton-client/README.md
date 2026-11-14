# Triton gRPC + POSIX Shared-Memory Client (`bench/triton-client`)

[← 13 — Chaos Benchmark §3](../../docs/components/13-chaos-benchmark.md#3-systems-compared) · Server harness in [`bench/baselines/triton`](../baselines/triton/README.md)

Low-overhead Rust client (`crate scz-bench-triton`) for `System B` in the chaos benchmark. Communicates with Triton Inference Server over HTTP/2 gRPC while placing input and output tensors in registered POSIX shared memory (`/dev/shm`).

```rust
let mut t = Triton::connect("http://127.0.0.1:8001", "fraud", "t0")?; // one per caller thread
let score: f32 = t.infer(&features, Duration::from_millis(2))?;      // features: &[f32; 256]
```

---

## Design and Protocol

- **Zero-Serialization Tensor Payloads:** `Triton::connect` creates two POSIX shared-memory objects (`/dev/shm/scz-<tag>-<pid>-in` of 1,024 bytes and `-out` of 4 bytes) and registers them with Triton via `SystemSharedMemoryRegister`. Because the `ModelInferRequest` protobuf message references only the shared-memory region names and byte sizes, `connect` pre-encodes the `ModelInferRequest` wire bytes once and reuses them on every call.
- **High-Precision `timerfd` Deadlines:** Rather than relying on Tokio's 1 ms timer wheel, `infer` arms a Linux `timerfd` (`CLOCK_MONOTONIC`) for the exact per-call budget (`2 ms`), ensuring accurate deadline enforcement during server stalls and kills.
- **Automatic Re-Registration Across Server Restarts:** When `tritonserver` is killed and restarted, it loses its in-memory registry of shared-memory regions. Upon any RPC error or timeout, the client automatically unregisters and re-registers both `/dev/shm` regions on the next call before issuing `ModelInfer`.
- **Clean Teardown:** `Drop` unregisters both regions from Triton and unmaps/unlinks the `/dev/shm` segments.

---

## Building and Testing

`proto/grpc_service.proto` and `proto/model_config.proto` are vendored from `triton-inference-server/common` (`r26.04`) and compiled via `tonic-prost-build`.

```bash
scripts/dev cargo test -p scz-bench-triton
```
