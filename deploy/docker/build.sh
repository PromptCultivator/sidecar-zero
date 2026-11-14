#!/usr/bin/env bash
# Build the images of deploy/docker/Dockerfile for this machine's architecture.
#
#   deploy/docker/build.sh [TAG]        default TAG: dev
#
# The binaries are built in the pinned dev image (scripts/dev), with the release profile and
# without debug information: the supervisor, sczctl, and the demo host for musl (static), the
# workers for glibc; the Python runner's runtime is staged from the dev image's own packages
# (deploy/docker/stage-python.sh). They are staged in deploy/docker/.stage and copied into distroless bases.
# Multi-arch builds, signing, and SBOMs are release work (kubernetes-deploy §2).
set -euo pipefail
cd "$(dirname "$0")/../.."
TAG=${1:-dev}
# Already inside the dev image (the benchmark calls this from there): build directly.
dev() { if [ "${SCZ_IN_DEV:-}" = 1 ]; then "$@"; else scripts/dev "$@"; fi; }
dev bash -c '
set -euo pipefail
T=$(uname -m)-unknown-linux-musl
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=true
cargo build -q --release --target "$T" -p scz-supervisor -p scz-ctl -p scz-demo-host
cargo build -q --release -p scz-worker-ort -p scz-worker-core -p scz-worker-gguf --bins
S=deploy/docker/.stage
rm -rf "$S"; mkdir -p "$S/ort"
cp "target/$T/release/scz-supervisor" "$S/sidecarzero-supervisor"
cp "target/$T/release/sczctl" "target/$T/release/scz-demo-host" "$S/"
cp target/release/sidecarzero-worker target/release/scz-worker-chaos target/release/sidecarzero-worker-gguf "$S/"
# The Python runner: the SDK, then a runtime staged around it, and the NumPy test model.
sdks/python/build.sh
deploy/docker/stage-python.sh "$S/python"
mkdir -p "$S/demo"; cp sdks/python/tests/models/numpy_model.py "$S/demo/"
/opt/py/bin/python3 -c "import numpy as np; np.save(\"$S/demo/model.npy\", np.array([1.0, 0.25], np.float32))"
# The runtime under the soname the worker asks for.
so=$(readelf -d "$S/sidecarzero-worker" | sed -n "s/.*NEEDED.*\[\(libonnxruntime[^]]*\)\]/\1/p")
cp -L "$ORT_DIR/lib/$so" "$S/ort/$so"
python3 models/gen.py target/models
mkdir -p "$S/models"; cp target/models/fraud.ort "$S/models/"
cp "${GGUF_MODEL:-/opt/models/bge-small-f16.gguf}" "$S/models/bge-small-f16.gguf"
ls -l "$S" "$S/ort" | sed "s/^/  /"
'
for t in ort ort-fraud chaos gguf gguf-bge python python-demo demo-host; do
  name=$([ "$t" = demo-host ] && echo scz-demo-host || echo "sidecarzero-$t")
  docker build -q --target "$t" -t "$name:$TAG" -f deploy/docker/Dockerfile deploy/docker >/dev/null
  printf '%-28s %s\n' "$name:$TAG" "$(docker image inspect -f '{{.Size}}' "$name:$TAG" | awk '{printf "%.1f MB", $1/1e6}')"
done
