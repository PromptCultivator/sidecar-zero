// The producer side of a descriptor ring (shm-abi §4.3) and the waker rule (signaling §3.1).
// Mirrors scz-shm's `ring::enqueue`, including the guard that publishes a claimed cell as
// void if the call unwinds before publishing it.
#pragma once
#include <atomic>
#include <cstdint>
#include <cstdlib>

#include "layout.hpp"
#include "platform.hpp"
#include "shared.hpp"

#if SCZ_LINUX && defined(SCZ_CRASHPOINTS)
#include <csignal>
#include <cstring>
#endif

namespace scz::inline v1 {

#if SCZ_HOOKS_THROW
namespace testing {
/// Thrown by the unwind hook: what a call that unwinds inside the enqueue looks like.
struct Unwound {};
/// Armed, the next enqueue in this process throws between claiming a cell and publishing it
/// (sdk-contract §11 item 11). Only the conformance agent and the tests arm it.
inline std::atomic<bool> unwind_once{false};
}  // namespace testing
#endif

namespace detail {

/// Crash points (testing §2.4) for the crash-point build of the agent: with SCZ_CRASHPOINTS
/// defined and SCZ_CRASH_AT naming the tag, the process kills itself there.
inline void crash_point([[maybe_unused]] const char* tag) noexcept {
#if SCZ_LINUX && defined(SCZ_CRASHPOINTS)
    static const char* const at = std::getenv("SCZ_CRASH_AT");
    if (at != nullptr && std::strcmp(at, tag) == 0) ::kill(::getpid(), SIGKILL);
#endif
}

/// What a descriptor cell carries apart from `seq` (shm-abi §4.2).
struct Descriptor {
    std::uint64_t request_id = 0;
    std::uint32_t slot_index = 0;
    std::uint64_t deadline_ns = 0;
    std::uint32_t slot_epoch = 0;
    std::uint16_t slab_class = 0;
    std::uint64_t enqueue_ns = 0;
    std::uint32_t flags = 0;
    std::uint32_t route = 0;
};

namespace cl = abi::cell;
static_assert(cl::SEQ == 0 && cl::REQUEST_ID == 0x08 && cl::SLOT_INDEX == 0x1C && cl::DEADLINE_MONO_NS == 0x20 && cl::SLOT_EPOCH == 0x28 &&
                  cl::SLAB_CLASS == 0x2C && cl::ENQUEUE_MONO_NS == 0x30 && cl::FLAGS == 0x38 && cl::ROUTE == 0x3C,
              "the reserved bytes write_cell clears sit between these fields");

/// Write every byte of a cell after `seq`, the reserved ones as zero, as the Rust reference
/// does (testing §3, "Byte equality").
inline void write_cell(std::byte* c, const Descriptor& d) noexcept {
    namespace a = abi::acc::cell;
    a::set_request_id(c, d.request_id);
    store_relaxed<std::uint64_t>(c + 0x10, 0);
    store_relaxed<std::uint32_t>(c + 0x18, 0);
    a::set_slot_index(c, d.slot_index);
    a::set_deadline_mono_ns(c, d.deadline_ns);
    a::set_slot_epoch(c, d.slot_epoch);
    a::set_slab_class(c, d.slab_class);
    store_relaxed<std::uint16_t>(c + 0x2E, 0);
    a::set_enqueue_mono_ns(c, d.enqueue_ns);
    a::set_flags(c, d.flags);
    a::set_route(c, d.route);
}

#if SCZ_LINUX
/// The waker rule (signaling §3.1). The caller has already issued a full fence.
inline void wake_if_parked(const RingRef& r) noexcept {
    namespace h = abi::acc::ring_header;
    std::uint32_t w = h::consumer_wait(r.hdr);
    if (w == abi::consumer_wait::AWAKE) return;
    if (w == abi::consumer_wait::PARKED) (void)h::cas_consumer_wait(r.hdr, w, abi::consumer_wait::WAKING);
    futex_wake(r.hdr + abi::ring_header::CONSUMER_WAIT, 1);
}
#endif

enum class Enqueue { Ok, Full, Exhausted };

/// Publishes a claimed cell as void if the producer unwinds before publishing it: an
/// unpublished cell would stall the ring for every later call. The consumer skips it.
struct VoidOnUnwind {
    const RingRef* r;
    std::byte* cell;
    std::uint64_t pos;
    bool armed = true;
    ~VoidOnUnwind() {
        if (!armed) return;
        Descriptor d{};
        d.flags = abi::cell_flag::VOID;
        write_cell(cell, d);
        abi::acc::cell::set_seq(cell, pos + 1);
        full_fence();
#if SCZ_LINUX
        wake_if_parked(*r);
#endif
    }
};

/// Producer side (§4.3). Publishes `d` and wakes the consumer if it has parked. Gives up as
/// full after ENQUEUE_ATTEMPTS: the ring's words live where a worker can write.
[[nodiscard]] inline Enqueue enqueue(const RingRef& r, const Descriptor& d) SCZ_CALL_NOEXCEPT {
    namespace h = abi::acc::ring_header;
    namespace c = abi::acc::cell;
    for (std::uint64_t attempt = 0; attempt < abi::ENQUEUE_ATTEMPTS; ++attempt) {
        std::uint64_t pos = h::tail(r.hdr);
        std::byte* cell = r.cell(pos);
        const std::uint64_t seq = c::seq(cell);
        if (seq == pos) {
            if (h::cas_weak_tail(r.hdr, pos, pos + 1)) {
                crash_point("ring.enqueue.claimed");
                VoidOnUnwind guard{&r, cell, pos};
#if SCZ_HOOKS_THROW
                if (testing::unwind_once.load(std::memory_order_relaxed) && testing::unwind_once.exchange(false, std::memory_order_relaxed))
                    throw testing::Unwound{};
#endif
                write_cell(cell, d);
                crash_point("ring.enqueue.written");
                guard.armed = false;
                c::set_seq(cell, pos + 1);  // publish
                full_fence();               // the publish before the wait-word load, and before the caller's gate re-check
#if SCZ_LINUX
                wake_if_parked(r);
#endif
                return Enqueue::Ok;
            }
        } else if (seq < pos) {
            return Enqueue::Full;
        }
        // Another producer took the cell first, or the word is corrupt: go round again.
        cpu_relax();
    }
    return Enqueue::Exhausted;
}

}  // namespace detail
}  // namespace scz::inline v1
