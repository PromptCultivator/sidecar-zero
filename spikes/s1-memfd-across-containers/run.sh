#!/usr/bin/env bash
# S1: a sealed memfd shared between two containers.
#   ./run.sh           the two-container runs in plain Docker
#   ./run.sh kind      the same check inside a Kind pod with a native sidecar
set -uo pipefail
cd "$(dirname "$0")"
mkdir -p results
rm -rf s7-probes && mkdir s7-probes && cp ../s7-platform-facts/probes/clone3_probe.c s7-probes/
docker build -q -t scz-spike-s1 . >/dev/null || { echo "build failed"; exit 1; }
rm -rf s7-probes

docker_case() {
  local name=$1 prefault=$2 cli_user=$3
  echo "=================== docker: $name"
  docker rm -f scz-spike-s1-sup >/dev/null 2>&1
  docker volume rm -f scz-spike-s1-sock >/dev/null 2>&1
  docker volume create scz-spike-s1-sock >/dev/null
  # Make the socket directory group-writable for gid 2000, as an fsGroup would.
  docker run --rm -v scz-spike-s1-sock:/run/scz scz-spike-s1 sh -c 'chown 1000:2000 /run/scz && chmod 2770 /run/scz'
  docker run -d --name scz-spike-s1-sup --user 1000:2000 --memory 512m \
    -v scz-spike-s1-sock:/run/scz scz-spike-s1 /spike/sup --prefault="$prefault" --data-mb=64 >/dev/null
  docker run --rm --name scz-spike-s1-cli --user "$cli_user" --memory 512m \
    -v scz-spike-s1-sock:/run/scz scz-spike-s1 /spike/cli > "results/docker-$name-client.txt" 2>&1
  docker wait scz-spike-s1-sup >/dev/null 2>&1
  docker logs scz-spike-s1-sup > "results/docker-$name-supervisor.txt" 2>&1
  docker rm -f scz-spike-s1-sup >/dev/null 2>&1
  cat "results/docker-$name-supervisor.txt" "results/docker-$name-client.txt"
}

if [ "${1:-docker}" = "docker" ]; then
  docker_case prefaulted 1 1001:2000
  docker_case not-prefaulted 0 1001:2000
  docker volume rm -f scz-spike-s1-sock >/dev/null 2>&1
  exit 0
fi

# ── Kind ──
KIND=${KIND:-../.tools/bin/kind}
export KUBECONFIG="$PWD/../.tools/kubeconfig-s1"
[ -x "$KIND" ] || { echo "kind is not installed at $KIND; see ../README.md"; exit 1; }
"$KIND" get clusters 2>/dev/null | grep -qx scz-s1 || "$KIND" create cluster --name scz-s1 --wait 120s || exit 1
"$KIND" load docker-image scz-spike-s1 --name scz-s1 >/dev/null

kind_case() {
  local name=$1 prefault=$2
  echo "=================== kind: $name"
  kubectl delete pod scz-s1 --ignore-not-found --wait=true >/dev/null 2>&1
  sed "s/PREFAULT/$prefault/" k8s/pod.yaml | kubectl apply -f - >/dev/null
  # When both ordinary containers have finished, the kubelet stops the sidecar and the pod succeeds.
  kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/scz-s1 --timeout=180s >/dev/null 2>&1 \
    || { kubectl get pod scz-s1 -o wide; kubectl describe pod scz-s1 | tail -25; }
  kubectl logs scz-s1 -c sidecar > "results/kind-$name-supervisor.txt" 2>&1
  kubectl logs scz-s1 -c app > "results/kind-$name-client.txt" 2>&1
  kubectl logs scz-s1 -c seccomp-probe > "results/kind-$name-clone3.txt" 2>&1
  cat "results/kind-$name-supervisor.txt" "results/kind-$name-client.txt" "results/kind-$name-clone3.txt"
}
kubectl version 2>/dev/null | tee results/kind-versions.txt
kubectl get nodes -o wide | tee -a results/kind-versions.txt
kind_case prefaulted 1
kind_case not-prefaulted 0
kubectl delete pod scz-s1 --ignore-not-found >/dev/null 2>&1
echo "cluster scz-s1 left running; delete it with: $KIND delete cluster --name scz-s1"
