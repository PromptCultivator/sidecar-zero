#!/bin/bash
# Runs INSIDE the spike image. Usage: inner.sh <result-label> [full|quick]
# Starts the helper, runs each Java step in its own JVM, writes one file per step.
set -u
LABEL=${1:?result label}
MODE=${2:-full}
OUT=/results/$LABEL
rm -rf "$OUT"
mkdir -p "$OUT"
SOCK=/tmp/scz-s3.sock

/spike/helper "$SOCK" > "$OUT/00-helper.log" 2>&1 &
HELPER=$!
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "helper did not start"; cat "$OUT/00-helper.log"; exit 1; }

{
  echo "label: $LABEL   mode: $MODE   date: $(date -u +%FT%TZ)"
  echo "uname -a: $(uname -a)"
  echo "nproc: $(nproc)   page size: $(getconf PAGESIZE)"
  echo "cgroup cpu.max: $(cat /sys/fs/cgroup/cpu.max 2>/dev/null)   memory.max: $(cat /sys/fs/cgroup/memory.max 2>/dev/null)"
  echo "libc: $(ldd --version 2>&1 | head -1)"
  java -version 2>&1
} > "$OUT/00-env.txt"

CP="-cp /spike/classes"
JAVA="java --enable-native-access=ALL-UNNAMED -Ds3.out=$OUT $CP"
# One carrier only, so the virtual-thread allocation reading has exactly one counter to sum.
ONE_CARRIER="-Djdk.virtualThreadScheduler.parallelism=1 -Djdk.virtualThreadScheduler.maxPoolSize=1"

FAILED=0
run() { # run <name> <timeout-seconds> <command...>
  local name=$1 t=$2
  shift 2
  echo "== $LABEL/$name"
  {
    echo "\$ $*"
    timeout "$t" "$@"
    echo "[exit status $?]"
  } > "$OUT/$name.txt" 2>&1
  tail -n 2 "$OUT/$name.txt" | sed 's/^/     /'
  grep -q '^\[exit status 0\]$' "$OUT/$name.txt" || FAILED=$((FAILED + 1))
}

run 01-transport 300 $JAVA S3 $SOCK handshake atomics futex
if [ "$MODE" = full ]; then
  run 02-alloc-c2 900 $JAVA $ONE_CARRIER S3 $SOCK alloc
  run 03-alloc-c1 900 $JAVA $ONE_CARRIER -XX:TieredStopAtLevel=1 S3 $SOCK alloc
  run 03b-alloc-xint 900 $JAVA $ONE_CARRIER -Xint -Ds3.iters=100000 -Ds3.warm=20000 S3 $SOCK alloc
  run 04-arena 300 $JAVA S3 $SOCK arena
  run 05-pinning 300 $JAVA -Djdk.tracePinnedThreads=full S3 $SOCK pin
fi
run 06-critical 300 $JAVA S3 $SOCK critical
# Expected to print a warning (and, for the second, to fail): recorded, not counted as failures.
F=$FAILED
run 07-no-native-access-flag 300 java -Ds3.out=$OUT $CP Probe $SOCK handshake
run 08-illegal-native-access-deny 300 java --illegal-native-access=deny -Ds3.out=$OUT $CP Probe $SOCK handshake
FAILED=$F

kill $HELPER 2>/dev/null
echo "== $LABEL: $FAILED step(s) did not exit 0"
exit $FAILED
