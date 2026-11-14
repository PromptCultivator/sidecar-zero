#!/usr/bin/env bash
# S6: time from kill -9 of the active worker to the standby serving, against worker memory.
#   ./run.sh [kills-per-row]     default 20
set -uo pipefail
cd "$(dirname "$0")"
mkdir -p results
docker build -q -t scz-spike-s6 . >/dev/null || { echo "build failed"; exit 1; }
{
  echo "host: $(docker info --format '{{.OperatingSystem}}, kernel {{.KernelVersion}}, {{.Architecture}}, {{.NCPU}} CPUs')"
  docker run --rm --name scz-spike-s6 scz-spike-s6 /promo "${1:-20}"
} 2>&1 | tee results/promotion.txt
