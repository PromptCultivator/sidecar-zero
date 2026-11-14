#!/usr/bin/env bash
# Stage a Python runtime for the Python-runner image (deploy/docker/Dockerfile, target
# python-runner), from the dev image's own packages. Runs inside the dev image (scripts/dev);
# deploy/docker/build.sh calls it after sdks/python/build.sh.
#
#   deploy/docker/stage-python.sh STAGE_DIR
#
# The image's base is distroless cc-debian13, the same Debian release (trixie) as the dev
# image, so Debian's interpreter runs on it unchanged. Staged, as a tree laid over the base:
#
#   /usr/bin/python3.13 (and python3)        Debian's interpreter
#   /usr/lib/python3.13                      its standard library, without tests, IDLE, Tk,
#                                            ensurepip, and Debian's sitecustomize (a link to /etc)
#   /usr/lib/python3/dist-packages           NumPy (the version models/gen.py pins) and sidecarzero
#   /usr/lib/<triplet>                       the shared libraries those need that the base lacks
#
# Everything is under /usr, so the supervisor's default sandbox view covers it: the runner
# needs --worker-read only for the user's module and the model.
set -euo pipefail
S=$(realpath -m "$1")
PY=/usr/bin/python3.13
SITE=/opt/py/lib/python3.13/site-packages
PKG=target/sdk/python/sidecarzero
[ -f "$PKG/_core.abi3.so" ] || { echo "stage-python.sh: run sdks/python/build.sh first" >&2; exit 1; }
rm -rf "$S"
mkdir -p "$S/usr/bin" "$S/usr/lib/python3/dist-packages"
cp "$PY" "$S/usr/bin/python3.13"
ln -s python3.13 "$S/usr/bin/python3"
tar -C /usr/lib -cf - \
  --exclude=python3.13/test --exclude=python3.13/idlelib --exclude=python3.13/tkinter --exclude=python3.13/turtledemo \
  --exclude=python3.13/ensurepip --exclude=python3.13/sitecustomize.py --exclude=__pycache__ \
  --exclude='python3.13/lib-dynload/_test*' --exclude='python3.13/lib-dynload/xx*' --exclude='python3.13/lib-dynload/_ctypes_test*' \
  python3.13 | tar -C "$S/usr/lib" -xf -
for d in numpy numpy.libs "$(cd "$SITE" && ls -d numpy-*.dist-info)"; do
  cp -a "$SITE/$d" "$S/usr/lib/python3/dist-packages/"
done
cp -a "$PKG" "$S/usr/lib/python3/dist-packages/"
find "$S" -name __pycache__ -prune -exec rm -rf {} +
# The libraries the interpreter and the extension modules load, less those the base has.
T=$(gcc -print-multiarch)
mkdir -p "$S/usr/lib/$T"
base="libc.so libm.so ld-linux libgcc_s.so libstdc++.so libpthread.so libdl.so librt.so libz.so libssl.so libcrypto.so libzstd.so libgomp.so libutil.so libresolv.so libnsl.so libanl.so libmvec.so"
{ echo "$S/usr/bin/python3.13"; find "$S" -name '*.so*' -type f; } | while read -r f; do ldd "$f" 2>/dev/null || true; done \
  | awk '/=> \//{print $3}' | sort -u | while read -r lib; do
  case $lib in "$S"/*) continue ;; esac # NumPy's own, under numpy.libs
  name=$(basename "$lib")
  skip=0
  for b in $base; do case $name in "$b"*) skip=1 ;; esac; done
  [ $skip = 1 ] || cp -L "$lib" "$S/usr/lib/$T/$name"
done
du -sh "$S" | sed 's/^/  python runtime: /'
ls "$S/usr/lib/$T" | tr '\n' ' ' | sed 's/^/  added libraries: /'
echo
