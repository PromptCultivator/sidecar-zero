package io.sidecarzero;

/**
 * The caller's own answer, used whenever the model's is not available in time. It must be pure,
 * fast, allocation-free, and accept any input, including one the codec rejected (sdk-contract
 * §3.2). If it throws, {@code infer} throws, after the SDK has released everything it held.
 */
@FunctionalInterface
public interface FallbackPolicy<I, O> {
    O apply(I input, FallbackReason reason);
}
