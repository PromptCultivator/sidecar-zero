// The client (sdk-cpp20 §2, §4, §5): Client<Codec, Fallback>, the call algorithm of
// sdk-contract §4, the caller-state table with in-call markers, the background thread of §6
// (connect, bind, hold, rebind, retire, reconnect), and fork handling (§7).
//
// Request threads read one atomic snapshot id and claim one caller entry per call; they never
// lock, allocate, log, or unmap. Everything else happens on the background thread `scz-conn`.
#pragma once
#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <new>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>

#include "abi.hpp"
#include "codec.hpp"
#include "detail/handshake.hpp"
#include "detail/layout.hpp"
#include "detail/platform.hpp"
#include "detail/registry.hpp"
#include "detail/ring.hpp"
#include "detail/shared.hpp"
#include "detail/slot.hpp"
#include "fallback.hpp"
#include "metrics.hpp"

#if SCZ_LINUX
#include <pthread.h>
#include <signal.h>
#include <sys/mman.h>
#include <time.h>
#else
#include <thread>
#endif

namespace scz::inline v1 {

/// Where the session socket lives: a filesystem path, or `@name` in the abstract namespace.
class Endpoint {
public:
    /// `@name` is the abstract name `name`; anything else is a path. A name too long for a
    /// socket address leaves the endpoint empty, and the client never connects.
    static Endpoint parse(std::string_view s) noexcept { return Endpoint(s); }
    static Endpoint path(std::string_view p) noexcept { return Endpoint(p); }
    [[nodiscard]] std::string_view str() const noexcept { return {buf_, len_}; }

private:
    explicit Endpoint(std::string_view s) noexcept {
        if (s.size() < sizeof buf_) {
            std::memcpy(buf_, s.data(), s.size());
            len_ = s.size();
        }
        buf_[len_] = '\0';
    }
    char buf_[108] = {};
    std::size_t len_ = 0;
};

struct Options {
    /// Stop waiting this long before the deadline, to leave the fallback its time (§1).
    std::chrono::nanoseconds fallback_reserve{0};
    /// Applied to the connect and to each handshake message.
    std::chrono::milliseconds connect_timeout{1000};
    /// Caller-state entries: at most this many calls can be in flight at once.
    std::uint32_t callers = 256;
    /// Fix the spin ceiling (sdk-contract §7), zero included; unset follows the sidecar's spin_ns.
    std::optional<std::chrono::nanoseconds> spin_ceiling{};
    /// Time one call in this many on each thread, counted over every client the thread calls,
    /// for the latency histograms (sdk-contract §8, "Sampling"); 0 times none.
    std::uint32_t sample_every = kDefaultSampleEvery;
};

/// The client states of sdk-contract §6.
enum class ClientState : std::uint32_t { Disconnected, Connecting, Bound, Rejected, SchemaRejected };

/// Counters summed over every caller entry, and the sampled histograms (sdk-contract §8).
struct Stats {
    std::uint64_t model = 0;
    std::array<std::uint64_t, kReasonCount> fallbacks{};
    std::uint64_t sessions = 0;  // sessions bound so far
    std::uint64_t retired = 0;   // arenas unmapped after their session ended
    /// Waits that timed out and then found the slot done more than 1 ms earlier.
    std::uint64_t late_wakes = 0;
    /// Calls whose budget exceeded the sidecar's max_deadline_ns.
    std::uint64_t clamped = 0;
    /// Callers whose breaker is open now (shedding).
    std::uint64_t breaker_open = 0;
    /// The highest model generation that has answered any caller.
    std::uint64_t model_gen = 0;
    /// End-to-end infer time of the timed calls the model answered, and of those that fell
    /// back (the fallback included); and the time spent inside the fallback function.
    Histogram latency_model;
    Histogram latency_fallback;
    Histogram fallback_time;
};

namespace detail {

inline constexpr std::uint32_t kSdkVersion = 0x000100;
inline constexpr std::uint32_t kSnapSlots = 16;   // replaced snapshots that may await reclamation
inline constexpr std::uint32_t kEntryProbe = 8;   // entries a call tries before NO_SLOT
inline constexpr std::uint32_t kShards = 64;
inline constexpr std::uint32_t kBreakerAfter = 8;
inline constexpr std::uint64_t kBreakerMaxNs = 100'000'000;

[[nodiscard]] inline std::uint64_t mix64(std::uint64_t x) noexcept {
    x += 0x9e3779b97f4a7c15ULL;
    x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9ULL;
    x = (x ^ (x >> 27)) * 0x94d049bb133111ebULL;
    return x ^ (x >> 31);
}

/// Per-caller state (sdk-cpp20 §4). A call claims an entry by writing its in-call marker
/// `(snapshot id << 32) | kernel thread id` with one compare-and-swap, and clears it on every
/// exit. Only the holder touches the rest; the claim and the release order successive holders.
/// Counters are atomics because stats() sums them from another thread; so is the breaker's
/// `brk_until`, which stats() reads for the breaker_open gauge (relaxed: a plain load and store).
struct alignas(64) CallerEntry {
    std::atomic<std::uint64_t> in_call{0};
    std::uint32_t cached_slot[abi::MAX_SLAB_CLASSES] = {};
    std::uint32_t ring_cursor = 0;
    std::uint64_t next_request = 0;
    // The breaker (sdk-contract §4, "Shedding under overload"): consecutive misses while
    // closed; when open, the time it stops shedding (0: closed), and its current interval.
    std::uint32_t brk_misses = 0;
    std::atomic<std::uint64_t> brk_until{0};
    std::uint64_t brk_interval = 0;
    std::atomic<std::uint64_t> model{0};
    // sdk-contract §8: the last model generation that answered this entry's callers (read on
    // every model answer, so on the line `model` is on), and calls whose budget was above
    // max_deadline_ns.
    std::atomic<std::uint64_t> model_gen{0};
    std::atomic<std::uint64_t> clamped{0};
    std::atomic<std::uint64_t> late_wakes{0};
    std::atomic<std::uint64_t> fallbacks[kReasonCount] = {};

    [[nodiscard]] bool shedding(std::uint64_t t0) const noexcept {
        const std::uint64_t until = brk_until.load(std::memory_order_relaxed);
        return until != 0 && t0 < until;
    }
    void open(std::uint64_t interval) noexcept {
        const std::uint64_t now = monotonic_ns();
        // Jittered between half and one and a half intervals, so callers do not probe together.
        const std::uint64_t j = mix64(now ^ reinterpret_cast<std::uintptr_t>(this)) % std::max<std::uint64_t>(interval, 1);
        brk_interval = interval;
        brk_until.store(now + interval / 2 + j, std::memory_order_relaxed);
    }
    /// The breaker's view of a finished call. `reason` is ignored for a model answer.
    void record(bool model_answer, FallbackReason reason, std::uint64_t budget_ns) noexcept {
        if (!model_answer && reason == FallbackReason::Overloaded) return;
        if (!model_answer && (reason == FallbackReason::Deadline || reason == FallbackReason::RingFull)) {
            if (brk_until.load(std::memory_order_relaxed) != 0) {
                open(std::min(brk_interval * 2, kBreakerMaxNs));  // the probe missed: shed again, for twice as long
            } else if (++brk_misses >= kBreakerAfter) {
                brk_misses = 0;
                open(std::clamp<std::uint64_t>(budget_ns * 2, 1, kBreakerMaxNs));
            }
            return;
        }
        brk_misses = 0;
        brk_until.store(0, std::memory_order_relaxed);
    }
};

inline void bump(std::atomic<std::uint64_t>& c) noexcept { c.store(c.load(std::memory_order_relaxed) + 1, std::memory_order_relaxed); }

/// The thread-local state: the kernel thread id, cached, and an index hint into the caller
/// table, both trivially destructible integers (sdk-cpp20 §4); and t_sample below.
struct ThreadIds {
    std::uint32_t tid = 0;
    std::uint32_t hint = 0;
};
inline thread_local ThreadIds t_ids{};

/// This thread's calls so far, over every client that samples (sdk-contract §8, "Sampling"):
/// a trivially constructible integer, so static TLS and no allocation on first touch. Kept
/// apart from t_ids, which fork_child resets: in a forked child the count runs on, as in the
/// Rust SDK.
inline thread_local std::uint32_t t_sample = 0;

[[nodiscard]] inline std::uint32_t this_tid() noexcept {
#if SCZ_LINUX
    if (t_ids.tid == 0) {
        t_ids.tid = kernel_tid();
        t_ids.hint = (t_ids.tid * 0x9E3779B9U) >> 8;
    }
#else
    if (t_ids.tid == 0) t_ids.tid = 1;
#endif
    return t_ids.tid;
}

/// One mapped session: the control segment (read-only) and the data segment.
struct Arena {
    std::byte* ctl = nullptr;
    std::size_t ctl_len = 0;
    std::byte* data = nullptr;
    std::size_t data_len = 0;
    Directory dir{};
    std::uint32_t refs = 0;  // snapshots that name it; background thread only
};

/// How the codec meets the published registry (arrow-data-plane §9).
enum class Mode : std::uint8_t {
    Raw,       // an empty registry and an opaque codec
    Frames,    // the registry's schema is the codec's
    Mismatch,  // they differ: SCHEMA_MISMATCH
    Unbound,   // the registry could not be read; the next admission asks again
};

/// What request threads see: one arena and the binding to its registry, loaded together.
struct Snapshot {
    Arena* arena = nullptr;
    std::uint64_t session_id = 0;
    Mode mode = Mode::Unbound;
    std::uint32_t epoch = 0;
    /// The gate generation the binding was last confirmed under; request threads update it.
    std::atomic<std::uint32_t> confirmed{0};
    Registry* reg = nullptr;  // owned; set in Frames mode
};

enum class Enter { Ok, NoSnapshot, NoEntry };

/// Everything the client and its background thread share. Not a template: the codec enters
/// only as the fingerprint it wants.
class Core {
public:
    Core(const Endpoint& ep, const Options& o, std::uint64_t want, bool has_want) noexcept
        : endpoint(ep),
          want_fp(want),
          has_want(has_want),
          connect_timeout_ms(static_cast<std::uint64_t>(std::max<std::int64_t>(o.connect_timeout.count(), 1))),
          entry_count(std::max<std::uint32_t>(o.callers, 1)),
          spin_ceiling_ns(o.spin_ceiling ? static_cast<std::uint64_t>(std::max<std::int64_t>(o.spin_ceiling->count(), 0)) : UINT64_MAX) {
        entries = new (std::nothrow) CallerEntry[entry_count];
        if (entries == nullptr) entry_count = 0;
        // Each entry starts its ring cursor and its slot probes at a hash of itself, so that
        // callers that arrive together do not all probe the same few slots.
        for (std::uint32_t i = 0; i < entry_count; ++i) {
            const std::uint64_t h = mix64(i);
            entries[i].ring_cursor = static_cast<std::uint32_t>(h);
            for (std::uint32_t c = 0; c < abi::MAX_SLAB_CLASSES; ++c) entries[i].cached_slot[c] = static_cast<std::uint32_t>(mix64(h + c) >> 32);
        }
    }
    ~Core() { delete[] entries; }
    Core(const Core&) = delete;
    Core& operator=(const Core&) = delete;

    // ── published snapshots (sdk-contract §6, "Retiring an old arena") ──
    std::atomic<std::uint64_t> current{0};
    std::atomic<Snapshot*> slots[kSnapSlots] = {};
    std::uint64_t next_id = 1;  // background thread only

    CallerEntry* entries = nullptr;
    struct alignas(64) Shard {
        std::atomic<std::uint64_t> n[kReasonCount] = {};
    };
    /// Fallbacks taken with no snapshot, so with no entry to count them in.
    Shard shards[kShards];
    /// The sampled histograms (sdk-contract §8), sharded by the thread's index hint; only
    /// timed calls touch them.
    HistShard hists[kHistShards];

    Endpoint endpoint;
    std::uint64_t want_fp;
    bool has_want;
    std::uint64_t connect_timeout_ms;
    std::uint32_t entry_count;
    std::atomic<std::uint64_t> spin_ceiling_ns;
    std::atomic<std::uint32_t> state{static_cast<std::uint32_t>(ClientState::Disconnected)};
    std::atomic<bool> stop{false};
    std::atomic<bool> rebind{false};
    /// Set in a forked child: no background thread exists. The next call without a snapshot
    /// starts one (sdk-cpp20 §5, "Fork").
    std::atomic<bool> orphaned{false};
    std::atomic<bool> child_cleanup{false};
    /// The session socket, from socket() to close: one owner, so a forked child can close its copy.
    std::atomic<int> session_fd{-1};
    std::atomic<std::uint64_t> sessions{0};
    std::atomic<std::uint64_t> retired{0};
#if SCZ_LINUX
    pthread_t bg{};
#endif
    bool bg_running = false;

    [[nodiscard]] FallbackReason unattached_reason() const noexcept {
        return state.load(std::memory_order_relaxed) == static_cast<std::uint32_t>(ClientState::Rejected) ? FallbackReason::AbiMismatch
                                                                                                       : FallbackReason::NotConnected;
    }

    /// Claim an entry for the current snapshot: marker CAS (seq_cst), fence, re-check that the
    /// snapshot is still current; if not, give the entry back and try again with the new one.
    [[nodiscard]] Enter enter(std::uint32_t tid, std::uint32_t& hint, Snapshot*& snap, CallerEntry*& entry) noexcept {
        const std::uint32_t n = entry_count;
        if (n == 0) return Enter::NoEntry;
        for (int round = 0; round < 4; ++round) {
            const std::uint64_t id = current.load(std::memory_order_seq_cst);
            if (id == 0) return Enter::NoSnapshot;
            const std::uint64_t marker = (id << 32) | tid;
            const std::uint32_t start = hint % n;
            std::uint32_t found = n;
            for (std::uint32_t k = 0; k < std::min(kEntryProbe, n); ++k) {
                const std::uint32_t i = (start + k) % n;
                std::uint64_t zero = 0;
                if (entries[i].in_call.compare_exchange_strong(zero, marker, std::memory_order_seq_cst, std::memory_order_relaxed)) {
                    found = i;
                    break;
                }
            }
            if (found == n) return Enter::NoEntry;
            hint = found;
            std::atomic_thread_fence(std::memory_order_seq_cst);  // the marker before the re-check
            if (current.load(std::memory_order_seq_cst) != id) {
                entries[found].in_call.store(0, std::memory_order_release);
                continue;
            }
            // Snapshot `id` was current after our marker was visible, so its reclamation,
            // which runs only after `current` moved on, finds the marker and waits.
            snap = slots[id % kSnapSlots].load(std::memory_order_acquire);
            entry = &entries[found];
            return Enter::Ok;
        }
        return Enter::NoSnapshot;
    }

    /// arrow-data-plane §9: a new worker generation in the gate is confirmed by reading the
    /// registry epoch under the seqlock. Unchanged: confirmed. Changed: REBINDING, and the
    /// background thread rebinds. Costs no shared read on a normal call.
    [[nodiscard]] bool confirm(Snapshot& s, std::uint32_t gen) noexcept {
        if (s.confirmed.load(std::memory_order_relaxed) == gen) return true;
        std::uint32_t e = 0;
        if (read_epoch(s.arena->ctl, 64, e) && e == s.epoch && s.mode != Mode::Unbound) {
            s.confirmed.store(gen, std::memory_order_relaxed);
            return true;
        }
        rebind.store(true, std::memory_order_release);
        return false;
    }

    void set_state(ClientState s) noexcept { state.store(static_cast<std::uint32_t>(s), std::memory_order_release); }

#if SCZ_LINUX
    // ── the background thread (sdk-contract §6) ──────────────────────────────────────────

    bool start() noexcept {
        sigset_t all, old;
        sigfillset(&all);
        pthread_sigmask(SIG_SETMASK, &all, &old);  // the application's handlers never run on it
        const int rc = pthread_create(&bg, nullptr, &Core::thread_main, this);
        pthread_sigmask(SIG_SETMASK, &old, nullptr);
        if (rc != 0) return false;
        pthread_setname_np(bg, "scz-conn");
        bg_running = true;
        return true;
    }

    /// Stop the background thread: it leaves the session, retires the arena, closes the
    /// socket last, and exits (sdk-contract §6, close()).
    void shutdown() noexcept {
        stop.store(true, std::memory_order_release);
        if (bg_running) {
            pthread_join(bg, nullptr);
            bg_running = false;
        }
    }

    /// In a forked child (sdk-contract §7): DISCONNECTED, no snapshot, the child's copy of the
    /// session socket closed, and the markers of the parent's threads (which do not exist
    /// here) cleared. Atomic stores and close() only.
    void on_fork_child() noexcept {
        orphaned.store(true, std::memory_order_relaxed);
        child_cleanup.store(true, std::memory_order_relaxed);
        bg_running = false;
        current.store(0, std::memory_order_seq_cst);
        const int fd = session_fd.exchange(-1);
        if (fd >= 0) ::close(fd);
        for (std::uint32_t i = 0; i < entry_count; ++i) entries[i].in_call.store(0, std::memory_order_relaxed);
        set_state(ClientState::Disconnected);
    }

    /// The child connects for itself, lazily: the first call that finds no snapshot.
    void restart_after_fork() noexcept {
        bool expected = true;
        if (!orphaned.compare_exchange_strong(expected, false)) return;
        if (!stop.load(std::memory_order_acquire)) (void)start();
    }

    static void* thread_main(void* p) noexcept {
        static_cast<Core*>(p)->run();
        return nullptr;
    }

    /// Sleep, waking within 2 ms if the client is closing.
    void nap(std::uint64_t ns) noexcept {
        const std::uint64_t end = monotonic_ns() + ns;
        while (!stop.load(std::memory_order_acquire)) {
            const std::uint64_t now = monotonic_ns();
            if (now >= end) return;
            const std::uint64_t step = std::min<std::uint64_t>(end - now, 2'000'000);
            timespec ts{0, static_cast<long>(step)};
            nanosleep(&ts, nullptr);
        }
    }

    [[nodiscard]] static std::uint64_t jitter(std::uint64_t ns) noexcept {
        const std::uint64_t half = ns / 2;
        return half + monotonic_ns() % std::max<std::uint64_t>(half, 1);
    }

    void close_session() noexcept {
        const int fd = session_fd.exchange(-1);
        if (fd >= 0) ::close(fd);
    }

    static void unmap(Arena* a) noexcept {
        if (a->ctl != nullptr) ::munmap(a->ctl, a->ctl_len);
        if (a->data != nullptr) ::munmap(a->data, a->data_len);
        delete a;
    }

    void free_snapshot(Snapshot* s) noexcept {
        if (s->arena != nullptr && --s->arena->refs == 0) {
            unmap(s->arena);
            retired.fetch_add(1, std::memory_order_relaxed);
        }
        delete s->reg;
        delete s;
    }

    /// Publish `s` (or clear the reference with nullptr) and return the id that was current.
    /// UINT64_MAX if every slot still holds a snapshot awaiting reclamation.
    [[nodiscard]] std::uint64_t replace(Snapshot* s) noexcept {
        std::uint64_t new_id = 0;
        if (s != nullptr) {
            std::uint64_t id = next_id;
            std::uint32_t tries = 0;
            while (slots[id % kSnapSlots].load(std::memory_order_acquire) != nullptr) {
                id = id == UINT32_MAX ? 1 : id + 1;
                if (++tries == kSnapSlots) return UINT64_MAX;
            }
            next_id = id == UINT32_MAX ? 1 : id + 1;
            slots[id % kSnapSlots].store(s, std::memory_order_release);
            new_id = id;
        }
        const std::uint64_t old = current.exchange(new_id, std::memory_order_seq_cst);
        std::atomic_thread_fence(std::memory_order_seq_cst);  // the store of `current` before the scans that follow
        return old;
    }

    /// Free snapshot `id` if no live caller is inside it. A marker whose thread no longer
    /// exists is ignored: a thread killed inside a call would otherwise pin the arena.
    [[nodiscard]] bool reclaim(std::uint64_t id) noexcept {
        std::atomic_thread_fence(std::memory_order_seq_cst);
        for (std::uint32_t i = 0; i < entry_count; ++i) {
            const std::uint64_t m = entries[i].in_call.load(std::memory_order_seq_cst);
            if ((m >> 32) == id && thread_alive(static_cast<std::uint32_t>(m))) return false;
        }
        Snapshot* s = slots[id % kSnapSlots].exchange(nullptr, std::memory_order_acq_rel);
        if (s != nullptr) free_snapshot(s);
        return true;
    }

    /// Unmap a replaced snapshot once no call is inside it; request threads never do this.
    /// False if a call stayed inside for 5 s while closing: the mapping is left in place.
    bool retire(std::uint64_t id) noexcept {
        const std::uint64_t t0 = monotonic_ns();
        while (!reclaim(id)) {
            if (stop.load(std::memory_order_acquire) && monotonic_ns() - t0 > 5'000'000'000ULL) {
                std::fprintf(stderr, "sidecarzero: an arena is still in use after 5 s; leaving it mapped\n");
                return false;
            }
            timespec ts{0, 1'000'000};
            nanosleep(&ts, nullptr);
        }
        return true;
    }

    /// Read the registry under the model seqlock and bind the codec to it. The gate
    /// generation is read first: the supervisor writes the model block before the gate shows
    /// a new generation, so the block read after it is at least as new.
    [[nodiscard]] Snapshot* bind(Arena* a, std::uint64_t session_id) noexcept {
        Snapshot* s = new (std::nothrow) Snapshot();
        if (s == nullptr) return nullptr;
        s->arena = a;
        s->session_id = session_id;
        const std::uint32_t gen = gate_gen(abi::acc::control_header::gate(a->ctl));
        s->confirmed.store(gen, std::memory_order_relaxed);
        Registry* reg = new (std::nothrow) Registry();
        ModelBlock b{};
        if (reg == nullptr || !read_model(a->ctl, a->ctl_len, 1000, b, *reg)) {
            delete reg;
            s->mode = Mode::Unbound;
            a->refs++;
            return s;
        }
        std::uint32_t caps[abi::MAX_SLAB_CLASSES][2];
        for (std::uint32_t i = 0; i < a->dir.class_count; ++i) {
            caps[i][0] = a->dir.slabs[i].req_cap;
            caps[i][1] = a->dir.slabs[i].resp_cap;
        }
        const char* err = parse(*reg, caps, a->dir.class_count);
        s->epoch = b.registry_epoch;
        if (err == nullptr && !has_want && reg->template_count == 0) {
            s->mode = Mode::Raw;
        } else if (err == nullptr && has_want && reg->template_count > 0 && reg->fp == want_fp && b.schema_fingerprint == want_fp) {
            s->mode = Mode::Frames;
            s->reg = reg;
            reg = nullptr;
        } else if (err != nullptr || b.registry_epoch == 0) {
            if (err != nullptr) std::fprintf(stderr, "sidecarzero: the registry does not parse: %s\n", err);
            s->mode = Mode::Unbound;
        } else {
            s->mode = Mode::Mismatch;
        }
        delete reg;
        a->refs++;
        return s;
    }

    enum class Conn { Ok, Busy, Abi, Other };

    [[nodiscard]] Conn connect_once(Snapshot*& out) noexcept {
        out = nullptr;
        const int fd = seqpacket(connect_timeout_ms);
        if (fd < 0) return Conn::Other;
        session_fd.store(fd);  // owned there from now on, so a forked child closes its copy
        const std::string_view ep = endpoint.str();
        Msg m;
        int fds[2] = {-1, -1};
        auto fail = [&](Conn c) {
            for (int f : fds)
                if (f >= 0) ::close(f);
            close_session();
            return c;
        };
        if (ep.empty() || !connect_to(fd, ep.data(), ep.size())) return fail(Conn::Other);
        encode_hello(m, static_cast<std::uint16_t>(abi::sdk_lang::CPP), kSdkVersion);
        if (!send_msg(fd, m)) return fail(Conn::Other);
        if (!recv_msg(fd, m, fds)) return fail(Conn::Other);
        Welcome w{};
        if (!decode_welcome(m, w)) return fail(Conn::Other);
        if (w.status == abi::welcome_status::BUSY) return fail(Conn::Busy);
        if (w.status == abi::welcome_status::ABI_MISMATCH) return fail(Conn::Abi);
        if (w.status != abi::welcome_status::OK || fds[0] < 0 || fds[1] < 0) return fail(Conn::Other);
        // control: sealed against growth, shrinking, new writable mappings; data: against resizing.
        const int cs = seals_of(fds[0]);
        const int ds = seals_of(fds[1]);
        if (cs < 0 || (cs & kernel::kCtlSeals) != kernel::kCtlSeals || ds < 0 || (ds & kernel::kDataSeals) != kernel::kDataSeals) return fail(Conn::Other);
        const std::int64_t ctl_len = file_len(fds[0]);
        const std::int64_t data_len = file_len(fds[1]);
        if (ctl_len < static_cast<std::int64_t>(abi::CTL_HEADER_LEN) || data_len <= 0 || static_cast<std::uint64_t>(ctl_len) != w.ctl_len ||
            static_cast<std::uint64_t>(data_len) != w.data_len)
            return fail(Conn::Other);
        Arena* a = new (std::nothrow) Arena();
        if (a == nullptr) return fail(Conn::Other);
        auto fail_mapped = [&](Conn c) {
            unmap(a);
            return fail(c);
        };
        void* cp = ::mmap(nullptr, static_cast<std::size_t>(ctl_len), PROT_READ, MAP_SHARED, fds[0], 0);
        if (cp == MAP_FAILED) return fail_mapped(Conn::Other);
        a->ctl = static_cast<std::byte*>(cp);
        a->ctl_len = static_cast<std::size_t>(ctl_len);
        if (const char* why = read_directory(a->ctl, a->ctl_len, static_cast<std::uint64_t>(data_len), a->dir)) {
            std::fprintf(stderr, "sidecarzero: the control segment is refused: %s\n", why);
            return fail_mapped(Conn::Other);
        }
        if (a->dir.arena_id != w.arena_id) return fail_mapped(Conn::Other);
        void* dp = ::mmap(nullptr, static_cast<std::size_t>(data_len), PROT_READ | PROT_WRITE, MAP_SHARED, fds[1], 0);
        if (dp == MAP_FAILED) return fail_mapped(Conn::Other);
        a->data = static_cast<std::byte*>(dp);
        a->data_len = static_cast<std::size_t>(data_len);
        const std::byte* sp = a->data + a->dir.status_off;
        if (abi::acc::status_page::magic(sp) != abi::DATA_MAGIC || abi::acc::status_page::arena_id(sp) != a->dir.arena_id) return fail_mapped(Conn::Other);
        // Page tables now, on this thread, so request threads never take a first-touch fault.
        ::madvise(a->ctl, a->ctl_len, kernel::kMadvPopulateRead);
        ::madvise(a->data, a->data_len, kernel::kMadvPopulateWrite);
        ::close(fds[0]);
        ::close(fds[1]);
        fds[0] = fds[1] = -1;  // the mappings stay
        encode_ready(m, w.arena_id);
        if (!send_msg(fd, m)) return fail_mapped(Conn::Other);
        Snapshot* s = bind(a, w.session_id);
        if (s == nullptr) return fail_mapped(Conn::Other);
        out = s;
        return Conn::Ok;
    }

    /// Keep the session until the supervisor's end closes or the client stops, rebinding when
    /// a request thread asks, and re-checking a rejected or unread schema once a second.
    void hold(Snapshot* cur) noexcept {
        std::uint64_t last_check = monotonic_ns();
        while (!stop.load(std::memory_order_acquire)) {
            if (rebind.exchange(false, std::memory_order_acq_rel)) {
                Snapshot* s = bind(cur->arena, cur->session_id);
                if (s != nullptr) {
                    set_state(s->mode == Mode::Mismatch ? ClientState::SchemaRejected : ClientState::Bound);
                    const std::uint64_t old = replace(s);
                    if (old == UINT64_MAX) {
                        free_snapshot(s);
                    } else {
                        cur = s;
                        if (old != 0) (void)retire(old);
                    }
                }
            }
            if ((cur->mode == Mode::Mismatch || cur->mode == Mode::Unbound) && monotonic_ns() - last_check > 1'000'000'000ULL) {
                last_check = monotonic_ns();
                std::uint32_t e = 0;
                if (cur->mode == Mode::Unbound || (read_epoch(cur->arena->ctl, 64, e) && e != cur->epoch)) rebind.store(true, std::memory_order_release);
            }
            const int fd = session_fd.load();
            if (fd < 0) return;
            const int r = wait_readable(fd, 5);
            if (r == 0) continue;
            if (r < 0) return;
            const int pc = peer_closed(fd);
            if (pc != 0) return;
            Msg m;
            int fds[2];
            if (recv_msg(fd, m, fds)) {  // nothing is defined yet; drop it
                for (int f : fds)
                    if (f >= 0) ::close(f);
            }
        }
    }

    void run() noexcept {
        if (child_cleanup.exchange(false)) {
            // A forked child: the parent's snapshots are no one's; unmap the inherited arenas.
            for (auto& sl : slots) {
                Snapshot* s = sl.exchange(nullptr);
                if (s != nullptr) free_snapshot(s);
            }
        }
        std::uint64_t backoff = 10'000'000;
        std::uint64_t busy_since = 0;
        bool warned_abi = false;
        while (!stop.load(std::memory_order_acquire)) {
            set_state(ClientState::Connecting);
            Snapshot* snap = nullptr;
            switch (connect_once(snap)) {
            case Conn::Ok: {
                busy_since = 0;
                backoff = 10'000'000;
                warned_abi = false;
                const bool rejected = snap->mode == Mode::Mismatch;
                if (replace(snap) == UINT64_MAX) {  // cannot happen: every old one was retired
                    free_snapshot(snap);
                    close_session();
                    continue;
                }
                set_state(rejected ? ClientState::SchemaRejected : ClientState::Bound);
                sessions.fetch_add(1, std::memory_order_relaxed);
                hold(snap);
                // Retirement starts now, before any reconnect (sdk-contract §6).
                set_state(ClientState::Disconnected);
                const std::uint64_t old = replace(nullptr);
                const bool unmapped = old == 0 || old == UINT64_MAX || retire(old);
                if (unmapped) close_session();  // last: its closing tells the supervisor nothing maps the arena
                break;
            }
            case Conn::Busy: {
                set_state(ClientState::Disconnected);
                const std::uint64_t now = monotonic_ns();
                if (busy_since == 0) busy_since = now;
                if (now - busy_since < 1'000'000'000ULL) {
                    nap(20'000'000);
                } else {
                    nap(jitter(backoff));
                    backoff = std::min<std::uint64_t>(backoff * 2, 1'000'000'000ULL);
                }
                break;
            }
            case Conn::Abi:
                set_state(ClientState::Rejected);
                if (!warned_abi) {
                    std::fprintf(stderr, "sidecarzero: the sidecar at %s does not speak this SDK's ABI; serving fallbacks, retrying every 30 s\n",
                                 endpoint.str().data());
                    warned_abi = true;
                }
                nap(30'000'000'000ULL);
                break;
            case Conn::Other:
                set_state(ClientState::Disconnected);
                nap(jitter(backoff));
                backoff = std::min<std::uint64_t>(backoff * 2, 1'000'000'000ULL);
                break;
            }
        }
        set_state(ClientState::Disconnected);
    }
#else
    bool start() noexcept { return false; }
    void shutdown() noexcept { stop.store(true); }
    void restart_after_fork() noexcept {}
#endif
};

/// Clears a call's in-call marker on every exit, unwinding included.
struct EntryGuard {
    CallerEntry* e;
    ~EntryGuard() { e->in_call.store(0, std::memory_order_release); }
};

// ── fork (sdk-cpp20 §5): one handler per process, walking a registry of live clients ──
inline constexpr std::size_t kForkMax = 64;
inline std::atomic_flag g_fork_lock = ATOMIC_FLAG_INIT;
inline Core* g_cores[kForkMax] = {};
inline std::atomic<bool> g_atfork_installed{false};

inline void fork_lock() noexcept {
    while (g_fork_lock.test_and_set(std::memory_order_acquire)) cpu_relax();
}
inline void fork_unlock() noexcept { g_fork_lock.clear(std::memory_order_release); }

#if SCZ_LINUX
inline void fork_prepare() noexcept { fork_lock(); }
inline void fork_parent() noexcept { fork_unlock(); }
inline void fork_child() noexcept {
    t_ids = ThreadIds{};  // this thread has a new id in the child
    for (Core* c : g_cores)
        if (c != nullptr) c->on_fork_child();
    fork_unlock();
}
#endif

inline void register_core(Core* c) noexcept {
#if SCZ_LINUX
    bool expected = false;
    if (g_atfork_installed.compare_exchange_strong(expected, true)) pthread_atfork(&fork_prepare, &fork_parent, &fork_child);
#endif
    fork_lock();
    for (Core*& slot : g_cores)
        if (slot == nullptr) {
            slot = c;
            break;
        }
    fork_unlock();
}

inline void unregister_core(Core* c) noexcept {
    fork_lock();
    for (Core*& slot : g_cores)
        if (slot == c) slot = nullptr;
    fork_unlock();
}

[[nodiscard]] inline FallbackReason gate_fallback(std::uint64_t g) noexcept {
    switch (gate_state(g)) {
    case abi::gate_state::INIT: return FallbackReason::GateInit;
    case abi::gate_state::PSI_SHEDDING: return FallbackReason::GateShedding;
    case abi::gate_state::RELOADING: return FallbackReason::GateReloading;
    case abi::gate_state::DRAINING: return FallbackReason::GateDraining;
    default: return FallbackReason::GateDead;
    }
}

[[nodiscard]] inline FallbackReason status_fallback(std::uint32_t s) noexcept {
    switch (s) {
    case abi::status::DEADLINE_EXCEEDED: return FallbackReason::Deadline;
    case abi::status::BAD_REQUEST: return FallbackReason::BadRequest;
    case abi::status::MODEL_ERROR: return FallbackReason::ModelError;
    case abi::status::RESOURCE_EXHAUSTED: return FallbackReason::ResourceExhausted;
    case abi::status::STALE_REGISTRY: return FallbackReason::Rebinding;
    default: return FallbackReason::WorkerFailed;
    }
}

/// Takes back the slot if the call unwinds while it holds it: CLAIMED → FREE, or, once READY
/// (inside the enqueue, whose own guard has already voided the cell), cancel it.
struct SlotGuard {
    std::byte* rec;
    std::uint32_t ep;
    int stage = 0;  // 0 claimed, 1 ready, 2 disarmed
    ~SlotGuard() {
        if (stage == 0) release(rec, ep);
        else if (stage == 1 && cancel_if_ready(rec, ep) == Cancel::NotReady) release(rec, ep);  // the sweep failed it meanwhile
    }
};

}  // namespace detail

/// A SidecarZero client: `infer` always returns an output, the model's if it answers within
/// the budget, else the fallback's (sdk-contract §1). There is no constructor without a
/// fallback, the fallback must be noexcept (FallbackPolicy), and `infer` is noexcept. `H` is
/// the optional on_fallback hook (FallbackHook; NoHook, the default, calls nothing).
template <Codec C, FallbackPolicy<typename C::input_type, typename C::output_type> F, FallbackHook H = NoHook>
class Client {
public:
    using input_type = typename C::input_type;
    using output_type = typename C::output_type;

    /// Returns at once; a background thread connects. Calls fall back with NOT_CONNECTED until
    /// it has (sdk-contract §6, "Startup"). Failures surface through the connection state.
    /// `on_fallback`, if given, is called with the reason before each fallback (sdk-contract §8).
    Client(Endpoint endpoint, C codec, F fallback, Options options = {}, H on_fallback = {}) noexcept
        : codec_(std::move(codec)),
          fallback_(std::move(fallback)),
          on_fallback_(std::move(on_fallback)),
          reserve_ns_(static_cast<std::uint64_t>(std::max<std::int64_t>(options.fallback_reserve.count(), 0))),
          sample_every_(options.sample_every) {
        const std::uint64_t fp = schema_fingerprint(codec_);
        core_ = new (std::nothrow) detail::Core(endpoint, options, fp, FrameCodec<C>);
        if (core_ == nullptr) return;
        detail::register_core(core_);
        (void)core_->start();
    }
    ~Client() {
        close();
        if (core_ != nullptr) {
            detail::unregister_core(core_);
            delete core_;
        }
    }
    Client(const Client&) = delete;  // owns mappings, a thread, and the caller-state table
    Client& operator=(const Client&) = delete;
    Client(Client&&) = delete;
    Client& operator=(Client&&) = delete;

    /// Always returns an output (sdk-contract §1). Never throws: a failing codec is a
    /// CODEC_ERROR fallback, and the fallback is noexcept. A timed call (sdk-contract §8) reads
    /// the clock at entry and at return, and around the fallback; the others read it no more.
    [[nodiscard]] Outcome<output_type> infer(const input_type& input, std::chrono::nanoseconds budget) SCZ_CALL_NOEXCEPT {
        const bool timed = sampled();
        const std::uint64_t t_start = timed ? detail::monotonic_ns() : 0;
        const std::uint64_t budget_ns = budget.count() <= 0 ? 0 : static_cast<std::uint64_t>(budget.count());
        output_type out{};
        std::uint32_t gen = 0;
        FallbackReason reason = FallbackReason::NotConnected;
        if (try_model(input, budget_ns, out, gen, reason)) {
            if (timed && core_ != nullptr) hists().model.record(elapsed(t_start, detail::monotonic_ns()));
            return Outcome<output_type>{std::move(out), Source::Model, FallbackReason::NotConnected, gen};
        }
        // The slot and the caller entry have already been released.
        if constexpr (!std::is_same_v<H, NoHook>) std::as_const(on_fallback_)(reason);
        if (!timed || core_ == nullptr) return Outcome<output_type>{fallback_(input, reason), Source::Fallback, reason, 0};
        const std::uint64_t t1 = detail::monotonic_ns();
        Outcome<output_type> o{fallback_(input, reason), Source::Fallback, reason, 0};
        const std::uint64_t t2 = detail::monotonic_ns();
        detail::HistShard& h = hists();
        h.fallback_fn.record(elapsed(t1, t2));
        h.fallback.record(elapsed(t_start, t2));
        return o;
    }

    /// Wait until an arena is mapped and its gate is open, for callers that prefer to start
    /// warm. Mapped alone is not enough: calls would still fall back with GATE_INIT.
    [[nodiscard]] bool await_bound(std::chrono::milliseconds timeout) noexcept {
        const std::uint64_t end = detail::monotonic_ns() + static_cast<std::uint64_t>(std::max<std::int64_t>(timeout.count(), 0)) * 1'000'000ULL;
        for (;;) {
            if (gate_open()) return true;
            if (detail::monotonic_ns() >= end) return false;
#if SCZ_LINUX
            timespec ts{0, 1'000'000};
            nanosleep(&ts, nullptr);
#else
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
#endif
        }
    }

    [[nodiscard]] ClientState state() const noexcept {
        return core_ == nullptr ? ClientState::Disconnected : static_cast<ClientState>(core_->state.load(std::memory_order_acquire));
    }

    /// Counters summed over every caller entry and the unattached shards, and the sampled
    /// histograms summed over their shards.
    [[nodiscard]] Stats stats() const noexcept {
        Stats s;
        if (core_ == nullptr) return s;
        s.sessions = core_->sessions.load(std::memory_order_relaxed);
        s.retired = core_->retired.load(std::memory_order_relaxed);
        const std::uint64_t now = detail::monotonic_ns();
        for (std::uint32_t i = 0; i < core_->entry_count; ++i) {
            const detail::CallerEntry& e = core_->entries[i];
            s.model += e.model.load(std::memory_order_relaxed);
            s.late_wakes += e.late_wakes.load(std::memory_order_relaxed);
            s.clamped += e.clamped.load(std::memory_order_relaxed);
            s.model_gen = std::max(s.model_gen, e.model_gen.load(std::memory_order_relaxed));
            if (e.shedding(now)) ++s.breaker_open;
            for (std::size_t r = 0; r < kReasonCount; ++r) s.fallbacks[r] += e.fallbacks[r].load(std::memory_order_relaxed);
        }
        for (const auto& sh : core_->shards)
            for (std::size_t r = 0; r < kReasonCount; ++r) s.fallbacks[r] += sh.n[r].load(std::memory_order_relaxed);
        s.latency_model = detail::sum_of(core_->hists, detail::kHistShards, &detail::HistShard::model);
        s.latency_fallback = detail::sum_of(core_->hists, detail::kHistShards, &detail::HistShard::fallback);
        s.fallback_time = detail::sum_of(core_->hists, detail::kHistShards, &detail::HistShard::fallback_fn);
        return s;
    }

    /// Every metric of sdk-contract §8, in one pass, to `sink`, in the order every SDK uses:
    /// the calls by source (every reason, zeros included), the latency histogram per source,
    /// the fallback histogram, then budget_clamped_total, breaker_open, late_wakes_total,
    /// reconnects_total (sessions after the first), state (0 to 4), and model_gen. The Rust
    /// SDK's `export` (`export` is a keyword in C++). Not on the call path.
    void export_metrics(MetricsSink& sink) const {
        const Stats s = stats();
        const Label model[] = {{"source", "model"}};
        sink.counter("scz_client_calls_total", model, s.model);
        for (std::size_t r = 0; r < kReasonCount; ++r) {
            const Label l[] = {{"source", "fallback"}, {"reason", reason_name(static_cast<FallbackReason>(r))}};
            sink.counter("scz_client_calls_total", l, s.fallbacks[r]);
        }
        const Label fallback[] = {{"source", "fallback"}};
        const auto hist = [&sink](std::string_view name, std::span<const Label> labels, const Histogram& h) {
            sink.histogram(name, labels, kBucketsSeconds, h.cumulative, h.sum_seconds, h.count);
        };
        hist("scz_client_latency_seconds", model, s.latency_model);
        hist("scz_client_latency_seconds", fallback, s.latency_fallback);
        hist("scz_client_fallback_seconds", {}, s.fallback_time);
        sink.counter("scz_client_budget_clamped_total", {}, s.clamped);
        sink.gauge("scz_client_breaker_open", {}, static_cast<double>(s.breaker_open));
        sink.counter("scz_client_late_wakes_total", {}, s.late_wakes);
        sink.counter("scz_client_reconnects_total", {}, s.sessions > 0 ? s.sessions - 1 : 0);
        sink.gauge("scz_client_state", {}, static_cast<double>(static_cast<std::uint32_t>(state())));
        sink.gauge("scz_client_model_gen", {}, static_cast<double>(s.model_gen));
    }

    /// export_metrics in the Prometheus text exposition format (PrometheusText).
    [[nodiscard]] std::string prometheus_text() const {
        PrometheusText t;
        export_metrics(t);
        return t.finish();
    }

    /// Fix the spin ceiling (sdk-contract §7): zero parks at once; nullopt restores the sidecar's.
    void set_spin_ceiling(std::optional<std::chrono::nanoseconds> d) noexcept {
        if (core_ != nullptr)
            core_->spin_ceiling_ns.store(d ? static_cast<std::uint64_t>(std::max<std::int64_t>(d->count(), 0)) : UINT64_MAX, std::memory_order_relaxed);
    }

    /// sdk-contract §6 close(): clear the snapshot, wait until no call is inside an arena,
    /// unmap, close the socket last; then join the thread. Later calls fall back with
    /// NOT_CONNECTED. Also what the destructor does. In a forked child whose client never
    /// called, there is no thread to join.
    void close() noexcept {
        if (core_ != nullptr) core_->shutdown();
    }

private:
    /// Whether this call is timed: those whose number on this thread, counted over every client
    /// that samples, is a multiple of sample_every, the thread's first call among them.
    [[nodiscard]] bool sampled() const noexcept {
        const std::uint32_t every = sample_every_;
        if (every == 0) return false;
        const std::uint32_t n = detail::t_sample++;
        return (every & (every - 1)) == 0 ? (n & (every - 1)) == 0 : n % every == 0;
    }
    [[nodiscard]] detail::HistShard& hists() const noexcept { return core_->hists[detail::t_ids.hint % detail::kHistShards]; }
    [[nodiscard]] static std::uint64_t elapsed(std::uint64_t from, std::uint64_t to) noexcept { return to > from ? to - from : 0; }

    [[nodiscard]] bool gate_open() noexcept {
        if (core_ == nullptr) return false;
        detail::Snapshot* snap = nullptr;
        detail::CallerEntry* entry = nullptr;
        if (core_->enter(detail::this_tid(), detail::t_ids.hint, snap, entry) != detail::Enter::Ok) return false;
        detail::EntryGuard g{entry};
        const std::uint64_t gate = abi::acc::control_header::gate(snap->arena->ctl);
        return detail::healthy(gate) && core_->confirm(*snap, detail::gate_gen(gate)) && (snap->mode == detail::Mode::Raw || snap->mode == detail::Mode::Frames);
    }

    bool try_model(const input_type& in, std::uint64_t budget_ns, output_type& out, std::uint32_t& gen, FallbackReason& r) SCZ_CALL_NOEXCEPT {
        if (core_ == nullptr) {
            r = FallbackReason::NotConnected;
            return false;
        }
        detail::Core& c = *core_;
        const std::uint32_t tid = detail::this_tid();
        detail::Snapshot* snap = nullptr;
        detail::CallerEntry* entry = nullptr;
        const detail::Enter en = c.enter(tid, detail::t_ids.hint, snap, entry);
        if (en != detail::Enter::Ok) {
            r = en == detail::Enter::NoEntry ? FallbackReason::NoSlot : c.unattached_reason();
            // Threads can share a shard, so an atomic add.
            c.shards[detail::t_ids.hint % detail::kShards].n[static_cast<std::size_t>(r)].fetch_add(1, std::memory_order_relaxed);
            if (en == detail::Enter::NoSnapshot && c.orphaned.load(std::memory_order_relaxed)) c.restart_after_fork();
            return false;
        }
        detail::EntryGuard guard{entry};
        const bool model = call(*snap, *entry, in, budget_ns, out, gen, r);
        detail::bump(model ? entry->model : entry->fallbacks[static_cast<std::size_t>(r)]);
        if (model && entry->model_gen.load(std::memory_order_relaxed) != gen) entry->model_gen.store(gen, std::memory_order_relaxed);
        entry->record(model, r, std::min(budget_ns, snap->arena->dir.max_deadline_ns));
        return model;
    }

    /// sdk-contract §4, from admission to resolution. Every path out of here has released the
    /// slot, handed it to the worker, or (after a bounded loop gave up) forgotten it.
    bool call(detail::Snapshot& snap, detail::CallerEntry& e, const input_type& in, std::uint64_t budget_ns, output_type& out, std::uint32_t& gen,
              FallbackReason& r) SCZ_CALL_NOEXCEPT {
#if SCZ_LINUX
        using namespace detail;
        namespace sr = abi::acc::slot_record;
        Arena& a = *snap.arena;
        const Directory& dir = a.dir;
        const std::byte* ctl = a.ctl;
        std::byte* data = a.data;
        const std::uint64_t t0 = monotonic_ns();
        if (budget_ns > dir.max_deadline_ns) bump(e.clamped);  // sdk-contract §8
        const std::uint64_t deadline = t0 + std::min(budget_ns, dir.max_deadline_ns);
        const std::uint64_t wait_until = deadline > reserve_ns_ ? deadline - reserve_ns_ : 0;

        // ── Admission: one clock read and two loads from one cache line.
        const std::uint64_t g = abi::acc::control_header::gate(ctl);
        if (!healthy(g)) return fail(r, gate_fallback(g));
        if (abi::acc::control_header::supervisor_hb_mono_ns(ctl) + dir.hb_stale_ns < t0) return fail(r, FallbackReason::StaleHeartbeat);
        if (!core_->confirm(snap, gate_gen(g))) return fail(r, FallbackReason::Rebinding);
        const Registry* reg = nullptr;
        std::uint32_t rows = 0;
        switch (snap.mode) {
        case Mode::Raw:
            if constexpr (FrameCodec<C>) return fail(r, FallbackReason::SchemaMismatch);
            break;
        case Mode::Frames:
            if constexpr (OpaqueCodec<C>) {
                return fail(r, FallbackReason::SchemaMismatch);
            } else {
                reg = snap.reg;
                rows = static_cast<std::uint32_t>(codec_.rows(in));
            }
            break;
        case Mode::Mismatch: return fail(r, FallbackReason::SchemaMismatch);
        case Mode::Unbound:
            core_->rebind.store(true, std::memory_order_release);
            return fail(r, FallbackReason::Rebinding);
        }
        if (e.shedding(t0)) return fail(r, FallbackReason::Overloaded);  // claims no slot, publishes nothing

        // ── Claim a slot in the smallest class that holds the request: the cached one, then
        //    a bounded probe.
        int cls_i = -1;
        if constexpr (OpaqueCodec<C>) {
            const std::size_t need = codec_.request_len(in);
            for (std::uint32_t k = 0; k < dir.class_count; ++k)
                if (dir.slabs[k].req_cap >= need) {
                    cls_i = static_cast<int>(k);
                    break;
                }
        } else {
            cls_i = reg->class_for(rows);
            if (cls_i >= static_cast<int>(dir.class_count)) cls_i = -1;
        }
        if (cls_i < 0) return fail(r, FallbackReason::TooLarge);
        const auto cls = static_cast<std::uint32_t>(cls_i);
        const std::uint32_t count = dir.slabs[cls].slot_count;
        const std::uint32_t start = e.cached_slot[cls];
        SlotRef s{};
        std::uint32_t idx = 0;
        std::uint32_t ep = 0;
        bool claimed = false;
        for (std::uint32_t k = 0; k < std::min<std::uint32_t>(static_cast<std::uint32_t>(abi::SLOT_PROBE), count); ++k) {
            idx = (start + k) % count;
            s = slot_at(data, dir, cls, idx);
            if (try_claim(s.rec, ep)) {
                claimed = true;
                break;
            }
        }
        if (!claimed) return fail(r, FallbackReason::NoSlot);
        e.cached_slot[cls] = idx;
        auto forget = [&] { e.cached_slot[cls] = (idx + 1) % count; };
        std::byte* status_page = data + dir.status_off;
        auto ring_fault = [&] { abi::acc::status_page::add_client_ring_faults(status_page, 1); };
        SlotGuard sg{s.rec, ep};

        // ── Encode. A codec failure releases the slot.
        const std::uint32_t epoch = snap.epoch;
        bool encoded = false;
        std::uint32_t req_len = 0;
        const Template* rt = nullptr;
        if constexpr (OpaqueCodec<C>) {
            SlotWriter w(s.req, s.req_cap);
            encoded = codec_.encode(in, w);
            req_len = static_cast<std::uint32_t>(w.size());
        } else {
            rt = reg->find(cls, abi::direction::REQUEST);
            if (rt != nullptr) {
                // Stamp the region with the template's prefix if it holds another, and set the
                // row count's length fields if it differs (arrow-data-plane §7.1).
                prepare(*reg, s.req, s.rec, *rt, rt->stamp(epoch), rows);
                FrameWriter w(s.req, *rt, rows);
                encoded = codec_.encode(in, w);
                req_len = rt->frame_cap;
            }
        }
        if (!encoded) {
            sg.stage = 2;
            release(s.rec, ep);
            return fail(r, FallbackReason::CodecError);
        }
        const std::uint64_t rid = ++e.next_request;
        const std::uint64_t t_enq = monotonic_ns();
        sr::set_request_id(s.rec, rid);
        sr::set_req_len(s.rec, req_len);
        sr::set_t_enqueue_ns(s.rec, t_enq);
        set_ready(s.rec, ep);  // the request region is the worker's from here
        sg.stage = 1;

        // ── Enqueue: round robin per caller, skipping a full ring. If the call unwinds inside
        //    the enqueue, the ring's guard publishes its cell as void and `sg` takes the slot back.
        Descriptor desc{};
        desc.request_id = rid;
        desc.slot_index = idx;
        desc.deadline_ns = wait_until;
        desc.slot_epoch = ep;
        desc.slab_class = static_cast<std::uint16_t>(cls);
        desc.enqueue_ns = t_enq;
        const std::uint32_t nr = dir.ring_count;
        const std::uint32_t cur = e.ring_cursor++;
        bool sent = false;
        for (std::uint32_t k = 0; k < nr && !sent; ++k) {
            switch (enqueue(ring_at(data, dir, (cur + k) % nr), desc)) {
            case Enqueue::Ok: sent = true; break;
            case Enqueue::Full: break;
            case Enqueue::Exhausted: ring_fault(); break;
            }
        }
        sg.stage = 2;
        if (!sent) {
            // No descriptor exists, but the sweep may have failed the slot meanwhile.
            switch (cancel_if_ready(s.rec, ep)) {
            case Cancel::Cancelled: break;
            case Cancel::NotReady: release(s.rec, ep); break;
            case Cancel::GaveUp:
                ring_fault();
                forget();
                break;
            }
            return fail(r, FallbackReason::RingFull);
        }

        // ── Wait: gate re-check, spin, then futex.
        const std::uint64_t spin = std::min(dir.spin_ns, core_->spin_ceiling_ns.load(std::memory_order_relaxed));
        const std::uint64_t now = monotonic_ns();
        const std::uint64_t spin_until = spin > UINT64_MAX - now ? UINT64_MAX : now + spin;
        const WaitResult w = wait(s.rec, ctl, ep, wait_until, spin_until);
        switch (w.end) {
        case WaitEnd::Done:
            if (w.after_deadline) late_wake(e, s.rec);
            return resolve(snap, s, ep, reg, cls, rows, out, gen, r);
        case WaitEnd::Failed: release(s.rec, ep); return fail(r, FallbackReason::WorkerFailed);
        case WaitEnd::Cancelled: return fail(r, gate_fallback(w.value));  // the cancel freed the slot
        case WaitEnd::Timeout:
            switch (abandon(s.rec, ep)) {
            case Abandon::Freed: return fail(r, FallbackReason::Deadline);
            case Abandon::Abandoned: forget(); return fail(r, FallbackReason::Deadline);  // the worker, or the sweep, frees it
            case Abandon::Done: {
                // The worker won the race.
                late_wake(e, s.rec);
                return resolve(snap, s, ep, reg, cls, rows, out, gen, r);
            }
            case Abandon::Failed: release(s.rec, ep); return fail(r, FallbackReason::WorkerFailed);
            case Abandon::GaveUp:
                ring_fault();
                forget();
                return fail(r, FallbackReason::WorkerFailed);
            }
            break;
        case WaitEnd::GaveUp:
            ring_fault();
            forget();
            return fail(r, FallbackReason::WorkerFailed);
        }
        return fail(r, FallbackReason::WorkerFailed);
#else
        (void)snap, (void)e, (void)in, (void)budget_ns, (void)out, (void)gen;
        return fail(r, FallbackReason::NotConnected);
#endif
    }

#if SCZ_LINUX
    /// A wait that ended at its deadline found the slot DONE: a wake-up more than 1 ms late is
    /// counted, which is how a lost or stalled wake-up shows (signaling §7).
    static void late_wake(detail::CallerEntry& e, const std::byte* rec) noexcept {
        const std::uint64_t done = abi::acc::slot_record::t_done_ns(rec);
        if (done != 0 && detail::monotonic_ns() > done + 1'000'000) detail::bump(e.late_wakes);
    }
#endif

    static bool fail(FallbackReason& r, FallbackReason why) noexcept {
        r = why;
        return false;
    }

#if SCZ_LINUX
    /// A DONE slot: read each result field once, check, decode, release. A response that
    /// fails its checks is refused: counted for the supervisor (client_rejects), and the call
    /// falls back with WORKER_FAILED.
    bool resolve(detail::Snapshot& snap, const detail::SlotRef& s, std::uint32_t ep, const detail::Registry* reg, std::uint32_t cls, std::uint32_t rows,
                 output_type& out, std::uint32_t& gen, FallbackReason& r) noexcept {
        using namespace detail;
        namespace sr = abi::acc::slot_record;
        const std::uint32_t status = sr::status(s.rec);
        const std::uint32_t model_gen = sr::model_gen(s.rec);
        const std::uint32_t resp_len = sr::resp_len(s.rec);
        if (status != abi::status::OK) {
            release(s.rec, ep);
            if (status == abi::status::STALE_REGISTRY) core_->rebind.store(true, std::memory_order_release);
            return fail(r, status_fallback(status));
        }
        auto refuse = [&] {
            abi::acc::status_page::add_client_rejects(snap.arena->data + snap.arena->dir.status_off, 1);
            release(s.rec, ep);
            return fail(r, FallbackReason::WorkerFailed);
        };
        bool ok = false;
        if constexpr (OpaqueCodec<C>) {
            (void)reg, (void)cls, (void)rows;
            if (resp_len > s.resp_cap) return refuse();
            const SlotReader rd(s.resp, resp_len);
            ok = codec_.decode(rd, out);
        } else {
            // arrow-data-plane §7.3: the response must be this template's, under this registry
            // epoch, the template's frame length, and as many rows as were asked.
            const Template* t = reg->find(cls, abi::direction::RESPONSE);
            if (t == nullptr) return refuse();
            const std::uint32_t stamp = sr::resp_stamp(s.rec);
            if (stamp != t->stamp(snap.epoch) || resp_len != t->frame_cap || frame_rows(s.resp, *t) != rows) return refuse();
            const FrameReader rd(s.resp, *t, rows);
            ok = codec_.decode(rd, out);
        }
        release(s.rec, ep);
        if (!ok) return fail(r, FallbackReason::CodecError);
        gen = model_gen;
        return true;
    }
#endif

    C codec_;
    F fallback_;
    [[no_unique_address]] H on_fallback_;
    std::uint64_t reserve_ns_;
    std::uint32_t sample_every_;
    detail::Core* core_ = nullptr;
};

}  // namespace scz::inline v1
