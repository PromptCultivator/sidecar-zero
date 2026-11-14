// Interleaving stress of the jcstress kind (testing §2.3; sdk-cpp20 §9): the client's slot
// claim, cancel, abandon, enqueue, and caller-entry claim, each raced by two or three threads
// over many independent instances against a C++ model of the worker side, millions of times.
// Every outcome is tallied; a forbidden one fails the run.
//
//   stress [ROUNDS]     ROUNDS scales every test (default 1000: millions of races each)
#include <barrier>
#include <cinttypes>
#include <cstdio>
#include <cstdlib>
#include <map>
#include <string>
#include <thread>
#include <vector>

#include "scz/sidecarzero.hpp"

using namespace scz::detail;
namespace abi = scz::abi;
namespace ss = abi::slot_state;
namespace sr = abi::acc::slot_record;

namespace {

int g_failed = 0;

struct alignas(64) Rec {
    std::byte b[64];
};

/// A tally of named outcomes; `forbidden` ones fail the test.
struct Tally {
    std::map<std::string, std::uint64_t> n;
    void report(const char* test, const std::vector<std::string>& forbidden) {
        bool bad = false;
        std::printf("%s\n", test);
        for (auto& [k, v] : n) {
            bool f = false;
            for (const auto& x : forbidden) f = f || k == x;
            bad = bad || f;
            std::printf("    %-44s %12" PRIu64 "%s\n", k.c_str(), v, f ? "   FORBIDDEN" : "");
        }
        if (bad) ++g_failed;
    }
};

/// The worker's READY → PROCESSING, keeping WAITER, setting OWNER (worker §4).
enum class Claim { Claimed, Stale, GaveUp };
Claim worker_claim(std::byte* r, std::uint32_t e, std::uint32_t own) {
    for (std::uint64_t i = 0; i < abi::CAS_ATTEMPTS; ++i) {
        std::uint64_t w = sr::word(r);
        if (epoch(w) != e || state(w) != ss::READY) return Claim::Stale;
        if (sr::cas_weak_word(r, w, with_owner(slot_word(e, ss::PROCESSING) | (w & abi::slot_word::WAITER_MASK), own))) return Claim::Claimed;
    }
    return Claim::GaveUp;
}

/// The worker's completing CAS: PROCESSING → DONE, or ABANDONED → FREE.
enum class Completed { Delivered, Freed, NotOurs };
Completed worker_complete(std::byte* r, std::uint32_t e) {
    for (std::uint64_t i = 0; i < abi::CAS_ATTEMPTS; ++i) {
        std::uint64_t w = sr::word(r);
        if (epoch(w) != e) return Completed::NotOurs;
        if (state(w) == ss::ABANDONED) {
            if (sr::cas_weak_word(r, w, slot_word(e, ss::FREE))) return Completed::Freed;
        } else if (state(w) == ss::PROCESSING) {
            if (sr::cas_weak_word(r, w, slot_word(e, ss::DONE))) return Completed::Delivered;
        } else {
            return Completed::NotOurs;
        }
    }
    return Completed::NotOurs;
}

constexpr std::size_t K = 1024;  // instances per round

/// Three threads claim the same FREE slots: each slot is won exactly once, with the epoch advanced.
void claim_race(int rounds) {
    std::vector<Rec> slots(K);
    std::vector<int> winner(K * 3);
    Tally t;
    std::barrier sync(3);
    auto body = [&](int me) {
        for (int r = 0; r < rounds; ++r) {
            sync.arrive_and_wait();
            for (std::size_t k = 0; k < K; ++k) {
                const std::size_t i = me == 1 ? K - 1 - k : (me == 2 ? (k * 7) % K : k);
                std::uint32_t e = 0;
                winner[i * 3 + static_cast<std::size_t>(me)] = try_claim(slots[i].b, e) ? static_cast<int>(e) : 0;
            }
            sync.arrive_and_wait();
            if (me == 0) {
                for (std::size_t i = 0; i < K; ++i) {
                    int wins = 0, ep = 0;
                    for (int m = 0; m < 3; ++m)
                        if (winner[i * 3 + static_cast<std::size_t>(m)] != 0) {
                            ++wins;
                            ep = winner[i * 3 + static_cast<std::size_t>(m)];
                        }
                    const std::uint64_t w = sr::word(slots[i].b);
                    const bool consistent = state(w) == ss::CLAIMED && static_cast<int>(epoch(w)) == ep;
                    t.n[wins == 1 && consistent ? "one winner, epoch advanced" : "winners=" + std::to_string(wins)]++;
                    release(slots[i].b, epoch(w));
                }
            }
        }
    };
    std::thread a(body, 1), b(body, 2);
    body(0);
    a.join();
    b.join();
    t.report("slot claim: three claimers", {"winners=0", "winners=2", "winners=3", "winners=1"});
}

/// A READY slot: the client cancels while the worker claims. Exactly one of them wins.
void cancel_vs_claim(int rounds) {
    std::vector<Rec> slots(K);
    std::vector<Cancel> c(K);
    std::vector<Claim> w(K);
    Tally t;
    std::barrier sync(2);
    std::uint32_t e = 1;
    auto client = [&] {
        for (int r = 0; r < rounds; ++r) {
            for (auto& s : slots) set_ready(s.b, e);
            sync.arrive_and_wait();
            for (std::size_t i = 0; i < K; ++i) c[i] = cancel_if_ready(slots[i].b, e);
            sync.arrive_and_wait();
            for (std::size_t i = 0; i < K; ++i) {
                const std::uint64_t word = sr::word(slots[i].b);
                std::string o;
                if (c[i] == Cancel::Cancelled && w[i] == Claim::Stale && state(word) == ss::FREE) o = "client cancelled";
                else if (c[i] == Cancel::NotReady && w[i] == Claim::Claimed && state(word) == ss::PROCESSING && owner(word) == 1) o = "worker claimed";
                else o = "c=" + std::to_string(static_cast<int>(c[i])) + " w=" + std::to_string(static_cast<int>(w[i])) + " s=" + std::to_string(state(word));
                t.n[o]++;
            }
            ++e;
            sync.arrive_and_wait();
        }
    };
    auto worker = [&] {
        for (int r = 0; r < rounds; ++r) {
            sync.arrive_and_wait();
            const std::uint32_t ep = e;
            for (std::size_t i = K; i-- > 0;) w[i] = worker_claim(slots[i].b, ep, 1);
            sync.arrive_and_wait();
            sync.arrive_and_wait();
        }
    };
    std::thread b(worker);
    client();
    b.join();
    Tally ok;
    std::vector<std::string> forbidden;
    for (auto& [k, v] : t.n)
        if (k != "client cancelled" && k != "worker claimed") forbidden.push_back(k);
    t.report("cancel (client) vs claim (worker)", forbidden);
}

/// A PROCESSING slot with OWNER 2: the client abandons at its deadline while the worker
/// completes, and a third thread announces WAITER as a parking client would. Either the
/// client sees DONE and the worker delivered, or the client abandoned with OWNER kept and the
/// worker freed it. Nothing is left ABANDONED, and nothing is freed twice.
void abandon_vs_complete(int rounds) {
    std::vector<Rec> slots(K);
    std::vector<Abandon> a(K);
    std::vector<Completed> w(K);
    Tally t;
    std::barrier sync(3);
    std::uint32_t e = 1;
    auto client = [&] {
        for (int r = 0; r < rounds; ++r) {
            for (auto& s : slots) sr::set_word(s.b, with_owner(slot_word(e, ss::PROCESSING), 2));
            sync.arrive_and_wait();
            for (std::size_t i = 0; i < K; ++i) {
                a[i] = abandon(slots[i].b, e);
            }
            sync.arrive_and_wait();
            for (std::size_t i = 0; i < K; ++i) {
                const std::uint32_t s = state(sr::word(slots[i].b));
                std::string o;
                if (a[i] == Abandon::Done && w[i] == Completed::Delivered && s == ss::DONE) o = "worker won: client resolves DONE";
                else if (a[i] == Abandon::Abandoned && w[i] == Completed::Freed && s == ss::FREE) o = "client abandoned, worker freed";
                else o = "a=" + std::to_string(static_cast<int>(a[i])) + " w=" + std::to_string(static_cast<int>(w[i])) + " s=" + std::to_string(s);
                t.n[o]++;
            }
            ++e;
            sync.arrive_and_wait();
        }
    };
    auto worker = [&] {
        for (int r = 0; r < rounds; ++r) {
            sync.arrive_and_wait();
            const std::uint32_t ep = e;
            for (std::size_t i = K; i-- > 0;) {
                // The abandon keeps OWNER: the supervisor's sweep finds abandoned slots by it.
                const std::uint64_t word = sr::word(slots[i].b);
                if (state(word) == ss::ABANDONED && owner(word) != 2) t.n["ABANDONED without OWNER"]++;
                w[i] = worker_complete(slots[i].b, ep);
            }
            sync.arrive_and_wait();
            sync.arrive_and_wait();
        }
    };
    auto waiter_announce = [&] {
        for (int r = 0; r < rounds; ++r) {
            sync.arrive_and_wait();
            for (std::size_t k = 0; k < K; ++k) {
                const std::size_t i = (k * 13) % K;
                std::uint64_t word = sr::word(slots[i].b);
                if (state(word) == ss::PROCESSING && !waiter(word)) (void)sr::cas_weak_word(slots[i].b, word, with_waiter(word));
            }
            sync.arrive_and_wait();
            sync.arrive_and_wait();
        }
    };
    std::thread b(worker), c(waiter_announce);
    client();
    b.join();
    c.join();
    std::vector<std::string> forbidden;
    for (auto& [k, v] : t.n)
        if (k != "worker won: client resolves DONE" && k != "client abandoned, worker freed") forbidden.push_back(k);
    t.report("abandon (client) vs complete (worker) vs WAITER announce", forbidden);
}

/// Three producers enqueue into one small ring while a consumer model dequeues (shm-abi §4.4),
/// one producer now and then unwinding inside the enqueue (when exceptions are on). Every
/// descriptor arrives exactly once, in each producer's order; every unwound one as a void cell.
void enqueue_race(int rounds) {
    constexpr std::uint64_t cap = 16;
    alignas(64) static std::byte ring[256 + cap * 64];
    for (std::uint64_t i = 0; i < cap; ++i) abi::acc::cell::set_seq(ring + 256 + i * 64, i);
    const RingRef r{ring, cap};
    const std::uint64_t per = static_cast<std::uint64_t>(rounds) * 2000;
    std::atomic<std::uint64_t> voids_sent{0};
    std::atomic<bool> done{false};
    Tally t;
    auto producer = [&](std::uint32_t p) {
        for (std::uint64_t s = 0; s < per; ++s) {
            Descriptor d{};
            d.request_id = (static_cast<std::uint64_t>(p) << 48) | s;
            d.slot_index = p;
#if SCZ_HOOKS_THROW
            if (p == 0 && s % 997 == 0) scz::testing::unwind_once.store(true, std::memory_order_relaxed);  // once: any producer may take it
#endif
            for (;;) {
#if SCZ_HOOKS_THROW
                try {
                    if (enqueue(r, d) == Enqueue::Ok) break;
                } catch (const scz::testing::Unwound&) {
                    voids_sent.fetch_add(1);
                    continue;  // the cell went out void: send the descriptor again
                }
#else
                if (enqueue(r, d) == Enqueue::Ok) break;
#endif
                cpu_relax();
            }
        }
    };
    std::vector<std::uint64_t> next(3, 0);
    std::uint64_t voids = 0, got = 0, wrong = 0;
    std::thread consumer([&] {
        std::uint64_t head = 0;
        std::uint64_t idle_since = 0;
        while (got < 3 * per) {
            std::byte* cell = r.cell(head);
            if (abi::acc::cell::seq(cell) != head + 1) {
                const std::uint64_t now = monotonic_ns();
                if (idle_since == 0) idle_since = now;
                if (done.load() && now - idle_since > 5'000'000'000ULL) break;  // something was lost
                if (now - idle_since > 20'000'000'000ULL) {
                    std::printf("    stalled: head %" PRIu64 " tail %" PRIu64 " got %" PRIu64 " seqs", head, abi::acc::ring_header::tail(ring), got);
                    for (std::uint64_t i = 0; i < cap; ++i) std::printf(" %" PRIu64, abi::acc::cell::seq(ring + 256 + i * 64));
                    std::printf("\n");
                    std::fflush(stdout);
                    std::abort();
                }
                cpu_relax();
                continue;
            }
            idle_since = 0;
            const std::uint64_t id = abi::acc::cell::request_id(cell);
            const std::uint32_t flags = abi::acc::cell::flags(cell);
            abi::acc::cell::set_seq(cell, head + cap);
            abi::acc::ring_header::set_head(ring, head + 1);
            ++head;
            if (flags & abi::cell_flag::VOID) {
                ++voids;
                continue;
            }
            const auto p = static_cast<std::size_t>(id >> 48);
            const std::uint64_t s = id & ((1ULL << 48) - 1);
            if (p >= 3 || s != next[p]) ++wrong;
            else ++next[p];
            ++got;
        }
    });
    std::thread a(producer, 1), b(producer, 2);
    producer(0);
    a.join();
    b.join();
    done = true;
    consumer.join();
    t.n["descriptors delivered once, in order"] = got - wrong;
    if (got < 3 * per) t.n["lost, duplicated, or reordered"] += 3 * per - got;
    if (wrong) t.n["lost, duplicated, or reordered"] = wrong;
    t.n["void cells skipped"] = voids;
    if (voids != voids_sent.load()) t.n["void cells that did not arrive"] = voids_sent.load() - voids;
    t.report("enqueue: three producers, one consumer, unwinding producers", {"lost, duplicated, or reordered", "void cells that did not arrive"});
}

/// Callers claim entries while the background thread replaces and reclaims snapshots. A caller
/// inside a snapshot always finds it alive (its id matches its marker); every replaced
/// snapshot is eventually reclaimed. Built with ASan too, where a use after free aborts.
void entries_vs_retire(int rounds) {
    scz::Options o;
    o.callers = 8;
    Core core(scz::Endpoint::parse("@unused"), o, 0, false);
    std::atomic<bool> stop{false};
    std::atomic<std::uint64_t> inside{0}, dead{0}, noentry{0};
    auto caller = [&](std::uint32_t k) {
        const std::uint32_t tid = kernel_tid();  // a live thread: retire honours its markers
        std::uint32_t hint = k;
        while (!stop.load(std::memory_order_relaxed)) {
            Snapshot* s = nullptr;
            CallerEntry* e = nullptr;
            const Enter en = core.enter(tid, hint, s, e);
            if (en == Enter::NoEntry) noentry.fetch_add(1, std::memory_order_relaxed);
            if (en != Enter::Ok) continue;
            const std::uint64_t id = e->in_call.load(std::memory_order_relaxed) >> 32;
            if (s->session_id != id) dead.fetch_add(1, std::memory_order_relaxed);
            inside.fetch_add(1, std::memory_order_relaxed);
            e->in_call.store(0, std::memory_order_release);
        }
    };
    std::vector<std::thread> ts;
    for (std::uint32_t k = 0; k < 3; ++k) ts.emplace_back(caller, k);
    std::uint64_t replaced = 0, reclaimed = 0;
    std::vector<std::uint64_t> pending;
    const int n = rounds * 200;
    for (int i = 0; i < n; ++i) {
        // The id replace() will publish it under (only this thread publishes or reclaims).
        std::uint64_t id = core.next_id;
        for (std::uint32_t tries = 0; tries < kSnapSlots && core.slots[id % kSnapSlots].load() != nullptr; ++tries) id = id == UINT32_MAX ? 1 : id + 1;
        auto* s = new Snapshot();
        s->session_id = id;
        const std::uint64_t old = core.replace(s);
        if (old == UINT64_MAX) {
            delete s;
            continue;
        }
        if (core.current.load() != id) dead.fetch_add(1000000);  // the prediction is part of the test
        ++replaced;
        if (old != 0) pending.push_back(old);
        std::vector<std::uint64_t> still;
        for (std::uint64_t id : pending) {
            if (core.reclaim(id)) ++reclaimed;
            else still.push_back(id);
        }
        pending.swap(still);
    }
    stop = true;
    for (auto& t : ts) t.join();
    for (std::uint64_t id : pending)
        if (core.reclaim(id)) ++reclaimed;
    if (const std::uint64_t last = core.replace(nullptr); last != 0 && core.reclaim(last)) ++reclaimed;
    Tally t;
    t.n["calls inside a live snapshot"] = inside.load() - dead.load();
    if (dead.load()) t.n["a call inside a reclaimed snapshot"] = dead.load();
    t.n["snapshots published"] = replaced;
    t.n["snapshots reclaimed"] = reclaimed;
    if (reclaimed != replaced) t.n["snapshots never reclaimed"] = replaced - reclaimed;
    t.n["no entry free (probe limit)"] = noentry.load();
    t.report("caller-entry claim vs replace and reclaim", {"a call inside a reclaimed snapshot", "snapshots never reclaimed"});
}

}  // namespace

int main(int argc, char** argv) {
    const int rounds = argc > 1 ? std::atoi(argv[1]) : 1000;
    const bool trace = std::getenv("SCZ_STRESS_TRACE") != nullptr;
    auto step = [&](const char* name, auto&& f) {
        if (trace) std::fprintf(stderr, "start %s at %.3f s\n", name, static_cast<double>(monotonic_ns() % 1'000'000'000'000ULL) / 1e9);
        f();
    };
    step("claim", [&] { claim_race(rounds * 5); });
    step("cancel", [&] { cancel_vs_claim(rounds * 5); });
    step("abandon", [&] { abandon_vs_complete(rounds * 5); });
    step("enqueue", [&] { enqueue_race(rounds); });
    step("entries", [&] { entries_vs_retire(rounds); });
    std::printf("stress: %s\n", g_failed == 0 ? "no forbidden outcome" : "FORBIDDEN OUTCOMES");
    return g_failed == 0 ? 0 : 1;
}
