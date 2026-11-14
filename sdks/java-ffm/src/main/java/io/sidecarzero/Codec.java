package io.sidecarzero;

/**
 * How an input becomes a request and a response becomes an output (sdk-contract §2). A codec
 * either declares the model's columns ({@link #schema()}) and writes and reads Arrow frames laid
 * out by the registry's templates (arrow-data-plane §7), or declares none and carries opaque
 * bytes. A codec with columns binds only to a registry with the same schema fingerprint; any
 * other pairing falls back with {@code SCHEMA_MISMATCH}.
 *
 * <p>The writers and readers are valid only during the call that passes them; never keep one.
 * A codec that throws resolves the call to {@code CODEC_ERROR} (use {@link CodecException}'s
 * preallocated instances to fail without allocating).
 */
public interface Codec<I, O> {
    /** The request and response columns, or null for opaque bytes. Read once, at build time. */
    default Schema schema() {
        return null;
    }

    // ── opaque bytes ───────────────────────────────────────────────────────────────────────────

    /** Bytes the request needs; the smallest slot class whose request region holds them is used. */
    default int requestLen(I input) {
        return 0;
    }

    default void encode(I input, SlotWriter w) {
        throw CodecException.SHAPE;
    }

    default O decode(SlotReader r) {
        throw CodecException.SHAPE;
    }

    // ── Arrow frames ───────────────────────────────────────────────────────────────────────────

    /** Rows the request carries; the smallest class whose template holds them is used. */
    default int rows(I input) {
        return 1;
    }

    /** Write the request's column values. The frame's metadata is already in place. */
    default void encodeFrame(I input, FrameWriter w) {
        throw CodecException.SHAPE;
    }

    default O decodeFrame(FrameReader r) {
        throw CodecException.SHAPE;
    }
}
