package io.sidecarzero;

import java.util.Objects;

/**
 * One column of a model's schema, as the fingerprint sees it (arrow-data-plane §9): a name, a
 * physical element type ({@link ElemType}), a list size (1 for a scalar, N for
 * {@code FixedSizeList<T>[N]}, the byte width for {@code FIXED_BINARY}), and nullability.
 */
public final class Column {
    private final String name;
    private final int elemType;
    private final int listSize;
    private final boolean nullable;

    public Column(String name, int elemType, int listSize, boolean nullable) {
        this.name = Objects.requireNonNull(name, "name");
        this.elemType = elemType;
        this.listSize = listSize;
        this.nullable = nullable;
    }

    public Column(String name, int elemType, int listSize) {
        this(name, elemType, listSize, false);
    }

    public String name() {
        return name;
    }

    public int elemType() {
        return elemType;
    }

    public int listSize() {
        return listSize;
    }

    public boolean nullable() {
        return nullable;
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof Column c && c.name.equals(name) && c.elemType == elemType && c.listSize == listSize && c.nullable == nullable;
    }

    @Override
    public int hashCode() {
        return Objects.hash(name, elemType, listSize, nullable);
    }

    @Override
    public String toString() {
        return name + ":" + ElemType.name(elemType) + (nullable ? "?" : "") + "[" + listSize + "]";
    }
}
