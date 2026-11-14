// Must not compile: the fallback is not noexcept (FallbackPolicy, sdk-contract §3.2).
#include <scz/sidecarzero.hpp>
int main() {
    scz::Client client{scz::Endpoint::path("/run/scz.sock"), scz::codecs::DenseF32ToScore<256>{},
                       [](const std::array<float, 256>&, scz::FallbackReason) { return 0.0f; }};
    (void)client;
}
