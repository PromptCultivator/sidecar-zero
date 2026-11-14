#!/usr/bin/env bash
# The Python SDK's own tests (run inside the container: scripts/dev), after build.sh:
#
#   1. unit tests without a sidecar: construction, fingerprint golden vectors, the codecs'
#      input checks through the in-process double, native fallbacks
#   2. tests against a real supervisor (target/stable/bin): keyed answers, threads and a new
#      thread per call, the GIL released while waiting, deadline, close with calls in flight,
#      stale heartbeat, fork, codec errors; the fraud model through the dense codec
#   3. the worker runner (python -m sidecarzero.worker) behind a real supervisor, with a NumPy
#      model: keyed answers from the Python client and the Rust agent, MODEL_ERROR for raises
#      and mismatched answers, a hang killed by the watchdog, kill -9, a leak under
#      RLIMIT_DATA, the sandbox (no network socket, socket pairs only when allowed, Landlock)
#   4. the allocation gate (testing §7): allocated-block count flat over a million calls on
#      the model path and on the fallback path, and over fewer on the slower paths
#   5. the wheel (wheel.sh): installed into a fresh environment without NumPy, and used
#
# Non-zero on failure.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$ROOT/target/sdk/python
PY=${SCZ_PYTHON:-/opt/py/bin/python3}
[ -f "$OUT/sidecarzero/_core.abi3.so" ] || { echo "test.sh: run sdks/python/build.sh first" >&2; exit 1; }

# The runner tests need a supervisor with --worker-arg, --worker-read and --worker-socketpair:
# the snapshot's if it has them, else one built from the workspace's source into a target
# directory of its own (so as not to wait on the workspace's build lock).
probe=$("$ROOT/target/stable/bin/scz-supervisor" --worker-socketpair 2>&1 || true)
if [ -z "${SCZ_RUNNER_BINS:-}" ] && [[ $probe == *"unknown argument"* ]]; then
  CARGO_TARGET_DIR=$ROOT/target/python-sup CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} nice -n 10 \
    cargo build --release --quiet -p scz-supervisor -p scz-ctl --manifest-path "$ROOT/Cargo.toml"
fi

# The core is tested through Python only: a Rust test binary would have to link libpython,
# which the image's interpreter does not ship (it is statically linked).
export PYTHONPATH="$OUT:$HERE/tests" PYTHONDONTWRITEBYTECODE=1 OPENBLAS_NUM_THREADS=1 OMP_NUM_THREADS=1
cd "$HERE/tests"
nice -n 10 "$PY" -m unittest -q test_unit test_sidecar test_runner
nice -n 10 "$PY" alloc_gate.py
# 5. the wheel: packed, installed into an environment of its own, and called from there
"$HERE/wheel.sh"
