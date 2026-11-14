package io.sidecarzero;

/** The fallback of a {@link BatchScoreClient}: writes {@code rows} scores into {@code scores}. */
@FunctionalInterface
public interface BatchFallback {
    void apply(float[] features, int rows, float[] scores, FallbackReason reason);
}
