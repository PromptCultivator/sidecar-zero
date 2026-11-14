// SidecarZero C++20 SDK: model inference through shared memory, with a fallback that always
// answers (sdk-contract §1). Header-only; Linux x86-64 and aarch64; elsewhere every call
// falls back with NOT_CONNECTED.
//
//   scz::Client client{
//       scz::Endpoint::path("/run/sidecarzero/scz.sock"),
//       scz::codecs::DenseF32ToScore<256>{},
//       [](const std::array<float, 256>& x, scz::FallbackReason) noexcept { return x[17] > 0.8f ? 0.9f : 0.1f; }};
//   auto r = client.infer(features, std::chrono::milliseconds(2));
//   std::string metrics = client.prometheus_text();   // sdk-contract §8
#pragma once
#include "abi.hpp"
#include "client.hpp"
#include "codec.hpp"
#include "fallback.hpp"
#include "metrics.hpp"
