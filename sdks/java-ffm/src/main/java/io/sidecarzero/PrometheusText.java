package io.sidecarzero;

import java.math.BigDecimal;

/**
 * The Prometheus text exposition format (sdk-contract §8, "A text adapter in the core"), for a
 * service that serves {@code /metrics} itself. One {@code # TYPE} line per metric family, then its
 * series; histograms as {@code _bucket} lines (with {@code le}), {@code _sum}, and {@code _count}.
 * Numbers are written as the Rust SDK writes them (shortest round-trip form, never an exponent:
 * {@code 0.000001}, {@code 1.5}, {@code 2}), so every SDK renders the same numbers as the same text.
 *
 * <pre>{@code
 * PrometheusText t = new PrometheusText();
 * client.export(t);
 * String body = t.finish();      // or client.prometheusText()
 * }</pre>
 */
public final class PrometheusText implements MetricsSink {
    private final StringBuilder out = new StringBuilder();
    private String last = "";

    public PrometheusText() {}

    /** The text written so far. */
    public String finish() {
        return out.toString();
    }

    @Override
    public String toString() {
        return out.toString();
    }

    private void kind(String name, String kind) {
        if (!last.equals(name)) {
            out.append("# TYPE ").append(name).append(' ').append(kind).append('\n');
            last = name;
        }
    }

    private void labelSet(String[] labels, String extraName, String extraValue) {
        int pairs = labels == null ? 0 : labels.length / 2;
        if (labels != null && labels.length % 2 != 0) throw new IllegalArgumentException("labels are name, value pairs: an odd count");
        if (pairs == 0 && extraName == null) return;
        out.append('{');
        for (int i = 0; i < pairs; i++) {
            if (i > 0) out.append(',');
            pair(labels[2 * i], labels[2 * i + 1]);
        }
        if (extraName != null) {
            if (pairs > 0) out.append(',');
            pair(extraName, extraValue);
        }
        out.append('}');
    }

    private void pair(String name, String value) {
        out.append(name).append("=\"");
        for (int i = 0; i < value.length(); i++) {
            char ch = value.charAt(i);
            switch (ch) {
                case '\\' -> out.append("\\\\");
                case '"' -> out.append("\\\"");
                case '\n' -> out.append("\\n");
                default -> out.append(ch);
            }
        }
        out.append('"');
    }

    @Override
    public void counter(String name, String[] labels, long value) {
        kind(name, "counter");
        out.append(name);
        labelSet(labels, null, null);
        out.append(' ').append(Long.toUnsignedString(value)).append('\n');
    }

    @Override
    public void gauge(String name, String[] labels, double value) {
        kind(name, "gauge");
        out.append(name);
        labelSet(labels, null, null);
        out.append(' ').append(number(value)).append('\n');
    }

    @Override
    public void histogram(String name, String[] labels, double[] bounds, long[] cumulative, double sum, long count) {
        kind(name, "histogram");
        for (int i = 0; i < cumulative.length; i++) {
            out.append(name).append("_bucket");
            labelSet(labels, "le", i < bounds.length ? number(bounds[i]) : "+Inf");
            out.append(' ').append(Long.toUnsignedString(cumulative[i])).append('\n');
        }
        out.append(name).append("_sum");
        labelSet(labels, null, null);
        out.append(' ').append(number(sum)).append('\n');
        out.append(name).append("_count");
        labelSet(labels, null, null);
        out.append(' ').append(Long.toUnsignedString(count)).append('\n');
    }

    /**
     * A double as Rust's {@code Display} for {@code f64} writes it: the shortest decimal that
     * reads back as the same double, the closest to it of those, in positional notation, without a
     * trailing {@code .0}; {@code NaN}, {@code inf}, {@code -inf}, and {@code -0} as Rust spells them.
     * Where the double lies exactly halfway between two shortest decimals, Java's
     * {@code Double.toString} takes the even one and Rust the one further from zero: Rust's here.
     */
    static String number(double v) {
        if (Double.isNaN(v)) return "NaN";
        if (Double.isInfinite(v)) return v > 0 ? "inf" : "-inf";
        if (v == 0) return Double.doubleToRawLongBits(v) < 0 ? "-0" : "0";
        BigDecimal d = new BigDecimal(Double.toString(v)).stripTrailingZeros();
        BigDecimal off = new BigDecimal(v).subtract(d);
        if (off.signum() != 0) {
            BigDecimal unit = BigDecimal.ONE.scaleByPowerOfTen(-d.scale());
            if (off.abs().multiply(BigDecimal.TWO).compareTo(unit) == 0) {
                BigDecimal other = off.signum() > 0 ? d.add(unit) : d.subtract(unit);
                if (other.abs().compareTo(d.abs()) > 0 && Double.parseDouble(other.toString()) == v) d = other.stripTrailingZeros();
            }
        }
        return d.toPlainString();
    }
}
