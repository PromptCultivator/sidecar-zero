#!/usr/bin/env bash
# Builds the Go SDK and its conformance agent into target/sdk/go/ (run inside the container:
# scripts/dev bash -c 'sdks/go/build.sh'). Idempotent, quiet on success.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$root/target/sdk/go"
mkdir -p "$out"
cd "$here"
export CGO_ENABLED=0
nice -n 10 go build ./...
# Off Linux the SDK must still compile and serve fallbacks (sdk-contract §9).
GOOS=darwin GOARCH=arm64 nice -n 10 go build ./...
GOOS=windows GOARCH=amd64 nice -n 10 go build . ./internal/...
nice -n 10 go build -trimpath -o "$out/scz-agent-go" ./cmd/scz-agent-go
mkdir -p "$out/crash"
nice -n 10 go build -tags crashpoints -trimpath -o "$out/crash/scz-agent-go" ./cmd/scz-agent-go
