#!/usr/bin/env bash
# S2: which OOM shield modes keep PID 1 alive when a worker exhausts the container's memory.
# Runs five pod shapes on a stock Kind cluster, then one on a cluster whose kubelet has
# singleProcessOOMKill turned on.
#   ./run.sh          both parts
#   ./run.sh stock    only the stock cluster
set -uo pipefail
cd "$(dirname "$0")"
mkdir -p results
KIND=${KIND:-../.tools/bin/kind}
[ -x "$KIND" ] || { echo "kind is not installed at $KIND; see ../README.md"; exit 1; }

cat > Dockerfile <<'EOF'
FROM gcc:14 AS build
COPY oomtest.c /src/
RUN gcc -O2 -Wall -static -o /oomtest /src/oomtest.c
FROM debian:stable-slim
COPY --from=build /oomtest /spike/oomtest
EOF
docker build -q -t scz-spike-s2 . >/dev/null || { echo "build failed"; exit 1; }

pod() { # name  args  securityContext(json)
  cat <<EOF
apiVersion: v1
kind: Pod
metadata: { name: scz-s2 }
spec:
  restartPolicy: Always
  terminationGracePeriodSeconds: 1
  containers:
    - name: sidecar
      image: scz-spike-s2
      imagePullPolicy: Never
      command: ["/spike/oomtest"$2]
      securityContext: $3
      resources:
        requests: { memory: 96Mi, cpu: 100m }
        limits: { memory: 96Mi }
EOF
}

RESTRICTED='{ "runAsUser": 1000, "runAsNonRoot": true, "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] }, "seccompProfile": { "type": "RuntimeDefault" } }'
NONROOT_CAP='{ "runAsUser": 1000, "runAsNonRoot": true, "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"], "add": ["SYS_RESOURCE"] }, "seccompProfile": { "type": "RuntimeDefault" } }'
ROOT_CAP='{ "runAsUser": 0, "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"], "add": ["SYS_RESOURCE"] }, "seccompProfile": { "type": "RuntimeDefault" } }'
PRIVILEGED='{ "runAsUser": 0, "privileged": true }'

case_run() { # label  args  securityContext
  local name=$1
  [ -n "${ONLY:-}" ] && [ "$ONLY" != "$name" ] && return
  echo "=================== $name"
  kubectl delete pod scz-s2 --ignore-not-found --wait=true >/dev/null 2>&1
  pod "$name" "$2" "$3" | kubectl apply -f - >/dev/null || return
  # Follow the first container instance from its start; if the kernel kills it, this is
  # the only complete record of what it printed.
  kubectl wait --for=condition=Ready pod/scz-s2 --timeout=60s >/dev/null 2>&1
  kubectl logs -f scz-s2 > "results/$name.first-instance.log" 2>&1 &
  local follower=$!
  # Three rounds take a few seconds when PID 1 survives. If it is killed, wait long
  # enough to see a restart or two.
  for i in $(seq 1 40); do
    kubectl logs scz-s2 2>/dev/null | grep -q '^RESULT' && break
    [ "$(kubectl get pod scz-s2 -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)" -ge 2 ] 2>/dev/null && break
    sleep 1
  done
  kill $follower 2>/dev/null; wait $follower 2>/dev/null
  {
    echo "restarts: $(kubectl get pod scz-s2 -o jsonpath='{.status.containerStatuses[0].restartCount}')   last exit: $(kubectl get pod scz-s2 -o jsonpath='{.status.containerStatuses[0].lastState.terminated.reason}')"
    echo "--- what the first container instance printed"
    cat "results/$name.first-instance.log"
  } | tee "results/$name.txt"
  rm -f "results/$name.first-instance.log"
  kubectl delete pod scz-s2 --ignore-not-found --wait=true >/dev/null 2>&1
}

cluster() { # name  [config file]
  export KUBECONFIG="$PWD/../.tools/kubeconfig-$1"
  if ! "$KIND" get clusters 2>/dev/null | grep -qx "$1"; then
    if [ -n "${2:-}" ]; then "$KIND" create cluster --name "$1" --config "$2" --wait 120s; else "$KIND" create cluster --name "$1" --wait 120s; fi || exit 1
  fi
  "$KIND" export kubeconfig --name "$1" --kubeconfig "$KUBECONFIG" >/dev/null 2>&1
  "$KIND" load docker-image scz-spike-s2 --name "$1" >/dev/null
  kubectl get nodes -o wide | tee "results/nodes-$1.txt"
}

WHICH="${1:-all}"
if [ "$WHICH" = all ] || [ "$WHICH" = stock ]; then
cluster scz-s1
case_run 1-default            ''                 "$RESTRICTED"
case_run 2-adj-nonroot-cap    ', "--adj"'        "$NONROOT_CAP"
case_run 3-adj-root-cap       ', "--adj"'        "$ROOT_CAP"
case_run 4-nested-restricted  ', "--nested"'     "$RESTRICTED"
case_run 5-nested-privileged  ', "--nested"'     "$PRIVILEGED"
"$KIND" delete cluster --name scz-s1 >/dev/null 2>&1   # one cluster at a time, to keep memory free
fi
[ "$WHICH" = stock ] && exit 0

cat > kind-single-process.yaml <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
kubeadmConfigPatches:
  - |
    kind: KubeletConfiguration
    singleProcessOOMKill: true
EOF
cluster scz-s2 kind-single-process.yaml
case_run 6-single-process     ''                 "$RESTRICTED"
"$KIND" delete cluster --name scz-s2 >/dev/null 2>&1
echo "done; both Kind clusters are deleted"
