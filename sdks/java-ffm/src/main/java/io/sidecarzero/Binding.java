package io.sidecarzero;

import io.sidecarzero.internal.Registry;

/** How the codec meets the published registry (arrow-data-plane §9). */
final class Binding {
    static final int RAW = 0, FRAMES = 1, MISMATCH = 2, UNBOUND = 3;
    final int mode;
    final int epoch;
    final Registry reg;
    /** The gate generation this binding was last confirmed under; updated by request threads. */
    int confirmedGen;

    Binding(int mode, int epoch, Registry reg, int confirmedGen) {
        this.mode = mode;
        this.epoch = epoch;
        this.reg = reg;
        this.confirmedGen = confirmedGen;
    }
}
