// Unit tests of the C++20 SDK: the hashes against reference vectors (the schema-fingerprint
// golden vectors of abi/golden/fingerprints.txt among them), the slot and ring protocols on
// local memory, the registry parser, and the whole client against the in-process sidecar of
// local_arena.hpp: answers, every admission fallback it can produce, deadlines and abandon,
// parking, the breaker, frames, close, reconnect with retirement, two clients, the unwind
// hook, and fork; and the metrics of sdk-contract §8 (mirroring sdks/rust/tests/metrics.rs,
// the Go SDK's metrics_test.go, and the Java SDK's MetricsTests). test.sh also runs this
// under TSan, ASan+UBSan, and -fno-exceptions.
#include <sys/wait.h>

#include <cinttypes>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <functional>
#include <limits>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include "local_arena.hpp"

using namespace std::chrono_literals;
using namespace scztest;

namespace {

int g_failures = 0;
int g_checks = 0;

#define CHECK(cond)                                                                \
    do {                                                                           \
        ++g_checks;                                                                \
        if (!(cond)) {                                                             \
            ++g_failures;                                                          \
            std::fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
        }                                                                          \
    } while (0)

#define TEST(name) static void name()
#define RUN(name)                                \
    do {                                         \
        const int before = g_failures;           \
        name();                                  \
        std::fprintf(stderr, "%s %s\n", g_failures == before ? "ok  " : "FAIL", #name); \
    } while (0)

// ── hashes ────────────────────────────────────────────────────────────────────────────

TEST(xxh3_matches_the_reference_at_every_length_class) {
    // xxhash 3.5.0 (Python), bytes (i*31+7) & 0xff.
    const std::pair<std::size_t, std::uint64_t> v[] = {
        {0, 0x2d06800538d394c2ULL},    {1, 0x4c5cca45d0f4811fULL},    {2, 0xa7e250c97710ff27ULL},    {3, 0x15f7093b173d005cULL},
        {4, 0xdca012f95811b6b9ULL},    {7, 0x7561869c23da3c1bULL},    {8, 0xdec6a9a43575982eULL},    {9, 0xcbe393399f17ffbdULL},
        {15, 0x545e19990471dc37ULL},   {16, 0x7e484c18d74895d0ULL},   {17, 0x208bde5ee2bed407ULL},   {31, 0xa937652b0119ca11ULL},
        {32, 0x03df0ac5255d1446ULL},   {33, 0x199a362122d71f46ULL},   {64, 0xdd30702ab46b3745ULL},   {65, 0xfab36b851b94ce20ULL},
        {96, 0xd245cd2541582982ULL},   {97, 0x60e3e1d0d43785b3ULL},   {127, 0xa915ed6396db8cc0ULL},  {128, 0xf92b70eaa21a6288ULL},
        {129, 0xf8f76713f2bb60faULL},  {200, 0x12fdb864685f344dULL},  {239, 0xcaa9b7a588464745ULL},  {240, 0xccc7375172c41f03ULL},
        {241, 0x0b3b630948ce4a00ULL},  {500, 0xe1e3ce93f69c5043ULL},  {1023, 0xf0d330ce2b3300fbULL}, {1024, 0x23bc880ebf0d29c6ULL},
        {1025, 0xc09fdfbc398c7d82ULL}, {2048, 0x19f6f9c987331373ULL}, {3000, 0x6eb4b5bfe14d9786ULL}, {10000, 0x441f01d9711bebedULL},
    };
    std::vector<std::uint8_t> b(10000);
    for (std::size_t i = 0; i < b.size(); ++i) b[i] = static_cast<std::uint8_t>((i * 31 + 7) & 0xff);
    for (auto [n, h] : v) {
        const std::uint64_t got = xxh3_64(b.data(), n);
        if (got != h) std::fprintf(stderr, "  xxh3 len %zu: %016" PRIx64 " != %016" PRIx64 "\n", n, got, h);
        CHECK(got == h);
    }
}

TEST(crc32c_matches_the_check_value) {
    CHECK(crc32c_update(0, "123456789", 9) == 0xE3069283U);
    CHECK(crc32c_update(crc32c_update(0, "1234", 4), "56789", 5) == 0xE3069283U);
}

/// `name:TYPE[list]` with `?` for nullable, `,` between columns, `-` for none (generate.py).
struct ParsedSchema {
    std::vector<std::string> names[2];
    std::vector<scz::Column> cols[2];
};

bool parse_side(const std::string& text, std::vector<std::string>& names, std::vector<scz::Column>& cols) {
    if (text == "-") return true;
    static const std::pair<const char*, scz::Elem> types[] = {{"I8", scz::Elem::I8},   {"I16", scz::Elem::I16}, {"I32", scz::Elem::I32},
                                                              {"I64", scz::Elem::I64}, {"U8", scz::Elem::U8},   {"U16", scz::Elem::U16},
                                                              {"U32", scz::Elem::U32}, {"U64", scz::Elem::U64}, {"F16", scz::Elem::F16},
                                                              {"F32", scz::Elem::F32}, {"F64", scz::Elem::F64}, {"FIXED_BINARY", scz::Elem::FixedBinary}};
    std::stringstream ss(text);
    for (std::string c; std::getline(ss, c, ',');) {
        const auto colon = c.rfind(':');
        const auto lb = c.find('[', colon);
        std::string ty = c.substr(colon + 1, lb - colon - 1);
        const bool nullable = !ty.empty() && ty.back() == '?';
        if (nullable) ty.pop_back();
        bool found = false;
        scz::Column col;
        for (auto& [n, e] : types)
            if (ty == n) {
                col.elem = e;
                found = true;
            }
        if (!found) return false;
        col.list_size = static_cast<std::uint32_t>(std::stoul(c.substr(lb + 1)));
        col.nullable = nullable;
        names.push_back(c.substr(0, colon));
        cols.push_back(col);
    }
    return true;
}

TEST(fingerprints_reproduce_the_golden_vectors) {
    // The vectors as checked in; the file is read too when the test runs from the repository.
    std::vector<std::string> lines = {
        "0x57078e8d7a2e3733 features:F32[256] score:F32[1]",
        "0x98a14c33dfa1052a features:F32[128] score:F32[1]",
        "0x8699196fe544e9fa features:F32[256] score:F32[2]",
        "0xebde5c7cd522340a feature:F32[256] score:F32[1]",
        "0xc9c3c5e0c7ab2d81 a:I64[1],b:F64?[1],c:FIXED_BINARY[16] emb:F32[8],p:F16[1]",
        "0x9e3341d1b016503a ids:U32[4],w:U8[1] -",
        "0x62e7909031d9d3c9 - -",
        "0x8f2cc84078f28b1a caf\xc3\xa9:I8[3] r\xc3\xa9sum\xc3\xa9:U64[1]",
    };
    for (const char* p : {"abi/golden/fingerprints.txt", "../../abi/golden/fingerprints.txt"}) {
        std::ifstream f(p);
        if (!f) continue;
        std::vector<std::string> from_file;
        for (std::string l; std::getline(f, l);)
            if (!l.empty() && l[0] != '#') from_file.push_back(l);
        CHECK(from_file == lines);
        lines = from_file;
        break;
    }
    CHECK(lines.size() == 8);
    for (const auto& l : lines) {
        std::stringstream ss(l);
        std::string fp, req, resp;
        ss >> fp >> req >> resp;
        ParsedSchema s;
        CHECK(parse_side(req, s.names[0], s.cols[0]) && parse_side(resp, s.names[1], s.cols[1]));
        for (int d = 0; d < 2; ++d)
            for (std::size_t i = 0; i < s.cols[d].size(); ++i) s.cols[d][i].name = s.names[d][i];
        const scz::Schema schema{s.cols[0].data(), s.cols[0].size(), s.cols[1].data(), s.cols[1].size()};
        const std::uint64_t got = scz::fingerprint(schema);
        if (got != std::stoull(fp, nullptr, 16)) std::fprintf(stderr, "  fingerprint %s: %016" PRIx64 "\n", l.c_str(), got);
        CHECK(got == std::stoull(fp, nullptr, 16));
    }
    // The prebuilt codec declares exactly the launch schema.
    CHECK(scz::schema_fingerprint(scz::codecs::DenseF32ToScore<256>{}) == 0x57078e8d7a2e3733ULL);
}

// ── the slot and ring protocols on local memory ────────────────────────────────────────

struct alignas(64) LocalSlot {
    std::byte rec[64] = {};
};

TEST(slot_lifecycle_and_abandon_keeps_owner) {
    LocalSlot s;
    std::uint32_t e = 0;
    CHECK(try_claim(s.rec, e) && e == 1);
    std::uint32_t e2 = 0;
    CHECK(!try_claim(s.rec, e2));  // one owner at a time
    set_ready(s.rec, e);
    CHECK(cancel_if_ready(s.rec, e) == Cancel::Cancelled);
    CHECK(state(abi::acc::slot_record::word(s.rec)) == abi::slot_state::FREE);
    CHECK(try_claim(s.rec, e) && e == 2);
    set_ready(s.rec, e);
    // A worker claims it with OWNER 2 and WAITER kept; the client abandons.
    abi::acc::slot_record::set_word(s.rec, with_waiter(with_owner(slot_word(e, abi::slot_state::PROCESSING), 2)));
    CHECK(cancel_if_ready(s.rec, e) == Cancel::NotReady);
    CHECK(abandon(s.rec, e) == Abandon::Abandoned);
    const std::uint64_t w = abi::acc::slot_record::word(s.rec);
    CHECK(state(w) == abi::slot_state::ABANDONED && owner(w) == 2 && epoch(w) == e);
    CHECK(abandon(s.rec, e + 1) == Abandon::GaveUp);  // another epoch: not ours
    abi::acc::slot_record::set_word(s.rec, slot_word(e, abi::slot_state::DONE));
    CHECK(abandon(s.rec, e) == Abandon::Done);
    abi::acc::slot_record::set_word(s.rec, slot_word(e, abi::slot_state::READY));
    CHECK(abandon(s.rec, e) == Abandon::Freed);
}

TEST(wait_resolves_cancels_and_times_out) {
    alignas(64) std::byte ctl[4096] = {};
    LocalSlot s;
    std::uint32_t e = 0;
    CHECK(try_claim(s.rec, e));
    set_ready(s.rec, e);
    // An unhealthy gate cancels a queued request at once.
    store_relaxed<std::uint64_t>(ctl + abi::control_header::GATE, gate_word(1, 0, abi::gate_state::DEAD));
    WaitResult r = wait(s.rec, ctl, e, monotonic_ns() + 1'000'000, monotonic_ns() + 10'000);
    CHECK(r.end == WaitEnd::Cancelled && gate_state(r.value) == abi::gate_state::DEAD);
    CHECK(state(abi::acc::slot_record::word(s.rec)) == abi::slot_state::FREE);
    // Processing and never completed: a timeout at the deadline, after parking with WAITER.
    store_relaxed<std::uint64_t>(ctl + abi::control_header::GATE, gate_word(1, 0, abi::gate_state::HEALTHY));
    CHECK(try_claim(s.rec, e));
    abi::acc::slot_record::set_word(s.rec, with_owner(slot_word(e, abi::slot_state::PROCESSING), 1));
    const std::uint64_t t0 = monotonic_ns();
    r = wait(s.rec, ctl, e, t0 + 3'000'000, t0 + 10'000);
    const std::uint64_t took = monotonic_ns() - t0;
    CHECK(r.end == WaitEnd::Timeout && took >= 3'000'000 && took < 50'000'000);
    CHECK(waiter(abi::acc::slot_record::word(s.rec)));
    // A completion while parked wakes the waiter well before its deadline.
    std::thread worker([&] {
        while (!waiter(abi::acc::slot_record::word(s.rec))) std::this_thread::yield();
        std::this_thread::sleep_for(5ms);
        abi::acc::slot_record::set_word(s.rec, slot_word(e, abi::slot_state::DONE));
        futex_wake(s.rec, 1);
    });
    abi::acc::slot_record::set_word(s.rec, with_owner(slot_word(e, abi::slot_state::PROCESSING), 1));
    const std::uint64_t t1 = monotonic_ns();
    r = wait(s.rec, ctl, e, t1 + 2'000'000'000ULL, t1);
    worker.join();
    CHECK(r.end == WaitEnd::Done && !r.after_deadline && monotonic_ns() - t1 < 1'000'000'000ULL);
}

struct LocalRing {
    alignas(64) std::byte mem[256 + 8 * 64] = {};
    RingRef ref() { return {mem, 8}; }
    LocalRing() {
        for (std::uint64_t i = 0; i < 8; ++i) abi::acc::cell::set_seq(mem + 256 + i * 64, i);
    }
};

TEST(enqueue_fills_reports_full_and_writes_every_byte) {
    LocalRing r;
    Descriptor d{};
    d.request_id = 7;
    d.slot_index = 3;
    d.slot_epoch = 9;
    d.slab_class = 1;
    std::memset(r.mem + 256, 0xEE, 64);  // dirty reserved bytes: the enqueue clears them
    abi::acc::cell::set_seq(r.mem + 256, 0);
    for (int i = 0; i < 8; ++i) CHECK(enqueue(r.ref(), d) == Enqueue::Ok);
    CHECK(enqueue(r.ref(), d) == Enqueue::Full);
    const std::byte* c = r.mem + 256;
    CHECK(abi::acc::cell::seq(c) == 1 && abi::acc::cell::slot_index(c) == 3 && abi::acc::cell::slot_epoch(c) == 9 && abi::acc::cell::slab_class(c) == 1);
    CHECK(fetch_once<std::uint64_t>(c + 0x10) == 0 && fetch_once<std::uint32_t>(c + 0x18) == 0 && fetch_once<std::uint16_t>(c + 0x2E) == 0);
    // A sequence number left ahead of tail: every attempt is lost, and the enqueue gives up.
    LocalRing bad;
    abi::acc::cell::set_seq(bad.mem + 256, 5);
    CHECK(enqueue(bad.ref(), d) == Enqueue::Exhausted);
}

#if SCZ_HOOKS_THROW
TEST(an_unwind_inside_the_enqueue_publishes_a_void_cell) {
    LocalRing r;
    Descriptor d{};
    d.slot_index = 2;
    scz::testing::unwind_once.store(true);
    bool thrown = false;
    try {
        (void)enqueue(r.ref(), d);
    } catch (const scz::testing::Unwound&) {
        thrown = true;
    }
    CHECK(thrown);
    const std::byte* c = r.mem + 256;
    CHECK(abi::acc::cell::seq(c) == 1);                                  // published
    CHECK((abi::acc::cell::flags(c) & abi::cell_flag::VOID) != 0);       // as void
    CHECK(abi::acc::ring_header::tail(r.mem) == 1);
    CHECK(enqueue(r.ref(), d) == Enqueue::Ok);                           // and the ring moves on
}
#endif

// ── the registry parser ───────────────────────────────────────────────────────────────

TEST(the_synthetic_registry_parses_and_damage_is_refused) {
    Config c;
    c.frames = true;
    FakeSidecar sc(unique_endpoint("reg"), c);
    CHECK(sc.start());
    auto* reg = new Registry();
    ModelBlock b{};
    CHECK(read_model(sc.ctl(), abi::CTL_HEADER_LEN + abi::CTL_REGISTRY_LEN, 100, b, *reg));
    const std::uint32_t caps[1][2] = {{c.req_cap, c.resp_cap}};
    CHECK(parse(*reg, caps, 1) == nullptr);
    CHECK(reg->template_count == 2 && reg->fp == 0x57078e8d7a2e3733ULL && b.schema_fingerprint == reg->fp);
    CHECK(reg->class_for(1) == 0 && reg->class_for(2) == -1);
    const Template* t = reg->find(0, abi::direction::REQUEST);
    CHECK(t != nullptr && t->frame_cap == 1088 && t->columns[0].list_size == 256 && t->stamp(1) == ((1U << 16) | 1U));
    // A frame larger than its region.
    const std::uint32_t small[1][2] = {{1024, c.resp_cap}};
    CHECK(parse(*reg, small, 1) != nullptr);
    // A template for a class that does not exist.
    CHECK(parse(*reg, caps, 0) != nullptr);
    // Bad magic, and a length that disagrees with the bytes.
    Registry* bad = new Registry(*reg);
    bad->bytes[0] ^= 1;
    CHECK(parse(*bad, caps, 1) != nullptr);
    *bad = *reg;
    bad->len -= 8;
    CHECK(parse(*bad, caps, 1) != nullptr);
    // No patch rewrites the row count.
    *bad = *reg;
    put<std::uint32_t>(reinterpret_cast<std::byte*>(bad->bytes), 224 + abi::patch_desc::PREFIX_OFF, 16);
    CHECK(parse(*bad, caps, 1) != nullptr);
    delete bad;
    delete reg;
    sc.destroy();
}

// ── the client against the in-process sidecar ──────────────────────────────────────────

struct Keyed {
    using input_type = std::span<const std::byte>;
    using output_type = std::uint64_t;
    std::atomic<int>* fault = nullptr;  // 1 encode, 2 decode
    [[nodiscard]] std::size_t request_len(const input_type& in) const noexcept { return in.size(); }
    [[nodiscard]] bool encode(const input_type& in, scz::SlotWriter& w) const noexcept { return (fault == nullptr || fault->load() != 1) && w.write(in); }
    [[nodiscard]] bool decode(const scz::SlotReader& r, output_type& out) const noexcept {
        return (fault == nullptr || fault->load() != 2) && r.size() == 8 && r.read_u64(0, out);
    }
};

struct Zero {
    std::uint64_t operator()(const Keyed::input_type&, scz::FallbackReason) const noexcept { return 0; }
};

using KeyedClient = scz::Client<Keyed, Zero>;

std::uint64_t fb(const scz::Stats& s, scz::FallbackReason r) { return s.fallbacks[static_cast<std::size_t>(r)]; }

TEST(answers_admission_fallbacks_and_counters) {
    FakeSidecar sc(unique_endpoint("basic"));
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    {
        const KeyedReq q(op::NORMAL, 0, 0, 0);
        const auto o = c.infer(q.bytes(), 2ms);
        CHECK(!o.is_model() && o.reason == scz::FallbackReason::NotConnected);
    }
    CHECK(sc.start());
    CHECK(c.await_bound(5s));
    CHECK(c.state() == scz::ClientState::Bound);
    for (std::uint32_t s = 0; s < 200; ++s) {
        const KeyedReq q(op::NORMAL, 0, 1, s, 3, s % 8);
        const auto o = c.infer(q.bytes(), 20ms);
        CHECK(o.is_model() && o.value == q.answer() && o.model_gen == 1);
    }
    // Gate states, then a stale heartbeat.
    const std::pair<std::uint32_t, scz::FallbackReason> gates[] = {{abi::gate_state::INIT, scz::FallbackReason::GateInit},
                                                                    {abi::gate_state::PSI_SHEDDING, scz::FallbackReason::GateShedding},
                                                                    {abi::gate_state::RELOADING, scz::FallbackReason::GateReloading},
                                                                    {abi::gate_state::DEAD, scz::FallbackReason::GateDead},
                                                                    {abi::gate_state::DRAINING, scz::FallbackReason::GateDraining}};
    for (auto [g, r] : gates) {
        sc.set_gate(g);
        const KeyedReq q(op::NORMAL, 0, 1, 0);
        CHECK(c.infer(q.bytes(), 2ms).reason == r);
    }
    sc.set_gate(abi::gate_state::HEALTHY);
    sc.heartbeat(false);
    std::this_thread::sleep_for(30ms);
    {
        const KeyedReq q(op::NORMAL, 0, 1, 0);
        CHECK(c.infer(q.bytes(), 2ms).reason == scz::FallbackReason::StaleHeartbeat);
    }
    sc.heartbeat(true);
    std::this_thread::sleep_for(5ms);
    // TOO_LARGE: more bytes than any request region holds.
    {
        std::vector<std::byte> big(4096);
        CHECK(c.infer(std::span<const std::byte>(big), 2ms).reason == scz::FallbackReason::TooLarge);
    }
    // Status codes from a DONE slot.
    {
        const KeyedReq q(op::STATUS, abi::status::MODEL_ERROR, 1, 1);
        CHECK(c.infer(q.bytes(), 20ms).reason == scz::FallbackReason::ModelError);
        const KeyedReq q2(op::STATUS, abi::status::INTERNAL, 1, 2);
        CHECK(c.infer(q2.bytes(), 20ms).reason == scz::FallbackReason::WorkerFailed);
    }
    const scz::Stats s = c.stats();
    CHECK(s.model == 200 && s.sessions == 1 && fb(s, scz::FallbackReason::NotConnected) == 1 && fb(s, scz::FallbackReason::GateDead) == 1);
    CHECK(fb(s, scz::FallbackReason::StaleHeartbeat) == 1 && fb(s, scz::FallbackReason::TooLarge) == 1 && fb(s, scz::FallbackReason::ModelError) == 1);
    c.close();
    sc.destroy();
}

TEST(codec_failures_release_the_slot) {
    FakeSidecar sc(unique_endpoint("codec"));
    CHECK(sc.start());
    std::atomic<int> fault{1};
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{&fault}, Zero{});
    CHECK(c.await_bound(5s));
    for (int mode : {1, 2}) {
        fault = mode;
        for (std::uint32_t s = 0; s < 100; ++s) {  // more calls than slots: a leaked slot would show
            const KeyedReq q(op::NORMAL, 0, 0, s);
            CHECK(c.infer(q.bytes(), 20ms).reason == scz::FallbackReason::CodecError);
        }
    }
    fault = 0;
    const KeyedReq q(op::NORMAL, 0, 0, 1);
    CHECK(c.infer(q.bytes(), 20ms).is_model());
    for (std::uint32_t i = 0; i < sc.config().slot_count; ++i)
        CHECK(state(abi::acc::slot_record::word(sc.data() + sc.table_off() + i * 64)) == abi::slot_state::FREE);
    c.close();
    sc.destroy();
}

TEST(deadlines_abandon_with_owner_and_the_breaker_sheds) {
    FakeSidecar sc(unique_endpoint("deadline"));
    CHECK(sc.start());
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    CHECK(c.await_bound(5s));
    // A hung request: the call returns at its deadline; the slot is left ABANDONED, OWNER kept.
    {
        const KeyedReq q(op::HANG, 50, 0, 0);
        const std::uint64_t t0 = monotonic_ns();
        const auto o = c.infer(q.bytes(), 3ms);
        const std::uint64_t took = monotonic_ns() - t0;
        CHECK(o.reason == scz::FallbackReason::Deadline && took >= 3'000'000 && took < 40'000'000);
        bool abandoned_with_owner = false;
        for (std::uint32_t i = 0; i < sc.config().slot_count; ++i) {
            const std::uint64_t w = abi::acc::slot_record::word(sc.data() + sc.table_off() + i * 64);
            if (state(w) == abi::slot_state::ABANDONED && owner(w) == 1) abandoned_with_owner = true;
        }
        CHECK(abandoned_with_owner);
        std::this_thread::sleep_for(80ms);  // the worker finishes it and frees the slot
        const KeyedReq ok(op::NORMAL, 0, 0, 1);
        CHECK(c.infer(ok.bytes(), 20ms).is_model());  // an answer closes the miss streak
    }
    // Eight misses in a row open the breaker: shed calls return OVERLOADED and publish nothing.
    for (std::uint32_t s = 0; s < 8; ++s) {
        const KeyedReq q(op::DELAY, 3000, 0, s);
        CHECK(c.infer(q.bytes(), 1ms).reason == scz::FallbackReason::Deadline);
    }
    const std::uint64_t tail = abi::acc::ring_header::tail(sc.data() + sc.ring_off());
    {
        const KeyedReq q(op::NORMAL, 0, 0, 100);
        const auto o = c.infer(q.bytes(), 1ms);
        CHECK(o.reason == scz::FallbackReason::Overloaded);
        CHECK(abi::acc::ring_header::tail(sc.data() + sc.ring_off()) == tail);
    }
    // Once the worker has drained the slow requests and the interval (1 to 3 ms here) is
    // over, a probe goes through and closes the breaker.
    std::this_thread::sleep_for(60ms);
    {
        const KeyedReq q(op::NORMAL, 0, 0, 101);
        CHECK(c.infer(q.bytes(), 20ms).is_model());
        const KeyedReq q2(op::NORMAL, 0, 0, 102);
        CHECK(c.infer(q2.bytes(), 20ms).is_model());
    }
    c.close();
    sc.destroy();
}

TEST(spin_off_parks_and_is_woken) {
    FakeSidecar sc(unique_endpoint("park"));
    CHECK(sc.start());
    scz::Options o;
    o.spin_ceiling = 0ns;
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{}, o);
    CHECK(c.await_bound(5s));
    std::vector<std::thread> ts;
    std::atomic<int> model{0};
    for (std::uint32_t t = 0; t < 4; ++t)
        ts.emplace_back([&, t] {
            for (std::uint32_t s = 0; s < 200; ++s) {
                const KeyedReq q(op::NORMAL, 0, t, s);
                const auto r = c.infer(q.bytes(), 50ms);
                if (r.is_model() && r.value == q.answer()) model.fetch_add(1);
            }
        });
    for (auto& t : ts) t.join();
    CHECK(model.load() == 800);
    CHECK(c.stats().late_wakes == 0);
    c.close();
    sc.destroy();
}

TEST(frames_bind_by_fingerprint_and_round_trip) {
    Config cfg;
    cfg.frames = true;
    FakeSidecar sc(unique_endpoint("frames"), cfg);
    CHECK(sc.start());
    auto zero = [](const std::array<float, 256>&, scz::FallbackReason) noexcept { return -1.0f; };
    scz::Client c(scz::Endpoint::parse(sc.endpoint()), scz::codecs::DenseF32ToScore<256>{}, zero);
    CHECK(c.await_bound(5s));
    for (int k = 0; k < 50; ++k) {
        std::array<float, 256> x{};
        float sum = 0;
        for (std::size_t i = 0; i < 256; ++i) {
            x[i] = static_cast<float>((i * 7 + static_cast<std::size_t>(k)) % 13) * 0.25f;
            sum += x[i];
        }
        const auto o = c.infer(x, 20ms);
        CHECK(o.is_model() && o.value == sum);
    }
    // An opaque codec does not bind to a registry with templates: SCHEMA_MISMATCH.
    c.close();
    KeyedClient k(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    const std::uint64_t until = monotonic_ns() + 2'000'000'000ULL;
    while (k.state() != scz::ClientState::SchemaRejected && monotonic_ns() < until) std::this_thread::sleep_for(1ms);
    const KeyedReq q(op::NORMAL, 0, 0, 0);
    CHECK(k.infer(q.bytes(), 2ms).reason == scz::FallbackReason::SchemaMismatch);
    k.close();
    sc.destroy();
}

TEST(close_unmaps_and_a_restart_reconnects_and_retires) {
    const std::string ep = unique_endpoint("restart");
    auto* a = new FakeSidecar(ep);
    CHECK(a->start());
    const int base = scz_mappings();  // the fake sidecar's own two
    KeyedClient c(scz::Endpoint::parse(ep), Keyed{}, Zero{});
    CHECK(c.await_bound(5s));
    CHECK(scz_mappings() == base + 2);
    // The supervisor dies: the client leaves the arena, closes its socket, reconnects.
    a->stop();
    const std::uint64_t until = monotonic_ns() + 3'000'000'000ULL;
    while (c.stats().retired < 1 && monotonic_ns() < until) std::this_thread::sleep_for(1ms);
    CHECK(c.stats().retired == 1);
    CHECK(scz_mappings() == base);
    a->destroy();
    delete a;
    FakeSidecar b(ep);
    CHECK(b.start());
    CHECK(c.await_bound(5s));
    const KeyedReq q(op::NORMAL, 0, 0, 0);
    CHECK(c.infer(q.bytes(), 20ms).is_model());
    CHECK(c.stats().sessions == 2);
    c.close();
    CHECK(c.infer(q.bytes(), 20ms).reason == scz::FallbackReason::NotConnected);
    CHECK(scz_mappings() == base);
    const std::uint64_t until2 = monotonic_ns() + 1'000'000'000ULL;
    while (!b.client_gone() && monotonic_ns() < until2) std::this_thread::sleep_for(1ms);
    CHECK(b.client_gone());  // the socket was closed after the unmap
    b.destroy();
}

TEST(two_clients_keep_separate_state_and_the_second_is_busy) {
    FakeSidecar sc(unique_endpoint("two"));
    CHECK(sc.start());
    KeyedClient c1(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    CHECK(c1.await_bound(5s));
    KeyedClient c2(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    std::this_thread::sleep_for(50ms);
    for (std::uint32_t s = 0; s < 20; ++s) {  // alternately on one thread
        const KeyedReq q(op::NORMAL, 0, 0, s);
        CHECK(c1.infer(q.bytes(), 20ms).is_model());
        CHECK(c2.infer(q.bytes(), 2ms).reason == scz::FallbackReason::NotConnected);
    }
    CHECK(c1.stats().model == 20 && c2.stats().model == 0 && fb(c2.stats(), scz::FallbackReason::NotConnected) == 20);
    CHECK(sc.busy_sent.load() >= 1);
    c2.close();
    c1.close();
    sc.destroy();
}

TEST(threads_that_come_and_go_claim_and_return_entries) {
    FakeSidecar sc(unique_endpoint("threads"));
    CHECK(sc.start());
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    CHECK(c.await_bound(5s));
    std::atomic<int> model{0};
    for (std::uint32_t s = 0; s < 300; ++s) {
        std::thread t([&, s] {
            const KeyedReq q(op::NORMAL, 0, 9, s);
            if (c.infer(q.bytes(), 20ms).is_model()) model.fetch_add(1);
        });
        t.join();
    }
    CHECK(model.load() == 300);
    c.close();
    sc.destroy();
}

#if SCZ_HOOKS_THROW
TEST(an_unwound_call_leaves_a_void_cell_and_a_free_slot) {
    FakeSidecar sc(unique_endpoint("unwind"));
    CHECK(sc.start());
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    CHECK(c.await_bound(5s));
    const std::uint64_t tail = abi::acc::ring_header::tail(sc.data() + sc.ring_off());
    scz::testing::unwind_once.store(true);
    bool unwound = false;
    try {
        const KeyedReq q(op::NORMAL, 0, 0, 0);
        (void)c.infer(q.bytes(), 20ms);
    } catch (const scz::testing::Unwound&) {
        unwound = true;
    }
    CHECK(unwound);
    CHECK(abi::acc::ring_header::tail(sc.data() + sc.ring_off()) == tail + 1);
    for (std::uint32_t s = 0; s < 100; ++s) {
        const KeyedReq q(op::NORMAL, 0, 0, s);
        CHECK(c.infer(q.bytes(), 20ms).is_model());
    }
    std::this_thread::sleep_for(5ms);
    for (std::uint32_t i = 0; i < sc.config().slot_count; ++i)
        CHECK(state(abi::acc::slot_record::word(sc.data() + sc.table_off() + i * 64)) == abi::slot_state::FREE);
    const scz::Stats s = c.stats();
    CHECK(s.model == 100);  // the unwound call counts nowhere
    c.close();
    sc.destroy();
}
#endif

[[maybe_unused]] TEST(a_forked_child_falls_back_and_exits) {
    FakeSidecar sc(unique_endpoint("fork"));
    CHECK(sc.start());
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    CHECK(c.await_bound(5s));
    std::fflush(nullptr);
    const pid_t pid = fork();
    if (pid == 0) {
        const KeyedReq q(op::NORMAL, 0, 0, 0);
        const auto o = c.infer(q.bytes(), 20ms);
        c.close();  // must not hang: the parent's thread does not exist here
        _exit(o.reason == scz::FallbackReason::NotConnected && !o.is_model() ? 0 : 1);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    const KeyedReq q(op::NORMAL, 0, 0, 1);
    CHECK(c.infer(q.bytes(), 20ms).is_model());  // the parent's session is untouched
    c.close();
    sc.destroy();
}

// ── metrics (sdk-contract §8) ──────────────────────────────────────────────────────────

const scz::Endpoint kAbsent = scz::Endpoint::parse("/nonexistent/scz.sock");  // nothing listens

scz::Options every(std::uint32_t n) {
    scz::Options o;
    o.sample_every = n;
    return o;
}

/// Run `f` on a thread of its own, whose sample count starts at zero, and wait for it.
template <class Fn>
void on_fresh_thread(Fn f) {
    std::thread t(f);
    t.join();
}

std::size_t count_of(const std::string& text, const std::string& what) {
    std::size_t n = 0;
    for (std::size_t at = text.find(what); at != std::string::npos; at = text.find(what, at + 1)) ++n;
    return n;
}

bool has(const std::string& text, const std::string& line) {
    if (text.find(line) != std::string::npos) return true;
    std::fprintf(stderr, "  missing \"%s\" in\n%s\n", line.c_str(), text.c_str());
    return false;
}

TEST(a_duration_lands_in_the_first_bucket_at_least_as_large) {
    HistShard sh[1];
    for (std::uint64_t ns : {std::uint64_t{0}, std::uint64_t{1'000}, std::uint64_t{1'001}, std::uint64_t{100'000'000}, std::uint64_t{100'000'001},
                             std::numeric_limits<std::uint64_t>::max() / 4})
        sh[0].model.record(ns);
    const scz::Histogram r = sum_of(sh, 1, &HistShard::model);
    CHECK(r.cumulative[0] == 2);                     // 0 and exactly 1 µs
    CHECK(r.cumulative[1] == 3);                     // 1.001 µs
    CHECK(r.cumulative[15] == 4);                    // exactly 100 ms
    CHECK(r.cumulative[16] == 6 && r.count == 6);    // +Inf holds the rest
    for (std::size_t i = 0; i < kBoundsNs.size(); ++i) CHECK(scz::kBucketsSeconds[i] == static_cast<double>(kBoundsNs[i]) / 1e9);
    const scz::Histogram f = sum_of(sh, 1, &HistShard::fallback);
    CHECK(f.count == 0 && f.sum_seconds == 0);  // the other histograms are apart
}

TEST(the_text_format_names_each_family_once) {
    scz::PrometheusText p;
    const scz::Label fb_xy[] = {{"source", "fallback"}, {"reason", "x\"y"}};
    const scz::Label model[] = {{"source", "model"}};
    p.counter("a_total", fb_xy, 3);
    p.counter("a_total", model, 4);
    p.gauge("b", {}, 1.5);
    std::array<std::uint64_t, 17> cum{};
    cum[16] = 2;
    p.histogram("c_seconds", {}, scz::kBucketsSeconds, cum, 0.25, 2);
    const std::string s = p.str();
    CHECK(count_of(s, "# TYPE a_total counter") == 1);
    CHECK(has(s, "a_total{source=\"fallback\",reason=\"x\\\"y\"} 3\n"));
    CHECK(has(s, "a_total{source=\"model\"} 4\n"));
    CHECK(has(s, "# TYPE b gauge\nb 1.5\n"));
    CHECK(has(s, "c_seconds_bucket{le=\"0.000001\"} 0\n"));
    CHECK(has(s, "c_seconds_bucket{le=\"+Inf\"} 2\n"));
    CHECK(has(s, "c_seconds_sum 0.25\nc_seconds_count 2\n"));
    // Every le as Rust's Display writes the f64 bound.
    for (const char* le : {"0.000001", "0.000002", "0.000005", "0.00001", "0.00002", "0.00005", "0.0001", "0.0002", "0.0005", "0.001", "0.002", "0.005",
                           "0.01", "0.02", "0.05", "0.1"})
        CHECK(has(s, std::string("c_seconds_bucket{le=\"") + le + "\"} 0\n"));
    CHECK(count_of(s, "c_seconds_bucket") == 17);
    // The escapes, in order; labels with le after them; finish hands the text over.
    scz::PrometheusText e;
    const scz::Label k[] = {{"k", "a\\b\nc\"d"}};
    e.gauge("g", k, 0);
    const std::uint64_t one[] = {1, 1};
    e.histogram("h", model, std::span<const double>(scz::kBucketsSeconds).first(1), one, 1e-6, 1);
    const std::string want =
        "# TYPE g gauge\ng{k=\"a\\\\b\\nc\\\"d\"} 0\n"
        "# TYPE h histogram\nh_bucket{source=\"model\",le=\"0.000001\"} 1\nh_bucket{source=\"model\",le=\"+Inf\"} 1\n"
        "h_sum{source=\"model\"} 0.000001\nh_count{source=\"model\"} 1\n";
    const std::string got = e.finish();
    if (got != want) std::fprintf(stderr, "  got\n%s  want\n%s", got.c_str(), want.c_str());
    CHECK(got == want);
    CHECK(e.str().empty());
}

TEST(numbers_are_written_as_rust_writes_them) {
    const std::pair<double, std::string> v[] = {
        {0, "0"},
        {2, "2"},
        {1.5, "1.5"},
        {1e-6, "0.000001"},
        {123456789, "123456789"},
        {1e21, "1000000000000000000000"},
        {0.1, "0.1"},
        {0.30000000000000004, "0.30000000000000004"},
        {4.5e-7, "0.00000045"},
        {0.25, "0.25"},
        {static_cast<double>(std::numeric_limits<std::uint64_t>::max()), "18446744073709552000"},
        {5e-324, "0." + std::string(323, '0') + "5"},
        // Exactly halfway between two shortest decimals: Rust takes the one further from zero,
        // std::to_chars the even one.
        {751435862906.78125, "751435862906.7813"},
        {-751435862906.78125, "-751435862906.7813"},
        {2647518583414.28125, "2647518583414.2813"},
        {-0.0, "-0"},
        {std::numeric_limits<double>::infinity(), "inf"},
        {-std::numeric_limits<double>::infinity(), "-inf"},
        {std::numeric_limits<double>::quiet_NaN(), "NaN"},
    };
    for (const auto& [x, want] : v) {
        const std::string got = format_f64(x);
        const std::string portable = format_f64<true>(x);  // snprintf and strtod, for a build without std::to_chars
        if (got != want || portable != want) std::fprintf(stderr, "  %.17g: %s and %s, want %s\n", x, got.c_str(), portable.c_str(), want.c_str());
        CHECK(got == want && portable == want);
    }
}

/// A fallback that takes more than 200 µs.
struct Busy {
    std::uint64_t operator()(const Keyed::input_type&, scz::FallbackReason) const noexcept {
        const std::uint64_t t0 = monotonic_ns();
        while (monotonic_ns() - t0 <= 200'000) {
        }
        return 7;
    }
};

// The count is the thread's, over every client: two clients called in turn share it.
TEST(a_threads_sample_count_is_shared_by_every_client_it_calls) {
    KeyedClient a(kAbsent, Keyed{}, Zero{}, every(4));
    KeyedClient b(kAbsent, Keyed{}, Zero{}, every(4));
    KeyedClient c(kAbsent, Keyed{}, Zero{}, every(3));
    const KeyedReq q(op::NORMAL, 0, 0, 0);
    on_fresh_thread([&] {
        for (int i = 0; i < 8; ++i) (void)(i % 2 == 0 ? a : b).infer(q.bytes(), 1ms);
    });
    // The thread's calls 0..7: 0 and 4 are timed, both a's.
    CHECK(a.stats().latency_fallback.count == 2 && b.stats().latency_fallback.count == 0);
    // Not a power of two: calls 0, 3, 6, 9 of 10.
    on_fresh_thread([&] {
        for (int i = 0; i < 10; ++i) (void)c.infer(q.bytes(), 1ms);
    });
    const scz::Stats s = c.stats();
    CHECK(s.latency_fallback.count == 4 && s.fallback_time.count == 4 && fb(s, scz::FallbackReason::NotConnected) == 10);
}

TEST(the_hook_sees_every_fallback_and_the_fallback_is_timed) {
    std::atomic<int> seen{0}, wrong{0};
    auto hook = [&](scz::FallbackReason r) {
        if (r != scz::FallbackReason::NotConnected) wrong.fetch_add(1);
        seen.fetch_add(1);
    };
    scz::Client c(kAbsent, Keyed{}, Busy{}, every(1), hook);
    const KeyedReq q(op::NORMAL, 0, 0, 0);
    on_fresh_thread([&] {
        for (int i = 0; i < 20; ++i) {
            const auto o = c.infer(q.bytes(), 1ms);
            if (o.is_model() || o.value != 7) wrong.fetch_add(1);
        }
    });
    CHECK(seen.load() == 20 && wrong.load() == 0);
    const scz::Stats s = c.stats();
    CHECK(fb(s, scz::FallbackReason::NotConnected) == 20 && s.fallback_time.count == 20 && s.latency_fallback.count == 20);
    CHECK(s.fallback_time.sum_seconds >= 20 * 200e-6);
    CHECK(s.latency_fallback.sum_seconds >= s.fallback_time.sum_seconds);
    CHECK(s.fallback_time.cumulative[7] == 0);  // nothing at or under 200 µs
    CHECK(s.latency_model.count == 0);
}

// Called after the slot and the caller entry are released: with one entry, a call made from
// inside the hook gets it, and every slot is free.
TEST(the_hook_runs_after_the_slot_and_the_entry_are_released) {
    FakeSidecar sc(unique_endpoint("hook"));
    CHECK(sc.start());
    std::atomic<int> fault{0};
    std::function<bool()> nested;
    std::atomic<int> hooked{0}, slots_free{0}, nested_model{0};
    auto hook = [&](scz::FallbackReason r) {
        hooked.fetch_add(1);
        if (r != scz::FallbackReason::CodecError) return;
        bool free = true;
        for (std::uint32_t i = 0; i < sc.config().slot_count; ++i)
            if (state(abi::acc::slot_record::word(sc.data() + sc.table_off() + i * 64)) != abi::slot_state::FREE) free = false;
        if (free) slots_free.fetch_add(1);
        fault = 0;
        if (nested()) nested_model.fetch_add(1);  // with the entry still held, NO_SLOT
        fault = 1;
    };
    scz::Options o = every(1);
    o.callers = 1;
    scz::Client c(scz::Endpoint::parse(sc.endpoint()), Keyed{&fault}, Zero{}, o, hook);
    nested = [&c] {
        const KeyedReq q(op::NORMAL, 0, 0, 999);
        return c.infer(q.bytes(), 50ms).is_model();
    };
    CHECK(c.await_bound(5s));
    fault = 1;
    for (std::uint32_t s = 0; s < 10; ++s) {
        const KeyedReq q(op::NORMAL, 0, 0, s);
        CHECK(c.infer(q.bytes(), 50ms).reason == scz::FallbackReason::CodecError);
    }
    fault = 0;
    CHECK(hooked.load() == 10 && slots_free.load() == 10 && nested_model.load() == 10);
    const scz::Stats s = c.stats();
    CHECK(fb(s, scz::FallbackReason::CodecError) == 10 && s.model == 10);
    CHECK(s.latency_model.count == 10 && s.latency_fallback.count == 10 && s.fallback_time.count == 10);
    c.close();
    sc.destroy();
}

#if SCZ_HOOKS_THROW
struct Boom {};

struct CountingZero {
    std::atomic<int>* n;
    std::uint64_t operator()(const Keyed::input_type&, scz::FallbackReason) const noexcept {
        n->fetch_add(1);
        return 0;
    }
};

// What the hook throws propagates (here, where infer is noexcept(false); an application build
// terminates, as a throwing fallback does); the fallback does not run, and the call is counted.
TEST(a_throwing_hook_propagates) {
    std::atomic<int> fallbacks{0};
    scz::Client c(kAbsent, Keyed{}, CountingZero{&fallbacks}, every(1), [](scz::FallbackReason) { throw Boom{}; });
    bool caught = false;
    try {
        const KeyedReq q(op::NORMAL, 0, 0, 0);
        (void)c.infer(q.bytes(), 1ms);
    } catch (const Boom&) {
        caught = true;
    }
    CHECK(caught && fallbacks.load() == 0);
    const scz::Stats s = c.stats();
    CHECK(fb(s, scz::FallbackReason::NotConnected) == 1 && s.latency_fallback.count == 0 && s.fallback_time.count == 0);
}

// A hook that throws on a fallback taken with an entry held (TOO_LARGE): the entry and the
// slot are released, and the throw times nothing.
TEST(a_throwing_hook_releases_what_the_call_held) {
    FakeSidecar sc(unique_endpoint("hookthrow"));
    CHECK(sc.start());
    scz::Options o = every(1);
    o.callers = 2;
    scz::Client c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{}, o, [](scz::FallbackReason) { throw Boom{}; });
    CHECK(c.await_bound(5s));
    std::vector<std::byte> big(4096);
    int caught = 0;
    for (int i = 0; i < 10; ++i) {
        try {
            (void)c.infer(std::span<const std::byte>(big), 20ms);
        } catch (const Boom&) {
            ++caught;
        }
    }
    CHECK(caught == 10);
    // With two entries, a leaked one would show at once as NO_SLOT.
    for (std::uint32_t s = 0; s < 10; ++s) {
        const KeyedReq q(op::NORMAL, 0, 0, s);
        CHECK(c.infer(q.bytes(), 50ms).is_model());
    }
    const scz::Stats s = c.stats();
    CHECK(fb(s, scz::FallbackReason::TooLarge) == 10 && s.model == 10);
    CHECK(s.fallback_time.count == 0 && s.latency_fallback.count == 0 && s.latency_model.count == 10);
    std::this_thread::sleep_for(5ms);
    for (std::uint32_t i = 0; i < sc.config().slot_count; ++i)
        CHECK(state(abi::acc::slot_record::word(sc.data() + sc.table_off() + i * 64)) == abi::slot_state::FREE);
    c.close();
    sc.destroy();
}
#endif

TEST(sampling_can_be_turned_off) {
    KeyedClient c(kAbsent, Keyed{}, Zero{}, every(0));
    const KeyedReq q(op::NORMAL, 0, 0, 0);
    for (int i = 0; i < 100; ++i) (void)c.infer(q.bytes(), 1ms);
    const scz::Stats s = c.stats();
    CHECK(fb(s, scz::FallbackReason::NotConnected) == 100 && s.latency_fallback.count == 0 && s.fallback_time.count == 0);
    const std::string text = c.prometheus_text();
    CHECK(has(text, "scz_client_calls_total{source=\"fallback\",reason=\"NOT_CONNECTED\"} 100\n"));
    CHECK(has(text, "scz_client_fallback_seconds_count 0\n"));
    CHECK(has(text, "scz_client_model_gen 0\n"));
    CHECK(has(text, "scz_client_reconnects_total 0\n"));
}

/// A sink that notes which call it got, keeps the labels, and checks the histograms' shape.
struct Recorder final : scz::MetricsSink {
    std::string pass;
    std::vector<std::vector<std::pair<std::string, std::string>>> labels;
    bool bad = false;
    void keep(std::span<const scz::Label> l) {
        auto& v = labels.emplace_back();
        for (const auto& x : l) v.emplace_back(x.name, x.value);
    }
    void counter(std::string_view, std::span<const scz::Label> l, std::uint64_t) override {
        pass += 'c';
        keep(l);
    }
    void gauge(std::string_view, std::span<const scz::Label> l, double) override {
        pass += 'g';
        keep(l);
    }
    void histogram(std::string_view, std::span<const scz::Label> l, std::span<const double> bounds, std::span<const std::uint64_t> cumulative, double,
                   std::uint64_t count) override {
        if (bounds.size() != 16 || cumulative.size() != 17 || cumulative[16] != count || bounds[15] != 0.1) bad = true;
        pass += 'h';
        keep(l);
    }
};

TEST(sampled_calls_are_timed_and_exported) {
    FakeSidecar sc(unique_endpoint("metrics"));
    CHECK(sc.start());
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{}, every(4));
    CHECK(c.await_bound(5s));
    std::atomic<int> model{0};
    on_fresh_thread([&] {
        for (std::uint32_t i = 0; i < 400; ++i) {
            const KeyedReq q(op::NORMAL, 0, 0, i);
            const auto o = c.infer(q.bytes(), 90ms);
            if (o.is_model() && o.value == q.answer()) model.fetch_add(1);
        }
        // A budget above the sidecar's 100 ms maximum is clamped, and counted.
        const KeyedReq q(op::NORMAL, 0, 0, 400);
        if (c.infer(q.bytes(), 1s).is_model()) model.fetch_add(1);
    });
    CHECK(model.load() == 401);
    const scz::Stats s = c.stats();
    CHECK(s.model == 401);
    CHECK(s.latency_model.count == 101);  // the first call, then one in four
    CHECK(s.latency_model.sum_seconds > 0 && s.latency_model.cumulative[16] == 101);
    CHECK(s.latency_fallback.count == 0 && s.fallback_time.count == 0);
    CHECK(s.clamped == 1 && s.model_gen == 1 && s.breaker_open == 0);
    const std::string text = c.prometheus_text();
    for (const char* line : {"scz_client_calls_total{source=\"model\"} 401\n", "scz_client_calls_total{source=\"fallback\",reason=\"DEADLINE\"} 0\n",
                             "scz_client_latency_seconds_count{source=\"model\"} 101\n", "scz_client_latency_seconds_bucket{source=\"model\",le=\"+Inf\"} 101\n",
                             "scz_client_latency_seconds_count{source=\"fallback\"} 0\n", "scz_client_budget_clamped_total 1\n",
                             "scz_client_late_wakes_total 0\n", "scz_client_reconnects_total 0\n", "scz_client_state 2\n", "scz_client_model_gen 1\n",
                             "# TYPE scz_client_breaker_open gauge\nscz_client_breaker_open 0\n", "# TYPE scz_client_fallback_seconds histogram\n"})
        CHECK(has(text, line));
    CHECK(count_of(text, "scz_client_calls_total{") == 1 + scz::kReasonCount);  // every reason, zeros included
    for (std::size_t r = 0; r < scz::kReasonCount; ++r)
        CHECK(has(text, "scz_client_calls_total{source=\"fallback\",reason=\"" + std::string(scz::reason_name(static_cast<scz::FallbackReason>(r))) + "\"} "));
    // The families in the order every SDK exports them, each with one TYPE line.
    const char* order[] = {"# TYPE scz_client_calls_total counter",     "# TYPE scz_client_latency_seconds histogram",
                           "# TYPE scz_client_fallback_seconds histogram", "# TYPE scz_client_budget_clamped_total counter",
                           "# TYPE scz_client_breaker_open gauge",      "# TYPE scz_client_late_wakes_total counter",
                           "# TYPE scz_client_reconnects_total counter", "# TYPE scz_client_state gauge",
                           "# TYPE scz_client_model_gen gauge"};
    std::size_t at = 0;
    bool in_order = true;
    for (const char* t : order) {
        const std::size_t i = text.find(std::string(t) + "\n");
        if (i == std::string::npos || i < at) in_order = false;
        at = i;
    }
    CHECK(in_order);
    CHECK(count_of(text, "# TYPE ") == std::size(order));
    // A sink sees the same pass.
    Recorder r;
    c.export_metrics(r);
    CHECK(r.pass == std::string(1 + scz::kReasonCount, 'c') + "hhhcgccgg" && !r.bad);
    using L = std::vector<std::pair<std::string, std::string>>;
    CHECK(r.labels.size() == r.pass.size() && r.labels[0] == (L{{"source", "model"}}));
    CHECK(r.labels[1 + static_cast<std::size_t>(scz::FallbackReason::Deadline)] == (L{{"source", "fallback"}, {"reason", "DEADLINE"}}));
    CHECK(r.labels[1 + scz::kReasonCount] == (L{{"source", "model"}}) && r.labels[2 + scz::kReasonCount] == (L{{"source", "fallback"}}));
    CHECK(r.labels[3 + scz::kReasonCount].empty() && r.labels.back().empty());
    c.close();
    sc.destroy();
}

// The gauge counts the callers shedding now: a worker that stalls, eight misses in a row.
TEST(the_breaker_gauge_counts_callers_shedding_now) {
    FakeSidecar sc(unique_endpoint("breaker"));
    CHECK(sc.start());
    KeyedClient c(scz::Endpoint::parse(sc.endpoint()), Keyed{}, Zero{});
    CHECK(c.await_bound(5s));
    sc.hold_worker(true);
    // 50 ms budgets: the breaker opens for 100 ms, jittered between 50 and 150 ms.
    for (std::uint32_t s = 0; s < 8; ++s) {
        const KeyedReq q(op::NORMAL, 0, 0, s);
        CHECK(c.infer(q.bytes(), 50ms).reason == scz::FallbackReason::Deadline);
    }
    CHECK(c.stats().breaker_open == 1);
    CHECK(has(c.prometheus_text(), "scz_client_breaker_open 1\n"));
    sc.hold_worker(false);
    std::this_thread::sleep_for(200ms);
    CHECK(c.stats().breaker_open == 0);
    const KeyedReq q(op::NORMAL, 0, 0, 100);
    CHECK(c.infer(q.bytes(), 50ms).is_model());
    CHECK(c.stats().breaker_open == 0);
    c.close();
    sc.destroy();
}

}  // namespace

int main() {
    RUN(xxh3_matches_the_reference_at_every_length_class);
    RUN(crc32c_matches_the_check_value);
    RUN(fingerprints_reproduce_the_golden_vectors);
    RUN(slot_lifecycle_and_abandon_keeps_owner);
    RUN(wait_resolves_cancels_and_times_out);
    RUN(enqueue_fills_reports_full_and_writes_every_byte);
#if SCZ_HOOKS_THROW
    RUN(an_unwind_inside_the_enqueue_publishes_a_void_cell);
#endif
    RUN(the_synthetic_registry_parses_and_damage_is_refused);
    RUN(answers_admission_fallbacks_and_counters);
    RUN(codec_failures_release_the_slot);
    RUN(deadlines_abandon_with_owner_and_the_breaker_sheds);
    RUN(spin_off_parks_and_is_woken);
    RUN(frames_bind_by_fingerprint_and_round_trip);
    RUN(close_unmaps_and_a_restart_reconnects_and_retires);
    RUN(two_clients_keep_separate_state_and_the_second_is_busy);
    RUN(threads_that_come_and_go_claim_and_return_entries);
#if SCZ_HOOKS_THROW
    RUN(an_unwound_call_leaves_a_void_cell_and_a_free_slot);
#endif
#if !defined(__SANITIZE_THREAD__)  // TSan cannot start threads in a child of a multithreaded process
    RUN(a_forked_child_falls_back_and_exits);
#endif
    RUN(a_duration_lands_in_the_first_bucket_at_least_as_large);
    RUN(the_text_format_names_each_family_once);
    RUN(numbers_are_written_as_rust_writes_them);
    RUN(a_threads_sample_count_is_shared_by_every_client_it_calls);
    RUN(the_hook_sees_every_fallback_and_the_fallback_is_timed);
    RUN(the_hook_runs_after_the_slot_and_the_entry_are_released);
#if SCZ_HOOKS_THROW
    RUN(a_throwing_hook_propagates);
    RUN(a_throwing_hook_releases_what_the_call_held);
#endif
    RUN(sampling_can_be_turned_off);
    RUN(sampled_calls_are_timed_and_exported);
    RUN(the_breaker_gauge_counts_callers_shedding_now);
    std::fprintf(stderr, "%d checks, %d failed\n", g_checks, g_failures);
    return g_failures == 0 ? 0 : 1;
}
