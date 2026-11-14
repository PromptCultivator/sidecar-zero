// The allocation gate (testing §7): zero bytes allocated per call on every path the C++ SDK
// has, counted by a replaced global allocator (malloc, calloc, realloc, the aligned
// allocators, and every operator new) in this binary, per calling thread. Against the
// in-process sidecar of local_arena.hpp, whose own threads allocate as they like.
//
// Paths: no snapshot (NOT_CONNECTED), a model answer while spinning, one after parking (spin
// ceiling 0), a fallback at admission (gate closed; stale heartbeat; TOO_LARGE), a shed call
// (OVERLOADED), an abandon at the deadline (DEADLINE), a codec failure (CODEC_ERROR), and the
// first call from a thread that has never called (timed: a thread's first call is). These time
// one call in 64 (the default). Then, with every call timed (sample_every 1) and an
// on_fallback hook (sdk-contract §8): no snapshot, a model answer spinning and after parking,
// the gate closed, TOO_LARGE, and a codec failure. C++ has no polling wait.
#include <malloc.h>

#include <atomic>
#include <cinttypes>
#include <cstdio>
#include <cstdlib>
#include <new>
#include <thread>
#include <vector>

#include "local_arena.hpp"

extern "C" {
void* __libc_malloc(std::size_t);
void* __libc_calloc(std::size_t, std::size_t);
void* __libc_realloc(void*, std::size_t);
void* __libc_memalign(std::size_t, std::size_t);
void __libc_free(void*);
}

namespace {
thread_local std::uint64_t t_allocs = 0;
thread_local std::uint64_t t_bytes = 0;
std::atomic<std::uint64_t> g_allocs{0};
inline void count(std::size_t n) noexcept {
    ++t_allocs;
    t_bytes += n;
    g_allocs.fetch_add(1, std::memory_order_relaxed);
}
}  // namespace

extern "C" {
void* malloc(std::size_t n) {
    count(n);
    return __libc_malloc(n);
}
void* calloc(std::size_t a, std::size_t b) {
    count(a * b);
    return __libc_calloc(a, b);
}
void* realloc(void* p, std::size_t n) {
    count(n);
    return __libc_realloc(p, n);
}
void free(void* p) { __libc_free(p); }
void* memalign(std::size_t al, std::size_t n) {
    count(n);
    return __libc_memalign(al, n);
}
void* aligned_alloc(std::size_t al, std::size_t n) {
    count(n);
    return __libc_memalign(al, n);
}
int posix_memalign(void** out, std::size_t al, std::size_t n) {
    count(n);
    *out = __libc_memalign(al, n);
    return *out == nullptr ? ENOMEM : 0;
}
}

void* operator new(std::size_t n) {
    void* p = malloc(n == 0 ? 1 : n);
    if (p == nullptr) throw std::bad_alloc();
    return p;
}
void* operator new[](std::size_t n) { return operator new(n); }
void* operator new(std::size_t n, const std::nothrow_t&) noexcept { return malloc(n == 0 ? 1 : n); }
void* operator new[](std::size_t n, const std::nothrow_t&) noexcept { return malloc(n == 0 ? 1 : n); }
void* operator new(std::size_t n, std::align_val_t al) {
    void* p = aligned_alloc(static_cast<std::size_t>(al), n == 0 ? 1 : n);
    if (p == nullptr) throw std::bad_alloc();
    return p;
}
void* operator new[](std::size_t n, std::align_val_t al) { return operator new(n, al); }
void* operator new(std::size_t n, std::align_val_t al, const std::nothrow_t&) noexcept { return aligned_alloc(static_cast<std::size_t>(al), n == 0 ? 1 : n); }
void* operator new[](std::size_t n, std::align_val_t al, const std::nothrow_t&) noexcept { return aligned_alloc(static_cast<std::size_t>(al), n == 0 ? 1 : n); }
void operator delete(void* p) noexcept { free(p); }
void operator delete[](void* p) noexcept { free(p); }
void operator delete(void* p, std::size_t) noexcept { free(p); }
void operator delete[](void* p, std::size_t) noexcept { free(p); }
void operator delete(void* p, std::align_val_t) noexcept { free(p); }
void operator delete[](void* p, std::align_val_t) noexcept { free(p); }
void operator delete(void* p, std::size_t, std::align_val_t) noexcept { free(p); }
void operator delete[](void* p, std::size_t, std::align_val_t) noexcept { free(p); }

using namespace std::chrono_literals;
using namespace scztest;

namespace {

struct Keyed {
    using input_type = std::span<const std::byte>;
    using output_type = std::uint64_t;
    std::atomic<bool>* fail = nullptr;
    [[nodiscard]] std::size_t request_len(const input_type& in) const noexcept { return in.size(); }
    [[nodiscard]] bool encode(const input_type& in, scz::SlotWriter& w) const noexcept { return !fail->load() && w.write(in); }
    [[nodiscard]] bool decode(const scz::SlotReader& r, output_type& out) const noexcept { return r.size() == 8 && r.read_u64(0, out); }
};
struct Zero {
    std::uint64_t operator()(const Keyed::input_type&, scz::FallbackReason) const noexcept { return 0; }
};
using Client = scz::Client<Keyed, Zero>;

/// The on_fallback hook of the timed client: it counts.
struct CountHook {
    std::atomic<std::uint64_t>* n;
    void operator()(scz::FallbackReason) const noexcept { n->fetch_add(1, std::memory_order_relaxed); }
};
using TimedClient = scz::Client<Keyed, Zero, CountHook>;

int g_failed = 0;

/// The reasons of calls that ended otherwise than their path expects, printed under its line.
struct Others {
    std::uint64_t n[scz::kReasonCount] = {};
    template <class O>
    void note(const O& o) {
        if (!o.is_model()) ++n[static_cast<std::size_t>(o.reason)];
    }
    void print() const {
        for (std::size_t r = 0; r < scz::kReasonCount; ++r)
            if (n[r] != 0) std::printf("       %s %" PRIu64 "\n", scz::reason_name(static_cast<scz::FallbackReason>(r)).data(), n[r]);
    }
};

/// Calls that end otherwise than their path expects: a starved worker or heartbeat thread on a
/// loaded machine, not the SDK. Allocations must be zero; up to one call in a thousand may take
/// another path (as in the Go gate), and their reasons are printed.
inline bool few(int wrong, int calls) { return static_cast<long long>(wrong) * 1000 <= calls; }

/// Run `calls` calls of `req` and report the calling thread's allocations over them. The
/// outcome must be `want` (or a model answer when `model`), but for a few (`few`).
template <class Cl>
void gate(const char* path, Cl& c, const KeyedReq& req, std::chrono::nanoseconds budget, int calls, bool model, scz::FallbackReason want) {
    int wrong = 0;
    Others other;
    for (int i = 0; i < 20; ++i) (void)c.infer(req.bytes(), budget);  // warm up
    const std::uint64_t a0 = t_allocs, b0 = t_bytes;
    for (int i = 0; i < calls; ++i) {
        const auto o = c.infer(req.bytes(), budget);
        if (model ? !(o.is_model() && o.value == req.answer()) : (o.is_model() || o.reason != want)) {
            ++wrong;
            other.note(o);
        }
    }
    const std::uint64_t da = t_allocs - a0, db = t_bytes - b0;
    const bool ok = da == 0 && few(wrong, calls);
    if (!ok) ++g_failed;
    std::printf("%-4s %-34s %7d calls  %" PRIu64 " allocations  %" PRIu64 " bytes  (%d with another outcome)\n", ok ? "ok" : "FAIL", path, calls, da, db, wrong);
    other.print();
}

}  // namespace

int main() {
    {
        // The counting allocator is live: a malloc through an opaque pointer, and a vector, count.
        void* (*volatile m)(std::size_t) = &malloc;
        const std::uint64_t a0 = t_allocs;
        free(m(16));
        std::vector<int> v(10);
        if (t_allocs - a0 < 2) {
            std::printf("FAIL the counting allocator does not count\n");
            return 1;
        }
    }
    std::atomic<bool> fail{false};
    // The test sidecar's heartbeat thread beats every 500 µs; on a loaded machine it can miss
    // the default 15 ms limit, and the model-answer paths then end STALE_HEARTBEAT. This gate
    // counts allocations, so the limit is generous here (the stale path sleeps past it), and
    // the model-answer budgets are the sidecar's ceiling.
    Config cfg;
    cfg.hb_stale_ns = 250'000'000;
    FakeSidecar sc(unique_endpoint("alloc"), cfg);
    Client c(scz::Endpoint::parse(sc.endpoint()), Keyed{&fail}, Zero{});
    const KeyedReq normal(op::NORMAL, 0, 0, 0);

    gate("no snapshot (NOT_CONNECTED)", c, normal, 2ms, 100000, false, scz::FallbackReason::NotConnected);
    if (!sc.start() || !c.await_bound(5s)) {
        std::printf("FAIL the client did not bind\n");
        return 1;
    }
    gate("model answer, spinning", c, normal, 100ms, 20000, true, {});
    c.set_spin_ceiling(0ns);
    gate("model answer, after parking", c, normal, 100ms, 5000, true, {});
    c.set_spin_ceiling(std::nullopt);
    sc.set_gate(abi::gate_state::DEAD);
    gate("admission: gate closed", c, normal, 2ms, 100000, false, scz::FallbackReason::GateDead);
    sc.set_gate(abi::gate_state::HEALTHY);
    sc.heartbeat(false);
    std::this_thread::sleep_for(300ms);
    gate("admission: stale heartbeat", c, normal, 2ms, 100000, false, scz::FallbackReason::StaleHeartbeat);
    sc.heartbeat(true);
    std::this_thread::sleep_for(5ms);
    {
        static std::byte big[4096];
        const std::uint64_t a0 = t_allocs;
        int wrong = 0;
        Others other;
        for (int i = 0; i < 100000; ++i) {
            const auto o = c.infer(std::span<const std::byte>(big), 2ms);
            if (o.is_model() || o.reason != scz::FallbackReason::TooLarge) {
                ++wrong;
                other.note(o);
            }
        }
        const bool ok = t_allocs == a0 && few(wrong, 100000);
        g_failed += ok ? 0 : 1;
        std::printf("%-4s %-34s %7d calls  %" PRIu64 " allocations  (%d with another outcome)\n", ok ? "ok" : "FAIL", "admission: TOO_LARGE", 100000, t_allocs - a0, wrong);
        other.print();
    }
    fail = true;
    gate("encode failure (CODEC_ERROR)", c, normal, 20ms, 20000, false, scz::FallbackReason::CodecError);
    fail = false;
    // An abandon at the deadline: the worker takes 300 µs, the budget is 100 µs. Each call
    // abandons its slot (READY or PROCESSING); answered calls in between keep the breaker shut.
    {
        const KeyedReq slow(op::DELAY, 300, 0, 1);
        const std::uint64_t a0 = t_allocs;
        int deadlines = 0;
        for (int i = 0; i < 2000; ++i) {
            if (c.infer(slow.bytes(), 100us).reason == scz::FallbackReason::Deadline) ++deadlines;
            std::this_thread::sleep_for(400us);
            (void)c.infer(normal.bytes(), 20ms);
        }
        const std::uint64_t da = t_allocs - a0;
        // The sleeps between calls are the test's, and std::this_thread::sleep_for does not allocate.
        const bool ok = da == 0 && deadlines > 1500;
        g_failed += ok ? 0 : 1;
        std::printf("%-4s %-34s %7d calls  %" PRIu64 " allocations  (%d DEADLINE)\n", ok ? "ok" : "FAIL", "abandon at the deadline", 2000, da, deadlines);
    }
    // A shed call: eight misses in a row open the breaker; the calls that follow shed.
    {
        const KeyedReq slow(op::DELAY, 3000, 0, 2);
        for (int i = 0; i < 8; ++i) (void)c.infer(slow.bytes(), 500us);
        const std::uint64_t a0 = t_allocs;
        int shed = 0;
        for (int i = 0; i < 100000; ++i)
            if (c.infer(normal.bytes(), 500us).reason == scz::FallbackReason::Overloaded) ++shed;
        const std::uint64_t da = t_allocs - a0;
        const bool ok = da == 0 && shed > 1000;
        g_failed += ok ? 0 : 1;
        std::printf("%-4s %-34s %7d calls  %" PRIu64 " allocations  (%d OVERLOADED)\n", ok ? "ok" : "FAIL", "shed call (OVERLOADED)", 100000, da, shed);
        std::this_thread::sleep_for(150ms);
    }
    // The first call from threads that have never called (thread creation itself allocates;
    // only the call is counted).
    {
        std::atomic<std::uint64_t> allocs{0};
        std::atomic<int> model{0};
        for (std::uint32_t t = 0; t < 200; ++t) {
            std::thread th([&, t] {
                const KeyedReq q(op::NORMAL, 0, 100 + t, 0);
                const std::uint64_t a0 = t_allocs;
                const auto o = c.infer(q.bytes(), 100ms);
                allocs.fetch_add(t_allocs - a0);
                if (o.is_model() && o.value == q.answer()) model.fetch_add(1);
            });
            th.join();
        }
        const bool ok = allocs.load() == 0 && model.load() == 200;
        g_failed += ok ? 0 : 1;
        std::printf("%-4s %-34s %7d calls  %" PRIu64 " allocations  (%d model answers)\n", ok ? "ok" : "FAIL", "first call from a new thread", 200, allocs.load(), model.load());
    }
    {
        const scz::Stats s = c.stats();
        const bool ok = s.latency_model.count > 0 && s.latency_fallback.count > 0;
        g_failed += ok ? 0 : 1;
        std::printf("%-4s %-34s %" PRIu64 " model, %" PRIu64 " fallback\n", ok ? "ok" : "FAIL", "timed one in 64 above", s.latency_model.count, s.latency_fallback.count);
    }
    c.close();
    sc.destroy();

    // Every call timed, and a hook, against a sidecar of its own (one client per sidecar).
    std::atomic<std::uint64_t> hooked{0};
    FakeSidecar sc2(unique_endpoint("alloc-timed"), cfg);
    scz::Options every1;
    every1.sample_every = 1;
    TimedClient t(scz::Endpoint::parse(sc2.endpoint()), Keyed{&fail}, Zero{}, every1, CountHook{&hooked});
    gate("timed + hook: no snapshot", t, normal, 2ms, 100000, false, scz::FallbackReason::NotConnected);
    if (!sc2.start() || !t.await_bound(5s)) {
        std::printf("FAIL the timed client did not bind\n");
        return 1;
    }
    gate("timed: model answer, spinning", t, normal, 100ms, 20000, true, {});
    t.set_spin_ceiling(0ns);
    gate("timed: model answer, after parking", t, normal, 100ms, 5000, true, {});
    t.set_spin_ceiling(std::nullopt);
    sc2.set_gate(abi::gate_state::DEAD);
    gate("timed + hook: gate closed", t, normal, 2ms, 100000, false, scz::FallbackReason::GateDead);
    sc2.set_gate(abi::gate_state::HEALTHY);
    {
        static std::byte big[4096];
        const std::uint64_t a0 = t_allocs;
        int wrong = 0;
        Others other;
        for (int i = 0; i < 100000; ++i) {
            const auto o = t.infer(std::span<const std::byte>(big), 2ms);
            if (o.is_model() || o.reason != scz::FallbackReason::TooLarge) {
                ++wrong;
                other.note(o);
            }
        }
        const bool ok = t_allocs == a0 && few(wrong, 100000);
        g_failed += ok ? 0 : 1;
        std::printf("%-4s %-34s %7d calls  %" PRIu64 " allocations  (%d with another outcome)\n", ok ? "ok" : "FAIL", "timed + hook: TOO_LARGE", 100000, t_allocs - a0, wrong);
        other.print();
    }
    fail = true;
    gate("timed + hook: CODEC_ERROR", t, normal, 20ms, 20000, false, scz::FallbackReason::CodecError);
    fail = false;
    {
        // Every call was timed, whatever its outcome, and the hook saw every fallback.
        const scz::Stats s = t.stats();
        std::uint64_t fallbacks = 0;
        for (const std::uint64_t n : s.fallbacks) fallbacks += n;
        const std::uint64_t calls = (20 + 20000) + (20 + 5000) + (20 + 100000) * 2 + 100000 + (20 + 20000);
        const bool ok = s.model + fallbacks == calls && s.latency_model.count == s.model && s.latency_fallback.count == fallbacks &&
                        s.fallback_time.count == fallbacks && hooked.load() == fallbacks;
        g_failed += ok ? 0 : 1;
        std::printf("%-4s %-34s %" PRIu64 " calls, %" PRIu64 " model and %" PRIu64 " fallback timed, %" PRIu64 " hooked\n", ok ? "ok" : "FAIL", "timed: every call",
                    calls, s.latency_model.count, s.latency_fallback.count, hooked.load());
    }
    t.close();
    sc2.destroy();
    std::printf("allocation gate: %s\n", g_failed == 0 ? "passed" : "FAILED");
    return g_failed == 0 ? 0 : 1;
}
