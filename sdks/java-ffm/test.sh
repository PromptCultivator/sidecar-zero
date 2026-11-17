#!/usr/bin/env bash
# The Java SDK's own tests (run inside the dev container): the unit tests (golden fingerprint
# and arena vectors, byte layout, slot transitions, the metrics without a sidecar, a stress test
# against a Java-side model of the worker), the metrics against scz-worker-echo (sdk-contract §8),
# then the allocation gate (testing §7) against real supervisors from SCZ_BINS
# (default target/release): the chaos worker for opaque bytes, the ONNX worker with the fraud
# model from SCZ_MODELS (default target/models) for the dense float32 frames. Non-zero on any failure.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SDK=$ROOT/sdks/java-ffm
OUT=$ROOT/target/sdk/java-ffm
BINS=${SCZ_BINS:-$ROOT/target/release}
MODELS=${SCZ_MODELS:-$ROOT/target/models}
[ -n "${SCZ_MODELS:-}" ] || python3 "$ROOT/models/gen.py" "$MODELS"
"$SDK/build.sh"
rm -rf "$OUT/test-classes"
mkdir -p "$OUT/test-classes"
nice -n 10 javac --release 25 -encoding UTF-8 -Xlint:all,-restricted,-serial -Werror -cp "$OUT/classes" -d "$OUT/test-classes" \
  $(find "$SDK/src/test/java" -name '*.java' | sort)
JAVA=(nice -n 10 java -Xms64m -Xmx512m -XX:+UseSerialGC --enable-native-access=ALL-UNNAMED -cp "$OUT/classes:$OUT/test-classes")

echo "== unit tests"
"${JAVA[@]}" io.sidecarzero.Tests "$ROOT"
echo "== off Linux (os.name overridden): constructs, falls back with NOT_CONNECTED"
"${JAVA[@]}" -Dos.name=Darwin io.sidecarzero.Tests "$ROOT" offlinux

DIR=$(mktemp -d /tmp/scz-java-test.XXXXXX)
SUP=
cleanup() {
  if [ -n "$SUP" ]; then kill -KILL "$SUP" 2>/dev/null || true; wait "$SUP" 2>/dev/null || true; fi
  rm -rf "$DIR"
}
trap cleanup EXIT
sidecar() { # NAME WORKER ARGS...: start a supervisor, wait for its socket
  local name=$1 worker=$2
  shift 2
  "$BINS/scz-supervisor" --socket="$DIR/$name.sock" --worker="$BINS/$worker" --admin="$DIR/$name.admin" --hb-stale-ms=100 "$@" \
    >"$DIR/$name.log" 2>&1 &
  SUP=$!
  for _ in $(seq 200); do [ -S "$DIR/$name.sock" ] && return 0; sleep 0.05; done
  echo "the $name supervisor did not start:" >&2
  cat "$DIR/$name.log" >&2
  return 1
}
stop() {
  kill -KILL "$SUP" 2>/dev/null
  wait "$SUP" 2>/dev/null || true
  SUP=
}

echo "== metrics (sdk-contract §8): against scz-worker-echo"
sidecar echo scz-worker-echo
"${JAVA[@]}" io.sidecarzero.Tests "$ROOT" metrics "$DIR/echo.sock"
stop

echo "== allocation gate: opaque bytes, against scz-worker-chaos"
sidecar keyed scz-worker-chaos
"${JAVA[@]}" io.sidecarzero.AllocGate keyed "$DIR/keyed.sock"
stop
if [ "${SCZ_GATE_C1:-0}" = 1 ]; then
  echo "== allocation gate: opaque bytes, C1 only (the zero must not depend on C2's escape analysis)"
  sidecar keyed-c1 scz-worker-chaos
  "${JAVA[@]}" -XX:TieredStopAtLevel=1 io.sidecarzero.AllocGate keyed "$DIR/keyed-c1.sock"
  stop
fi
echo "== allocation gate: dense float32 frames, against sidecarzero-worker (fraud model)"
sidecar frames sidecarzero-worker --model="$MODELS/fraud.ort" --batch-slots=8 --batch-slot-size=65536 --batch-resp-cap=4096
"${JAVA[@]}" io.sidecarzero.AllocGate frames "$DIR/frames.sock"
echo "== metrics: the primitive clients, against the same supervisor"
"${JAVA[@]}" io.sidecarzero.Tests "$ROOT" metrics-frames "$DIR/frames.sock"
stop
