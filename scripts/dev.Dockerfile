# The build and test image that scripts/dev runs everything in. Everything is pinned: the
# Rust toolchain, ONNX Runtime (by the digests of its official release archives), the Python
# packages that generate and check the test models, and the toolchains of the other SDKs
# (Debian's signed packages of the trixie release the Rust image is built on).
#
# scripts/dev tags the image with a hash of this file, so editing it rebuilds the image.
FROM rust:1.99

# The musl target: the supervisor, sczctl, and the demo host ship as static binaries (deploy/docker).
RUN rustup component add clippy rustfmt && rustup target add "$(uname -m)-unknown-linux-musl"

# Python for the model generator (models/gen.py) and the golden-vector generator
# (abi/golden/generate.py, which needs xxhash for the schema fingerprint).
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3-venv \
 && rm -rf /var/lib/apt/lists/*

# The other SDKs' toolchains (phase 4): JDK 25 for the Java SDK (FFM is final from JDK 22),
# Go 1.24, and g++ 14 (in the base image) for C++20. The deployment tests' clients (phase 5):
# Docker's CLI, which Kind drives through the mounted socket, and kubectl. protoc for the
# benchmark's Triton gRPC client (phase 6).
RUN apt-get update \
 && apt-get install -y --no-install-recommends openjdk-25-jdk-headless=25.0.5+8-1~deb13u1 golang-1.24-go=1.24.4-1 \
      docker-cli=26.1.5+dfsg1-9+deb13u1 kubernetes-client=1.32.3+ds-2 protobuf-compiler=3.21.12-11+deb13u1 \
 && rm -rf /var/lib/apt/lists/*

# Kind and Helm for the deployment tests (deploy/kind), each checked against its published digest.
ARG KIND_VERSION=v0.33.0
ARG KIND_SHA256_ARM64=20022bee6cfcd5086cb7234d218e3454e6090022f2a8f55d1fa7fcf42c3867a2
ARG KIND_SHA256_AMD64=aee6151561422756b764a4ae28e7f44cda5af5a9eead3cc9985112b1de8d8e0d
ARG HELM_VERSION=v3.22.1
ARG HELM_SHA256_ARM64=4c29e286b279d170fa048ddfc3e4e83dcd069ca9c52102766ba672d7149e694d
ARG HELM_SHA256_AMD64=896cf63684a42a723d61c980369d6e588330927deb13676ca0d85006dd6da0ea
RUN set -e; arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      arm64) ks=$KIND_SHA256_ARM64; hs=$HELM_SHA256_ARM64 ;; \
      amd64) ks=$KIND_SHA256_AMD64; hs=$HELM_SHA256_AMD64 ;; \
      *) echo "no Kind or Helm pinned for $arch"; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/kind "https://github.com/kubernetes-sigs/kind/releases/download/${KIND_VERSION}/kind-linux-${arch}"; \
    echo "$ks  /usr/local/bin/kind" | sha256sum -c -; chmod 755 /usr/local/bin/kind; \
    curl -fsSL -o /tmp/helm.tgz "https://get.helm.sh/helm-${HELM_VERSION}-linux-${arch}.tar.gz"; \
    echo "$hs  /tmp/helm.tgz" | sha256sum -c -; \
    tar -xzf /tmp/helm.tgz -C /usr/local/bin --strip-components=1 "linux-${arch}/helm"; rm /tmp/helm.tgz

# The official prebuilt ONNX Runtime (headers and libonnxruntime.so), the release spike S5
# measured, checked against its published digest before it is unpacked.
ARG ORT_VERSION=1.31.1
ARG ORT_SHA256_AARCH64=5317505e2da99be65f8e2775ba3c2afd6d2961fd78aba1bd8f56159f48820c78
ARG ORT_SHA256_X64=c849e36a1ef3e22d4ff217b9d1ce4a3cf5b9467b975610913ae959ad5b16bdc9
ARG TARGETARCH
RUN set -e; arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
      arm64) name=aarch64; sum=$ORT_SHA256_AARCH64 ;; \
      amd64) name=x64;     sum=$ORT_SHA256_X64 ;; \
      *) echo "no ONNX Runtime build pinned for $arch"; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/ort.tgz "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-${name}-${ORT_VERSION}.tgz"; \
    echo "$sum  /tmp/ort.tgz" | sha256sum -c -; \
    mkdir -p /opt/ort && tar -xzf /tmp/ort.tgz -C /opt/ort --strip-components=1 && rm /tmp/ort.tgz

# The same ONNX Runtime from PyPI, used only to build the test models, convert them to ORT
# format, and compute their reference outputs.
RUN python3 -m venv /opt/py \
 && /opt/py/bin/pip install --no-cache-dir onnx==1.23.2 onnxruntime==${ORT_VERSION} numpy==2.3.4 xxhash==3.5.0

# llama.cpp for the GGUF backend (phase 7, crates/scz-worker-gguf): the b11988 release's source,
# checked against its digest, built CPU-only into static libraries in /opt/llama, and the bge-small
# embedding model its tests run, in GGUF f16 from ggml-org's model repository. Portable builds:
# armv8.2-a with dot products and half floats on arm64 (Apple silicon, Graviton 2 and later), AVX2
# on amd64. ggml's own thread pool instead of OpenMP, so the worker links statically.
ARG LLAMA_TAG=b11988
ARG LLAMA_SHA256=cc27b4158b6737cdd445e01091a7ec25d88d7051d0b928ec95613e470b21557e
ARG BGE_SHA256=f0b2fef971e8366438bfd2d9aefea1b0115919389448806d290237f638bae999
RUN set -e; apt-get update; apt-get install -y --no-install-recommends cmake=3.31.6-2; rm -rf /var/lib/apt/lists/*; \
    case "$(dpkg --print-architecture)" in \
      arm64) cpu="-DGGML_CPU_ARM_ARCH=armv8.2-a+dotprod+fp16" ;; \
      amd64) cpu="-DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON" ;; \
      *) echo "no llama.cpp build flags for this architecture"; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/llama.tgz "https://github.com/ggml-org/llama.cpp/archive/refs/tags/${LLAMA_TAG}.tar.gz"; \
    echo "$LLAMA_SHA256  /tmp/llama.tgz" | sha256sum -c -; \
    mkdir /tmp/llama && tar -xzf /tmp/llama.tgz -C /tmp/llama --strip-components=1; \
    cmake -S /tmp/llama -B /tmp/llama/build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/llama \
      -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DGGML_OPENMP=OFF $cpu \
      -DLLAMA_BUILD_COMMON=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
      -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_APP=OFF -DLLAMA_OPENSSL=OFF; \
    cmake --build /tmp/llama/build -j 6; cmake --install /tmp/llama/build; rm -rf /tmp/llama /tmp/llama.tgz; \
    mkdir -p /opt/models; \
    curl -fsSL -o /opt/models/bge-small-f16.gguf "https://huggingface.co/ggml-org/models/resolve/main/bert-bge-small/ggml-model-f16.gguf"; \
    echo "$BGE_SHA256  /opt/models/bge-small-f16.gguf" | sha256sum -c -

# Go's caches live in the target volume, so they survive between runs.
ENV ORT_DIR=/opt/ort LLAMA_DIR=/opt/llama PATH=/opt/py/bin:/usr/lib/go-1.24/bin:$PATH \
    GOPATH=/w/target/go GOCACHE=/w/target/go/cache GOFLAGS=-mod=mod GOTOOLCHAIN=local GOTELEMETRY=off
