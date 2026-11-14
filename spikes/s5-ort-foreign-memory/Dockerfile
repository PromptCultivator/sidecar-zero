# Spike S5 - ONNX Runtime over foreign memory. Throwaway.
# Everything is pinned: base image family, ONNX Runtime release (by sha256), Python packages.
FROM python:3.12-slim-bookworm

ARG ORT_VERSION=1.31.1
# Published digests of the two official release archives (GitHub's release API, 2026-11-12).
ARG ORT_SHA256_AARCH64=5317505e2da99be65f8e2775ba3c2afd6d2961fd78aba1bd8f56159f48820c78
ARG ORT_SHA256_X64=c849e36a1ef3e22d4ff217b9d1ce4a3cf5b9467b975610913ae959ad5b16bdc9
ARG TARGETARCH

# g++ and binutils (objdump) to build and inspect; libjemalloc2 is the stand-in for
# "an allocator with configurable decay that replaces malloc for every library".
RUN apt-get update \
 && apt-get install -y --no-install-recommends g++ binutils libjemalloc2 \
 && rm -rf /var/lib/apt/lists/*

# Official prebuilt ONNX Runtime for this architecture (headers + libonnxruntime.so), checked
# against its published digest before it is unpacked.
RUN set -e; arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      arm64) name=aarch64; sum=$ORT_SHA256_AARCH64 ;; \
      amd64) name=x64;     sum=$ORT_SHA256_X64 ;; \
      *) echo "no ONNX Runtime build pinned for $arch"; exit 1 ;; \
    esac; \
    python3 -c 'import hashlib, sys, urllib.request; d = urllib.request.urlopen(sys.argv[1]).read(); h = hashlib.sha256(d).hexdigest(); sys.exit("digest mismatch: " + h) if h != sys.argv[2] else open("/opt/ort.tgz", "wb").write(d)' \
      "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${name}-${ORT_VERSION}.tgz" "$sum"; \
    mkdir -p /opt/ort && tar -xzf /opt/ort.tgz -C /opt/ort --strip-components=1 && rm /opt/ort.tgz

# Same ONNX Runtime version from PyPI, used only to convert .onnx to ORT format.
RUN pip install --no-cache-dir onnx==1.23.2 onnxruntime==${ORT_VERSION} numpy==2.3.4

WORKDIR /work
COPY gen_models.py /work/
RUN mkdir -p /work/models && python gen_models.py /work/models > /work/gen.log 2>&1; rc=$?; cat /work/gen.log; mv /work/gen.log /work/models/gen.log; exit $rc

COPY s5.cc mhook.c /work/
RUN gcc -O2 -fPIC -shared -o /work/libmhook.so mhook.c \
 && g++ -O2 -g -std=c++17 -Wall -Wno-unused-function -I/opt/ort/include s5.cc \
        -L/opt/ort/lib -lonnxruntime -Wl,-rpath,/opt/ort/lib -ldl -lpthread -o /work/s5

COPY inside.sh share.py /work/
ENV ORT_VERSION=${ORT_VERSION}
CMD ["bash", "/work/inside.sh"]
