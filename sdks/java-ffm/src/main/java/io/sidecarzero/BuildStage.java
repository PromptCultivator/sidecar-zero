package io.sidecarzero;

import java.util.function.Consumer;

/** The last stage: options, then {@link #build()}, which returns at once and connects in the background. */
public interface BuildStage<I, O> {
    BuildStage<I, O> options(Options o);

    /**
     * Called with the reason once per fallback, on the calling thread, just before the fallback
     * function, after the slot and the caller entry have been released (sdk-contract §8): for
     * sampled logging. Off by default, and not on the zero-allocation path: what it does costs
     * every fallback. What it throws propagates as the fallback's would.
     */
    BuildStage<I, O> onFallback(Consumer<FallbackReason> hook);

    Client<I, O> build();
}
