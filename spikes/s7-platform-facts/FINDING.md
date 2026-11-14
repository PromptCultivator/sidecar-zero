# S7 — Kernel & Concurrency Platform Invariants

[← Findings Summary](../FINDINGS.md)

## Objective

Empirically verify five Linux kernel and Rust model-checking invariants relied upon by SidecarZero:

1. **`SOCK_SEQPACKET` half-close (`shutdown(SHUT_WR)`):** Does half-closing the supervisor's sending side of the session socket deliver immediate EOF (`POLLIN | POLLRDHUP`, `recv == 0`) to the SDK while allowing the supervisor to detect when the SDK subsequently closes its descriptor?
2. **Thread liveness detection via `tgkill(pid, tid, 0)`:** Can the SDK's arena retirement loop reliably distinguish a live thread from an exited thread using its kernel `tid`?
3. **`clone3` under container `RuntimeDefault` seccomp:** Do Docker and `containerd` default seccomp profiles return `ENOSYS` for `clone3` while permitting `clone(SIGCHLD | CLONE_PIDFD)`?
4. **Loom store-buffering fence semantics:** How does `loom` (`0.7.3`) evaluate store-then-load synchronization pairs written with `Ordering::SeqCst` loads/stores versus `Release`/`Acquire` paired with an explicit `atomic::fence(Ordering::SeqCst)`?
5. **Model file `ctime` vs `mtime` across atomic file replacements:** How do `unlink`, `rename`, and Kubernetes ConfigMap/Secret symlink swaps affect an open file descriptor's `st_ctime` vs `st_mtime`?

---

## Measured Results (`results/*.txt`)

| # | Invariant Under Test | Empirical Result | Evidence File |
|---|---|---|---|
| 1 | `SOCK_SEQPACKET` half-close handshake for live session resets | **Verified.** Calling `shutdown(fd, SHUT_WR)` on the supervisor's socket causes the client's `poll` to wake immediately with `POLLIN \| POLLRDHUP` and `recv` to return `0`. The supervisor's descriptor stays quiet while the client drains in-flight calls and unmaps the arena, then receives `POLLHUP` and `recv == 0` at the exact instant the client calls `close(fd)`. | `results/halfclose.txt` |
| 2 | Thread liveness via `tgkill(getpid(), tid, 0)` | **Verified.** Returns `0` while the thread is alive (0 false negatives across 1,000,000 checks) and returns `-1` (`errno == ESRCH`) immediately once the thread exits, including when the thread exits via raw `_exit` syscall without stack unwinding. | `results/thread_liveness.txt` |
| 3 | `clone3` vs `clone(SIGCHLD \| CLONE_PIDFD)` under `RuntimeDefault` seccomp | **Verified.** Both Docker's default seccomp filter and `containerd`'s Kubernetes `RuntimeDefault` filter reject `clone3` with `ENOSYS` and allow `clone(SIGCHLD \| CLONE_PIDFD)`. | `results/clone3-*.txt` |
| 4 | `Loom` model checking of store-then-load wake protocols | **Verified.** `Release` store + `atomic::fence(Ordering::SeqCst)` + `Acquire` load passes all interleavings; omitting the fence is caught as a lost-wake bug; using `Ordering::SeqCst` stores/loads *without* an explicit `fence(Ordering::SeqCst)` is falsely rejected by `loom` because `loom` models `SeqCst` operations as `Acquire`/`Release`. | `results/loom.txt` |
| 5 | Open FD metadata across `unlink` and atomic `rename` on `ext4`, `tmpfs`, and `overlayfs` | **Verified.** Unlinking a file or renaming a new file over its path updates `st_ctime` on the already-open file descriptor while leaving `(st_dev, st_ino, st_size, st_mtime)` unchanged across `ext4`, `tmpfs`, and `overlayfs`. | `results/ctime.txt` |

---

## Architectural Conclusions

1. **Zero-restart session resets ([04 — Supervisor §4](../../docs/components/04-supervisor.md#4-handshake-and-session-management)):** The supervisor initiates a live session reset by half-closing the session socket (`shutdown(SHUT_WR)`) and waiting for the client's final `close()` before re-initializing the arena.
2. **Orphaned in-call marker reclamation ([06 — SDK Contract §7](../../docs/components/06-sdk-contract.md#7-client-side-arena-retirement)):** Native SDKs (`Rust`, `C++20`) pack `(snapshot_id << 32) | kernel_tid` into each `CallerEntry.in_call` marker and verify thread liveness via `tgkill(pid, tid, 0) == ESRCH` so a thread that died mid-call never pins a retired arena mapping.
3. **Explicit `SeqCst` fences ([01 — Shared-Memory ABI §4.3](../../docs/components/01-shm-abi.md#43-enqueue-producer)):** All four store-then-load synchronization pairs in `scz-shm` use `Release` store → `atomic::fence(Ordering::SeqCst)` → `Acquire` load, satisfying both hardware store-buffer ordering (`x86-64` and `aarch64`) and `loom` exhaustive verification.
4. **Rollback-safe model identity check ([04 — Supervisor §5.1](../../docs/components/04-supervisor.md#51-spawning)):** The supervisor verifies `(st_dev, st_ino, st_size, st_mtime)` and SHA-256 rather than `st_ctime` so an open descriptor to a replaced model file can be cleanly rolled back (`#34`).
