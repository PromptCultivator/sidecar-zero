// scz-agent-cpp20: the C++20 SDK's conformance agent (testing §3, "Agent protocol", v2),
// following the Rust agent (sdks/rust/conformance-agent/src/main.rs) line for line.
//
// It builds one client for --endpoint=PATH and obeys one command per line on stdin, answering
// one line on stdout. Words after the command are key=value; unknown keys are ignored, and a
// key left out keeps its value.
//
// | Command | Reply |
// |---|---|
// | (on start) | HELLO sdk=cpp version=V caps=threads,per-call,frames,unwind,nospin,fork,spawn[,crashpoints] |
// | CONFIG threads=N fallback_us=N kind=K fallback=F spin=S codec=C inputs=PATH | OK, or ERR unsupported |
// | BOUND ms=N | BOUND true|false |
// | CALL count=N budget_us=N rows=N op=O arg=N seed=N | one R line per call, then END calls=N |
// | CODEC_FAULT mode=off|encode|decode | OK |
// | UNWIND | OK: the next call unwinds between claiming a ring cell and publishing it |
// | CLOCK_SKEW ns=N | ERR unsupported: this SDK has no cheap clock |
// | FORK | FORK src=S reason=R status=N |
// | SPAWN | SPAWNED pid=N |
// | STATS | STATS model=N sessions=N retired=N late_wakes=N fb.REASON=N ... |
// | PROC | PROC fds=N maps=N threads=N |
// | CLOSE | CLOSED; later commands that need the client answer ERR closed |
// | EXIT | BYE, and the process exits |
//
// `fallback=throws` is refused: the C++ contract requires a noexcept fallback (sdk-contract
// §3.2), so a throwing one does not compile. `kind` is platform or per-call.

#include <dirent.h>
#include <fcntl.h>
#include <spawn.h>
#include <sys/wait.h>
#include <unistd.h>

#include <atomic>
#include <cinttypes>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <iostream>
#include <map>
#include <memory>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include "scz/sidecarzero.hpp"

extern char** environ;

namespace {

using namespace std::chrono_literals;

// ── the request format (conformance/proto): every agent builds requests byte for byte alike ──
constexpr std::uint64_t kMagic = 0x534F4148435A4353ULL;  // "SCZCHAOS" little-endian

std::uint64_t splitmix64(std::uint64_t x) {
    x += 0x9e3779b97f4a7c15ULL;
    x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9ULL;
    x = (x ^ (x >> 27)) * 0x94d049bb133111ebULL;
    return x ^ (x >> 31);
}

std::vector<std::byte> request(std::uint64_t op, std::uint64_t arg, std::uint32_t thread, std::uint32_t seq, std::uint64_t seed, std::size_t rows) {
    const std::uint64_t nonce = (static_cast<std::uint64_t>(thread) << 32) | seq;
    std::vector<std::uint64_t> words = {kMagic, op, arg, nonce, seed};
    for (std::uint64_t i = 0; i < rows; ++i) words.push_back(splitmix64(seed ^ nonce ^ i));
    std::vector<std::byte> b(words.size() * 8);
    std::memcpy(b.data(), words.data(), b.size());
    return b;
}

bool parse_op(const std::string& s, std::uint64_t& out) {
    static const std::map<std::string, std::uint64_t> ops = {{"normal", 0}, {"delay", 1}, {"crash", 2}, {"hang", 3}, {"status", 4}, {"bad_len", 5}, {"stall_wake", 6}};
    if (auto it = ops.find(s); it != ops.end()) {
        out = it->second;
        return true;
    }
    char* end = nullptr;
    const unsigned long long v = std::strtoull(s.c_str(), &end, 10);
    if (s.empty() || *end != '\0') return false;
    out = v;
    return true;
}

constexpr std::uint8_t ENCODE = 1;
constexpr std::uint8_t DECODE = 2;
std::atomic<std::uint8_t> g_fault{0};
std::atomic<std::uint64_t> g_fallback_us{0};

/// Request bytes in, the 8-byte keyed answer out.
struct Keyed {
    using input_type = std::span<const std::byte>;
    using output_type = std::uint64_t;
    [[nodiscard]] std::size_t request_len(const input_type& in) const noexcept { return in.size(); }
    [[nodiscard]] bool encode(const input_type& in, scz::SlotWriter& w) const noexcept {
        if (g_fault.load(std::memory_order_relaxed) == ENCODE) return false;
        return w.write(in);
    }
    [[nodiscard]] bool decode(const scz::SlotReader& r, output_type& out) const noexcept {
        if (g_fault.load(std::memory_order_relaxed) == DECODE || r.size() != 8) return false;
        return r.read_u64(0, out);
    }
};

constexpr std::size_t N = 256;
constexpr std::size_t MAX_ROWS = 64;
using Row = std::array<float, N>;
using Batch = scz::codecs::DenseF32Batch<N, MAX_ROWS>;

/// The fraud model's codec for any number of rows: the prebuilt dense codec, with the faults.
struct Dense {
    using input_type = Batch::input_type;
    using output_type = Batch::output_type;
    Batch inner{};
    [[nodiscard]] scz::Schema schema() const noexcept { return inner.schema(); }
    [[nodiscard]] std::uint32_t rows(const input_type& in) const noexcept { return inner.rows(in); }
    [[nodiscard]] bool encode(const input_type& in, scz::FrameWriter& w) const noexcept {
        return g_fault.load(std::memory_order_relaxed) != ENCODE && inner.encode(in, w);
    }
    [[nodiscard]] bool decode(const scz::FrameReader& r, output_type& out) const noexcept {
        return g_fault.load(std::memory_order_relaxed) != DECODE && inner.decode(r, out);
    }
};

void fallback_wait() noexcept {
    const std::uint64_t us = g_fallback_us.load(std::memory_order_relaxed);
    if (us > 0) {
        timespec ts{static_cast<time_t>(us / 1'000'000), static_cast<long>((us % 1'000'000) * 1000)};
        while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
        }
    }
}

/// Takes fallback_us, then answers 0.
struct SlowKeyed {
    std::uint64_t operator()(const Keyed::input_type&, scz::FallbackReason) const noexcept {
        fallback_wait();
        return 0;
    }
};
struct SlowDense {
    Dense::output_type operator()(const Dense::input_type& in, scz::FallbackReason) const noexcept {
        fallback_wait();
        Dense::output_type s{};
        s.n = static_cast<std::uint32_t>(std::min(in.size(), MAX_ROWS));
        return s;
    }
};

using KeyedClient = scz::Client<Keyed, SlowKeyed>;
using DenseClient = scz::Client<Dense, SlowDense>;

struct Req {
    std::uint64_t op = 0, arg = 0, seed = 1;
    std::size_t rows = 0;
    std::chrono::nanoseconds budget = 20ms;
};

struct Agent {
    std::string endpoint;
    std::unique_ptr<KeyedClient> keyed;
    std::unique_ptr<DenseClient> dense;
    std::vector<Row> inputs;
    std::uint32_t threads = 1;
    bool per_call = false;
    bool spin_off = false;
    std::string dense_path;  // empty: the keyed codec

    bool open() const { return keyed != nullptr || dense != nullptr; }

    void apply_spin() {
        const std::optional<std::chrono::nanoseconds> d = spin_off ? std::optional<std::chrono::nanoseconds>(0ns) : std::nullopt;
        if (keyed) keyed->set_spin_ceiling(d);
        if (dense) dense->set_spin_ceiling(d);
    }

    void close() {
        keyed.reset();
        dense.reset();
    }

    std::string build() {
        if (dense_path.empty()) {
            keyed = std::make_unique<KeyedClient>(scz::Endpoint::parse(endpoint), Keyed{}, SlowKeyed{});
        } else {
            std::ifstream f(dense_path, std::ios::binary);
            std::vector<char> b((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
            if (!f && !f.eof()) return dense_path + ": cannot read";
            if (b.empty() || b.size() % (N * 4) != 0) return dense_path + ": not rows of 256 float32";
            inputs.assign(b.size() / (N * 4), Row{});
            std::memcpy(inputs.data(), b.data(), b.size());
            dense = std::make_unique<DenseClient>(scz::Endpoint::parse(endpoint), Dense{}, SlowDense{});
        }
        apply_spin();
        return {};
    }

    bool await_bound(std::chrono::milliseconds ms) {
        if (keyed) return keyed->await_bound(ms);
        return dense->await_bound(ms);
    }

    scz::Stats stats() const { return keyed ? keyed->stats() : dense->stats(); }

    /// One call, as its protocol line. The unwind hook's exception is caught here.
    std::string call(const Req& req, std::uint32_t t, std::uint32_t s) {
        char buf[256];
        std::string value;
        scz::Source src = scz::Source::Fallback;
        scz::FallbackReason reason = scz::FallbackReason::NotConnected;
        std::uint32_t gen = 0;
        bool unwound = false;
        std::uint64_t t0 = 0, t1 = 0;
        if (keyed) {
            const auto r = request(req.op, req.arg, t, s, req.seed, req.rows);
            t0 = scz::detail::monotonic_ns();
            try {
                const auto o = keyed->infer(std::span<const std::byte>(r), req.budget);
                t1 = scz::detail::monotonic_ns();
                src = o.source;
                reason = o.reason;
                gen = o.model_gen;
                std::snprintf(buf, sizeof buf, "d=0x%" PRIx64, o.value);
                value = buf;
            } catch (const scz::testing::Unwound&) {
                t1 = scz::detail::monotonic_ns();
                unwound = true;
            }
        } else {
            const std::size_t k = std::max<std::size_t>(req.rows, 1);
            std::vector<Row> rows(k);
            for (std::size_t i = 0; i < k; ++i) rows[i] = inputs[(s + i) % inputs.size()];
            t0 = scz::detail::monotonic_ns();
            try {
                const auto o = dense->infer(std::span<const Row>(rows), req.budget);
                t1 = scz::detail::monotonic_ns();
                src = o.source;
                reason = o.reason;
                gen = o.model_gen;
                value = "v=";
                for (std::uint32_t i = 0; i < o.value.n; ++i) {
                    std::uint32_t bits;
                    std::memcpy(&bits, &o.value.v[i], 4);
                    std::snprintf(buf, sizeof buf, "%s0x%x", i ? "," : "", bits);
                    value += buf;
                }
            } catch (const scz::testing::Unwound&) {
                t1 = scz::detail::monotonic_ns();
                unwound = true;
            }
        }
        if (unwound) {
            std::snprintf(buf, sizeof buf, "R t=%u s=%u src=unwound t0=%" PRIu64 " t1=%" PRIu64, t, s, t0, t1);
            return buf;
        }
        if (src == scz::Source::Model) {
            std::snprintf(buf, sizeof buf, "R t=%u s=%u src=model gen=%u ", t, s, gen);
            std::string line = buf;
            line += value;
            std::snprintf(buf, sizeof buf, " t0=%" PRIu64 " t1=%" PRIu64, t0, t1);
            return line + buf;
        }
        std::snprintf(buf, sizeof buf, "R t=%u s=%u src=fallback reason=%s t0=%" PRIu64 " t1=%" PRIu64, t, s, std::string(scz::reason_name(reason)).c_str(), t0, t1);
        return buf;
    }
};

std::map<std::string, std::string> kv(std::istringstream& in) {
    std::map<std::string, std::string> m;
    std::string w;
    while (in >> w)
        if (auto eq = w.find('='); eq != std::string::npos) m[w.substr(0, eq)] = w.substr(eq + 1);
    return m;
}

std::uint64_t num(const std::map<std::string, std::string>& m, const char* k, std::uint64_t def) {
    auto it = m.find(k);
    if (it == m.end()) return def;
    char* end = nullptr;
    const unsigned long long v = std::strtoull(it->second.c_str(), &end, 10);
    return (it->second.empty() || *end != '\0') ? def : v;
}

std::size_t count_dir(const char* path) {
    DIR* d = opendir(path);
    if (d == nullptr) return 0;
    std::size_t n = 0;
    while (dirent* e = readdir(d))
        if (std::strcmp(e->d_name, ".") != 0 && std::strcmp(e->d_name, "..") != 0) ++n;
    closedir(d);
    return n;
}

std::string proc_counts() {
    const std::size_t fds = count_dir("/proc/self/fd");
    std::size_t maps = 0;
    std::ifstream m("/proc/self/maps");
    for (std::string l; std::getline(m, l);)
        if (l.find("memfd:scz-") != std::string::npos) ++maps;
    const std::size_t threads = count_dir("/proc/self/task");
    return "PROC fds=" + std::to_string(fds) + " maps=" + std::to_string(maps) + " threads=" + std::to_string(threads);
}

/// FORK: the child makes one keyed call and reports it through a pipe, closes its client
/// (which must not hang), and exits 0; the parent waits.
std::string fork_once(Agent& a) {
    int p[2];
    if (pipe2(p, O_CLOEXEC) != 0) return "ERR pipe";
    std::fflush(stdout);
    const pid_t pid = fork();
    if (pid < 0) return "ERR fork";
    if (pid == 0) {
        ::close(p[0]);
        const std::string line = a.call(Req{0, 0, 1, 0, 20ms}, 0, 0);
        std::istringstream in(line);
        std::string w;
        in >> w;
        const auto f = kv(in);
        auto get = [&](const char* k, const char* d) { auto it = f.find(k); return it == f.end() ? std::string(d) : it->second; };
        const std::string msg = "src=" + get("src", "?") + " reason=" + get("reason", "-");
        (void)!::write(p[1], msg.data(), msg.size());
        ::close(p[1]);
        a.close();  // the child's client: no thread of the parent's to join
        _exit(0);
    }
    ::close(p[1]);
    char buf[256];
    std::size_t got = 0;
    for (;;) {
        const ssize_t n = ::read(p[0], buf + got, sizeof buf - got);
        if (n <= 0) break;
        got += static_cast<std::size_t>(n);
    }
    ::close(p[0]);
    int st = 0;
    waitpid(pid, &st, 0);
    const int status = WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
    return "FORK " + std::string(buf, got) + " status=" + std::to_string(status);
}

}  // namespace

int main(int argc, char** argv) {
    Agent a;
    bool have = false;
    for (int i = 1; i < argc; ++i) {
        const std::string s = argv[i];
        if (s.rfind("--endpoint=", 0) == 0) {
            a.endpoint = s.substr(11);
            have = true;
        }
    }
    if (!have) {
        std::fprintf(stderr, "usage: scz-agent-cpp20 --endpoint=PATH\n");
        return 2;
    }
    if (std::string e = a.build(); !e.empty()) {
        std::fprintf(stderr, "%s\n", e.c_str());
        return 1;
    }
    auto say = [](const std::string& s) {
        std::fputs(s.c_str(), stdout);
        std::fputc('\n', stdout);
        std::fflush(stdout);
    };
#if defined(SCZ_CRASHPOINTS)
    const char* crash = ",crashpoints";
#else
    const char* crash = "";
#endif
    say(std::string("HELLO sdk=cpp version=0.1.0 caps=threads,per-call,frames,unwind,nospin,fork,spawn") + crash);

    for (std::string line; std::getline(std::cin, line);) {
        std::istringstream in(line);
        std::string cmd;
        if (!(in >> cmd)) continue;
        const auto m = kv(in);
        if (cmd == "CONFIG") {
            auto kind = m.find("kind");
            if (kind != m.end() && kind->second != "platform" && kind->second != "per-call") {
                say("ERR unsupported");
                continue;
            }
            auto fb = m.find("fallback");
            if (fb != m.end() && fb->second != "constant") {
                say("ERR unsupported");  // a throwing fallback does not compile in C++ (FallbackPolicy)
                continue;
            }
            a.threads = static_cast<std::uint32_t>(std::max<std::uint64_t>(num(m, "threads", a.threads), 1));
            if (kind != m.end()) a.per_call = kind->second == "per-call";
            g_fallback_us.store(num(m, "fallback_us", g_fallback_us.load()));
            if (auto sp = m.find("spin"); sp != m.end()) a.spin_off = sp->second == "off";
            std::string dense = a.dense_path;
            if (auto c = m.find("codec"); c != m.end()) {
                if (c->second == "features") {
                    auto ip = m.find("inputs");
                    dense = ip == m.end() ? std::string() : ip->second;
                    if (dense.empty()) {
                        say("ERR codec=features needs inputs=PATH");
                        continue;
                    }
                } else {
                    dense.clear();
                }
            }
            if (dense != a.dense_path && a.open()) {
                // Another codec is another client: the old session ends first.
                a.dense_path = dense;
                a.close();
                if (std::string e = a.build(); !e.empty()) {
                    say("ERR " + e);
                    continue;
                }
            }
            a.dense_path = dense;
            a.apply_spin();
            say("OK");
        } else if (cmd == "CODEC_FAULT") {
            auto md = m.find("mode");
            const std::string mode = md == m.end() ? "off" : md->second;
            g_fault.store(mode == "encode" ? ENCODE : mode == "decode" ? DECODE : 0);
            say("OK");
        } else if (cmd == "UNWIND") {
            scz::testing::unwind_once.store(true, std::memory_order_relaxed);
            say("OK");
        } else if (cmd == "CLOCK_SKEW") {
            say("ERR unsupported");
        } else if (cmd == "SPAWN") {
            pid_t pid;
            char arg0[] = "sleep", arg1[] = "600";
            char* args[] = {arg0, arg1, nullptr};
            if (posix_spawnp(&pid, "sleep", nullptr, nullptr, args, environ) == 0) say("SPAWNED pid=" + std::to_string(pid));
            else say("ERR spawn");
        } else if (cmd == "PROC") {
            say(proc_counts());
        } else if (cmd == "EXIT") {
            say("BYE");
            a.close();
            std::exit(0);
        } else if (!a.open()) {
            say("ERR closed");
        } else if (cmd == "BOUND") {
            say(std::string("BOUND ") + (a.await_bound(std::chrono::milliseconds(num(m, "ms", 5000))) ? "true" : "false"));
        } else if (cmd == "FORK") {
            say(fork_once(a));
        } else if (cmd == "CALL") {
            const auto count = static_cast<std::uint32_t>(num(m, "count", 1));
            Req req;
            if (auto op = m.find("op"); op != m.end() && !parse_op(op->second, req.op)) req.op = 0;
            req.arg = num(m, "arg", 0);
            req.seed = num(m, "seed", 1);
            req.rows = static_cast<std::size_t>(num(m, "rows", 0));
            req.budget = std::chrono::microseconds(num(m, "budget_us", 20000));
            std::vector<std::vector<std::string>> lines(a.threads);
            std::vector<std::thread> hs;
            for (std::uint32_t t = 0; t < a.threads; ++t) {
                hs.emplace_back([&, t] {
                    for (std::uint32_t s = 0; s < count; ++s) {
                        if (a.per_call) {
                            std::string l;
                            std::thread one([&] { l = a.call(req, t, s); });
                            one.join();
                            lines[t].push_back(std::move(l));
                        } else {
                            lines[t].push_back(a.call(req, t, s));
                        }
                    }
                });
            }
            for (auto& h : hs) h.join();
            std::size_t total = 0;
            for (const auto& v : lines)
                for (const auto& l : v) {
                    std::fputs(l.c_str(), stdout);
                    std::fputc('\n', stdout);
                    ++total;
                }
            std::fprintf(stdout, "END calls=%zu\n", total);
            std::fflush(stdout);
        } else if (cmd == "STATS") {
            const scz::Stats s = a.stats();
            std::string out = "STATS model=" + std::to_string(s.model) + " sessions=" + std::to_string(s.sessions) + " retired=" + std::to_string(s.retired) +
                              " late_wakes=" + std::to_string(s.late_wakes);
            for (std::size_t r = 0; r < scz::kReasonCount; ++r)
                if (s.fallbacks[r] > 0) out += " fb." + std::string(scz::reason_name(static_cast<scz::FallbackReason>(r))) + "=" + std::to_string(s.fallbacks[r]);
            say(out);
        } else if (cmd == "CLOSE") {
            a.close();
            say("CLOSED");
        } else {
            say("ERR unknown command " + cmd);
        }
    }
    a.close();
    return 0;
}
