// An application of the installed package: no sidecar, so the call falls back; the hook sees
// it, and the metrics text counts it (sdk-contract §8).
#include <scz/sidecarzero.hpp>

#include <array>
#include <chrono>
#include <cstdio>
#include <string>

int main() {
    int hooked = 0;
    scz::Options options;
    options.sample_every = 1;
    scz::Client client{scz::Endpoint::path("/nonexistent/scz.sock"), scz::codecs::DenseF32ToScore<4>{},
                       [](const std::array<float, 4>&, scz::FallbackReason) noexcept { return 0.5f; }, options,
                       [&hooked](scz::FallbackReason) { ++hooked; }};
    auto r = client.infer(std::array<float, 4>{1, 2, 3, 4}, std::chrono::milliseconds(1));
    const std::string text = client.prometheus_text();
    const bool counted = text.find("scz_client_calls_total{source=\"fallback\",reason=\"NOT_CONNECTED\"} 1\n") != std::string::npos &&
                         text.find("scz_client_fallback_seconds_count 1\n") != std::string::npos;
    std::printf("value %.2f, from the %s; hooked %d; metrics %s\n", static_cast<double>(r.value), r.is_model() ? "model" : "fallback", hooked,
                counted ? "counted" : "MISSING");
    return r.value == 0.5f && hooked == 1 && counted ? 0 : 1;
}
