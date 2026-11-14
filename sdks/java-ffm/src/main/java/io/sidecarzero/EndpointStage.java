package io.sidecarzero;

/** The first stage of the builder: choose the codec (or a primitive-specialized client). */
public interface EndpointStage {
    <I, O> CodecStage<I, O> codec(Codec<I, O> codec);

    /** A {@link ScoreClient}: one row of {@code features} float32 features in, one score out. */
    ScoreClient.FallbackStage score(int features);

    ScoreClient.FallbackStage score(DenseCodec codec);

    /** A {@link BatchScoreClient}: rows of features in, as many scores out. */
    BatchScoreClient.FallbackStage batch(DenseCodec codec);
}
