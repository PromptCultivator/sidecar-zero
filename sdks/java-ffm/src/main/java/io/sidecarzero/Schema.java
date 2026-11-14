package io.sidecarzero;

import io.sidecarzero.internal.Xxh3;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Objects;

/** A model's request and response columns, and their fingerprint (arrow-data-plane §9). */
public final class Schema {
    private final List<Column> request;
    private final List<Column> response;

    public Schema(List<Column> request, List<Column> response) {
        this.request = List.copyOf(request);
        this.response = List.copyOf(response);
    }

    public List<Column> request() {
        return request;
    }

    public List<Column> response() {
        return response;
    }

    /**
     * xxHash3-64, seed 0, over {@code "SCZSCHEMA" 0x01}, then per direction the column count (u16)
     * and per column name length (u16), name (UTF-8), element type (u8), list size (u32),
     * nullable (u8), all little-endian.
     */
    public long fingerprint() {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        b.writeBytes("SCZSCHEMA".getBytes(StandardCharsets.US_ASCII));
        b.write(1);
        for (List<Column> cols : List.of(request, response)) {
            le(b, cols.size(), 2);
            for (Column c : cols) {
                byte[] n = c.name().getBytes(StandardCharsets.UTF_8);
                le(b, n.length, 2);
                b.writeBytes(n);
                b.write(c.elemType());
                le(b, c.listSize(), 4);
                b.write(c.nullable() ? 1 : 0);
            }
        }
        return Xxh3.hash(b.toByteArray());
    }

    private static void le(ByteArrayOutputStream b, long v, int n) {
        for (int i = 0; i < n; i++) b.write((int) (v >>> (8 * i)) & 0xFF);
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof Schema s && s.request.equals(request) && s.response.equals(response);
    }

    @Override
    public int hashCode() {
        return Objects.hash(request, response);
    }

    @Override
    public String toString() {
        return request + " -> " + response;
    }
}
