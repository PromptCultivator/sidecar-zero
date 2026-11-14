#!/usr/bin/env bash
# Start Triton for the benchmark's system B (bench §3) and wait until it serves gRPC.
#
#   bench/baselines/triton/run.sh [options] MODELS_DIR
#
# MODELS_DIR is a host directory holding the generated onnx/fraud.onnx (models/gen.py's layout).
# The model repository is made from repository/ in this directory: the config with the instance
# count set, and the ONNX file as fraud/1/model.onnx.
#
#   --name NAME          container name (default scz-triton)
#   --instances N        model instances, one ONNX Runtime thread each (default 1)
#   --supervised         run tritonserver under a restart loop, so killing it does not end the
#                        container (bench §3, fairness rule 5)
#   --pod                keep the network and IPC namespaces in a holder container, NAME-ns, as a
#                        Kubernetes pod's sandbox does, and run Triton in them. Needed when the
#                        Triton container is to restart (--restart): a restarted container gets a
#                        new network and /dev/shm unless something else holds them
#   --repository DIR     where the repository is made (default .repository/ next to this script)
#   --cpus N, --cpuset-cpus LIST, --memory SIZE, --restart POLICY
#                        passed to docker run: the serving container's limits (fairness rule 3)
#
# The image is TRITON_IMAGE, by default the pinned 26.04 digest below. The client joins the
# namespaces' owner, NAME (or NAME-ns with --pod): docker run --ipc=container:NAME
# --network=container:NAME ..., then reaches gRPC at http://127.0.0.1:8001. See README.md.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
IMAGE=${TRITON_IMAGE:-nvcr.io/nvidia/tritonserver@sha256:aa5fca20416939f243d587d56557d15f9d2bb81de30c6a4464707225250320fb}
NAME=scz-triton
INSTANCES=1
SUPERVISED=0
POD=0
REPO=$HERE/.repository
DOCKER_OPTS=()
MODELS=
usage() { sed -n '2,/^set -e/{/^set -e/d;s/^# \{0,1\}//;p;}' "$0"; exit "${1:-0}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --name) NAME=$2; shift 2 ;;
    --instances) INSTANCES=$2; shift 2 ;;
    --supervised) SUPERVISED=1; shift ;;
    --pod) POD=1; shift ;;
    --repository) REPO=$2; shift 2 ;;
    --cpus|--cpuset-cpus|--memory|--restart) DOCKER_OPTS+=("$1=$2"); shift 2 ;;
    -h|--help) usage ;;
    -*) echo "run.sh: unknown option $1" >&2; usage 2 >&2 ;;
    *) [ -z "$MODELS" ] || usage 2 >&2; MODELS=$1; shift ;;
  esac
done
[ -n "$MODELS" ] || usage 2 >&2
[ -f "$MODELS/onnx/fraud.onnx" ] || { echo "run.sh: no $MODELS/onnx/fraud.onnx (python3 models/gen.py writes it)" >&2; exit 1; }
[[ "$INSTANCES" =~ ^[1-9][0-9]*$ ]] || { echo "run.sh: --instances takes a positive integer" >&2; exit 2; }

# The repository: this directory's template, with the instance count, and the model file.
rm -rf "$REPO/fraud"
mkdir -p "$REPO/fraud/1"
sed -E "s/^( *count: )1( # run.sh --instances)$/\1$INSTANCES\2/" "$HERE/repository/fraud/config.pbtxt" >"$REPO/fraud/config.pbtxt"
grep -q "count: $INSTANCES # run.sh --instances" "$REPO/fraud/config.pbtxt" || { echo "run.sh: the template's instance count line is missing" >&2; exit 1; }
cp "$MODELS/onnx/fraud.onnx" "$REPO/fraud/1/model.onnx"
REPO=$(cd "$REPO" && pwd)

SERVER=(tritonserver
  --model-repository=/models
  --disable-auto-complete-config
  --allow-grpc=true --grpc-port=8001
  --allow-http=false
  --allow-metrics=false
  --allow-client-shm=true
  --pinned-memory-pool-byte-size=0)
if [ "$SUPERVISED" = 1 ]; then
  # Restart tritonserver whenever it exits, with no delay. bash is PID 1; the trap only makes
  # docker stop end the loop instead of waiting out its timeout.
  CMD=(bash -c 'trap "kill -TERM \$pid 2>/dev/null; wait \$pid; exit 0" TERM INT
while :; do "$@" & pid=$!; wait $pid; echo "supervisor: tritonserver exited with status $?; restarting" >&2; done' supervise "${SERVER[@]}")
else
  # tritonserver is PID 1 (the image's entrypoint execs it): killing it ends the container.
  CMD=("${SERVER[@]}")
fi

docker rm -f "$NAME" >/dev/null 2>&1 || true
# --ipc=shareable: the client joins this IPC namespace, so both see the same /dev/shm, where the
# client's regions live; and this network namespace, for 127.0.0.1:8001.
OWNER=$NAME
NAMESPACES=(--ipc=shareable)
if [ "$POD" = 1 ]; then
  OWNER=$NAME-ns
  if [ "$(docker inspect -f '{{.State.Running}}' "$OWNER" 2>/dev/null)" != true ]; then
    docker rm -f "$OWNER" >/dev/null 2>&1 || true
    docker run -d --name "$OWNER" --ipc=shareable --entrypoint sleep "$IMAGE" infinity >/dev/null
  fi
  NAMESPACES=(--ipc=container:"$OWNER" --network=container:"$OWNER")
fi
# TRITON_REPO_HOST: the repository's path as the Docker daemon sees it, when this script runs
# in a container of its own (the benchmark runs it in the dev image).
docker run -d --name "$NAME" "${NAMESPACES[@]}" ${DOCKER_OPTS[@]+"${DOCKER_OPTS[@]}"} \
  -v "${TRITON_REPO_HOST:-$REPO}:/models:ro" "$IMAGE" "${CMD[@]}" >/dev/null

for _ in $(seq 600); do
  if docker logs "$NAME" 2>&1 | grep -q 'Started GRPCInferenceService'; then
    echo "$NAME: serving fraud ($INSTANCES instance(s)$([ "$SUPERVISED" = 1 ] && echo ', supervised')) at 127.0.0.1:8001; clients join $OWNER"
    exit 0
  fi
  if [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" != true ]; then
    docker logs "$NAME" 2>&1 | tail -30 >&2
    echo "run.sh: $NAME exited before serving" >&2
    exit 1
  fi
  sleep 0.5
done
docker logs "$NAME" 2>&1 | tail -30 >&2
echo "run.sh: $NAME did not start serving within 300 s" >&2
exit 1
