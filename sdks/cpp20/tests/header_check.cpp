// The headers as an application includes them: no test hooks. Compiled with -Wconversion,
// and with -fno-exceptions -fno-rtti, by the Makefile (sdk-cpp20 §9, "Both build modes").
#include <scz/sidecarzero.hpp>

#include <array>
#include <chrono>
#include <cstdio>
#include <string>

namespace {
[[maybe_unused]] float use() {
    scz::Client client{scz::Endpoint::path("/run/sidecarzero/scz.sock"), scz::codecs::DenseF32ToScore<256>{},
                       [](const std::array<float, 256>& x, scz::FallbackReason) noexcept { return x[17] > 0.8f ? 0.9f : 0.1f; }};
    std::array<float, 256> features{};
    const std::chrono::nanoseconds budget = std::chrono::milliseconds(2);
    static_assert(noexcept(client.infer(features, budget)), "infer is noexcept without test hooks");
    auto r = client.infer(features, budget);
    return r.value;
}

// sdk-contract §8: the sampling option, the on_fallback hook (deduced, and not noexcept), the
// export to a sink of the application's, and the text format.
struct CountingSink final : scz::MetricsSink {
    std::size_t n = 0;
    void counter(std::string_view, std::span<const scz::Label>, std::uint64_t) override { ++n; }
    void gauge(std::string_view, std::span<const scz::Label>, double) override { ++n; }
    void histogram(std::string_view, std::span<const scz::Label>, std::span<const double>, std::span<const std::uint64_t>, double, std::uint64_t) override {
        ++n;
    }
};

[[maybe_unused]] std::string metrics() {
    scz::Options o;
    o.sample_every = 16;
    scz::Client client{scz::Endpoint::path("/run/sidecarzero/scz.sock"), scz::codecs::DenseF32ToScore<4>{},
                       [](const std::array<float, 4>&, scz::FallbackReason) noexcept { return 0.0f; }, o,
                       [](scz::FallbackReason r) { std::fprintf(stderr, "fallback: %s\n", scz::reason_name(r).data()); }};
    std::array<float, 4> features{};
    const std::chrono::nanoseconds budget = std::chrono::milliseconds(2);
    static_assert(noexcept(client.infer(features, budget)), "infer is noexcept with a hook, too");
    (void)client.infer(features, budget);
    CountingSink sink;
    client.export_metrics(sink);
    const scz::Stats s = client.stats();
    return client.prometheus_text() + std::to_string(sink.n + s.latency_fallback.count);
}
}  // namespace

int main() { return 0; }
