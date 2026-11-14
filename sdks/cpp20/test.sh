#!/usr/bin/env bash
# The C++20 SDK's own tests (inside the dev container): the compile-fail checks, the unit tests
# (plain, -fno-exceptions -fno-rtti, TSan, ASan+UBSan), the allocation gate, and the
# interleaving stress (plain, TSan, ASan); and the CMake package. Non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")"
OUT="$(cd ../.. && pwd)/target/sdk/cpp20"
nice -n 10 make -s -j4 OUT="$OUT" agent tests sanitize

echo "== compile-fail: each must be refused by the compiler"
for f in tests/compile_fail/*.cpp; do
  if g++ -std=c++20 -I include -fsyntax-only "$f" 2>/dev/null; then
    echo "FAIL $f compiled"; exit 1
  fi
  echo "ok   $(basename "$f" .cpp)"
done

run() { echo "== $*"; timeout 600 nice -n 10 "$@"; }
run "$OUT/tests/unit"
run "$OUT/tests/unit-nx"
TSAN_OPTIONS=halt_on_error=1 run "$OUT/tests/unit-tsan"
run "$OUT/tests/unit-asan"
run "$OUT/tests/alloc_gate"
run "$OUT/tests/stress"
TSAN_OPTIONS=halt_on_error=1 run "$OUT/tests/stress-tsan" 100
run "$OUT/tests/stress-asan" 200

# The CMake package (CMakeLists.txt): installed, then found and used by another project.
echo "== CMake package: install, find_package, build, run"
P="$OUT/cmake"
rm -rf "$P"
cmake -S . -B "$P/build" -DCMAKE_INSTALL_PREFIX="$P/prefix" >/dev/null
cmake --install "$P/build" >/dev/null
cmake -S tests/cmake-consumer -B "$P/consumer" -DCMAKE_PREFIX_PATH="$P/prefix" >/dev/null
nice -n 10 cmake --build "$P/consumer" >/dev/null
"$P/consumer/consumer"
echo "cpp20: all tests passed"
