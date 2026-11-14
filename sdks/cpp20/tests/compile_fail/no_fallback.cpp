// Must not compile: there is no constructor without a fallback (sdk-contract §3.1).
#include <scz/sidecarzero.hpp>
int main() {
    scz::Client client{scz::Endpoint::path("/run/scz.sock"), scz::codecs::DenseF32ToScore<256>{}};
    (void)client;
}
