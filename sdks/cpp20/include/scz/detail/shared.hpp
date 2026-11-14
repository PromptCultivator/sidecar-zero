// Access to mapped memory (sdk-cpp20 §3): the single-fetch read, raw stores the generated
// accessors do not cover (reserved bytes the Rust reference also writes), and the packing of
// the two contended words. Every shared field itself is reached through the generated
// accessors in abi.hpp, whose ordering is fixed by the ABI file.
#pragma once
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>

#include "../abi.hpp"

namespace scz::inline v1::detail {

/// Read a field another process can write exactly once (sdk-cpp20 §3, "Single-fetch reads").
/// A plain read copied to a local may legally be performed twice by the compiler.
template <class T>
[[nodiscard]] inline T fetch_once(const std::byte* p) noexcept {
    return abi::acc::detail::load<T>(p, std::memory_order_relaxed);
}

/// A relaxed store of a word no accessor names (reserved bytes inside a cell).
template <class T>
inline void store_relaxed(std::byte* p, T v) noexcept {
    abi::acc::detail::store<T>(p, v, std::memory_order_relaxed);
}

/// The store-load barrier the wake protocols need (shm-abi §4.3: a release store, a full
/// fence, and an acquire load are the form scz-shm uses; this SDK uses the same).
inline void full_fence() noexcept { std::atomic_thread_fence(std::memory_order_seq_cst); }

// Payload bytes: request and response regions owned by the caller under the slot protocol.
inline void copy_in(std::byte* dst, const void* src, std::size_t n) noexcept { std::memcpy(dst, src, n); }
inline void copy_out(void* dst, const std::byte* src, std::size_t n) noexcept { std::memcpy(dst, src, n); }

// ── slot word: [ epoch:32 | reserved:21 | OWNER:2 | WAITER:1 | state:8 ] ──
namespace sw = abi::slot_word;
[[nodiscard]] constexpr std::uint32_t state(std::uint64_t w) noexcept { return static_cast<std::uint32_t>((w & sw::STATE_MASK) >> sw::STATE_SHIFT); }
[[nodiscard]] constexpr std::uint32_t epoch(std::uint64_t w) noexcept { return static_cast<std::uint32_t>((w & sw::EPOCH_MASK) >> sw::EPOCH_SHIFT); }
[[nodiscard]] constexpr bool waiter(std::uint64_t w) noexcept { return (w & sw::WAITER_MASK) != 0; }
[[nodiscard]] constexpr std::uint32_t owner(std::uint64_t w) noexcept { return static_cast<std::uint32_t>((w & sw::OWNER_MASK) >> sw::OWNER_SHIFT); }
/// A word with the given epoch and state, WAITER and OWNER clear.
[[nodiscard]] constexpr std::uint64_t slot_word(std::uint32_t e, std::uint32_t s) noexcept {
    return (static_cast<std::uint64_t>(e) << sw::EPOCH_SHIFT) | ((static_cast<std::uint64_t>(s) << sw::STATE_SHIFT) & sw::STATE_MASK);
}
[[nodiscard]] constexpr std::uint64_t with_owner(std::uint64_t w, std::uint32_t o) noexcept {
    return (w & ~sw::OWNER_MASK) | ((static_cast<std::uint64_t>(o) << sw::OWNER_SHIFT) & sw::OWNER_MASK);
}
[[nodiscard]] constexpr std::uint64_t with_waiter(std::uint64_t w) noexcept { return w | sw::WAITER_MASK; }
/// The half of the word a futex waits on (little-endian: the half at the word's address).
[[nodiscard]] constexpr std::uint32_t low32(std::uint64_t w) noexcept { return static_cast<std::uint32_t>(w); }

// ── gate word: [ worker_gen:32 | shed_permille:16 | reason:8 | state:8 ] ──
namespace gw = abi::gate_word;
[[nodiscard]] constexpr std::uint32_t gate_state(std::uint64_t g) noexcept { return static_cast<std::uint32_t>((g & gw::STATE_MASK) >> gw::STATE_SHIFT); }
[[nodiscard]] constexpr std::uint32_t gate_reason(std::uint64_t g) noexcept { return static_cast<std::uint32_t>((g & gw::REASON_MASK) >> gw::REASON_SHIFT); }
[[nodiscard]] constexpr std::uint32_t gate_gen(std::uint64_t g) noexcept { return static_cast<std::uint32_t>((g & gw::WORKER_GEN_MASK) >> gw::WORKER_GEN_SHIFT); }
[[nodiscard]] constexpr std::uint64_t gate_word(std::uint32_t gen, std::uint32_t reason, std::uint32_t s) noexcept {
    return (static_cast<std::uint64_t>(gen) << gw::WORKER_GEN_SHIFT) | ((static_cast<std::uint64_t>(reason) << gw::REASON_SHIFT) & gw::REASON_MASK) |
           ((static_cast<std::uint64_t>(s) << gw::STATE_SHIFT) & gw::STATE_MASK);
}

static_assert(state(with_waiter(with_owner(slot_word(0xDEADBEEF, abi::slot_state::PROCESSING), 2))) == abi::slot_state::PROCESSING);
static_assert(epoch(with_waiter(with_owner(slot_word(0xDEADBEEF, abi::slot_state::PROCESSING), 2))) == 0xDEADBEEF);
static_assert(owner(with_owner(slot_word(1, 1), 2)) == 2);
static_assert(gate_state(0) == abi::gate_state::INIT, "a zeroed gate must read as not healthy");

// ── little-endian byte access to private buffers (messages, the registry copy) ──
[[nodiscard]] inline std::uint16_t le16(const std::uint8_t* p) noexcept { std::uint16_t v; std::memcpy(&v, p, 2); return v; }
[[nodiscard]] inline std::uint32_t le32(const std::uint8_t* p) noexcept { std::uint32_t v; std::memcpy(&v, p, 4); return v; }
[[nodiscard]] inline std::uint64_t le64(const std::uint8_t* p) noexcept { std::uint64_t v; std::memcpy(&v, p, 8); return v; }
inline void put16(std::uint8_t* p, std::uint16_t v) noexcept { std::memcpy(p, &v, 2); }
inline void put32(std::uint8_t* p, std::uint32_t v) noexcept { std::memcpy(p, &v, 4); }
inline void put64(std::uint8_t* p, std::uint64_t v) noexcept { std::memcpy(p, &v, 8); }

}  // namespace scz::inline v1::detail
