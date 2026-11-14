package io.sidecarzero;

/**
 * A codec as the engine sees it. Each front end has its own, so the engine's calls into it stay
 * few-morphic; results go into the caller entry ({@code outBits}, {@code outObj}) or the front
 * end's own output array, never into a new object.
 */
abstract class Wire {
    /** The codec's columns, or null for opaque bytes. */
    final Schema schema;

    Wire(Schema schema) {
        this.schema = schema;
    }

    /** Bytes of the request (opaque bytes), or its rows (frames). */
    abstract int size(Caller c, Object in);

    void encode(Caller c, Object in, SlotWriter w) {
        throw CodecException.SHAPE;
    }

    void encodeFrame(Caller c, Object in, FrameWriter w) {
        throw CodecException.SHAPE;
    }

    void decode(Caller c, Object out, SlotReader r) {
        throw CodecException.SHAPE;
    }

    void decodeFrame(Caller c, Object out, FrameReader r) {
        throw CodecException.SHAPE;
    }

    /** The generic client's: a user codec, its output kept in the entry until the front end takes it. */
    static final class Generic<I, O> extends Wire {
        final Codec<I, O> codec;

        Generic(Codec<I, O> codec) {
            super(codec.schema());
            this.codec = codec;
        }

        @SuppressWarnings("unchecked")
        @Override
        int size(Caller c, Object in) {
            return schema == null ? codec.requestLen((I) in) : codec.rows((I) in);
        }

        @SuppressWarnings("unchecked")
        @Override
        void encode(Caller c, Object in, SlotWriter w) {
            codec.encode((I) in, w);
        }

        @SuppressWarnings("unchecked")
        @Override
        void encodeFrame(Caller c, Object in, FrameWriter w) {
            codec.encodeFrame((I) in, w);
        }

        @Override
        void decode(Caller c, Object out, SlotReader r) {
            c.outObj = codec.decode(r);
        }

        @Override
        void decodeFrame(Caller c, Object out, FrameReader r) {
            c.outObj = codec.decodeFrame(r);
        }
    }

    /** One row of features in, one score out, in the entry's {@code outBits}. */
    static final class Score extends Wire {
        final DenseCodec codec;

        Score(DenseCodec codec) {
            super(codec.schema());
            this.codec = codec;
        }

        @Override
        int size(Caller c, Object in) {
            return 1;
        }

        @Override
        void encodeFrame(Caller c, Object in, FrameWriter w) {
            codec.encodeRows((float[]) in, w);
        }

        @Override
        void decodeFrame(Caller c, Object out, FrameReader r) {
            c.outBits = Float.floatToRawIntBits(codec.decodeScore(r));
        }
    }

    /** {@code rows} rows of features in, as many scores out, into the caller's array. */
    static final class Batch extends Wire {
        final DenseCodec codec;

        Batch(DenseCodec codec) {
            super(codec.schema());
            this.codec = codec;
        }

        @Override
        int size(Caller c, Object in) {
            return c.argRows;
        }

        @Override
        void encodeFrame(Caller c, Object in, FrameWriter w) {
            codec.encodeRows((float[]) in, w);
        }

        @Override
        void decodeFrame(Caller c, Object out, FrameReader r) {
            codec.decodeInto(r, (float[]) out);
        }
    }
}
