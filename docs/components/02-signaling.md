# 02 — Signaling: Adaptive Spin-Then-Wake

[← High-level design](../high-level-design.md) · depends on [01 — SHM ABI](01-shm-abi.md)

This document specifies how producers, workers, and the supervisor notify one another across shared memory without paying for a system call when traffic is flowing and without burning CPU cores when idle.

---

## 1. Operating regimes and measured performance

Pickup latency depends on which of three regimes a ring is in, determined by how far apart requests arrive relative to the spin budget `spin_ns` (50 µs by default):

| Regime | Condition | Pickup latency | Syscalls per request | Measured round-trip (`scz-worker-echo`, 1 KB) |
|---|---|---|---|---|
| **Spinning** | Requests arrive closer together than `spin_ns` (> 20,000 RPS per ring at 50 µs default) | < 200 ns each way (one cache line transfer between cores) | 0 | **319 ns** p50 / **506 ns** p99 |
| **Waking** | Requests arrive further apart than `spin_ns` | One `futex_wake` on the worker side, plus one on the client side if the caller also parked (5–50 µs depending on core placement and C-states) | 1 or 2 | **19.1 µs** p50 at 2,000 RPS / **48.6 µs** p50 at 10 RPS (with 256×64 MLP) |
| **Idle** | Zero traffic | First request pays the waking cost | — | **0.09%** of one core across the entire sidecar (supervisor + active worker + warm standby) |

| Property | Specification |
|---|---|
| Idle CPU | Zero spinning when idle. Client threads and worker consume threads park in the kernel; only the worker's 5 ms heartbeat (200 Hz) and the supervisor's 1 ms watchdog tick remain active, consuming **0.09% of one CPU core** (22× below the 2% budget) |
| Wake-up guarantee | Wake-ups are never lost. A wake-up delayed by a preempted waker is bounded by `hb_interval_ns` (5 ms) ([§3.1](#31-worker-side-waiting-for-work)) |
| Language support | Rust, C++20, Go, Java 22+ (FFM), Python (`abi3` via Rust binding) |

---

## 2. Mechanism choice

**Shared `futex` on 32-bit words inside the `MAP_SHARED` arena.** A Linux futex on a `MAP_SHARED` mapping without `FUTEX_PRIVATE_FLAG` is keyed in the kernel by the backing inode (`memfd`) and byte offset, so processes that map the same `memfd` at different virtual addresses wait on and wake the exact same kernel wait queue. It requires zero extra file descriptors and executes purely in userspace on the uncontended path.

Why alternatives were rejected:

| Alternative | Reason rejected |
|---|---|
| Pure busy-polling | Burns a full CPU core per waiting thread and exhausts CFS CPU quotas |
| `eventfd` on the synchronous path | Requires a `write` + `read` syscall pair per request even at high load |
| Unix-socket ping | Same syscall overhead as `eventfd`, plus socket buffer management |
| Process-shared `pthread_cond_t` | Not robust to a holder dying mid-operation; unusable from Java and Go |
| `std::atomic::wait` / Java `LockSupport` | Process-private; cannot wake across address spaces |

---

## 3. Wait points

| Waiter | Waits for | Futex word | Segment | Woken by |
|---|---|---|---|---|
| Worker consume thread | A descriptor in its ring, or a pending control request | `ring.consumer_wait` | Data | A producer after publishing; the supervisor after changing `ctl_req`; the worker's own heartbeat thread as a backstop |
| Client thread | Its slot reaching `DONE` or `FAILED` | Low 32 bits of `slot.word` | Data | The worker on completion; the supervisor during a sweep |
| Warm or paused worker | Activation of its generation, or the end of a pause | `standby_wait` | Control (read-only to the waiter) | The supervisor |

The first two follow the **spin → announce intent to sleep → full fence → re-check → sleep** pattern, which rules out lost wake-ups. The third cannot write an announcement because the control segment is mapped read-only by the worker; it uses a monotonic event counter instead ([§3.3](#33-warm-or-paused-worker-waiting-on-standby_wait)).

### 3.1 Worker side: waiting for work

`consumer_wait` has three states: `AWAKE` (`0`), `PARKED` (`1`), and `WAKING` (`2`).

```
worker loop:
    if dequeue() succeeds: process it; last_work = now; continue
    if now - last_work < spin_budget: cpu_relax(); continue

    consumer_wait.store(PARKED, SeqCst)                       // announce
    if the head cell is published or a control request is pending:   // re-check
        consumer_wait.store(AWAKE, SeqCst); continue
    futex_wait(&consumer_wait, PARKED, timeout = none)        // returns immediately unless word == PARKED
    consumer_wait.store(AWAKE, SeqCst)
```

```
waker — a producer, immediately after its SeqCst publish;
        the supervisor, immediately after a SeqCst store to ctl_req:
    w = consumer_wait.load(SeqCst)
    if w == AWAKE: return                                     // loaded path: zero syscalls
    if w == PARKED: consumer_wait.compare_exchange(PARKED, WAKING)    // result ignored
    futex_wake(&consumer_wait, 1)                             // on both PARKED and WAKING
```

Exact predicates for the worker's re-check:

- **The head cell is published** means `cells[head & mask].seq == head + 1`. Testing `tail != head` would be wrong because `tail != head` is also true while a producer has claimed a cell and not yet published it.
- **A control request is pending** means `ctl_req != ctl_ack` ([worker §4](05-worker.md#4-the-consume-loop)). Testing `ctl_req != 0` would be wrong because an acknowledged bit remains set in `ctl_req` until the supervisor clears it.

**Why no wake-up is lost.** Both the worker's `store(PARKED)` and the waker's publish are sequentially consistent (or separated from the subsequent load by `fence(SeqCst)`). Either the waker's load sees `PARKED`, or the worker's re-check sees the published cell and aborts the sleep. Because a waker transitions `PARKED → WAKING` before calling `futex_wake`, a worker caught between its re-check and `futex_wait` sees `consumer_wait != PARKED` (`EAGAIN`) and returns immediately.

**Why `WAKING` exists.** With only two states (`AWAKE` and `PARKED`), whichever producer flipped `PARKED → AWAKE` would solely own the `futex_wake` call. If the OS scheduler preempted that producer thread before it executed `futex_wake`, the worker would remain asleep in the kernel while every subsequent producer saw `AWAKE` and skipped waking it. `WAKING` means "a wake-up is owed": every producer that observes `PARKED` or `WAKING` issues `futex_wake(&consumer_wait, 1)`. Only the worker thread itself ever stores `AWAKE`.

**Heartbeat backstop.** On every 5 ms beat, the worker's heartbeat thread applies the waker rule to each ring that has a published head cell or a pending control request. That covers the case where a lone producer stalls or dies between publishing and waking with no subsequent producer behind it.

### 3.2 Client side: waiting for a result

```
client, after enqueueing (the publish was a SeqCst store):
    if state(gate.load(SeqCst)) != HEALTHY:        // re-check, once, before waiting
        if cancel_if_ready(slot): return CANCELLED
        // otherwise the worker or the sweep already owns the outcome; wait for it

    spin until min(spin_deadline, deadline):
        w = slot.word.load(Acquire)
        if state(w) in {DONE, FAILED}: return w
        if state(w) == READY and state(gate) != HEALTHY:    // still queued, and the worker is gone or going
            if cancel_if_ready(slot): return CANCELLED
        cpu_relax()

    loop:
        w = slot.word.load(Acquire)
        if state(w) in {DONE, FAILED}: return w
        if now >= deadline: return TIMEOUT
        if !(w & WAITER):
            if !slot.word.compare_exchange(w, w | WAITER): continue     // announce
            w |= WAITER
        futex_wait_until(&slot.word.low32, low32(w), deadline)          // absolute deadline

cancel_if_ready(slot):                             // CAS loop: (e, READY) → (e, FREE)
    loop:
        w = slot.word.load()
        if state(w) != READY: return false
        if slot.word.compare_exchange(w, (epoch(w), FREE)): return true
```

```
worker, on completion:
    loop:
        w = slot.word.load()
        if state(w) == ABANDONED:
            slot.word.compare_exchange(w, (epoch(w), FREE)); return     // CAS, never a blind store
        if state(w) != PROCESSING: return           // not ours any more; touch nothing
        new = (epoch(w), DONE)                      // clears WAITER and OWNER
        if slot.word.compare_exchange(w, new): break
    if w & WAITER: futex_wake(&slot.word.low32, 1)
```

- The `WAITER` bit ensures the worker pays for `futex_wake` only when a client actually parked. In the spinning regime, callers are still spinning when the answer lands, so zero syscalls occur on either side.
- Both phases are bounded by the call's absolute `CLOCK_MONOTONIC` deadline (`FUTEX_WAIT_BITSET` with `FUTEX_BITSET_MATCH_ANY`), so spurious or repeated wake-ups never accumulate timeout drift.
- **The gate re-check after publishing** pairs with the supervisor's sweep order (`store gate (SeqCst)` then `sweep READY → FAILED (SeqCst)`). Either the client sees the non-`HEALTHY` gate and cancels, or the sweep sees the `READY` slot and fails it ([shm-abi §6.3](01-shm-abi.md#63-supervisor-sweep)).
- **A caller whose request is already `PROCESSING` keeps waiting when the gate changes.** Either the worker is alive (e.g. during `PSI_SHEDDING` or a planned reload) and about to finish the request, or the worker died and the sweep will transition the slot to `FAILED` within microseconds. Only a request still sitting in `READY` is cancelled by the client.

### 3.3 Warm or paused worker: waiting on `standby_wait`

A worker that is `WARM` and waiting for promotion, or that has been paused by `DRAIN`, waits on `standby_wait` in the control segment. Because the worker maps the control segment read-only, `standby_wait` is a monotonic **event counter** incremented exclusively by the supervisor:

```
waiter:
    loop:
        v = standby_wait.load(SeqCst)
        if my condition holds: break         // gate.worker_gen == my generation, or DRAIN is no longer set
        futex_wait(&standby_wait, v, timeout = none)

supervisor, after any SeqCst store to gate.worker_gen or ctl_req ("bump and wake"):
    standby_wait.fetch_add(1, SeqCst)
    futex_wake(&standby_wait, INT_MAX)       // wakes every waiting worker
```

Because the waiter reads `standby_wait` *before* checking its condition, any supervisor update that happens after the read increments `standby_wait` and causes `futex_wait` to return `EAGAIN` immediately. `FUTEX_WAIT` on a read-only (`PROT_READ`) `MAP_SHARED` mapping is supported by the Linux kernel and verified in [`spikes/s1-memfd-across-containers/`](../../spikes/s1-memfd-across-containers/FINDING.md) with a median wakeup latency of **2.58 µs**.

---

## 4. Adaptive spin budget

Spinning trades CPU time for sub-microsecond pickup latency. Both the worker and the SDKs adapt their spin budgets automatically.

### What `spin_ns` controls

| `spin_ns` | Spinning regime begins at | CPU trade-off |
|---|---|---|
| `0` | Never; always park immediately | Minimum CPU; every request pays `futex` wakeup |
| `50 µs` (default) | ~20,000 requests/s per ring | Worker core spins across inter-arrival gaps < 50 µs |
| `1 ms` | ~1,000 requests/s per ring | Worker core stays spinning under moderate load |

### Worker adaptive spin

Each worker consume thread maintains an exponentially weighted moving average (EWMA) of request inter-arrival time:

| Condition | Effective worker spin budget |
|---|---|
| Inter-arrival EWMA ≤ `spin_ns` | Full `spin_ns` |
| Inter-arrival EWMA > `spin_ns` | Decays toward zero; parks immediately |
| `spin_ceiling_ns` in control header < `spin_ns` | Capped at `spin_ceiling_ns` |

The supervisor monitors `nr_throttled` in the sidecar's `cpu.stat`. Whenever CFS throttling advances, the supervisor halves `spin_ceiling_ns` down to zero, and restores it step by step once throttling ceases.

### Client adaptive spin

Each SDK tracks an EWMA of observed service time (`t_done_ns − t_enqueue_ns`), capped by a process-wide ceiling maintained by the SDK's background thread:

| Condition | Client spin behavior |
|---|---|
| Service-time EWMA ≤ ceiling | Spins up to `min(ceiling, 2 × EWMA)`, then parks on `slot.word` |
| Service-time EWMA > ceiling (heavy model / GGUF) | Skips spinning and parks immediately |

The SDK's background thread lowers the process-wide client spin ceiling automatically in two situations:

1. **More active calling threads than available CPUs:** once per second the background thread counts calling threads active within the last second and scales the spin ceiling down proportionally when callers outnumber CPUs.
2. **Host container CFS throttling:** once per second the background thread reads `/sys/fs/cgroup/cpu.stat` (where present) and halves the ceiling whenever `nr_throttled` advances, restoring it gradually when unthrottled.

---

## 5. Syscall interface

| Operation | Linux system call |
|---|---|
| Wait with deadline | `futex(addr, FUTEX_WAIT_BITSET, expected, &abs_deadline, NULL, FUTEX_BITSET_MATCH_ANY)` (`CLOCK_MONOTONIC`) |
| Wait without deadline | `futex(addr, FUTEX_WAIT, expected, NULL)` |
| Wake | `futex(addr, FUTEX_WAKE, n)` |
| Spin hint | `PAUSE` (`x86_64`) / `YIELD` (`aarch64`) |

All `futex` calls omit `FUTEX_PRIVATE_FLAG` so the kernel keys wait queues by shared inode and offset.

How each SDK invokes `futex`:

| SDK | Mechanism | Runtime interaction |
|---|---|---|
| Rust, C++20 | `syscall(SYS_futex, …)` directly | Direct native thread block |
| Go | `syscall.Syscall6(SYS_FUTEX, …)` | Enters `entersyscallblock` so the Go scheduler detaches `P` from `M` ([sdk-go](09-sdk-go.md)) |
| Java 22+ | FFM downcall handle to `syscall` | Bounds concurrent carrier-pinned parks on virtual threads via semaphore ([sdk-java](07-sdk-java-ffm.md)) |
| Python | Rust binding inside `py.allow_threads` | Releases the Python GIL before entering `infer` ([sdk-python](11-sdk-python.md)) |

---

## 6. Failure interactions and observability

| Event | Effect on waiters |
|---|---|
| Worker killed while a client spins | If the request is still `READY`, the client sees the gate flip and cancels (`READY → FREE`) immediately. If `PROCESSING`, the client sees the sweep mark the slot `FAILED` |
| Worker killed while a client is parked | Supervisor sweep marks the slot `FAILED` and calls `futex_wake`; worst case the futex timeout fires at the call deadline |
| Worker dies just as a client publishes | Either the client's post-publish gate check or the supervisor's sweep catches the slot ([§3.2](#32-client-side-waiting-for-a-result)) |
| Supervisor and worker both killed | Parked clients wake at their absolute `futex` deadline; subsequent calls see the stale supervisor heartbeat and fall back at admission |
| A waker is preempted after `PARKED → WAKING` | The next producer sees `WAKING` and issues `futex_wake`; the ring does not stall |
| A lone producer stalls between publishing and waking | The worker's 5 ms heartbeat thread wakes the consume thread within `hb_interval_ns` |

### Detecting slow or late wake-ups

Because every wait has a backstop (the 5 ms heartbeat for workers and the per-call deadline for clients), a bug that lost wake-ups would not hang forever — it would manifest as unexplained tail latency or deadline fallbacks. Two dedicated counters expose any lost wake-up immediately:

| Counter | Definition |
|---|---|
| `scz_worker_slow_wakes_total` | Incremented when a parked worker consume thread wakes and finds its head request has been queued (`now − t_enqueue_ns`) for > 1 ms |
| `scz_client_late_wakes_total` | Incremented when a client `futex` wait times out and then finds the slot already `DONE` with `now − t_done_ns` > 1 ms |

Both counters are evaluated only off the fast path (after a park or timeout) and remain strictly zero under normal operation ([testing §3](14-testing-verification.md#universal-assertions)).
