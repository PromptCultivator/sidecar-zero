// Must not compile: the fallback returns another type than the codec's output.
#include <string>
#include <scz/sidecarzero.hpp>
int main() {
    scz::Client client{scz::Endpoint::path("/run/scz.sock"), scz::codecs::DenseF32ToScore<256>{},
                       [](const std::array<float, 256>&, scz::FallbackReason) noexcept { return std::string("no"); }};
    (void)client;
}
