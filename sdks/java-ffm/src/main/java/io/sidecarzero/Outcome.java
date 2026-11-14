package io.sidecarzero;

/** What {@link Client#infer} returns: always a value, from the model or from the fallback. */
public final class Outcome<O> {
    private final O value;
    private final FallbackReason reason;
    private final int modelGen;

    Outcome(O value, FallbackReason reason, int modelGen) {
        this.value = value;
        this.reason = reason;
        this.modelGen = modelGen;
    }

    public O value() {
        return value;
    }

    public boolean fromModel() {
        return reason == null;
    }

    /** Why the fallback answered; null when the model did. */
    public FallbackReason reason() {
        return reason;
    }

    /** The model generation that answered (unsigned); meaningful when {@link #fromModel()}. */
    public int modelGen() {
        return modelGen;
    }

    @Override
    public String toString() {
        return fromModel() ? "Outcome[model gen=" + Integer.toUnsignedString(modelGen) + " " + value + "]" : "Outcome[fallback " + reason + " " + value + "]";
    }
}
