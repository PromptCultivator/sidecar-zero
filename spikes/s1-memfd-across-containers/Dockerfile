FROM gcc:14 AS build
COPY src/ /src/
RUN cd /src && gcc -O2 -Wall -static -o /sup sup.c && gcc -O2 -Wall -static -o /cli cli.c
COPY s7-probes/ /s7/
RUN cd /s7 && gcc -O2 -Wall -static -o /clone3_probe clone3_probe.c

# A small runtime image, so it loads into Kind quickly. The binaries are static.
FROM debian:stable-slim
COPY --from=build /sup /cli /clone3_probe /spike/
