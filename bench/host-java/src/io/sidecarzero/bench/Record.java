package io.sidecarzero.bench;

import java.util.Arrays;

/** Everything one load loop records, per thread and then merged ({@code bench/common} Record). */
final class Record {
    final Hist latency = new Hist(), modelLatency = new Hist(), fallbackLatency = new Hist();
    final Hist fallbackEntry = new Hist(), admissionEntry = new Hist(), sendLag = new Hist();
    long model, error, timeout;
    final long[] fallback = new long[Ref.REASONS.length];
    /** Per 100 ms from the start: model, fallback, error or timeout, maximum latency. */
    long[][] timeline = new long[0][];

    /** {@code reason}: -1 for a model answer; {@code entryNs}: -1 for none. */
    void add(int reason, long latencyNs, long entryNs, long sinceStartNs) {
        latency.record(latencyNs);
        int slot = (int) (sinceStartNs / 100_000_000L);
        if (timeline.length <= slot) {
            int n = timeline.length;
            timeline = Arrays.copyOf(timeline, slot + 1);
            for (int i = n; i <= slot; i++) timeline[i] = new long[4];
        }
        long[] t = timeline[slot];
        if (Long.compareUnsigned(latencyNs, t[3]) > 0) t[3] = latencyNs;
        if (reason < 0) {
            model++;
            modelLatency.record(latencyNs);
            t[0]++;
        } else {
            fallback[reason]++;
            fallbackLatency.record(latencyNs);
            if (entryNs >= 0) {
                fallbackEntry.record(entryNs);
                if (Ref.isAdmission(reason)) admissionEntry.record(entryNs);
            }
            t[1]++;
        }
    }

    void merge(Record o) {
        latency.merge(o.latency);
        modelLatency.merge(o.modelLatency);
        fallbackLatency.merge(o.fallbackLatency);
        fallbackEntry.merge(o.fallbackEntry);
        admissionEntry.merge(o.admissionEntry);
        sendLag.merge(o.sendLag);
        model += o.model;
        for (int i = 0; i < fallback.length; i++) fallback[i] += o.fallback[i];
        error += o.error;
        timeout += o.timeout;
        if (timeline.length < o.timeline.length) {
            int n = timeline.length;
            timeline = Arrays.copyOf(timeline, o.timeline.length);
            for (int i = n; i < timeline.length; i++) timeline[i] = new long[4];
        }
        for (int i = 0; i < o.timeline.length; i++) {
            long[] a = timeline[i], b = o.timeline[i];
            a[0] += b[0];
            a[1] += b[1];
            a[2] += b[2];
            if (Long.compareUnsigned(b[3], a[3]) > 0) a[3] = b[3];
        }
    }

    long fallbacks() {
        long s = 0;
        for (long v : fallback) s += v;
        return s;
    }

    String json() {
        StringBuilder fb = new StringBuilder();
        for (int i = 0; i < Ref.REASONS.length; i++) {
            if (fallback[i] > 0) {
                if (fb.length() > 0) fb.append(',');
                fb.append('"').append(Ref.REASONS[i]).append("\":").append(fallback[i]);
            }
        }
        StringBuilder tl = new StringBuilder();
        for (long[] t : timeline) {
            if (tl.length() > 0) tl.append(',');
            tl.append('[').append(t[0]).append(',').append(t[1]).append(',').append(t[2]).append(',').append(Long.toUnsignedString(t[3])).append(']');
        }
        return "{\"model\":" + model + ",\"fallback\":{" + fb + "},\"error\":" + error + ",\"timeout\":" + timeout + ",\"latency_ns\":" + latency.json()
                + ",\"model_latency_ns\":" + modelLatency.json() + ",\"fallback_latency_ns\":" + fallbackLatency.json() + ",\"fallback_entry_ns\":"
                + fallbackEntry.json() + ",\"admission_entry_ns\":" + admissionEntry.json() + ",\"send_lag_ns\":" + sendLag.json() + ",\"timeline_100ms\":[" + tl
                + "]}";
    }
}
