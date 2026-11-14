#!/usr/bin/env bash
# The end-to-end smoke test of system B on this machine (README.md), in two stages:
#
#  1. Triton started by run.sh --supervised; bench/triton-client/examples/smoke.rs in the dev
#     image, joined to Triton's IPC, network and PID namespaces: reference rows, latency of
#     sequential calls, concurrent clients, a freeze and a kill of tritonserver, drop.
#  2. Triton unsupervised, restarted by Docker (run.sh --pod --restart always); the client joined
#     to the holder's namespaces awaits an outage, and tritonserver, PID 1 of its container, is
#     killed from the VM's PID namespace as the node's kill agent would kill it.
#
# Removes every container it started when it ends.
#
#   bench/baselines/triton/smoke.sh [smoke options for stage 1: --calls N, --timeout-ms MS]
set -euo pipefail
cd "$(dirname "$0")/../../.."
HERE=bench/baselines/triton
NAME=${NAME:-scz-triton-smoke}
cleanup() { docker rm -f "$NAME" "$NAME-pod" "$NAME-pod-ns" "$NAME-client" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# The dev image, named as scripts/dev names it (which builds it when it is missing).
scripts/dev true
IMAGE=${SCZ_DEV_IMAGE:-scz-dev:1.99-$( (shasum -a 256 2>/dev/null || sha256sum) <scripts/dev.Dockerfile | cut -c1-12)}

# The models land in the dev container's target volume; Triton gets a host-visible copy of the
# ONNX file. Then the smoke binary, also in the volume.
scripts/dev bash -c "python3 models/gen.py target/models && mkdir -p $HERE/.models/onnx && cp target/models/onnx/fraud.onnx $HERE/.models/onnx/"
scripts/dev cargo build -q --release -p scz-bench-triton --example smoke
CLIENT=(-v "$PWD:/w" -v scz-dev-target:/w/target -w /w)
SMOKE=(target/release/examples/smoke http://127.0.0.1:8001 target/models)

echo "=== 1. supervised Triton"
"$HERE/run.sh" --supervised --name "$NAME" "$HERE/.models"
docker run --rm "${CLIENT[@]}" --ipc=container:"$NAME" --network=container:"$NAME" --pid=container:"$NAME" \
  "$IMAGE" "${SMOKE[@]}" --chaos "$@"
echo "--- Triton's log: starts and restarts"
docker logs "$NAME" 2>&1 | grep -E 'Started GRPCInferenceService|supervisor:'
docker rm -f "$NAME" >/dev/null

echo "=== 2. unsupervised Triton, restarted by Docker, namespaces held by $NAME-pod-ns"
"$HERE/run.sh" --pod --restart always --name "$NAME-pod" "$HERE/.models"
docker run -d --name "$NAME-client" "${CLIENT[@]}" --ipc=container:"$NAME-pod-ns" --network=container:"$NAME-pod-ns" \
  "$IMAGE" "${SMOKE[@]}" --await-outage >/dev/null
for _ in $(seq 120); do
  docker logs "$NAME-client" 2>&1 | grep -q 'awaiting an outage' && break
  [ "$(docker inspect -f '{{.State.Running}}' "$NAME-client")" = true ] || break
  sleep 0.5
done
sleep 0.5
PID=$(docker inspect -f '{{.State.Pid}}' "$NAME-pod")
docker run --rm --pid=host "$IMAGE" kill -9 "$PID"
status=$(docker wait "$NAME-client")
docker logs "$NAME-client" 2>&1
echo "--- Triton's container: $(docker inspect -f 'restarted {{.RestartCount}} time(s), running={{.State.Running}}' "$NAME-pod")"
exit "$status"
