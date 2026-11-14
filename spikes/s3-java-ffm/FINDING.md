# S3 — Java FFM Shared-Memory Transport & Virtual Threads

[← Findings Summary](../FINDINGS.md)

## Objective

Verify on JDK 22 and JDK 25 using only the Foreign Function & Memory API (`java.lang.foreign`, zero JNI):

1. Whether `recvmsg` (`SCM_RIGHTS` with `MSG_CMSG_CLOEXEC`), `lseek`, `fcntl(F_GET_SEALS)`, `mmap`, `VarHandle` atomic operations, and `syscall(SYS_futex, FUTEX_WAIT_BITSET)` execute with **0 bytes/op** heap allocation on the steady-state call path across C2, C1, and the interpreter.
2. Whether closing a shared `Arena` while another thread is parked in `futex` on that mapping is memory-safe (never causing a JVM `SIGSEGV`).
3. How the JVM virtual-thread scheduler behaves when virtual threads park in native `futex` downcalls, and whether capping parked virtual threads at `carriers - 1` prevents carrier starvation.

---

## Experimental Setup

Executed via `./run.sh` on Eclipse Temurin `25.0.5` and `22.0.2` (`linux/arm64` native and `linux/amd64` verified for FFM linker descriptors). Allocation counts were measured using `ThreadMXBean.getCurrentThreadAllocatedBytes` across three rounds of 1,000,000 iterations after warm-up, with a positive control (an escaping boxed `Long`) verifying exact `24 B/op` detection.

---

## Measured Results

### 1. Transport and Downcall Verification (`results/*/01-transport.txt`)

| Operation | Result (JDK 22 & JDK 25, `arm64` & `amd64`) |
|---|---|
| `recvmsg` with `MSG_CMSG_CLOEXEC` | Delivers both `memfd` descriptors with `FD_CLOEXEC` set |
| `lseek(..., 0, SEEK_END)` and `fcntl(fd, F_GET_SEALS)` | Accurately reads segment sizes and seals (`0x17` control, `0x7` data) |
| `mmap` read-only control segment + `asReadOnly()` | Writable `mmap` rejected (`EPERM`), `mprotect` rejected (`EACCES`), Java write attempts throw `UnsupportedOperationException` |
| Variadic linkage (`Linker.Option.firstVariadicArg`) for `fcntl` and `syscall` | Links and executes cleanly on both `aarch64` and `x86-64` |
| `VarHandle` CAS, volatile, acquire/release, and opaque operations on `MemorySegment` | 4 threads × 1,000,000 atomic increments produce exact 4,000,000 total |
| `syscall(SYS_futex, FUTEX_WAIT_BITSET, ...)` with absolute `CLOCK_MONOTONIC` deadline | Wakes on peer `FUTEX_WAKE_BITSET`, times out with `ETIMEDOUT`, returns `EAGAIN` immediately on value mismatch |
| `System.nanoTime()` to `CLOCK_MONOTONIC` offset calibration | Calibrates within `< 1 µs` |

### 2. Steady-State Heap Allocation (`results/*/02-alloc-c2.txt`, `03-alloc-c1.txt`, `03b-alloc-xint.txt`)

Measured in bytes per operation on both platform threads and virtual threads:

| Hot-Path Operation | C2 JIT | C1 Only (`-XX:TieredStopAtLevel=1`) | Interpreter (`-Xint`) |
|---|---|---|---|
| Clock read + 4 `VarHandle` loads + 1 CAS + 3 stores | **`0 B/op`** | **`0 B/op`** | **`0 B/op`** |
| Above + `futex(FUTEX_WAKE_BITSET)` downcall (no `errno` capture) | **`0 B/op`** | **`0 B/op`** | **`0 B/op`** |
| Above + pre-allocated `timespec` write + `futex(FUTEX_WAIT_BITSET)` downcall | **`0 B/op`** | **`0 B/op`** | **`0 B/op`** |
| Downcall with `Linker.Option.captureCallState("errno")` | `0 B/op` | `40 B/op` | `40 B/op` |
| Per-call `MemorySegment.asSlice(...)` | `0 B/op` | `40 B/op` | `40 B/op` |
| Positive control (escaping boxed `Long`) | `24 B/op` | `24 B/op` | `24 B/op` |

The SidecarZero Java FFM call path allocates **0 B/op under every execution tier, including the interpreter**, because it uses fixed base-segment `VarHandle` offsets (never calling `asSlice`) and omits `captureCallState("errno")` on hot-path `futex` downcalls.

### 3. Arena Retirement Safety Under Parked Callers (`results/*/04-arena.txt`)

When `Arena.ofShared().close()` is invoked while a thread is parked in `futex_wait` (passing the raw `segment.address() + offset` as a `long`):

- `Arena.close()` unmaps the segment immediately without crashing the JVM.
- The parked thread remains safely asleep in the kernel on the underlying file inode offset until woken or until its timeout expires.
- Any subsequent `VarHandle` access to the closed segment throws `IllegalStateException: Already closed` cleanly in Java rather than faulting native memory.
- Pairing this with SidecarZero's `Caller` in-call marker table ensures `Arena.close()` is called only after all in-flight callers have exited the segment ([07 — Java FFM SDK §8](../../docs/components/07-sdk-java-ffm.md#8-arena-retirement-and-memory-safety)).

### 4. Virtual-Thread Carrier Pinning (`results/*/05-pinning.txt`)

Tested on a 4-carrier JVM (`jdk.virtualThreadScheduler.parallelism=4`) with an unrelated virtual thread ticking every ~6.5 ms:

| Scenario | Unrelated Virtual Threads | Carrier Pool Behavior |
|---|---|---|
| 3 virtual threads parked in `futex` (`carriers - 1` cap) | **Continue running normally** (37 ticks / 250 ms) | 1 carrier remains free to schedule all other virtual threads |
| 4 virtual threads parked in `futex` (all carriers pinned) | **Completely starved** for the entire `futex` duration | ForkJoinPool does not compensate for native FFM downcalls |
| 3 virtual threads parked in `futex` + 1 virtual thread looping on `Thread.yield()` | **Starved** | The yielding virtual thread monopolizes the single remaining carrier |

Additionally, neither JFR's `jdk.VirtualThreadPinned` event nor `-Djdk.tracePinnedThreads=full` reports pinning inside native FFM downcalls, and marking a blocking downcall as `Linker.Option.critical` stalls JVM garbage collection (`System.gc()` blocked for `1.3 s`).

---

## Architectural Conclusions

1. **Carrier-pinning cap (`availableProcessors - 1`):** The Java SDK caps concurrent virtual threads parked in `futex` at `carriers - 1` and routes excess virtual threads to `LockSupport.parkNanos` polling (never `Thread.yield()`), preventing carrier starvation ([07 — Java FFM SDK §6](../../docs/components/07-sdk-java-ffm.md#6-virtual-threads)).
2. **Never mark blocking downcalls `critical`:** All `futex` and socket downcalls are standard FFM downcalls so JVM safepoints and garbage collection are never delayed.
3. **Explicit parked-caller telemetry:** Because JFR does not observe FFM downcall pinning, the SDK tracks and exports `Stats.parkedCallers()`.
