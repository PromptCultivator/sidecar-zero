#!/usr/bin/env bash
# The Go SDK's own tests (run inside the container: scripts/dev bash -c 'sdks/go/test.sh').
# Non-zero on any failure.
#   1. gofmt and go vet, for Linux and for macOS (the off-Linux build serves fallbacks)
#   2. the unit tests under the race detector: protocol, golden arena and fingerprints,
#      xxh3 vectors, the in-process double, the arena-access lint (lint_test.go), and the
#      metrics of sdk-contract §8 (metrics_test.go), one test against a supervisor running
#      scz-worker-echo from SCZ_BINS (default target/release; missing binaries fail it)
#   3. the escape check: nothing on the call path moves to the heap (go build -gcflags=-m)
#   4. the allocation gate (testing §7): runtime.MemStats.Mallocs and TotalAlloc unmoved over
#      a million calls on each path
set -euo pipefail
cd "$(dirname "$0")"
calls="${SCZ_ALLOC_CALLS:-1000000}"
export SCZ_BINS="${SCZ_BINS:-$(cd ../.. && pwd)/target/release}"
export GOFLAGS="${GOFLAGS:+$GOFLAGS }-buildvcs=false"

unformatted="$(gofmt -l .)"
if [ -n "$unformatted" ]; then
  echo "gofmt: $unformatted" >&2
  exit 1
fi
nice -n 10 go vet ./...
GOOS=darwin GOARCH=arm64 nice -n 10 go vet ./...

CGO_ENABLED=1 nice -n 10 go test -race -count=1 ./... >/tmp/scz-go-race.log 2>&1 || { cat /tmp/scz-go-race.log; exit 1; }
grep -E '^(ok|FAIL|---)' /tmp/scz-go-race.log

# Escapes: the instantiated call path (the agent instantiates both codecs) must keep
# everything on the stack. Only the constructors may allocate.
esc="$(CGO_ENABLED=0 go build -o /dev/null -gcflags=-m ./cmd/scz-agent-go 2>&1 | grep -E '^\./(client|codec|reason|metrics)\.go' | grep -E 'escapes to heap|moved to heap' |
  grep -vE '&sidecarzero\.(Client|Session)\[|"@" \+ n|make\(\[\]shm\.Column|\[\]Column\{' || true)"
if [ -n "$esc" ]; then
  echo "escape check: on the call path:" >&2
  echo "$esc" >&2
  exit 1
fi
echo "escape check: nothing on the call path escapes"

SCZ_ALLOC_CALLS="$calls" CGO_ENABLED=0 nice -n 10 go test -count=1 -run 'TestAllocationGate' -v . >/tmp/scz-go-alloc.log 2>&1 || { cat /tmp/scz-go-alloc.log; exit 1; }
grep -E 'allocations|^(ok|FAIL)' /tmp/scz-go-alloc.log | sed 's/^ *alloc_test.go:[0-9]*: /alloc gate: /'
