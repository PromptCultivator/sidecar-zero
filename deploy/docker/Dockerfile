# The SidecarZero images (kubernetes-deploy §2). deploy/docker/build.sh builds the binaries in
# the pinned dev image, stages them in deploy/docker/.stage, and builds each target here:
#
#   sidecarzero-ort     the supervisor (static, PID 1), sczctl, and the ONNX Runtime worker
#   sidecarzero-chaos   the same supervisor with the chaos worker, for the deployment tests only
#   sidecarzero-ort-fraud  sidecarzero-ort with the test fraud model baked in (deployment tests)
#   sidecarzero-gguf    the supervisor with the GGUF embedding worker (llama.cpp, worker §7.2)
#   sidecarzero-gguf-bge   sidecarzero-gguf with bge-small (f16) baked in (deployment tests)
#   sidecarzero-python  the supervisor with the Python runner (sdk-python §5): Debian's
#                       interpreter, NumPy, and sidecarzero; a model's image derives from it
#   sidecarzero-python-demo  sidecarzero-python with the SDK's NumPy test model (deployment tests)
#   scz-demo-host       the deployment tests' host application (static)
#
# No shell and no init wrapper: the supervisor is the ENTRYPOINT. Non-root (distroless's
# nonroot user); nothing in the image is written at run time.

ARG CC_BASE=gcr.io/distroless/cc-debian13:nonroot
ARG STATIC_BASE=gcr.io/distroless/static-debian13:nonroot

FROM ${CC_BASE} AS ort
COPY .stage/sidecarzero-supervisor .stage/sczctl /usr/bin/
COPY .stage/sidecarzero-worker /usr/lib/sidecarzero/sidecarzero-worker
# Where the worker's rpath looks, and inside the sandbox's read-only view.
COPY .stage/ort/ /opt/ort/lib/
USER 65532:65532
ENTRYPOINT ["/usr/bin/sidecarzero-supervisor", "--worker=/usr/lib/sidecarzero/sidecarzero-worker"]

# A model baked into a derived image, the simplest delivery (kubernetes-deploy §2).
FROM ort AS ort-fraud
COPY .stage/models/fraud.ort /models/fraud.ort

FROM ${CC_BASE} AS chaos
COPY .stage/sidecarzero-supervisor .stage/sczctl /usr/bin/
COPY .stage/scz-worker-chaos /usr/lib/sidecarzero/scz-worker-chaos
USER 65532:65532
ENTRYPOINT ["/usr/bin/sidecarzero-supervisor", "--worker=/usr/lib/sidecarzero/scz-worker-chaos"]

# llama.cpp is linked in statically; the C++ runtime it needs is in the cc base.
FROM ${CC_BASE} AS gguf
COPY .stage/sidecarzero-supervisor .stage/sczctl /usr/bin/
COPY .stage/sidecarzero-worker-gguf /usr/lib/sidecarzero/sidecarzero-worker-gguf
USER 65532:65532
ENTRYPOINT ["/usr/bin/sidecarzero-supervisor", "--worker=/usr/lib/sidecarzero/sidecarzero-worker-gguf"]

FROM gguf AS gguf-bge
COPY .stage/models/bge-small-f16.gguf /models/bge-small-f16.gguf

# The interpreter is the worker; the runner's own arguments come first (sdk-python §5). The pod
# adds the module (--worker-arg=PATH or a module name on PYTHONPATH), --worker-read for its
# directory, and --model. Everything the runtime reads is under /usr, inside the default
# sandbox view. Socket pairs are allowed: asyncio and multiprocessing use them.
FROM ${CC_BASE} AS python
COPY .stage/python/ /
COPY .stage/sidecarzero-supervisor .stage/sczctl /usr/bin/
ENV PYTHONDONTWRITEBYTECODE=1 OPENBLAS_NUM_THREADS=1
USER 65532:65532
ENTRYPOINT ["/usr/bin/sidecarzero-supervisor", "--worker=/usr/bin/python3.13", "--worker-arg=-B", "--worker-arg=-m", "--worker-arg=sidecarzero.worker", "--worker-socketpair"]

# The SDK's NumPy test model (sdks/python/tests/models/numpy_model.py) and its weights.
FROM python AS python-demo
COPY .stage/demo/ /app/

FROM ${STATIC_BASE} AS demo-host
COPY .stage/scz-demo-host /usr/bin/scz-demo-host
USER 65532:65532
ENTRYPOINT ["/usr/bin/scz-demo-host"]
