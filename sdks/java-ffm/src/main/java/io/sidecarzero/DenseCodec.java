package io.sidecarzero;

import java.util.List;

/**
 * The prebuilt dense float32 codec: {@code features: float32[rows, N]} in, {@code score:
 * float32[rows, 1]} out (column names configurable), written into the registry's template
 * frames. The request is one {@code FixedSizeList<float32>[N]} column, the response one
 * {@code float32} column. Rust's {@code FeatureScore} and {@code FeatureBatch} in one.
 *
 * <p>As a {@link Codec}, an input of {@code k × N} floats is {@code k} rows and the output is a
 * new array of {@code k} scores. {@link ScoreClient} and {@link BatchScoreClient} use the
 * allocation-free methods {@link #encodeRows}, {@link #decodeScore}, and {@link #decodeInto},
 * which a subclass may override.
 */
public class DenseCodec implements Codec<float[], float[]> {
    private final int n;
    private final String input, output;

    public DenseCodec(int features) {
        this(features, "features", "score");
    }

    public DenseCodec(int features, String input, String output) {
        if (features <= 0) throw new IllegalArgumentException("features must be positive");
        this.n = features;
        this.input = input;
        this.output = output;
    }

    public final int features() {
        return n;
    }

    @Override
    public Schema schema() {
        return new Schema(List.of(new Column(input, ElemType.F32, n)), List.of(new Column(output, ElemType.F32, 1)));
    }

    @Override
    public int rows(float[] in) {
        if (in.length % n != 0) throw CodecException.SHAPE;
        return in.length / n;
    }

    @Override
    public void encodeFrame(float[] in, FrameWriter w) {
        encodeRows(in, w);
    }

    @Override
    public float[] decodeFrame(FrameReader r) {
        float[] out = new float[r.rows()];
        decodeInto(r, out);
        return out;
    }

    /** Write {@code w.rows()} rows of features, row-major from {@code features[0]}. */
    public void encodeRows(float[] features, FrameWriter w) {
        w.writeF32(0, features, 0);
    }

    /** The first row's score. */
    public float decodeScore(FrameReader r) {
        return r.f32(0, 0);
    }

    /** Every row's score into {@code scores[0..rows)}. */
    public void decodeInto(FrameReader r, float[] scores) {
        r.readF32(0, scores, 0);
    }
}
