# Formal Protocol Specifications (`spec/`)

[← 14 — Testing and Verification §2.1](../docs/components/14-testing-verification.md#21-formal-specification-in-pluscal--tla-spec)

PlusCal / TLA+ formal specifications of the SidecarZero shared-memory protocol, model-checked exhaustively with TLC. These specifications verify the slot lifecycle ([01 — Shared-Memory ABI §6](../docs/components/01-shm-abi.md#6-slot-lifecycle)), adaptive spin-then-wake signaling ([02 — Signaling §3](../docs/components/02-signaling.md#3-the-wait-points)), and worker lifecycle and handover state machines ([04 — Supervisor §5](../docs/components/04-supervisor.md#5-worker-lifecycle)) under sequentially consistent memory, while `loom` models verify weak-memory ordering on the Rust implementation ([14 — Testing and Verification §2.2](../docs/components/14-testing-verification.md#22-exhaustive-rust-concurrency-model-checking-loom)).

| File | Specification | Scope |
|---|---|---|
| [`SlotsRing.tla`](SlotsRing.tla) | Spec A | Two client threads executing the full call path, the SDK background session thread, one ring consumer at a time (plus a successor after crash), and the supervisor's gate, slot sweep, ring repair, and session reset. Models arbitrary worker crashes, host process exits, permanently stalled client threads, supervisor-initiated session resets, and producer unwinds |
| [`Consumers.tla`](Consumers.tla) | Spec B | Active worker, warm standby / reload successor, replacement spawning, and the supervisor with ranked gate holds, `HANDOVER`, status-pipe reports, per-thread death locks, and `pidfd` exit notifications across crash promotion, same-schema handover, and changed-schema reload |

```bash
scripts/tla-check     # Model-check all 14 safety and liveness configurations
scripts/tla-mutants   # Verify TLC catches all 13 seeded protocol mutations
```

---

## Verified Safety and Liveness Properties

| Spec | Property ([14 — Testing §2.1](../docs/components/14-testing-verification.md#21-formal-specification-in-pluscal--tla-spec)) | TLA+ Invariant / Property | Checked Configurations |
|---|---|---|---|
| A | A slot's data regions have at most one owner at any time | `OneOwner` | All safety configs |
| A | A descriptor from epoch `e` never causes processing of the slot at epoch `e′ ≠ e` | Assertion in `w_claim` | All safety configs |
| A | No call resolves to more than one outcome; a delivered answer matches that call | `OneOutcome`, assertions in `c_read` | All safety configs |
| A | Every call resolves provided its own calling thread continues stepping | `CallsResolve` | `-live`, `-live-crash`, `-live-session` |
| A | No sleeper is stranded: a parked thread whose wake condition holds has a wake-up pending | `ClientNotStranded`, `ConsumerNotStranded`, `ConsumerWoken` | All configs |
| A | After a worker crash at any step, repair and sweep leave the ring usable | `RingUsable` | `SlotsRing`, `-full`, `-spin` |
| A | After worker exit and sweep, no slot remains owned by that worker | `OwnedSwept`, `OwnerMeaningful` | All safety configs |
| A | A request published around a worker death is resolved by the gate re-check or by the sweep | `NoLateQueued`, `NoOrphan` | All safety configs |
| A | After host exit and session reset, every slot is `FREE` and every ring is empty and usable | `ResetClean` | `-session` |
| A | A session reset never runs while a client thread is inside the arena | `NoResetUnderClient` | `-session` |
| A | A producer that stops forever after claiming a cell does not stall its ring forever | `StuckHeadClears` | `-live-session` |
| A | Every slot returns to `FREE` once its call has resolved | `AbandonedFreed` | `-live`, `-live-crash`, `-live-session` |
| B | At most one worker consumes from a ring, and no slot can be written by two parties | `OneConsumer`, `OneWriter`, `BeatsDistinct` | `crash`, `same`, `changed` |
| B | Ring repair runs only after its consume thread's death lock; `PROCESSING` slots are failed only after process exit | Assertion in `s_d_repair`, `OneWriter` | `crash`, `same`, `changed` |
| B | A clean handover loses no queued request and serves none twice | `QueuedServed`, `ServedOnce` | `clean` |
| B | The gate reads `HEALTHY` only when no active recovery path holds it closed | `HealthyOnlyWhenOpen`, `GateFollowsHolds` | `crash`, `changed` |
| B | Every handover terminates (either completing or transitioning to crash recovery) and the gate eventually reopens | `HandoverEnds`, `GateReopens` | `live-*` |

### Key Protocol Invariants Enforced by the Specifications

- **Waking `DONE` slots during supervisor sweep (`ClientNotStranded`):** When a worker completes a slot (`CAS(PROCESSING → DONE)`), it clears the `WAITER` bit before calling `futex(WAKE)`. If the worker crashes in the instruction window between clearing `WAITER` and executing the `futex(WAKE)` syscall, a parked caller would remain asleep until its deadline unless the supervisor sweep also wakes parked callers on `DONE` slots (`slot::wake_if_done`, [01 — Shared-Memory ABI §6.3](../docs/components/01-shm-abi.md#63-supervisor-sweep)).
- **Applying the waker rule on void-cell publish (`ConsumerNotStranded`):** When a producer unwinds between claiming a ring cell and publishing a valid descriptor, its RAII scope guard publishes `SLOT_VOID` (`seq = pos + 1`) and executes the standard `RingWakeWord` check ([01 — Shared-Memory ABI §4.3](../docs/components/01-shm-abi.md#43-enqueue-producer)) so a consumer that just parked is woken immediately.

---

## Seeded Protocol Mutations (`scripts/tla-mutants`)

Each row enables a targeted protocol fault via the specification's `Bug` constant; `scripts/tla-mutants` verifies that TLC detects the violation and reports a counterexample trace:

| `Bug` Constant | Injected Protocol Fault | Violated Property |
|---|---|---|
| `no_recheck` | Client omits post-enqueue gate re-check | `NoLateQueued` |
| `sweep_before_gate` | Supervisor sweeps slots before closing the gate | `NoLateQueued` |
| `no_done_wake` | Supervisor sweep omits `futex(WAKE)` on `DONE` slots | `ClientNotStranded` |
| `no_complete_wake` | Worker omits `futex(WAKE)` when completing a slot with `WAITER == 1` | `ClientNotStranded` |
| `abandon_drops_owner` | Client `ABANDONED` transition clears `OWNER` | `OwnerMeaningful` |
| `repair_equality` | Ring repair tests `seq == head + capacity` instead of wrapping `<` | `RingUsable` |
| `ignore_markers` | SDK unmaps arena without waiting for `CallerEntry` in-call markers to clear | `NoResetUnderClient` |
| `void_no_wake` | Producer unwind guard publishes `SLOT_VOID` without running the waker rule | `ConsumerNotStranded` |
| `no_stuck_reset` | Supervisor never triggers a session reset when a ring head cell is stuck | `StuckHeadClears` |
| `fail_owned_on_lock` | Supervisor sweeps `PROCESSING` slots on the death lock before process `pidfd` exit | `OneWriter` |
| `promote_on_kill` | Standby is promoted upon `SIGKILL` before consume-thread death locks report | `OneConsumer` |
| `store_healthy` | A recovery path writes `HEALTHY` directly instead of releasing its ranked gate hold | `HealthyOnlyWhenOpen` |
| `handover_no_release` | Supervisor increments `worker_gen` without waiting for old worker `RELEASED` report | `OneConsumer` |

---

## TLC Model-Checking State Space and Runtimes

Measured with TLC across all 14 configurations:

| Configuration | Scenario Modeled | Distinct States | Verification Time |
|---|---|---|---|
| `SlotsRing` | Worker crash at any step; two client threads, one call each | 3,873,158 | 77 s |
| `SlotsRing-full` | One client thread, three calls (ring fills and wraps); worker crash | 6,095,992 | 57 s |
| `SlotsRing-spin` | Spinning clients checking gate during wait; worker crash | 7,008,509 | 187 s |
| `SlotsRing-session` | Host death, killed thread, supervisor-requested session reset, unwinding producer | 19,046,602 | 661 s |
| `SlotsRing-live` | Liveness verification, fault-free | 473,640 | 147 s |
| `SlotsRing-live-crash` | Liveness verification with worker crash; one thread, two calls | 754,674 | 136 s |
| `SlotsRing-live-session` | Liveness verification with producer killed after claiming a ring cell | 3,680 | 3 s |
| `Consumers-crash` | Standby promotion across two worker failures and memory pressure | 16,802,740 | 256 s |
| `Consumers-same` | Same-schema cooperative handover across two worker failures | 42,489,984 | 250 s |
| `Consumers-changed` | Changed-schema reload with worker failure and memory pressure | 6,260,216 | 87 s |
| `Consumers-clean` | Fault-free same-schema cooperative handover with waiting clients | 7,454 | 4 s |
| `Consumers-live-crash` | Liveness verification: standby promotion across two worker failures | 353,888 | 37 s |
| `Consumers-live-same` | Liveness verification: same-schema handover across two worker failures | 2,933,892 | 192 s |
| `Consumers-live-changed` | Liveness verification: changed-schema reload across two worker failures | 3,480,236 | 183 s |
