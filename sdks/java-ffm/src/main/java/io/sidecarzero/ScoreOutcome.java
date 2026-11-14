package io.sidecarzero;

/**
 * Decoders for the packed outcome {@link ScoreClient#scoreOutcome} and
 * {@link BatchScoreClient#scoreInto} return (sdk-java §4): a {@code long} carrying a 32-bit
 * value (the score, or for a batch the model generation) and, in bits 32–39, 0 for a model
 * answer or the fallback reason's ordinal plus one. No allocation.
 */
public final class ScoreOutcome {
    private ScoreOutcome() {}

    static long model(int bits) {
        return bits & 0xFFFF_FFFFL;
    }

    static long fallback(int bits, FallbackReason r) {
        return (bits & 0xFFFF_FFFFL) | ((long) (r.ordinal() + 1) << 32);
    }

    /** The score, from the model or from the fallback ({@link ScoreClient}). */
    public static float score(long packed) {
        return Float.intBitsToFloat((int) packed);
    }

    /** The model generation that answered ({@link BatchScoreClient}; unsigned). */
    public static int modelGen(long packed) {
        return (int) packed;
    }

    public static boolean fromModel(long packed) {
        return ((packed >>> 32) & 0xFF) == 0;
    }

    /** Why the fallback answered, or null for a model answer. */
    public static FallbackReason reason(long packed) {
        int r = (int) ((packed >>> 32) & 0xFF);
        return r == 0 ? null : FallbackReason.of(r - 1);
    }
}
