#!/usr/bin/env bash
# Build the Java benchmark host (bench §4.2, in-process mode) into target/sdk/java-bench (inside
# the dev container): the Java SDK first (sdks/java-ffm/build.sh), then this program, and a
# launcher, scz-bench-host-java. javac --release 25 and jar only. Quiet on success.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SRC=$ROOT/bench/host-java/src
OUT=$ROOT/target/sdk/java-bench
"$ROOT/sdks/java-ffm/build.sh"
SDK=$ROOT/target/sdk/java-ffm/sidecarzero.jar
TMP=$OUT/.build
rm -rf "$TMP"
mkdir -p "$TMP/classes"
nice -n 10 javac --release 25 -encoding UTF-8 -Xlint:all,-restricted,-serial -Werror -implicit:none -cp "$SDK" -d "$TMP/classes" \
  $(find "$SRC" -name '*.java' | sort)
jar --create --file "$TMP/scz-bench-host.jar" --main-class io.sidecarzero.bench.HostMain -C "$TMP/classes" .
cp "$SDK" "$TMP/sidecarzero.jar"
cat > "$TMP/scz-bench-host-java" <<'SH'
#!/bin/sh
# The Java benchmark host. A capped heap with a young generation large enough that system A,
# which allocates nothing per call, sees no collection in a run; the serial collector.
D=$(dirname "$(readlink -f "$0")")
exec java -Xms512m -Xmx512m -Xmn384m -XX:+UseSerialGC -XX:-UsePerfData --enable-native-access=ALL-UNNAMED \
  -cp "$D/sidecarzero.jar:$D/scz-bench-host.jar" io.sidecarzero.bench.HostMain "$@"
SH
chmod +x "$TMP/scz-bench-host-java"
for f in scz-bench-host.jar sidecarzero.jar scz-bench-host-java; do mv -f "$TMP/$f" "$OUT/$f"; done
rm -rf "$TMP"
