package io.sidecarzero;

/**
 * A codec's failure: the input or the response does not have the shape the codec expects, or
 * does not fit. The two instances are preallocated and carry no stack trace, so throwing one
 * allocates nothing. Any exception a codec throws resolves the call to its fallback with
 * {@link FallbackReason#CODEC_ERROR}.
 */
public final class CodecException extends RuntimeException {
    public static final CodecException SHAPE = new CodecException("the value does not have the codec's shape");
    public static final CodecException OVERFLOW = new CodecException("the value does not fit its region");

    private CodecException(String s) {
        super(s, null, false, false);
    }
}
