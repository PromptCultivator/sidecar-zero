# The supervisor is built against musl, statically, as the real one is; the worker against glibc.
FROM alpine:3.20 AS sup
RUN apk add --no-cache gcc musl-dev linux-headers
COPY sup.c sup2.c segment.h /src/
RUN gcc -O2 -Wall -static -pthread -o /sup /src/sup.c && gcc -O2 -Wall -static -pthread -o /sup2 /src/sup2.c

FROM gcc:14 AS wrk
COPY worker.c segment.h /src/
RUN gcc -O2 -Wall -pthread -o /worker /src/worker.c

FROM debian:stable-slim
COPY --from=sup /sup /sup2 /spike/
COPY --from=wrk /worker /spike/worker
