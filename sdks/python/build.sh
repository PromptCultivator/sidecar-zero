#!/usr/bin/env bash
# Build the Python SDK into target/sdk/python (run inside the container: scripts/dev).
#
#   target/sdk/python/sidecarzero/        the package: the Python sources and _core.abi3.so
#   target/sdk/python/scz_agent.py        the conformance agent
#   target/sdk/python/scz-agent-python    its launcher (python3 with PYTHONPATH set)
#
# The native core is a PyO3 cdylib over the Rust client, built with cargo (no maturin) into
# its own target directory, so it never waits on the workspace's build lock. Idempotent;
# quiet on success.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$ROOT/target/sdk/python
PY=${SCZ_PYTHON:-/opt/py/bin/python3}

export CARGO_TARGET_DIR=$ROOT/target/python
export PYO3_PYTHON=$PY PYO3_BUILD_EXTENSION_MODULE=1
CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} nice -n 10 cargo build --release --quiet --manifest-path "$HERE/Cargo.toml"
CARGO_TARGET_DIR=$ROOT/target/python-crash CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} nice -n 10 cargo build --release --quiet --features crashpoints --manifest-path "$HERE/Cargo.toml"

# Replace, never overwrite in place: a running agent may have the old library mapped.
install_file() { cp "$1" "$2.tmp.$$" && mv -f "$2.tmp.$$" "$2"; }
stage_agent() {
  local so=$1 dest=$2
  mkdir -p "$dest/sidecarzero"
  install_file "$so" "$dest/sidecarzero/_core.abi3.so"
  (cd "$HERE/python" && find sidecarzero -type d) | while read -r d; do mkdir -p "$dest/$d"; done
  (cd "$HERE/python" && find sidecarzero -type f) | while read -r f; do install_file "$HERE/python/$f" "$dest/$f"; done
  install_file "$HERE/agent/scz_agent.py" "$dest/scz_agent.py"

  cat > "$dest/scz-agent-python.tmp.$$" <<LAUNCHER
#!/bin/sh
# The Python SDK's conformance agent (sdks/python/agent/scz_agent.py), built by build.sh.
here=\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)
# One BLAS thread: NumPy's pool would otherwise add threads to the agent's PROC counts.
export OPENBLAS_NUM_THREADS=1 OMP_NUM_THREADS=1 MKL_NUM_THREADS=1
PYTHONPATH="\$here\${PYTHONPATH:+:\$PYTHONPATH}" exec "\${SCZ_PYTHON:-$PY}" -B "\$here/scz_agent.py" "\$@"
LAUNCHER
  chmod +x "$dest/scz-agent-python.tmp.$$"
  mv -f "$dest/scz-agent-python.tmp.$$" "$dest/scz-agent-python"
}
stage_agent "$ROOT/target/python/release/lib_core.so" "$OUT"
stage_agent "$ROOT/target/python-crash/release/lib_core.so" "$OUT/crash"
