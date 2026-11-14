# S6b — Per-Consume-Thread Robust Death Locks

[← Findings Summary](../FINDINGS.md) · Follow-up to [S6 — Warm Standby Promotion](../s6-standby-promotion/FINDING.md)

## Objective

Verify the complete per-consume-thread early death lock mechanism specified in [01 — Shared-Memory ABI §2](../../docs/components/01-shm-abi.md#2-two-segment-model) and [04 — Supervisor §5.2](../../docs/components/04-supervisor.md#52-detecting-death-and-hangs):

1. Does a worker with **one robust lock per consume thread** (`WorkerSlot.death_lock` at 64-byte strides) report all consume-thread deaths within `< 0.5 ms` regardless of worker RSS?
2. Can a static **`musl`-linked supervisor** reliably watch robust `pthread_mutex_t` locks acquired by a **`glibc`-linked worker** by operating solely on word 0 (`FUTEX_WAITERS` / `FUTEX_OWNER_DIED`) via raw `futex` syscalls?
3. Has every thread of the dying worker ceased executing user-space instructions by the time the first death lock reports, across external `SIGKILL`, internal `SIGSEGV`, and `abort()`?
4. How do core dumps (`RLIMIT_CORE`) affect the timing of the robust death lock?

---

## Experimental Setup

Executed via `./run.sh` on Linux `6.12.76` (`aarch64`):

- **`worker.c` (compiled against `glibc`):** Spawns 4 consume threads (each acquiring a `PTHREAD_MUTEX_ROBUST | PTHREAD_PROCESS_SHARED` lock at word 0 of its 64-byte `WorkerSlot` cell and tight-looping on an atomic counter) plus 2 unlocked runtime threads tight-looping on their own counters.
- **`sup.c` (compiled statically against `musl`):** Spawns one watcher thread per cell that sets `FUTEX_WAITERS` (`0x80000000`) via CAS and blocks in `syscall(SYS_futex, FUTEX_WAIT)` on word 0, signaling the supervisor's `epoll` loop via `eventfd` as soon as `(word & 0x40000000) != 0` (`FUTEX_OWNER_DIED`).

---

## Measured Results

### 1. Multi-Thread Death Lock Latency Across Worker RSS (`results/death-signal.txt`)

Milliseconds after `kill -9` (`median / p90 / max` across 20 kills per row):

| Worker Private RSS | First of 4 Locks Reports (`ms`) | Last of 4 Locks Reports (`ms`) | `pidfd` Readable (`ms`) | Worker Instructions Executed After First Report |
|---|---|---|---|---|
| `0 MiB` | **`0.056 / 0.092 / 0.410`** | **`0.099 / 0.240 / 0.450`** | `0.164 / 0.257 / 0.453` | **`0` (across all 6 threads)** |
| `512 MiB` | **`0.053 / 0.090 / 0.166`** | **`0.093 / 0.119 / 0.186`** | `0.576 / 2.049 / 5.743` | **`0` (across all 6 threads)** |
| `2,048 MiB` | **`0.064 / 0.115 / 0.233`** | **`0.132 / 0.342 / 0.367`** | `25.881 / 54.227 / 60.098` | **`0` (across all 6 threads)** |

### 2. Internal Fault Modes (`SIGSEGV` and `SIGABRT`, `results/crash-inside.txt`)

Medians across 20 runs per cell at `0 MiB / 512 MiB / 2,048 MiB` worker private RSS:

| Crash Cause | First of 4 Locks (`ms`) | Last of 4 Locks (`ms`) | `pidfd` Readable (`ms`) |
|---|---|---|---|
| External `SIGKILL` | `0.06 / 0.06 / 0.08` | `0.10 / 0.11 / 0.14` | `0.19 / 0.66 / 26.2` |
| `SIGSEGV` in a consume thread | `0.12 / 0.24 / 0.13` | `0.16 / 0.32 / 0.17` | `0.24 / 1.10 / 26.3` |
| `SIGSEGV` in an unlocked runtime thread | `0.12 / 0.11 / 0.12` | `0.16 / 0.14 / 0.16` | `0.24 / 0.74 / 27.1` |
| `abort()` (`SIGABRT`) in a consume thread | `0.11 / 0.12 / 0.13` | `0.15 / 0.16 / 0.17` | `0.24 / 0.75 / 25.7` |

In all **240 crash runs**, all 4 consume-thread locks reported with word value `0xc0000000` (`FUTEX_WAITERS | FUTEX_OWNER_DIED`), and zero worker threads executed any user-space instructions after the first lock reported.

### 3. Impact of Core Dumps (`results/crash-with-core-dump.txt`)

When `RLIMIT_CORE` is set to `RLIM_INFINITY` (`ulimit -c unlimited`) instead of `0`, a `SIGSEGV` in a `512 MiB` worker causes the kernel to suspend all threads of the dying process while writing the core file **before** walking their robust futex lists, delaying the first death lock report from `0.12 ms` to **`289 ms`** (and `pidfd` to `322 ms`).

---

## Architectural Conclusions

1. **Cross-libc ABI independence ([01 — Shared-Memory ABI §2](../../docs/components/01-shm-abi.md#2-two-segment-model)):** Because the Linux kernel's `robust_list_head` protocol operates on the 32-bit futex word at offset `0` of `pthread_mutex_t`, a `musl`-linked supervisor watches `glibc`, `musl`, or rustix-acquired robust locks identically.
2. **Immediate gate closure and per-ring handover ([04 — Supervisor §5.2](../../docs/components/04-supervisor.md#52-detecting-death-and-hangs)):** On the first consume-thread death-lock report, the supervisor closes the gate and fails all `READY` slots; once every consume thread's lock has reported, the rings are repaired and the warm standby is promoted (`≈ 0.22 ms` total), without waiting `26+ ms` for `pidfd` memory teardown.
3. **Disable worker core dumps by default (`RLIMIT_CORE = 0`):** The supervisor sets `RLIMIT_CORE = 0` on every worker before `exec` so core dumps never stall the early death lock (overridable via `--worker-core-dumps` / `worker.coreDumps: true` for debugging).
