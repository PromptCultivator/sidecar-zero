// Metrics and hooks (sdk-contract §8), as sdks/rust/src/metrics.rs has them: the bucket bounds,
// the sampled histograms the client sums into, the sink every metric is exported through, and
// a rendering of them in the Prometheus text exposition format that needs no dependency.
//
// Only Hist::record is on the call path, and only for a timed call: two relaxed atomic adds
// on a shard of the client's. The export and the text renderer allocate (std::string); they
// run when the application asks for them, never inside infer.
#pragma once
#include <algorithm>
#include <array>
#include <atomic>
#include <charconv>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <version>

// std::to_chars and std::from_chars for double: libstdc++ 11 and later. Without them (libc++
// on an older macOS deployment target) numbers go through snprintf and strtod instead.
#if defined(__cpp_lib_to_chars) && __cpp_lib_to_chars >= 201611L
#define SCZ_FLOAT_TO_CHARS 1
#else
#define SCZ_FLOAT_TO_CHARS 0
#endif

namespace scz::inline v1 {

/// Upper bounds of the histogram buckets, in seconds (sdk-contract §8); a last bucket, +Inf,
/// follows them. A duration lands in the first bucket whose bound is at least as large.
inline constexpr std::array<double, 16> kBucketsSeconds = {1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4, 2e-4,
                                                           5e-4, 1e-3, 2e-3, 5e-3, 1e-2, 2e-2, 5e-2, 0.1};
/// One call in this many, per calling thread, is timed (sdk-contract §8, "Sampling").
inline constexpr std::uint32_t kDefaultSampleEvery = 64;

/// A histogram as read: counts per bucket, cumulative as the Prometheus format has them.
struct Histogram {
    /// `cumulative[i]`: durations of at most `kBucketsSeconds[i]`; the last (+Inf), every one.
    std::array<std::uint64_t, kBucketsSeconds.size() + 1> cumulative{};
    double sum_seconds = 0;
    std::uint64_t count = 0;
    bool operator==(const Histogram&) const = default;
};

/// One label of a series. The views are valid during the sink call that receives them.
struct Label {
    std::string_view name;
    std::string_view value;
};

/// Where `Client::export_metrics` writes each metric of sdk-contract §8. Adapters for a
/// metrics library implement it; PrometheusText is the one in the core. Labels come in order
/// ({source, fallback}, {reason, DEADLINE}); every span is valid during the call only.
class MetricsSink {
public:
    virtual ~MetricsSink() = default;
    /// A counter's value: exact, never decreasing.
    virtual void counter(std::string_view name, std::span<const Label> labels, std::uint64_t value) = 0;
    virtual void gauge(std::string_view name, std::span<const Label> labels, double value) = 0;
    /// A sampled histogram: `bounds` are kBucketsSeconds; `cumulative` has one more entry, for
    /// +Inf, which equals `count`. `sum` is in seconds.
    virtual void histogram(std::string_view name, std::span<const Label> labels, std::span<const double> bounds,
                           std::span<const std::uint64_t> cumulative, double sum, std::uint64_t count) = 0;
};

namespace detail {

/// kBucketsSeconds in nanoseconds, the unit durations are recorded in.
inline constexpr std::array<std::uint64_t, 16> kBoundsNs = {1'000,   2'000,     5'000,     10'000,    20'000,     50'000,     100'000,    200'000,
                                                            500'000, 1'000'000, 2'000'000, 5'000'000, 10'000'000, 20'000'000, 50'000'000, 100'000'000};
inline constexpr std::uint32_t kHistShards = 16;

/// One histogram's buckets, added to by any thread (timed calls only, so rarely).
struct Hist {
    std::atomic<std::uint64_t> buckets[kBoundsNs.size() + 1] = {};
    std::atomic<std::uint64_t> sum_ns{0};
    void record(std::uint64_t ns) noexcept {
        std::size_t i = 0;
        while (i < kBoundsNs.size() && ns > kBoundsNs[i]) ++i;
        buckets[i].fetch_add(1, std::memory_order_relaxed);
        sum_ns.fetch_add(ns, std::memory_order_relaxed);
    }
};

/// The three sampled histograms, in one of kHistShards shards chosen by the thread's hint.
struct alignas(64) HistShard {
    Hist model;        // end-to-end infer time of calls the model answered,
    Hist fallback;     // and of those that fell back, the fallback included
    Hist fallback_fn;  // the fallback function alone
};

/// The shards' `which` histograms summed, cumulative.
[[nodiscard]] inline Histogram sum_of(const HistShard* shards, std::size_t n, Hist HistShard::*which) noexcept {
    std::array<std::uint64_t, kBoundsNs.size() + 1> raw{};
    std::uint64_t sum = 0;
    for (std::size_t s = 0; s < n; ++s) {
        const Hist& h = shards[s].*which;
        for (std::size_t i = 0; i < raw.size(); ++i) raw[i] += h.buckets[i].load(std::memory_order_relaxed);
        sum += h.sum_ns.load(std::memory_order_relaxed);  // wraps, as the Rust SDK's
    }
    Histogram out;
    std::uint64_t acc = 0;
    for (std::size_t i = 0; i < raw.size(); ++i) {
        acc += raw[i];
        out.cumulative[i] = acc;
    }
    out.sum_seconds = static_cast<double>(sum) / 1e9;
    out.count = acc;
    return out;
}

// ── numbers as Rust's Display for f64 writes them ──────────────────────────────────────────

/// A positive finite double in decimal: `d[0].d[1]d[2]… × 10^exp`, without leading or
/// trailing zeros.
struct Digits {
    std::string d;
    int exp = 0;
};

inline void strip_zeros(Digits& x) {
    while (x.d.size() > 1 && x.d.back() == '0') x.d.pop_back();
}

/// `d.ddde±x` as printed, whatever character stands for the point (a locale's, for snprintf);
/// trailing zeros kept.
[[nodiscard]] inline Digits parse_scientific(std::string_view s) {
    Digits r;
    std::size_t i = 0;
    for (; i < s.size() && s[i] != 'e'; ++i)
        if (s[i] >= '0' && s[i] <= '9') r.d.push_back(s[i]);
    bool neg = false;
    if (i < s.size()) ++i;
    if (i < s.size() && (s[i] == '+' || s[i] == '-')) neg = s[i++] == '-';
    int e = 0;
    for (; i < s.size() && s[i] >= '0' && s[i] <= '9'; ++i) e = e * 10 + (s[i] - '0');
    r.exp = neg ? -e : e;
    return r;
}

/// Whether `x`, written as an integer and a power of ten (no point, so no locale enters),
/// reads back as `v`.
template <bool Portable>
[[nodiscard]] bool reads_back(const Digits& x, double v) {
    std::string s = x.d;
    s += 'e';
    s += std::to_string(x.exp - static_cast<int>(x.d.size()) + 1);
#if SCZ_FLOAT_TO_CHARS
    if constexpr (!Portable) {
        double r = 0;
        const auto res = std::from_chars(s.data(), s.data() + s.size(), r);
        return res.ec == std::errc{} && res.ptr == s.data() + s.size() && r == v;
    }
#endif
    return std::strtod(s.c_str(), nullptr) == v;
}

/// `x` plus one unit in its last digit, trailing zeros stripped.
[[nodiscard]] inline Digits next_up(Digits x) {
    std::size_t i = x.d.size();
    while (i > 0 && x.d[i - 1] == '9') x.d[--i] = '0';
    if (i == 0) {
        x.d.insert(x.d.begin(), '1');
        ++x.exp;
    } else {
        ++x.d[i - 1];
    }
    strip_zeros(x);
    return x;
}

/// The shortest decimal that reads back as `a` (> 0, finite), the closest to it of those; on
/// an exact tie between two, either.
template <bool Portable>
[[nodiscard]] Digits shortest(double a) {
    char buf[64];
#if SCZ_FLOAT_TO_CHARS
    if constexpr (!Portable) {
        const auto r = std::to_chars(buf, buf + sizeof buf, a, std::chars_format::scientific);
        Digits d = parse_scientific({buf, static_cast<std::size_t>(r.ptr - buf)});
        strip_zeros(d);
        return d;
    }
#endif
    // The correctly rounded decimal of 1, 2, … 17 digits, the first that reads back; where the
    // double is a power of two, the interval that reads back is wider above it, and the
    // decimal one unit above may read back where the nearest does not.
    Digits d;
    for (int p = 0; p < 17; ++p) {
        const int n = std::snprintf(buf, sizeof buf, "%.*e", p, a);
        d = parse_scientific({buf, n > 0 ? static_cast<std::size_t>(n) : 0});
        Digits up = next_up(d);  // one unit in the (p+1)th digit
        strip_zeros(d);
        if (reads_back<Portable>(d, a)) return d;
        if (reads_back<Portable>(up, a)) return up;
    }
    return d;
}

/// The exact decimal value of `a` (> 0, finite): 767 digits after the first hold any double's.
template <bool Portable>
[[nodiscard]] Digits exact(double a) {
    char buf[800];
#if SCZ_FLOAT_TO_CHARS
    if constexpr (!Portable) {
        const auto r = std::to_chars(buf, buf + sizeof buf, a, std::chars_format::scientific, 767);
        Digits x = parse_scientific({buf, static_cast<std::size_t>(r.ptr - buf)});
        strip_zeros(x);
        return x;
    }
#endif
    const int n = std::snprintf(buf, sizeof buf, "%.767e", a);
    Digits x = parse_scientific({buf, n <= 0 ? 0 : std::min(static_cast<std::size_t>(n), sizeof buf - 1)});
    strip_zeros(x);
    return x;
}

/// A double as Rust's `Display` for f64 writes it: the shortest decimal that reads back as the
/// same double, the closest to it of those, in positional notation (never an exponent), with
/// no trailing ".0"; NaN, inf, -inf, and -0 as Rust spells them. Where the double is exactly
/// halfway between two shortest decimals, std::to_chars (Ryu) takes the even one and Rust the
/// one further from zero: Rust's here. `Portable` uses snprintf and strtod (the build without
/// std::to_chars for double); the tests run both.
template <bool Portable = !SCZ_FLOAT_TO_CHARS>
[[nodiscard]] std::string format_f64(double v) {
    if (std::isnan(v)) return "NaN";
    if (std::isinf(v)) return v > 0 ? "inf" : "-inf";
    if (v == 0) return std::signbit(v) ? "-0" : "0";
    const double a = std::fabs(v);
    Digits d = shortest<Portable>(a);
    // An exact tie: the value's exact digits are the shortest's and then a single 5.
    const Digits x = exact<Portable>(a);
    if (x.exp == d.exp && x.d.size() == d.d.size() + 1 && x.d.back() == '5' && x.d.compare(0, d.d.size(), d.d) == 0) {
        Digits up = next_up(d);
        if (reads_back<Portable>(up, a)) d = std::move(up);
    }
    std::string out = std::signbit(v) ? "-" : "";
    const auto n = static_cast<int>(d.d.size());
    if (d.exp >= n - 1) {
        out += d.d;
        out.append(static_cast<std::size_t>(d.exp - (n - 1)), '0');
    } else if (d.exp >= 0) {
        out.append(d.d, 0, static_cast<std::size_t>(d.exp) + 1);
        out += '.';
        out.append(d.d, static_cast<std::size_t>(d.exp) + 1);
    } else {
        out += "0.";
        out.append(static_cast<std::size_t>(-d.exp - 1), '0');
        out += d.d;
    }
    return out;
}

}  // namespace detail

/// The Prometheus text exposition format (sdk-contract §8, "A text adapter in the core"), for a
/// service that serves /metrics itself: one `# TYPE` line per metric family, then its series;
/// histograms as `_bucket` lines (with `le`), `_sum`, and `_count`. Label values escape
/// backslash, double quote, and newline. Numbers are written as the Rust SDK writes them
/// (detail::format_f64: 0.000001, 1.5, 2), so every SDK renders the same numbers as the same
/// text.
///
///     scz::PrometheusText t;
///     client.export_metrics(t);
///     std::string body = t.finish();   // or client.prometheus_text()
class PrometheusText final : public MetricsSink {
public:
    PrometheusText() = default;

    /// The text written so far.
    [[nodiscard]] const std::string& str() const noexcept { return out_; }
    /// The text written so far, handed over; the sink starts again empty.
    [[nodiscard]] std::string finish() {
        last_.clear();
        return std::exchange(out_, std::string());
    }

    void counter(std::string_view name, std::span<const Label> labels, std::uint64_t value) override {
        kind(name, "counter");
        line(name, "", labels, {}, std::to_string(value));
    }
    void gauge(std::string_view name, std::span<const Label> labels, double value) override {
        kind(name, "gauge");
        line(name, "", labels, {}, detail::format_f64(value));
    }
    void histogram(std::string_view name, std::span<const Label> labels, std::span<const double> bounds, std::span<const std::uint64_t> cumulative,
                   double sum, std::uint64_t count) override {
        kind(name, "histogram");
        for (std::size_t i = 0; i < cumulative.size(); ++i) {
            const std::string le = i < bounds.size() ? detail::format_f64(bounds[i]) : std::string("+Inf");
            line(name, "_bucket", labels, le, std::to_string(cumulative[i]));
        }
        line(name, "_sum", labels, {}, detail::format_f64(sum));
        line(name, "_count", labels, {}, std::to_string(count));
    }

private:
    void kind(std::string_view name, std::string_view k) {
        if (last_ == name) return;
        out_ += "# TYPE ";
        out_ += name;
        out_ += ' ';
        out_ += k;
        out_ += '\n';
        last_ = name;
    }
    void pair(std::string_view name, std::string_view value) {
        out_ += name;
        out_ += "=\"";
        for (const char c : value) {
            switch (c) {
            case '\\': out_ += "\\\\"; break;
            case '"': out_ += "\\\""; break;
            case '\n': out_ += "\\n"; break;
            default: out_ += c;
            }
        }
        out_ += '"';
    }
    /// `name suffix {labels, le="…"} value`: no braces when there is no label and no le.
    void line(std::string_view name, std::string_view suffix, std::span<const Label> labels, std::string_view le, std::string_view value) {
        out_ += name;
        out_ += suffix;
        if (!labels.empty() || !le.empty()) {
            out_ += '{';
            for (std::size_t i = 0; i < labels.size(); ++i) {
                if (i > 0) out_ += ',';
                pair(labels[i].name, labels[i].value);
            }
            if (!le.empty()) {
                if (!labels.empty()) out_ += ',';
                pair("le", le);
            }
            out_ += '}';
        }
        out_ += ' ';
        out_ += value;
        out_ += '\n';
    }

    std::string out_;
    std::string last_;
};

}  // namespace scz::inline v1
