// Must not compile: the on_fallback hook cannot be called with a reason (FallbackHook, sdk-contract §8).
#include <string>
#include <scz/sidecarzero.hpp>
int main() {
    scz::Client client{scz::Endpoint::path("/run/scz.sock"), scz::codecs::DenseF32ToScore<256>{},
                       [](const std::array<float, 256>&, scz::FallbackReason) noexcept { return 0.0f; }, scz::Options{},
                       [](const std::string&) {}};
    (void)client;
}
