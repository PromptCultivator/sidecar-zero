#!/usr/bin/env bash
# Build the Java SDK and its conformance agent into target/sdk/java-ffm (run inside the dev
# container: scripts/dev bash -c 'sdks/java-ffm/build.sh'). javac --release 25 and jar only.
#
#   target/sdk/java-ffm/sidecarzero.jar     the SDK (io.sidecarzero, io.sidecarzero.internal)
#   target/sdk/java-ffm/scz-agent.jar       the conformance agent (io.sidecarzero.agent)
#   target/sdk/java-ffm/scz-agent-java      the agent's launcher, taking --endpoint=PATH
#
# Idempotent and quiet on success; non-zero on failure.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SDK=$ROOT/sdks/java-ffm
OUT=$ROOT/target/sdk/java-ffm
TMP=$OUT/.build
rm -rf "$TMP"
mkdir -p "$TMP/main" "$TMP/agent"
JAVAC=(nice -n 10 javac --release 25 -encoding UTF-8 -Xlint:all,-restricted,-serial -Werror -implicit:none)
"${JAVAC[@]}" -d "$TMP/main" $(find "$SDK/src/main/java" -name '*.java' | sort)
"${JAVAC[@]}" -cp "$TMP/main" -d "$TMP/agent" $(find "$SDK/src/agent/java" -name '*.java' | sort)
jar --create --file "$TMP/sidecarzero.jar" -C "$TMP/main" .
jar --create --file "$TMP/scz-agent.jar" --main-class io.sidecarzero.agent.Main -C "$TMP/agent" .
cat > "$TMP/scz-agent-java" <<'SH'
#!/bin/sh
# The Java SDK's conformance agent. A capped heap with a young generation large enough that the
# agent's own output lines rarely trigger a collection during a scenario, the serial collector,
# and a fixed number of compiler threads, so that the JVM's thread count stays put (the runner
# compares it).
D=$(dirname "$(readlink -f "$0")")
exec java -Xms512m -Xmx512m -Xmn384m -XX:+UseSerialGC -XX:-UseDynamicNumberOfCompilerThreads -XX:CICompilerCount=2 \
  -XX:-UsePerfData -Xshare:auto --enable-native-access=ALL-UNNAMED \
  -cp "$D/sidecarzero.jar:$D/scz-agent.jar" io.sidecarzero.agent.Main "$@"
SH
chmod +x "$TMP/scz-agent-java"
mkdir -p "$OUT/crash"
cat > "$TMP/crash-scz-agent-java" <<'SH'
#!/bin/sh
D=$(dirname "$(readlink -f "$0")")/..
exec java -Xms512m -Xmx512m -Xmn384m -XX:+UseSerialGC -XX:-UseDynamicNumberOfCompilerThreads -XX:CICompilerCount=2 \
  -XX:-UsePerfData -Xshare:auto --enable-native-access=ALL-UNNAMED -Dscz.crashpoints=true \
  -cp "$D/sidecarzero.jar:$D/scz-agent.jar" io.sidecarzero.agent.Main "$@"
SH
chmod +x "$TMP/crash-scz-agent-java"
for f in sidecarzero.jar scz-agent.jar scz-agent-java; do mv -f "$TMP/$f" "$OUT/$f"; done
mv -f "$TMP/crash-scz-agent-java" "$OUT/crash/scz-agent-java"
rm -rf "$OUT/classes"
mv "$TMP/main" "$OUT/classes"
rm -rf "$TMP"
