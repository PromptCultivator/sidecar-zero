#!/usr/bin/env bash
# Spike S4 - Arrow template interop.
#
# Builds four images (scz-spike-s4-py, -rust, -go, -java), generates capacity-template
# frames, reads them with PyArrow (Arrow C++), arrow-rs, Arrow Go and Arrow Java, and
# prints the acceptance matrix. Raw output goes to results/.
#
# Needs only Docker. No arguments. Exit status 0 if every frame that must be accepted
# was accepted with correct values by every reader that was built.
set -uo pipefail
cd "$(dirname "$0")"
RES="$PWD/results"

mkdir -p "$RES"
rm -rf "$RES/frames" "$RES/rust-render"
rm -f "$RES"/*.jsonl "$RES"/*.stderr "$RES"/*.txt "$RES"/*.log
mkdir -p "$RES/rust-render"

# Other spikes share this Docker VM: builds are capped inside the Dockerfiles
# (cargo -j 4, go -p 4, single-threaded Maven) and every run is capped here.
RUN=(docker run --rm --cpus 2 --memory 1g -v "$RES:/s4/results")
JAVA_CP='/s4/java/app/classes:/s4/java/app/lib/*'
# (plain variables, not an associative array: macOS ships bash 3.2)
BUILT_py=0 BUILT_rust=0 BUILT_go=0 BUILT_java=0

build() {
  local name=$1 dir=$2
  printf '== build scz-spike-s4-%s ... ' "$name"
  if docker build -t "scz-spike-s4-$name" "$dir" >"$RES/build-$name.log" 2>&1; then
    eval "BUILT_$name=1"
    echo ok
  else
    echo "FAILED (see results/build-$name.log); continuing without it"
  fi
}

build py gen
build rust readers/rust
build go readers/go
build java readers/java

if [ "$BUILT_py" != 1 ]; then
  echo "the generator image did not build; nothing can run" >&2
  exit 2
fi

{
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "docker server: $(docker version --format '{{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}')"
  echo "docker VM: $(docker info --format 'kernel {{.KernelVersion}}, {{.NCPU}} CPUs, {{.MemTotal}} bytes of memory, {{.OperatingSystem}}')"
  echo "container: $("${RUN[@]}" scz-spike-s4-py uname -srm)"
  echo "--- python"
  "${RUN[@]}" scz-spike-s4-py python -c 'import sys, pyarrow as pa; print("python", sys.version.split()[0]); print("pyarrow", pa.__version__, "/ Arrow C++", pa.cpp_build_info.version, pa.cpp_build_info.build_type)'
  [ "$BUILT_rust" = 1 ] && { echo "--- rust"; "${RUN[@]}" --entrypoint cat scz-spike-s4-rust /s4/rust/versions.txt; }
  [ "$BUILT_go" = 1 ] && { echo "--- go"; "${RUN[@]}" --entrypoint cat scz-spike-s4-go /s4/go/versions.txt; }
  [ "$BUILT_java" = 1 ] && { echo "--- java"; "${RUN[@]}" --entrypoint cat scz-spike-s4-java /s4/java/versions.txt; }
} >"$RES/versions.txt" 2>&1

echo "== render: arrow-rs StreamWriter at alignment 8 and 64 (max_rows)"
GEN_ARGS=(/s4/results/frames)
if [ "$BUILT_rust" = 1 ]; then
  "${RUN[@]}" scz-spike-s4-rust render /s4/results/rust-render >"$RES/rust-render.txt" 2>&1 \
    && GEN_ARGS+=(--rust-render /s4/results/rust-render)
fi

echo "== generate: templates, in-place patching, frames  -> results/gen.txt, results/frames/"
if ! "${RUN[@]}" scz-spike-s4-py python gen_frames.py "${GEN_ARGS[@]}" >/dev/null 2>"$RES/gen.stderr"; then
  echo "generator failed:" >&2
  cat "$RES/gen.stderr" >&2
  exit 2
fi
tail -1 "$RES/gen.txt"

reader() {
  local name=$1
  shift
  printf '== read: %s ... ' "$name"
  "$@" >"$RES/reader-$name.jsonl" 2>"$RES/reader-$name.stderr"
  echo "exit $? ($(wc -l <"$RES/reader-$name.jsonl" | tr -d ' ') results)"
}

reader pyarrow "${RUN[@]}" scz-spike-s4-py python -W default read_pyarrow.py /s4/results/frames
[ "$BUILT_rust" = 1 ] && reader rust "${RUN[@]}" scz-spike-s4-rust read /s4/results/frames
[ "$BUILT_go" = 1 ] && reader go "${RUN[@]}" scz-spike-s4-go /s4/results/frames
[ "$BUILT_java" = 1 ] && reader java "${RUN[@]}" scz-spike-s4-java /s4/results/frames

echo "== side probes -> results/rust-alignment.txt, results/java-fsl-validate-probe.txt"
[ "$BUILT_rust" = 1 ] && "${RUN[@]}" scz-spike-s4-rust align-probe /s4/results/frames >"$RES/rust-alignment.txt" 2>&1
[ "$BUILT_java" = 1 ] && "${RUN[@]}" --entrypoint java scz-spike-s4-java \
  --add-opens=java.base/java.nio=ALL-UNNAMED -cp "$JAVA_CP" s4.FslValidateProbe >"$RES/java-fsl-validate-probe.txt" 2>&1

echo
"${RUN[@]}" scz-spike-s4-py python matrix.py /s4/results
status=$?
echo
echo "raw output: $RES"
exit $status
