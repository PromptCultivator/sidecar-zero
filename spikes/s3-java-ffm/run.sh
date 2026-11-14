#!/usr/bin/env bash
# Spike S3 — Java FFM transport. Builds the image(s) and runs everything.
# Raw output lands in results/<label>/, one file per step.
#
#   ./run.sh                 main run (JDK 25, native arch) + JDK 22 baseline + other-arch check
#   S3_ONLY_MAIN=1 ./run.sh  main run only
#   S3_SKIP_OTHER_ARCH=1 ./run.sh   skip the run of the other architecture under emulation
#
# Needs only Docker. Touches only images and containers named scz-spike-s3*.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p results

NATIVE=$(docker version --format '{{.Server.Arch}}')          # arm64 or amd64
case "$NATIVE" in aarch64) NATIVE=arm64 ;; x86_64) NATIVE=amd64 ;; esac
if [ "$NATIVE" = arm64 ]; then OTHER=amd64; else OTHER=arm64; fi
# Four CPUs, so the virtual-thread scheduler has four carriers; fewer on a smaller machine.
CPUS=$(docker info --format '{{.NCPU}}'); [ "$CPUS" -gt 4 ] && CPUS=4

build_and_run() { # <image tag> <platform> <jdk image> <label> <full|quick>
  local tag=$1 platform=$2 jdk=$3 label=$4 mode=$5
  echo "### build $tag ($jdk, linux/$platform)"
  docker build --quiet --platform "linux/$platform" --build-arg "JDK_IMAGE=$jdk" -t "$tag" .
  echo "### run $label"
  # 4 CPUs so the virtual-thread scheduler has exactly 4 carriers; 2 GB so the JVM picks G1.
  docker run --rm --name "scz-spike-s3-$label" --platform "linux/$platform" \
    --cpus "$CPUS" --memory 2g \
    -v "$PWD/results:/results" \
    "$tag" "$label" "$mode"
}

# 1. Main run: JDK 25 on the daemon's own architecture.
build_and_run scz-spike-s3 "$NATIVE" eclipse-temurin:25-jdk "jdk25-$NATIVE" full

if [ "${S3_ONLY_MAIN:-0}" != 1 ]; then
  # 2. The JDK 22 baseline, same architecture, same program.
  build_and_run scz-spike-s3:jdk22 "$NATIVE" eclipse-temurin:22-jdk "jdk22-$NATIVE" full \
    || echo "### JDK 22 run failed (see results/jdk22-$NATIVE)"
  # 3. The other architecture under emulation: only to check that the variadic downcalls link and work.
  if [ "${S3_SKIP_OTHER_ARCH:-0}" != 1 ]; then
    build_and_run "scz-spike-s3:jdk25-$OTHER" "$OTHER" eclipse-temurin:25-jdk "jdk25-$OTHER" quick \
      || echo "### linux/$OTHER run failed or is impractical under emulation (see results/jdk25-$OTHER)"
  fi
fi

echo "### done; results in $PWD/results"
