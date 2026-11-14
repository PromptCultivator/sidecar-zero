# The benchmark's service-mode images (bench §4.1), built by bench/service.py from binaries it
# stages in bench/docker/.stage (built in the pinned dev image):
#
#   scz-bench-host     the host application (scz-bench-host serve) for every system, the
#                      baseline server (scz-bench-server) for C and D, ONNX Runtime and the
#                      fraud model for E and the baselines, the reference rows
#   scz-loadgen        the open-loop load generator (static)

ARG CC_BASE=gcr.io/distroless/cc-debian13:nonroot
ARG STATIC_BASE=gcr.io/distroless/static-debian13:nonroot

FROM ${CC_BASE} AS host
COPY .stage/scz-bench-host .stage/scz-bench-server /usr/bin/
COPY .stage/ort/ /opt/ort/lib/
COPY .stage/models/ /models/
USER 65532:65532
ENTRYPOINT ["/usr/bin/scz-bench-host"]

FROM ${STATIC_BASE} AS loadgen
COPY .stage/scz-loadgen /usr/bin/scz-loadgen
USER 65532:65532
ENTRYPOINT ["/usr/bin/scz-loadgen"]
