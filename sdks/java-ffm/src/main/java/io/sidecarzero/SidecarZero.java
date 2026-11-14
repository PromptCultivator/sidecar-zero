package io.sidecarzero;

import java.nio.file.Path;
import java.util.Objects;
import java.util.function.Consumer;

/**
 * The entry point. A staged builder: {@code build()} is reachable only through
 * {@code .fallback(...)}, and a null fallback is rejected (sdk-contract §3.1).
 *
 * <pre>{@code
 * ScoreClient c = SidecarZero.connect("/run/sidecarzero/scz.sock")
 *         .score(256)
 *         .fallback((features, reason) -> 0.0f)
 *         .build();
 * long o = c.scoreOutcome(features, 2_000_000);   // 2 ms
 * }</pre>
 *
 * <p>On Linux the JVM must be started with {@code --enable-native-access=ALL-UNNAMED} (or the
 * module's name); building a client checks. Elsewhere a client builds normally, never
 * connects, and every call falls back with {@code NOT_CONNECTED} (sdk-contract §9).
 */
public final class SidecarZero {
    private SidecarZero() {}

    /** {@code endpoint}: a socket path, or {@code @name} for the abstract namespace. */
    public static EndpointStage connect(String endpoint) {
        Objects.requireNonNull(endpoint, "endpoint");
        return new EndpointStage() {
            @Override
            public <I, O> CodecStage<I, O> codec(Codec<I, O> codec) {
                Objects.requireNonNull(codec, "codec");
                return f -> {
                    Objects.requireNonNull(f, "a fallback is mandatory");
                    return new BuildStage<>() {
                        Options o = Options.defaults();
                        Consumer<FallbackReason> hook;

                        @Override
                        public BuildStage<I, O> options(Options opts) {
                            o = Objects.requireNonNull(opts);
                            return this;
                        }

                        @Override
                        public BuildStage<I, O> onFallback(Consumer<FallbackReason> h) {
                            hook = Objects.requireNonNull(h);
                            return this;
                        }

                        @Override
                        public Client<I, O> build() {
                            return new GenericClient<>(new Engine(endpoint, new Wire.Generic<>(codec), o, hook), f);
                        }
                    };
                };
            }

            @Override
            public ScoreClient.FallbackStage score(int features) {
                return ScoreClient.stage(endpoint, new DenseCodec(features));
            }

            @Override
            public ScoreClient.FallbackStage score(DenseCodec codec) {
                return ScoreClient.stage(endpoint, Objects.requireNonNull(codec, "codec"));
            }

            @Override
            public BatchScoreClient.FallbackStage batch(DenseCodec codec) {
                return BatchScoreClient.stage(endpoint, Objects.requireNonNull(codec, "codec"));
            }
        };
    }

    public static EndpointStage connect(Path socket) {
        return connect(socket.toString());
    }
}
