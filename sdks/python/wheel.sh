#!/usr/bin/env bash
# Pack what build.sh built (target/sdk/python/sidecarzero) into a wheel, then install it into
# a fresh virtual environment and use it from there. Inside the dev container, after build.sh:
#
#   sdks/python/wheel.sh            writes target/sdk/python/dist/sidecarzero-<v>-cp311-abi3-linux_<arch>.whl
#
# The native core uses CPython's stable ABI from 3.11 (abi3), so one wheel serves 3.11 and
# later. The platform tag is plain linux_<arch>: a wheel for PyPI needs a manylinux tag, which
# auditwheel checks and grants (release work). NumPy is an optional extra, `sidecarzero[numpy]`:
# the dense codec and the worker runner need it, the opaque-bytes client does not.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$ROOT/target/sdk/python
PY=${SCZ_PYTHON:-/opt/py/bin/python3}
[ -f "$OUT/sidecarzero/_core.abi3.so" ] || { echo "wheel.sh: run sdks/python/build.sh first" >&2; exit 1; }
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$HERE/Cargo.toml" | head -1)
# The package as installed, with the core's debug information stripped (build.sh keeps it, for
# the tests' backtraces): about a tenth of the size.
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -a "$OUT/sidecarzero" "$STAGE/"
find "$STAGE" -name __pycache__ -prune -exec rm -rf {} +
strip --strip-debug "$STAGE/sidecarzero/_core.abi3.so"
mkdir -p "$OUT/dist"
"$PY" -I - "$STAGE" "$VERSION" "$OUT/dist" <<'PY'
import base64, hashlib, os, platform, sys, zipfile

out, version, dest = sys.argv[1], sys.argv[2], sys.argv[3]
pkg = os.path.join(out, "sidecarzero")
tag = "cp311-abi3-linux_" + platform.machine()
name = "sidecarzero-%s-%s.whl" % (version, tag)
dist = "sidecarzero-%s.dist-info" % version
metadata = "\n".join([
    "Metadata-Version: 2.1",
    "Name: sidecarzero",
    "Version: " + version,
    "Summary: Model inference through shared memory with a SidecarZero sidecar, with a fallback that always answers",
    "License: Apache-2.0",
    "Requires-Python: >=3.11",
    "Provides-Extra: numpy",
    'Requires-Dist: numpy>=1.26; extra == "numpy"',
    "",
])
wheel = "Wheel-Version: 1.0\nGenerator: sdks/python/wheel.sh\nRoot-Is-Purelib: false\nTag: %s\n" % tag
path = os.path.join(dest, name)
record = []

def digest(data):
    return "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()

with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
    for base, dirs, files in os.walk(pkg):
        dirs[:] = sorted(d for d in dirs if d != "__pycache__")
        for f in sorted(files):
            full = os.path.join(base, f)
            arc = os.path.relpath(full, out)
            data = open(full, "rb").read()
            z.writestr(arc, data)
            record.append("%s,%s,%d" % (arc, digest(data), len(data)))
    for f, text in (("METADATA", metadata), ("WHEEL", wheel)):
        arc = dist + "/" + f
        z.writestr(arc, text)
        record.append("%s,%s,%d" % (arc, digest(text.encode()), len(text.encode())))
    record.append(dist + "/RECORD,,")
    z.writestr(dist + "/RECORD", "\n".join(record) + "\n")
print(path)
PY
whl=$(ls -t "$OUT"/dist/sidecarzero-*.whl | head -1)
# Installed into an environment of its own, without NumPy, and used from there.
V=$(mktemp -d)/venv
"$PY" -m venv --without-pip "$V"
"$PY" -m pip --python "$V/bin/python" install -q --no-deps --no-index "$whl"
(cd / && "$V/bin/python" -I -c '
import sidecarzero
from sidecarzero import Client, FallbackReason
from sidecarzero.codecs import Bytes
c = Client("/nonexistent/scz.sock", codec=Bytes(response_len=8), fallback=lambda x, r: b"fb")
o = c.infer(b"x", budget_ms=1.0)
assert o.value == b"fb" and o.reason is FallbackReason.NOT_CONNECTED, o
assert "scz_client_calls_total" in c.prometheus_text()
c.close()
print("  installed %s %s and called it: %s" % (sidecarzero.__name__, sidecarzero.__version__, o.reason.name))
')
rm -rf "$(dirname "$V")"
