package io.sidecarzero;

import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;

/** Little-endian layouts for payload regions (requests, responses, frames). */
final class Le {
    private Le() {}

    static final ValueLayout.OfByte BYTE = ValueLayout.JAVA_BYTE;
    static final ValueLayout.OfInt INT = ValueLayout.JAVA_INT_UNALIGNED.withOrder(ByteOrder.LITTLE_ENDIAN);
    static final ValueLayout.OfLong LONG = ValueLayout.JAVA_LONG_UNALIGNED.withOrder(ByteOrder.LITTLE_ENDIAN);
    static final ValueLayout.OfFloat FLOAT = ValueLayout.JAVA_FLOAT_UNALIGNED.withOrder(ByteOrder.LITTLE_ENDIAN);
}
