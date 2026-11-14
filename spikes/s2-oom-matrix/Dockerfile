FROM gcc:14 AS build
COPY oomtest.c /src/
RUN gcc -O2 -Wall -static -o /oomtest /src/oomtest.c
FROM debian:stable-slim
COPY --from=build /oomtest /spike/oomtest
