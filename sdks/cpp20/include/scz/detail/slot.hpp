// The client's side of the slot protocol (shm-abi §5–6; signaling §3.2): claim, ready,
// release, cancel, wait, abandon. Mirrors scz-shm's `slot.rs`. Every exit from a state more
// than one party can leave is a compare-and-swap loop bounded at CAS_ATTEMPTS; plain stores
// are used only out of CLAIMED, DONE, and FAILED, which belong to the client alone.
#pragma once
#include <cstdint>

#include "platform.hpp"
#include "shared.hpp"

namespace scz::inline v1::detail {

namespace ss = abi::slot_state;
namespace rec = abi::acc::slot_record;

/// FREE → CLAIMED with the epoch advanced. Writes the new epoch to `e`.
[[nodiscard]] inline bool try_claim(std::byte* r, std::uint32_t& e) noexcept {
    std::uint64_t w = rec::word(r);
    if (state(w) != ss::FREE) return false;
    const std::uint32_t next = epoch(w) + 1;
    if (!rec::cas_word(r, w, slot_word(next, ss::CLAIMED))) return false;
    e = next;
    return true;
}

/// CLAIMED → READY. After this the request region belongs to the worker.
inline void set_ready(std::byte* r, std::uint32_t e) noexcept { rec::set_word(r, slot_word(e, ss::READY)); }

/// CLAIMED, DONE, or FAILED → FREE.
inline void release(std::byte* r, std::uint32_t e) noexcept { rec::set_word(r, slot_word(e, ss::FREE)); }

enum class Cancel { Cancelled, NotReady, GaveUp };

/// READY → FREE, when the request never reached the worker.
[[nodiscard]] inline Cancel cancel_if_ready(std::byte* r, std::uint32_t e) noexcept {
    for (std::uint64_t i = 0; i < abi::CAS_ATTEMPTS; ++i) {
        std::uint64_t w = rec::word(r);
        if (state(w) != ss::READY || epoch(w) != e) return Cancel::NotReady;
        if (rec::cas_weak_word(r, w, slot_word(e, ss::FREE))) return Cancel::Cancelled;
    }
    return Cancel::GaveUp;
}

enum class WaitEnd { Done, Failed, Cancelled, Timeout, GaveUp };

struct WaitResult {
    WaitEnd end;
    std::uint64_t value;  // the slot word (Done, Failed) or the gate that triggered a cancel
    /// Done, found by a parked waiter only once its deadline had passed: a late wake-up if the
    /// slot was completed more than 1 ms earlier (signaling §7).
    bool after_deadline = false;
};

[[nodiscard]] inline bool healthy(std::uint64_t g) noexcept { return gate_state(g) == abi::gate_state::HEALTHY; }

#if SCZ_LINUX
/// Client side of signaling §3.2, called right after the descriptor was published (whose
/// publish ended in a full fence, so the first gate load is the re-check that pairs with the
/// supervisor's store-gate-then-sweep, shm-abi §6.3). Spin until `spin_until_ns` or the
/// deadline, then announce WAITER and sleep on the word's low half until `deadline_ns`.
[[nodiscard]] inline WaitResult wait(std::byte* r, const std::byte* ctl, std::uint32_t e, std::uint64_t deadline_ns, std::uint64_t spin_until_ns) noexcept {
    namespace h = abi::acc::control_header;
    std::uint64_t g = h::gate(ctl);
    if (!healthy(g) && cancel_if_ready(r, e) == Cancel::Cancelled) return {WaitEnd::Cancelled, g};
    auto resolved = [e](std::uint64_t w, WaitResult& out) {
        if (epoch(w) != e) {
            out = {WaitEnd::GaveUp, w};
            return true;
        }
        const std::uint32_t s = state(w);
        if (s == ss::DONE) {
            out = {WaitEnd::Done, w};
            return true;
        }
        if (s == ss::FAILED) {
            out = {WaitEnd::Failed, w};
            return true;
        }
        return false;
    };
    WaitResult out{WaitEnd::Timeout, 0};
    const std::uint64_t spin_end = spin_until_ns < deadline_ns ? spin_until_ns : deadline_ns;
    while (monotonic_ns() < spin_end) {
        const std::uint64_t w = rec::word(r);
        if (resolved(w, out)) return out;
        if (state(w) == ss::READY) {
            g = h::gate(ctl);
            if (!healthy(g) && cancel_if_ready(r, e) == Cancel::Cancelled) return {WaitEnd::Cancelled, g};
        }
        cpu_relax();
    }
    // Park: announce with the WAITER bit, then sleep on the word.
    std::uint64_t attempts = 0;
    bool parked = false;
    for (;;) {
        std::uint64_t w = rec::word(r);
        if (resolved(w, out)) {
            out.after_deadline = parked && out.end == WaitEnd::Done && monotonic_ns() >= deadline_ns;
            return out;
        }
        if (monotonic_ns() >= deadline_ns) return {WaitEnd::Timeout, 0};
        if (!waiter(w)) {
            std::uint64_t expected = w;
            if (!rec::cas_weak_word(r, expected, with_waiter(w))) {
                if (++attempts >= abi::CAS_ATTEMPTS) return {WaitEnd::GaveUp, expected};
                continue;
            }
            w = with_waiter(w);
        }
        (void)futex_wait_until(r + abi::slot_record::WORD, low32(w), deadline_ns);
        parked = true;
    }
}
#endif

enum class Abandon { Freed, Abandoned, Done, Failed, GaveUp };

/// What a client does when its deadline has passed. PROCESSING → ABANDONED keeps OWNER: the
/// supervisor uses it to find the slots an exited worker held.
[[nodiscard]] inline Abandon abandon(std::byte* r, std::uint32_t e) noexcept {
    for (std::uint64_t i = 0; i < abi::CAS_ATTEMPTS; ++i) {
        std::uint64_t w = rec::word(r);
        if (epoch(w) != e) return Abandon::GaveUp;
        switch (state(w)) {
        case ss::READY:
            if (rec::cas_weak_word(r, w, slot_word(e, ss::FREE))) return Abandon::Freed;
            break;
        case ss::PROCESSING:
            if (rec::cas_weak_word(r, w, with_owner(slot_word(e, ss::ABANDONED), owner(w)))) return Abandon::Abandoned;
            break;
        case ss::DONE:
            return Abandon::Done;
        case ss::FAILED:
            return Abandon::Failed;
        default:
            return Abandon::GaveUp;
        }
    }
    return Abandon::GaveUp;
}

}  // namespace scz::inline v1::detail
