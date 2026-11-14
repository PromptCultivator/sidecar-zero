// Must not compile: a codec whose encode returns void cannot report failure (sdk-cpp20 §2).
#include <scz/sidecarzero.hpp>
struct Bad {
    using input_type = std::array<float, 4>;
    using output_type = float;
    std::size_t request_len(const input_type&) const noexcept { return 16; }
    void encode(const input_type&, scz::SlotWriter&) const noexcept {}
    bool decode(const scz::SlotReader&, output_type&) const noexcept { return true; }
};
int main() {
    scz::Client client{scz::Endpoint::path("/run/scz.sock"), Bad{}, [](const std::array<float, 4>&, scz::FallbackReason) noexcept { return 0.0f; }};
    (void)client;
}
