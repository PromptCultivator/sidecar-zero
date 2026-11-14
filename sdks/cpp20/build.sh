#!/usr/bin/env bash
# Build the C++20 SDK's conformance agent (and its crash-point variant) into target/sdk/cpp20,
# and check that the headers compile as an application includes them. Quiet on success.
set -euo pipefail
cd "$(dirname "$0")"
OUT="$(cd ../.. && pwd)/target/sdk/cpp20"
nice -n 10 make -s -j4 OUT="$OUT" agent
