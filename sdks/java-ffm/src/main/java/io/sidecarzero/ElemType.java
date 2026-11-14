package io.sidecarzero;

import io.sidecarzero.internal.Abi;

/** Physical element types of a column ({@code abi/scz_abi.toml} {@code [enums.elem_type]}). */
public final class ElemType {
    private ElemType() {}

    public static final int I8 = Abi.ElemType.I8, I16 = Abi.ElemType.I16, I32 = Abi.ElemType.I32, I64 = Abi.ElemType.I64;
    public static final int U8 = Abi.ElemType.U8, U16 = Abi.ElemType.U16, U32 = Abi.ElemType.U32, U64 = Abi.ElemType.U64;
    public static final int F16 = Abi.ElemType.F16, F32 = Abi.ElemType.F32, F64 = Abi.ElemType.F64;
    public static final int FIXED_BINARY = Abi.ElemType.FIXED_BINARY;

    public static String name(int t) {
        return Abi.ElemType.name(t);
    }

    /** Bytes per element, or -1 for an unknown code. */
    public static int width(int t) {
        return switch (t) {
            case 1, 5, 12 -> 1;
            case 2, 6, 9 -> 2;
            case 3, 7, 10 -> 4;
            case 4, 8, 11 -> 8;
            default -> -1;
        };
    }
}
