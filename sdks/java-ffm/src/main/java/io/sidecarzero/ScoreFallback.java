package io.sidecarzero;

/** The fallback of a {@link ScoreClient}: one row's features in, a score out. */
@FunctionalInterface
public interface ScoreFallback {
    float apply(float[] features, FallbackReason reason);
}
