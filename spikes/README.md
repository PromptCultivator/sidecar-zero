# Kernel & Runtime Empirical Validation Suite (`spikes/`)

[← High-level design](../docs/high-level-design.md) · [Summary of Findings →](FINDINGS.md)

Self-contained kernel and runtime validation harnesses that verify the operating-system, container-runtime, shared-memory, and language-runtime invariants underlying SidecarZero's architecture.

## Directory Layout

```
spikes/
├── s1-memfd-across-containers/   Sealed memfd SCM_RIGHTS passing & cgroup v2 memory attribution
├── s2-oom-matrix/                Container OOM killer behavior across all four OOM shield modes
├── s3-java-ffm/                  JDK 22/25 Foreign Function & Memory zero-allocation & carrier pinning
├── s4-arrow-templates/           Cross-language Arrow IPC pre-stamped template interoperability
├── s5-ort-foreign-memory/        ONNX Runtime zero-copy slot I/O, memory profiles, & weight prepacking
├── s6-standby-promotion/         Warm standby promotion latency & pidfd teardown scaling vs RSS
├── s6b-death-signal/             Per-consume-thread robust death locks (musl supervisor ↔ glibc worker)
└── s7-platform-facts/            SOCK_SEQPACKET half-close, tgkill liveness, clone3 seccomp, & Loom fences
```

Each validation directory contains:

| File | Purpose |
|---|---|
| `run.sh` | Builds the container image(s) with Docker and executes the validation suite |
| `FINDING.md` | Experimental setup, measured tables, kernel/runtime behavior analysis, and architectural conclusions |
| `results/` | Raw output logs captured from the validation runs |

---

## Running the Validation Suite

`run-all.sh` executes every validation suite in order and packages the logs and raw results into an archive:

```bash
sudo ./run-all.sh --install   # Optional: installs Docker and curl on a fresh Linux host
./run-all.sh                 # Full validation run
./run-all.sh --quick         # Abbreviated run
./run-all.sh s6b s2          # Run specific validation suites
```

| Behavior | Detail |
|---|---|
| Isolated working copy | Copies the suite to `~/scz-spike-runs/<host>-<time>/` (override via `SCZ_RUNS`) |
| Self-contained tooling | Downloads pinned `kind` (`v0.33.0`) and `kubectl` binaries verified against upstream SHA-256 digests |
| Host environment capture | Records CPU topology, kernel version/cmdline, frequency governor, C-states, huge pages, and cgroup v2 configuration in `out/machine.txt` |
| Automatic cleanup | Deletes temporary `scz-*` Kind clusters upon completion or interrupt |

See [FINDINGS.md](FINDINGS.md) for the consolidated summary across all eight validation suites.
