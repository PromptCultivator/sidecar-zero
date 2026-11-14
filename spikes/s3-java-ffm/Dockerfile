# Spike S3 image: a statically linked C helper (the stand-in supervisor) plus the
# Java program compiled by the JDK under test.
ARG JDK_IMAGE=eclipse-temurin:25-jdk

FROM alpine:3.22 AS helper
RUN apk add --no-cache gcc musl-dev linux-headers
COPY helper/helper.c /src/helper.c
RUN gcc -O1 -Wall -Wextra -static -o /helper /src/helper.c

FROM ${JDK_IMAGE}
WORKDIR /spike
COPY --from=helper /helper /spike/helper
COPY java/ /spike/java/
# --release 22: the same source must build and run on the JDK 22 baseline.
RUN javac --release 22 -Xlint:all -d /spike/classes /spike/java/S3.java
COPY inner.sh /spike/inner.sh
ENTRYPOINT ["/bin/bash", "/spike/inner.sh"]
