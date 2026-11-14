#!/usr/bin/env bash
# Spike S5 - ONNX Runtime over foreign memory.
# Builds the image scz-spike-s5 and runs every test in one container, writing raw output to results/.
# No arguments needed. S5_QUICK=1 ./run.sh shortens the latency loops to 2,000 calls for a smoke test.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p results

docker build -t scz-spike-s5 .

docker rm -f scz-spike-s5-run >/dev/null 2>&1 || true
docker run --rm --name scz-spike-s5-run \
  --memory 2g --memory-swap 2g \
  -e S5_QUICK="${S5_QUICK:-0}" \
  -v "$PWD/results:/out" \
  scz-spike-s5

# Second pass: weight prepacking off, for the weight-sharing and latency comparison in the finding.
mkdir -p results/prepack-off
docker run --rm --name scz-spike-s5-prepack --memory 2g --memory-swap 2g -e S5_DISABLE_PREPACK=1 \
  -v "$PWD/results/prepack-off:/out" scz-spike-s5 bash -c '
M=/work/models
{ for m in mlp.ort tree.ort; do /work/s5 load $M/$m bytes+init; done
  python /work/share.py $M/mlp.ort bytes+init
  python /work/share.py $M/tree.ort bytes+init; } > /out/05-model-load.txt 2>&1
{ /work/s5 latency $M/tree.ort bind-per-call 100000 1 A
  /work/s5 latency $M/tree.ort bind-per-call 20000 32 A
  /work/s5 latency $M/mlp.ort bind-per-call 20000 1 A; } > /out/03-fixed-cost.txt 2>&1'
