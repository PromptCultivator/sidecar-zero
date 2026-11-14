# SidecarZero Rust SDK (`sdks/rust`)

[← 10 — Rust Reference SDK Spec](../../docs/components/10-sdk-rust.md) · [06 — Shared SDK Contract](../../docs/components/06-sdk-contract.md)

The reference client implementation of the SidecarZero SDK contract (`crate sidecarzero`). Compiled with `#![forbid(unsafe_code)]`, delegating all shared-memory operations and Linux system calls to the Loom-verified [`scz-shm`](../../crates/scz-shm) crate.

```rust
use std::time::Duration;
use sidecarzero::{Client, FallbackReason, codecs::DenseF32ToScore};

let client = Client::builder()
    .socket("/run/sidecarzero/scz.sock")
    .codec(DenseF32ToScore::<256>::new())
    .fallback(|features: &[f32; 256], _reason: FallbackReason| {
        if features[17] > 0.8 { 0.9 } else { 0.1 }
    })
    .build(); // returns immediately; never blocks on I/O

let out = client.infer(&features, Duration::from_millis(2));
let metrics_body = client.prometheus_text();
```

---

## Directory Layout

| Path | Description |
|---|---|
| `src/lib.rs` | `#![forbid(unsafe_code)]` crate root exporting `Client`, `ClientBuilder`, `Outcome`, `Source`, `FallbackReason`, `FallbackPolicy`, `Codec`, `Options`, `Stats`, `MetricsSink`, `PrometheusText`, `codecs`, `fallbacks`, and `testing` |
| `src/client.rs` | Typestate `ClientBuilder`, `Client<C, F, H>`, normative call algorithm ([06 — SDK Contract §4](../../docs/components/06-sdk-contract.md#4-the-call-algorithm-normative)), overload circuit breaker, 1,024-entry `CallerEntry` table, background session thread, and `PrometheusText` exporter |
| `src/codec.rs` | `Codec` trait, `SlotWriter`/`SlotReader`, `FrameWriter`/`FrameReader`, and prebuilt codecs (`DenseF32ToScore<N>`, `DenseF32Batch<N>`, `RawBytes`) |
| `src/fallback.rs` | `FallbackPolicy` trait and reference fallbacks (`Constant`, `Threshold`, `Linear`) |
| `src/testing.rs` | `InProcessClient` and `Fault` injection for deterministic unit testing without a sidecar process |
| `src/bin/scz-agent-rust.rs` | Reference conformance agent (`scz-agent-rust`) implementing conformance protocol v2 |
| `tests/` | Unit tests, compile-fail typestate checks, zero-allocation gate (`CountingAlloc`), and Prometheus metrics verification (`tests/metrics.rs`) |

---

## Building and Testing

Inside the development container (`scripts/dev`):

```bash
scripts/dev cargo test -p sidecarzero --release
scripts/dev bash -c 'target/release/scz-conformance --bins=target/release \
    --crash-bins=target/crash/release --agent=target/release/scz-agent-rust \
    --crash-agent=target/crash/release/scz-agent-rust \
    conformance/scenarios conformance/scenarios/crash'
```
