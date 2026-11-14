package io.sidecarzero;

/** The second stage: the fallback, without which there is no {@code build()} (sdk-contract §3.1). */
public interface CodecStage<I, O> {
    /** @throws NullPointerException if {@code fallback} is null */
    BuildStage<I, O> fallback(FallbackPolicy<I, O> fallback);
}
