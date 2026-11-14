#!/usr/bin/env bash
# S7: small platform facts. Builds the C probes, runs each one, then runs the loom check.
set -uo pipefail
cd "$(dirname "$0")"
mkdir -p results

docker build -q -t scz-spike-s7 . >/dev/null || { echo "build failed"; exit 1; }

run() {
  local name=$1; shift
  echo "=================== $name"
  docker run --rm "$@" 2>&1 | tee "results/$name.txt"
}

run halfclose           scz-spike-s7 /probes/halfclose
run thread_liveness     scz-spike-s7 /probes/thread_liveness
run clone3-default      scz-spike-s7 /probes/clone3_probe
run clone3-unconfined   --security-opt seccomp=unconfined scz-spike-s7 /probes/clone3_probe
run ctime               --tmpfs /mnt/tmpfs -v scz-spike-s7-vol:/mnt/volume scz-spike-s7 \
                        /probes/ctime_probe /tmp /mnt/tmpfs /mnt/volume
run features            scz-spike-s7 /probes/features

echo "=================== loom"
docker run --rm -v "$PWD/loom":/w -v scz-spike-cargo:/usr/local/cargo/registry -w /w \
  -e RUSTFLAGS="--cfg loom" -e CARGO_TARGET_DIR=/tmp/target rust:1 bash -c '
    cargo test --release --no-run 2>&1 | tail -3
    for t in seqcst_store_seqcst_load release_store_fence_acquire_load release_store_acquire_load_no_fence relaxed_store_fence_relaxed_load; do
      if cargo test --release -- --exact "tests::$t" >/tmp/out.txt 2>&1; then verdict="accepted by loom"; else verdict="REJECTED by loom"; fi
      echo "LOOM $t: $verdict"
      grep -E "lost wake-up|panicked at" /tmp/out.txt | head -2 | sed "s/^/    /"
    done' 2>&1 | tee results/loom.txt
