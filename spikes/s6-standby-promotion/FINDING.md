# S6 — Warm Standby Promotion Latency vs Process Memory Teardown

[← Findings Summary](../FINDINGS.md)

## Objective

Measure the time required to return the gate to `HEALTHY` after `kill -9 <active_worker>` with a warm standby worker waiting on `standby_wait`, and characterize how kernel `pidfd` exit notification latency scales with the dying worker's private anonymous RSS versus a kernel-watched robust futex lock.

---

## Experimental Setup

Executed via `./run.sh` on Linux `6.12.76` (`aarch64`, 10 CPUs, 20 `kill -9` iterations per memory size).

An active worker maps and touches a configurable amount of private anonymous memory (`0 MiB` to `2,048 MiB`), holds a robust lock in shared memory, and owns 48 in-flight slots while a warm standby worker sleeps on `standby_wait`. The supervisor sends `SIGKILL` to the active worker and records timestamps on `CLOCK_MONOTONIC` for:

1. **Robust lock death notification:** When the supervisor's watcher wakes with `FUTEX_OWNER_DIED`.
2. **`pidfd` readable notification:** When `epoll` reports the dead worker's `pidfd` readable.
3. **Standby `HEALTHY`:** After sweeping 1,088 slots, bumping `worker_gen`, waking the standby, and receiving `ACTIVE`.

---

## Measured Results (`results/promotion.txt`)

Milliseconds after `kill -9` (`median / p90 / max` across 20 kills per row):

| Worker Memory Footprint | Robust Lock Reports Death (`ms`) | `pidfd` Becomes Readable (`ms`) | Standby `HEALTHY` After `pidfd` (`ms`) |
|---|---|---|---|
| `0 MiB` extra private RSS | **`0.07 / 0.13 / 0.16`** | `0.16 / 0.37 / 0.40` | `0.22 / 0.51 / 0.52` |
| `16 MiB` private RSS | **`0.07 / 0.64 / 4.35`** | `1.07 / 2.95 / 8.20` | `1.15 / 3.04 / 9.39` |
| `128 MiB` private RSS | **`0.05 / 0.09 / 0.16`** | `3.88 / 4.77 / 5.80` | `3.92 / 4.84 / 5.86` |
| `512 MiB` private RSS | **`0.05 / 0.09 / 0.17`** | `9.44 / 10.01 / 10.31` | `9.48 / 10.06 / 10.35` |
| `1,024 MiB` private RSS | **`0.05 / 0.06 / 0.06`** | `15.68 / 16.92 / 17.07` | `15.71 / 16.95 / 17.12` |
| `2,048 MiB` private RSS | **`0.06 / 0.06 / 0.10`** | `29.89 / 32.65 / 41.33` | `29.93 / 32.73 / 41.39` |
| `1,024 MiB` private RSS (8 threads) | **`0.07 / 0.11 / 0.11`** | `12.09 / 13.06 / 13.41` | `12.16 / 13.46 / 14.89` |
| `1,024 MiB` shared file mapping | **`0.05 / 0.08 / 0.11`** | `8.03 / 8.65 / 8.74` | `8.07 / 8.69 / 8.79` |

### Key Observations

1. **`pidfd` waits for kernel page-table teardown (`exit_mmap`):** The kernel notifies a process's `pidfd` only after `do_exit` has unmapped and freed its entire virtual address space, costing **`≈ 15 ms per GiB` of private anonymous memory** (`≈ 8 ms per GiB` for shared file-backed pages).
2. **Supervisor sweep and standby promotion take microseconds:** Once notified, flipping the gate and sweeping 1,088 slots takes `1–3 µs`, and waking the warm standby to `ACTIVE` takes `30–80 µs`.
3. **Robust futex locks fire before memory teardown (`0.05–0.07 ms` at any RSS):** In the Linux kernel's `do_exit` path, `exit_mm` walks each dying thread's robust futex list (`exit_robust_list`) and wakes waiters with `FUTEX_OWNER_DIED` **before** calling `exit_mmap` to free the process's memory pages. At `2 GiB` RSS, the robust lock reports worker death **`500×` earlier** than `pidfd` (`0.06 ms` vs `29.89 ms`).

---

## Architectural Conclusions

To achieve sub-millisecond gate closure and standby promotion regardless of model memory size ([04 — Supervisor §5.2](../../docs/components/04-supervisor.md#52-detecting-death-and-hangs)), SidecarZero decouples **early death detection and ring handover** (driven by per-consume-thread robust locks in `≈ 0.05 ms`, validated further in [S6b](../s6b-death-signal/FINDING.md)) from **in-flight `PROCESSING` slot reclamation** (which waits for `pidfd` exit so no surviving worker thread can write to a reclaimed response buffer).
