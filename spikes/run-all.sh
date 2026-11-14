#!/usr/bin/env bash
# Run every phase 0 spike on this machine and pack the results into one archive.
#
#   sudo ./run-all.sh --install            first time on a fresh Linux machine: install Docker and curl
#   ./run-all.sh                           every spike, full length (about 1-2 hours)
#   ./run-all.sh --quick                   every spike, shortened (about 30-45 minutes)
#   ./run-all.sh s6b s2                    only these spikes
#   ./run-all.sh --list                    what each spike name means
#
# Intended for a throwaway machine. It builds about 20 Docker images, needs about 30 GB of
# free disk and 8 GB of memory available to Docker, starts Kind clusters, and runs some
# privileged containers (S2's OOM matrix, Kind itself).
#
# Nothing in the repository is changed. The spikes are copied to a working directory, run
# there, and the results land in  $SCZ_RUNS/<host>-<time>/  and in a .tar.gz beside it.
# $SCZ_RUNS defaults to ~/scz-spike-runs.
#
# Network access it needs: Docker Hub; github.com (kind, ONNX Runtime); dl.k8s.io (kubectl);
# and the Python, Rust, Go and Maven package registries.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
KIND_VERSION=v0.33.0
ALL_SPIKES="s7 s1 s6 s6b s2 s3 s4 s5"   # quick kernel checks first, then Kind, then the long builds

usage() { sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
say() { printf '%s\n' "$*"; }
die() { printf 'run-all: %s\n' "$*" >&2; exit 1; }

dir_of() {
  case "$1" in
    s1) echo s1-memfd-across-containers ;; s2) echo s2-oom-matrix ;; s3) echo s3-java-ffm ;;
    s4) echo s4-arrow-templates ;; s5) echo s5-ort-foreign-memory ;; s6) echo s6-standby-promotion ;;
    s6b) echo s6b-death-signal ;; s7) echo s7-platform-facts ;; *) return 1 ;;
  esac
}

list() {
  cat <<'EOF'
s7   small platform facts: half-close, thread liveness, clone3, change time, loom     (minutes)
s1   a sealed memfd across containers, in Docker and in a Kind pod                    (minutes)
s6   standby promotion: kill to pidfd against worker memory                          (minutes)
s6b  the early death signal: kills, crashes, core dumps, sixteen watchers             (10-20 min)
s2   OOM behaviour under each shield mode, on Kind                                    (10 min)
s3   Java FFM transport, JDK 25 and JDK 22                                            (10-20 min)
s4   Arrow template frames read by C++, Rust, Go and Java                             (20-40 min, mostly builds)
s5   ONNX Runtime over slot memory: zero copy, cost per call, memory, weight sharing  (15-30 min)
EOF
  exit 0
}

# ── arguments ──
INSTALL=0 QUICK=0 SPIKES=""
for a in "$@"; do
  case "$a" in
    --install) INSTALL=1 ;;
    --quick) QUICK=1 ;;
    --list) list ;;
    -h|--help) usage ;;
    s[0-9]*) dir_of "$a" >/dev/null || die "unknown spike '$a' (see --list)"; SPIKES="$SPIKES $a" ;;
    *) usage 2 ;;
  esac
done
[ -n "$SPIKES" ] || SPIKES=$ALL_SPIKES

# ── this machine ──
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$(uname -m)" in x86_64|amd64) ARCH=amd64 ;; aarch64|arm64) ARCH=arm64 ;; *) die "unsupported architecture $(uname -m)" ;; esac
case "$OS" in linux|darwin) ;; *) die "unsupported system $OS: the spikes need Linux, or macOS with Docker Desktop" ;; esac

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

install_prereqs() {
  [ "$OS" = linux ] || die "--install is for Linux; on macOS install Docker Desktop yourself"
  [ "$(id -u)" = 0 ] || die "--install needs root: run it with sudo"
  if command -v apt-get >/dev/null; then
    apt-get update -q
    command -v curl >/dev/null || apt-get install -y -q curl ca-certificates
    if ! command -v docker >/dev/null; then
      apt-get install -y -q docker.io
      apt-get install -y -q docker-buildx >/dev/null 2>&1 || apt-get install -y -q docker-buildx-plugin >/dev/null 2>&1 || true
    fi
  elif command -v dnf >/dev/null; then
    command -v curl >/dev/null || dnf install -y -q curl
    command -v docker >/dev/null || dnf install -y -q docker || dnf install -y -q moby-engine \
      || die "could not install Docker with dnf; install it from docker.com and re-run"
  else
    die "neither apt-get nor dnf found; install Docker and curl yourself and re-run"
  fi
  systemctl enable --now docker >/dev/null 2>&1 || service docker start >/dev/null 2>&1 || true
}
[ "$INSTALL" = 1 ] && install_prereqs

command -v docker >/dev/null || die "Docker is not installed (on a fresh Linux machine: sudo $0 --install)"
command -v curl >/dev/null || die "curl is not installed (on a fresh Linux machine: sudo $0 --install)"
docker info >/dev/null 2>&1 || die "cannot reach the Docker daemon as $(id -un): start it, or run as root, or add yourself to the docker group"
export DOCKER_BUILDKIT=1   # S4's builds use cache mounts, which need BuildKit

DOCKER_MEM=$(docker info --format '{{.MemTotal}}' 2>/dev/null || echo 0)
[ "${DOCKER_MEM:-0}" -ge 7000000000 ] 2>/dev/null || say "warning: Docker has $((DOCKER_MEM / 1048576)) MiB of memory; S6 and S6b use 2 GiB workers, and 8 GiB is recommended"
DOCKER_CPUS=$(docker info --format '{{.NCPU}}' 2>/dev/null || echo 0)
[ "${DOCKER_CPUS:-0}" -ge 4 ] 2>/dev/null || say "warning: $DOCKER_CPUS CPUs; S3's carrier-pinning test is laid out for four, and S6b's spinning threads will share cores. Four or more is recommended"
CGROUP_V=$(docker info --format '{{.CgroupVersion}}' 2>/dev/null || echo '?')
[ "$CGROUP_V" = 2 ] || say "warning: cgroup v$CGROUP_V; S1 and S2 read cgroup v2 files, so their memory results will be missing or wrong. Use a distribution that boots with cgroup v2"
CONTAINER_VIRT=$(systemd-detect-virt -c 2>/dev/null || true)
case "${CONTAINER_VIRT:-none}" in
  none) ;;
  *) say "warning: this looks like a container-based VPS ($CONTAINER_VIRT). Docker and Kind need a full virtual machine (KVM, Xen, Hyper-V) or real hardware" ;;
esac

SCZ_RUNS=${SCZ_RUNS:-$HOME/scz-spike-runs}
mkdir -p "$SCZ_RUNS" || die "cannot create $SCZ_RUNS"
FREE_KB=$(df -Pk "$SCZ_RUNS" | awk 'NR==2 {print $4}')
[ "${FREE_KB:-0}" -ge 31457280 ] || say "warning: only $((FREE_KB / 1048576)) GiB free under $SCZ_RUNS; about 30 GiB is needed for images and results"

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
HOST=$(hostname -s 2>/dev/null || hostname)
NAME="$HOST-$STAMP"
WORK="$SCZ_RUNS/$NAME"
OUT="$WORK/out"
mkdir -p "$OUT/logs" "$OUT/results" "$WORK/spikes/.tools"

# ── tools: kind (pinned) and kubectl, each checked against its published digest ──
TOOLS="$SCZ_RUNS/.tools-$OS-$ARCH"
mkdir -p "$TOOLS"
fetch_checked() { # url  checksum-url  destination
  local tmp="$3.tmp"
  curl -fsSL -o "$tmp" "$1" || die "download failed: $1"
  local want; want=$(curl -fsSL "$2" | awk '{print $1}') || die "download failed: $2"
  [ -n "$want" ] && [ "$(sha256 "$tmp")" = "$want" ] || { rm -f "$tmp"; die "checksum mismatch for $1"; }
  chmod +x "$tmp" && mv "$tmp" "$3"
}
if [ ! -x "$TOOLS/kind" ] || ! "$TOOLS/kind" version 2>/dev/null | grep -q "${KIND_VERSION#v}"; then
  say "fetching kind $KIND_VERSION"
  base="https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-$OS-$ARCH"
  fetch_checked "$base" "$base.sha256sum" "$TOOLS/kind"
fi
if [ ! -x "$TOOLS/kubectl" ]; then
  KV=$(curl -fsSL https://dl.k8s.io/release/stable.txt) || die "cannot read the current kubectl version"
  say "fetching kubectl $KV"
  base="https://dl.k8s.io/release/$KV/bin/$OS/$ARCH/kubectl"
  fetch_checked "$base" "$base.sha256" "$TOOLS/kubectl"
fi
export KIND="$TOOLS/kind" PATH="$TOOLS:$PATH"

# ── the machine, recorded first, since every number depends on it ──
{
  say "run:            $NAME"
  say "spikes:         $SPIKES   quick=$QUICK"
  say "uname:          $(uname -a)"
  [ -r /etc/os-release ] && say "os:             $(. /etc/os-release; echo "$PRETTY_NAME")"
  [ "$OS" = darwin ] && say "os:             macOS $(sw_vers -productVersion 2>/dev/null), Docker Desktop's Linux VM runs the spikes"
  VIRT=$(systemd-detect-virt 2>/dev/null || true)
  [ -z "$VIRT" ] && [ "$OS" = darwin ] && VIRT="yes (Docker Desktop VM)"
  say "virtualization: ${VIRT:-unknown}"
  say "cpus:           $( (nproc 2>/dev/null) || sysctl -n hw.ncpu)"
  command -v lscpu >/dev/null && lscpu | grep -E '^(Model name|Architecture|Socket|Core|Thread|CPU\(s\)|NUMA node\(s\)|CPU max MHz)' | sed 's/^/  /'
  [ -r /proc/cmdline ] && say "kernel cmdline: $(cat /proc/cmdline)"
  [ -r /sys/devices/system/cpu/isolated ] && say "isolated cpus:  $(cat /sys/devices/system/cpu/isolated)"
  [ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor ] && say "governor cpu0:  $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
  [ -d /sys/devices/system/cpu/cpu0/cpuidle ] && say "idle states:    $(cat /sys/devices/system/cpu/cpu0/cpuidle/state*/name 2>/dev/null | tr '\n' ' ')"
  [ -r /sys/kernel/mm/transparent_hugepage/enabled ] && say "huge pages:     $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
  [ -r /proc/sys/kernel/core_pattern ] && say "core_pattern:   $(cat /proc/sys/kernel/core_pattern)"
  [ "$OS" = linux ] && say "cgroup fs:      $(stat -fc %T /sys/fs/cgroup 2>/dev/null)"
  say "docker:         $(docker version --format 'client {{.Client.Version}}, server {{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}' 2>/dev/null)"
  say "docker host:    $(docker info --format '{{.OperatingSystem}}, kernel {{.KernelVersion}}, {{.NCPU}} CPUs, {{.MemTotal}} bytes, cgroup v{{.CgroupVersion}}' 2>/dev/null)"
  say "kind:           $("$KIND" version 2>/dev/null)"
  say "kubectl:        $(kubectl version --client 2>/dev/null | head -1)"
} > "$OUT/machine.txt"
cat "$OUT/machine.txt"
if grep -q '^virtualization: none' "$OUT/machine.txt"; then
  say "bare metal: timings from this run are the ones that count"
else
  say "note: this is not bare metal, so timings are indicative only; everything else in the findings still applies"
fi
say ""

# ── a private copy of the spikes, so the repository's own results are left alone ──
(cd "$HERE" && tar --exclude='./.tools' --exclude='*/results' -cf - .) | (cd "$WORK/spikes" && tar -xf -) \
  || die "could not copy the spikes to $WORK"

cleanup() {
  for c in $("$KIND" get clusters 2>/dev/null | grep -E '^scz-'); do "$KIND" delete cluster --name "$c" >/dev/null 2>&1; done
}
trap cleanup EXIT

# ── run ──
KILLS=20; [ "$QUICK" = 1 ] && KILLS=5
SUMMARY="$OUT/summary.txt"
printf '%-5s %-28s %-8s %s\n' spike directory minutes result > "$SUMMARY"
failed=0
for s in $SPIKES; do
  d=$(dir_of "$s")
  log="$OUT/logs/$s.log"
  say "== $s ($d) started $(date -u +%H:%M:%SZ); log: $log"
  t0=$(date +%s); rc=0
  (
    cd "$WORK/spikes/$d" || exit 1
    case "$s" in
      s1) ./run.sh && ./run.sh kind ;;
      s3) S3_SKIP_OTHER_ARCH=1 S3_ONLY_MAIN=$QUICK ./run.sh ;;
      s5) S5_QUICK=$QUICK ./run.sh ;;
      s6) ./run.sh "$KILLS" ;;
      s6b) ./run.sh all "$KILLS" ;;
      *) ./run.sh ;;
    esac
  ) > "$log" 2>&1 || rc=$?
  mins=$(( ($(date +%s) - t0 + 30) / 60 ))
  mkdir -p "$OUT/results/$d"
  [ -d "$WORK/spikes/$d/results" ] && cp -R "$WORK/spikes/$d/results/." "$OUT/results/$d/"
  if [ "$rc" = 0 ]; then result=ok; else result="FAILED (exit $rc)"; failed=$((failed + 1)); fi
  printf '%-5s %-28s %-8s %s\n' "$s" "$d" "$mins" "$result" >> "$SUMMARY"
  say "   $result after $mins min"
  [ "$rc" = 0 ] || { say "   last lines of the log:"; tail -n 15 "$log" | sed 's/^/     /'; }
done
cleanup

ARCHIVE="$SCZ_RUNS/$NAME.tar.gz"
tar -C "$WORK" -czf "$ARCHIVE" out
say ""
cat "$SUMMARY"
say ""
say "results:  $OUT"
say "archive:  $ARCHIVE   (copy it off this machine before deleting it)"
say "images:   docker images 'scz-spike*' lists what was built; remove them with"
say "          docker rmi \$(docker images -q 'scz-spike*')"
[ "$failed" = 0 ] || exit 1
