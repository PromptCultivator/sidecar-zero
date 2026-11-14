# 07 — Java SDK (`sdks/java-ffm`)

[← High-level design](../high-level-design.md) · implements [06 — SDK contract](06-sdk-contract.md)

The SidecarZero Java SDK is a pure-Java client built on the Foreign Function & Memory (FFM) API (`java.lang.foreign`). It requires zero JNI code, zero native companion libraries, zero `ByteBuffer` wrappers, and zero heap allocations on the request path.

---

## 1. JDK baseline and native access

The Java SDK targets **JDK 22+** (where JEP 454 finalized the `java.lang.foreign` API without `--enable-preview` flags) and is verified in CI on **JDK 25 LTS**.

Applications must pass `--enable-native-access=ALL-UNNAMED` (or the module name) at JVM launch. Because JDK 24+ only logs a warning by default until the first restricted FFM call is invoked on the background connection thread, the SDK checks `Module.isNativeAccessEnabled()` eagerly inside the builder's `.build()` method and fails fast with a clear error message naming the required JVM flag if native access is not enabled.

---

## 2. Foreign Function & Memory (FFM) usage

| Responsibility | FFM mechanism |
|---|---|
| Unix `SOCK_SEQPACKET` connect & `recvmsg` (`SCM_RIGHTS`) | Downcall handles to `socket`, `connect`, `sendmsg`, `recvmsg` (`MSG_CMSG_CLOEXEC`), `shutdown`, `close` |
| Arena mapping & page-table warm-up | Downcall handles to `mmap` (`PROT_READ` for `scz_ctl`, `PROT_READ \| PROT_WRITE` for `scz_data`) and `madvise` (`MADV_POPULATE_READ` / `MADV_POPULATE_WRITE`), wrapped via `MemorySegment.ofAddress(addr).reinterpret(len, arena, cleanup)` (`scz_ctl` is additionally wrapped `.asReadOnly()`) |
| File seal & size verification | Downcalls to `fcntl(fd, F_GET_SEALS)` and `lseek(fd, 0, SEEK_END)` |
| Atomic shared-memory access | Generated `VarHandle` accessors (`io.sidecarzero.internal.Abi`) |
| Shared `futex` wait & wake | Downcall handle to `syscall(SYS_futex, …)` |
| Monotonic clock calibration | Downcall handle to `clock_gettime(CLOCK_MONOTONIC, …)` |
| Bulk feature copy | `MemorySegment.copy(float[], 0, segment, ValueLayout.JAVA_FLOAT_UNALIGNED, offset, len)` (C2 intrinsic) |

Key engineering details:

- **`lseek` instead of `fstat`:** On older `glibc` versions (< 2.33), `fstat` is a libc inline wrapper around `__fxstat` rather than an exported dynamic symbol, and `struct stat` layouts differ between `x86_64` and `aarch64`. `lseek(fd, 0, SEEK_END)` is portable across all architectures and `glibc`/`musl` versions.
- **Variadic downcall linkage:** Both `syscall` and `fcntl` are variadic in C and are linked with `Linker.Option.firstVariadicArg(1)` and `Linker.Option.firstVariadicArg(2)` respectively so arguments pass in the ABI-mandated registers/stack slots on `aarch64` and `x86_64`.
- **No `Linker.Option.critical` on blocking syscalls:** Marking `futex` as critical would prevent the JVM from reaching a safepoint while a thread is parked in the kernel, which would stall garbage collection across the entire JVM.
- **Background lambda-form pre-customization:** After 127 invocations of a `MethodHandle`, the JDK customizes its internal `LambdaForm`, allocating ~64 KB on the calling thread once. To keep request threads strictly zero-allocation from their very first call, the SDK's background thread invokes every request-path downcall handle 300 times and warms up the ring/slot JIT paths 400 times over process-local memory before publishing the first session snapshot.
- **Zero-deadline `futex` calls:** When a test or caller invokes a zero-budget wait (`deadlineNs == 0`), the SDK passes a pre-allocated zero `timespec` (`&ZERO_TS`) to `FUTEX_WAIT_BITSET` so the kernel times out immediately instead of treating `NULL` as an infinite wait.

---

## 3. Memory access and generated `VarHandle`s

All shared-memory reads and writes go exclusively through `io.sidecarzero.internal.Abi`, generated from [`abi/scz_abi.toml`](../../abi/scz_abi.toml) by `scz-abi-gen`:

| Protocol access class | `VarHandle` method |
|---|---|
| `const` and `relaxed` fields (`status`, `resp_len`, `resp_stamp`, row counts) | `getOpaque` / `setOpaque` (guarantees single-instruction single-fetch without compiler load duplication or 64-bit tearing) |
| `acqrel` loads (`gate`, `supervisor_hb_mono_ns`, `slot.word`) | `getAcquire` (`getVolatile` for post-publish gate re-check) |
| `acqrel` stores (`READY` publish, `FREE` release) | `setRelease` |
| Fenced store-load pair (`cell.seq` publish → `consumer_wait` load) | `setVolatile` followed by `getVolatile` |
| Contended CAS transitions (slot claim, cancel, abandon, ring `tail`) | `compareAndSet` in bounded (≤ 64) retry loops |

`weakCompareAndSetPlain` is never used because it provides no acquire/release ordering on `aarch64`.

---

## 4. Public API and primitive specializations

```java
// Staged builder: build() is only reachable after .fallback(...)
ScoreClient client = SidecarZero.connect("/run/sidecarzero/scz.sock")
        .score(256)                                   // features: float32[1, 256] -> score: float32[1, 1]
        .fallback((features, reason) -> 0.0f)         // mandatory at compile time
        .options(Options.defaults().sampleEvery(64))
        .onFallback(reason -> { /* optional hook */ })
        .build();

// Zero-allocation primitive call returning packed 64-bit outcome
long packed = client.scoreOutcome(features, 2_000_000L);
float score = ScoreOutcome.score(packed);
boolean fromModel = ScoreOutcome.fromModel(packed);
FallbackReason reason = ScoreOutcome.reason(packed);
```

Because a generic `Outcome<Float>` boxes the `float` and allocates a wrapper object unless C2 scalar replacement succeeds at every call site, `ScoreClient.scoreOutcome` packs the 32-bit `float` (`Float.floatToRawIntBits`), the 1-bit `Source`, and the 8-bit `FallbackReason` into a single primitive `long` return value. Returning the packed outcome by value ensures thread-safety across both platform threads and virtual threads without relying on `ThreadLocal` state.

`BatchScoreClient` (writing scores into a caller-supplied `float[]`) and the generic `Client<I, O>` are also provided.

### Metrics (`MetricsSink` & `PrometheusText`)

`Client`, `ScoreClient`, and `BatchScoreClient` implement `export(MetricsSink)` and `prometheusText()` per [sdk-contract §7](06-sdk-contract.md#7-unified-metrics-and-hooks). `PrometheusText` formats floating-point values using the exact same shortest round-trip decimal rules (with ties rounding away from zero) as Rust's `f64` `Display`.

---

## 5. Zero-allocation caller state and sampling

A `ThreadLocal` allocates on the first call of every new thread — which violates the zero-allocation gate on newly spawned threads and allocates on every request in virtual-thread-per-request servers.

Instead, `Engine` maintains:

1. **Unified claimed `Caller` array (1,024 entries):** Every caller (platform thread or virtual thread) claims a `Caller` entry for the duration of `infer` by CASing `caller.inCall` from `0` to `snap.id << 32`, starting at `Long.hashCode(Thread.currentThread().threadId()) & 1023` and probing up to 16 neighbors, and releases it (`setRelease(0)`) in a `finally` block. Each `Caller` entry holds its own pre-allocated off-heap `timespec` segment, cached slot indices per class, ring cursor, overload `Breaker`, `SpinEwma`, and counter stripe.
2. **Striped sampling counter array (1,024 cache-line-padded stripes):** Call sampling (`Options.sampleEvery(n)`, default `64`) indexes into a process-wide array of 128-byte-spaced counters by `threadId & 1023`, avoiding any `ThreadLocal` allocation while keeping virtual-thread-per-request servers accurately sampled at `1 / n`.

Verified in `sdks/java-ffm/test.sh` (`AllocGate`): `ThreadMXBean.getThreadAllocatedBytes` deltas across 1,000,000 calls on both platform threads and virtual threads equal **0 bytes/call** across all model and fallback paths.

---

## 6. Monotonic clock calibration

On Linux HotSpot, `System.nanoTime()` is backed by `CLOCK_MONOTONIC` via the vDSO, but with an arbitrary JVM-startup epoch offset. At startup and every 250 ms on the background thread, `Clock` samples `System.nanoTime()` and `clock_gettime(CLOCK_MONOTONIC)` back-to-back and computes the tightest offset. Request threads compute `CLOCK_MONOTONIC` as `System.nanoTime() + offset` with zero FFM call overhead. If clock drift exceeds tolerance, calls fall back with `CLOCK_FAULT` until recalibrated.

---

## 7. Virtual threads and carrier-pinning protection

A virtual thread executing a native FFM downcall (`futex_wait`) is pinned to its carrier platform thread while blocked in the kernel. If all `N` carrier threads in `ForkJoinPool` blocked in `futex_wait` simultaneously, no carrier would remain free to run other virtual threads in the JVM.

To prevent carrier starvation:
- The SDK tracks the number of virtual threads currently parked in `futex_wait` in an atomic counter (`Stats.parkedCallers()`).
- While `parkedCallers < carriers − 1`, virtual threads park directly in `futex_wait` for sub-10 µs wakeup latency.
- Whenever `parkedCallers ≥ carriers − 1` (or when `Options.virtualPolling(true)` is set), additional virtual threads automatically switch to an unpinned polling loop using `LockSupport.parkNanos` (backing off from 2 µs to 200 µs), which unmounts the virtual thread from its carrier while waiting.
