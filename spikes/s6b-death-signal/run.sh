#!/usr/bin/env bash
# S6b: the early death signal as the design specifies it. One robust lock per consume
# thread, taken through glibc in the worker, watched with raw futex calls by a supervisor
# built against musl.
#
#   ./run.sh [kills]          everything (default 20 kills per case)
#   ./run.sh part1 [kills]    the first run: SIGKILL from outside, three sizes, and a forged death
#   ./run.sh part2 [kills]    crashes inside a thread, core dumps, sixteen watchers
#   ./run.sh amd64 [kills]    a short functional run of both parts as linux/amd64, under emulation
set -uo pipefail
cd "$(dirname "$0")"
mkdir -p results
PART=${1:-all}; KILLS=${2:-20}
case "$PART" in all|part1|part2|amd64) ;; *) KILLS=$PART; PART=all ;; esac

PLATFORM=""; TAG=scz-spike-s6b
if [ "$PART" = amd64 ]; then PLATFORM="--platform linux/amd64"; TAG=scz-spike-s6b:amd64; fi
docker build -q $PLATFORM -t "$TAG" . >/dev/null || { echo "build failed"; exit 1; }
RUN="docker run --rm $PLATFORM --ulimit core=0"
host="host: $(docker info --format '{{.OperatingSystem}}, kernel {{.KernelVersion}}, {{.Architecture}}, {{.NCPU}} CPUs')"

if [ "$PART" = all ] || [ "$PART" = part1 ]; then
  { echo "$host"; $RUN --name scz-spike-s6b "$TAG" /spike/sup 0,512,2048 "$KILLS" 4 2; } 2>&1 | tee results/death-signal.txt
  $RUN --name scz-spike-s6b-forge "$TAG" /spike/sup forge 2>&1 | tee results/forge.txt
fi

if [ "$PART" = all ] || [ "$PART" = part2 ]; then
  {
    echo "$host"
    echo "Times in ms from the moment the supervisor asks for the crash (or sends SIGKILL); median / p90 / max; $KILLS runs per row"
    echo "Core dumps off (ulimit -c 0), as the deployment should run"
    printf '%-14s %-9s %-22s  %-22s  %-22s\n' "how" "worker" "first lock reports" "last of 4 reports" "pidfd readable"
    for how in kill segv-consume segv-runtime abort-consume; do
      for mib in 0 512 2048; do $RUN --name scz-spike-s6b-crash "$TAG" /spike/sup2 crash "$how" "$KILLS" "$mib"; done
    done
  } 2>&1 | tee results/crash-inside.txt
  {
    echo "$host"
    echo "Core dumps ON (ulimit -c unlimited; core_pattern: $(docker run --rm $PLATFORM "$TAG" cat /proc/sys/kernel/core_pattern)); 5 runs per row"
    for mib in 0 512; do
      docker run --rm $PLATFORM --ulimit core=-1 --name scz-spike-s6b-core "$TAG" /spike/sup2 crash segv-consume 5 "$mib"
    done
  } 2>&1 | tee results/crash-with-core-dump.txt
  { echo "$host"; $RUN --name scz-spike-s6b-16 "$TAG" /spike/sup2 sixteen "$KILLS"; } 2>&1 | tee results/sixteen-watchers.txt
fi

if [ "$PART" = amd64 ]; then
  {
    echo "$host"
    echo "linux/amd64 containers on this host run under emulation, on the host's own arm64 kernel."
    $RUN "$TAG" sh -c 'uname -m; /spike/sup 0,512 '"$KILLS"' 4 2; /spike/sup forge; /spike/sup2 crash segv-consume '"$KILLS"' 0; /spike/sup2 sixteen '"$KILLS"
  } 2>&1 | tee results/amd64-emulated.txt
fi
