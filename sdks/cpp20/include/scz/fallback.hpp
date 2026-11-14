// Fallbacks and their reasons (sdk-contract §3), the outcome of a call, and the reference
// fallbacks of §3.2.
#pragma once
#include <array>
#include <concepts>
#include <cstddef>
#include <cstdint>
#include <span>
#include <string_view>
#include <type_traits>

namespace scz::inline v1 {

/// Why a call returned the fallback's answer: the label set of the fallback metric, the same
/// in every SDK. A minor version may add reasons; code that switches over it keeps a default.
enum class FallbackReason : std::uint8_t {
    NotConnected,
    AbiMismatch,
    SchemaMismatch,
    Rebinding,
    GateInit,
    GateShedding,
    GateReloading,
    GateDead,
    GateDraining,
    StaleHeartbeat,
    ClockFault,  // never raised: this SDK reads CLOCK_MONOTONIC itself and has no cheap clock
    Overloaded,
    TooLarge,
    NoSlot,
    CodecError,
    RingFull,
    Deadline,
    WorkerFailed,
    ModelError,
    BadRequest,
    ResourceExhausted,
};

inline constexpr std::size_t kReasonCount = 21;

/// The metric label.
[[nodiscard]] constexpr std::string_view reason_name(FallbackReason r) noexcept {
    constexpr std::array<std::string_view, kReasonCount> names = {
        "NOT_CONNECTED", "ABI_MISMATCH", "SCHEMA_MISMATCH", "REBINDING", "GATE_INIT",  "GATE_SHEDDING", "GATE_RELOADING",
        "GATE_DEAD",     "GATE_DRAINING", "STALE_HEARTBEAT", "CLOCK_FAULT", "OVERLOADED", "TOO_LARGE",     "NO_SLOT",
        "CODEC_ERROR",   "RING_FULL",     "DEADLINE",        "WORKER_FAILED", "MODEL_ERROR", "BAD_REQUEST", "RESOURCE_EXHAUSTED",
    };
    const auto i = static_cast<std::size_t>(r);
    return i < names.size() ? names[i] : std::string_view("UNKNOWN");
}

/// Where an answer came from.
enum class Source : std::uint8_t { Model, Fallback };

/// What `infer` returns: always a value.
template <class Out>
struct Outcome {
    Out value;
    Source source;
    FallbackReason reason;     // meaningful when source == Fallback
    std::uint32_t model_gen;   // meaningful when source == Model
    [[nodiscard]] bool is_model() const noexcept { return source == Source::Model; }
};

/// sdk-contract §3.1 for C++: the fallback must be invocable on the input and a reason,
/// return the output type, and be noexcept. A lambda without `noexcept`, or with the wrong
/// return type, is rejected here with a concept diagnostic.
template <class F, class In, class Out>
concept FallbackPolicy = std::is_nothrow_invocable_r_v<Out, const F&, const In&, FallbackReason> && std::is_nothrow_move_constructible_v<F>;

/// The `on_fallback` hook of sdk-contract §8: called with the reason once per fallback, on the
/// calling thread, just before the fallback function, after the slot and the caller entry are
/// released; for sampled logging. A template parameter of the client, as the fallback is, so
/// that it is inlined; a `std::function<void(FallbackReason)>` qualifies too. It need not be
/// noexcept: what it throws propagates as the fallback's would (infer is noexcept, so an
/// application build terminates). Not on the zero-allocation path: what it does costs every
/// fallback.
template <class H>
concept FallbackHook = std::invocable<const H&, FallbackReason> && std::is_nothrow_move_constructible_v<H>;

/// No hook, the default: nothing is called.
struct NoHook {
    void operator()(FallbackReason) const noexcept {}
};

// ── reference fallbacks (sdk-contract §3.2): pure, allocation-free, bounded time ─────────
namespace fallbacks {

/// Always the same value.
template <class In, class Out>
struct Constant {
    Out value;
    [[nodiscard]] Out operator()(const In&, FallbackReason) const noexcept { return value; }
};

/// `above` if feature `index` exceeds `threshold`, else `below`; `below` for a short input.
template <std::size_t N>
struct Threshold {
    std::size_t index = 0;
    float threshold = 0.5f;
    float above = 1.0f;
    float below = 0.0f;
    [[nodiscard]] float operator()(const std::array<float, N>& x, FallbackReason) const noexcept { return index < N && x[index] > threshold ? above : below; }
};

/// `bias + Σ weights[i] · x[i]`.
template <std::size_t N>
struct Linear {
    std::array<float, N> weights{};
    float bias = 0.0f;
    [[nodiscard]] float operator()(const std::array<float, N>& x, FallbackReason) const noexcept {
        float s = bias;
        for (std::size_t i = 0; i < N; ++i) s += weights[i] * x[i];
        return s;
    }
};

}  // namespace fallbacks
}  // namespace scz::inline v1
