# Spike S4: Java reader (Arrow Java).
FROM maven:3.9-eclipse-temurin-21
WORKDIR /s4/java
COPY pom.xml ./
COPY src ./src
# Maven builds single-threaded by default; other spikes build on the same VM.
RUN --mount=type=cache,target=/root/.m2,id=scz-spike-s4-m2 \
    mvn -q -B package \
    && mkdir -p /s4/java/app && cp -r target/classes target/lib /s4/java/app/ \
    && (java -version 2>&1; ls /s4/java/app/lib) > /s4/java/versions.txt
ENTRYPOINT ["java", "--add-opens=java.base/java.nio=ALL-UNNAMED", "-cp", "/s4/java/app/classes:/s4/java/app/lib/*", "s4.Reader"]
