// The two hashes the protocol names: XXH3-64 with seed 0 (arrow-data-plane §9, the schema
// fingerprint; golden vectors in abi/golden/fingerprints.txt) and CRC-32C (the control
// header's layout_crc32c). Written here from the published algorithms so the SDK has no
// dependency; the unit tests check both against reference vectors.
#pragma once
#include <cstddef>
#include <cstdint>
#include <cstring>

namespace scz::inline v1::detail {

namespace xxh3 {
inline constexpr std::uint8_t kSecret[192] = {
    0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c, 0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb,
    0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f, 0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
    0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c, 0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb,
    0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3, 0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
    0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d, 0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31,
    0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64, 0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
    0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e, 0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc,
    0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce, 0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
};
inline constexpr std::uint64_t P32_1 = 0x9E3779B1U, P32_2 = 0x85EBCA77U, P32_3 = 0xC2B2AE3DU;
inline constexpr std::uint64_t P64_1 = 0x9E3779B185EBCA87ULL, P64_2 = 0xC2B2AE3D27D4EB4FULL, P64_3 = 0x165667B19E3779F9ULL;
inline constexpr std::uint64_t P64_4 = 0x85EBCA77C2B2AE63ULL, P64_5 = 0x27D4EB2F165667C5ULL;
inline constexpr std::uint64_t MX1 = 0x165667919E3779F9ULL, MX2 = 0x9FB21C651E98DF25ULL;

inline std::uint64_t r64(const std::uint8_t* p) noexcept { std::uint64_t v; std::memcpy(&v, p, 8); return v; }
inline std::uint32_t r32(const std::uint8_t* p) noexcept { std::uint32_t v; std::memcpy(&v, p, 4); return v; }
inline std::uint64_t rotl(std::uint64_t x, int r) noexcept { return (x << r) | (x >> (64 - r)); }
__extension__ typedef unsigned __int128 u128;
inline std::uint64_t mul_fold(std::uint64_t a, std::uint64_t b) noexcept {
    const u128 p = static_cast<u128>(a) * b;
    return static_cast<std::uint64_t>(p) ^ static_cast<std::uint64_t>(p >> 64);
}
inline std::uint64_t avalanche(std::uint64_t h) noexcept {
    h ^= h >> 37;
    h *= MX1;
    return h ^ (h >> 32);
}
inline std::uint64_t xxh64_avalanche(std::uint64_t h) noexcept {
    h ^= h >> 33;
    h *= P64_2;
    h ^= h >> 29;
    h *= P64_3;
    return h ^ (h >> 32);
}
inline std::uint64_t rrmxmx(std::uint64_t h, std::uint64_t len) noexcept {
    h ^= rotl(h, 49) ^ rotl(h, 24);
    h *= MX2;
    h ^= (h >> 35) + len;
    h *= MX2;
    return h ^ (h >> 28);
}
inline std::uint64_t mix16(const std::uint8_t* in, const std::uint8_t* sec) noexcept {
    return mul_fold(r64(in) ^ r64(sec), r64(in + 8) ^ r64(sec + 8));
}
inline void accumulate512(std::uint64_t* acc, const std::uint8_t* in, const std::uint8_t* sec) noexcept {
    for (int i = 0; i < 8; ++i) {
        const std::uint64_t v = r64(in + 8 * i);
        const std::uint64_t k = v ^ r64(sec + 8 * i);
        acc[i ^ 1] += v;
        acc[i] += (k & 0xFFFFFFFFULL) * (k >> 32);
    }
}
inline void scramble(std::uint64_t* acc, const std::uint8_t* sec) noexcept {
    for (int i = 0; i < 8; ++i) {
        std::uint64_t a = acc[i];
        a ^= a >> 47;
        a ^= r64(sec + 8 * i);
        acc[i] = a * P32_1;
    }
}
inline std::uint64_t long_hash(const std::uint8_t* in, std::size_t len) noexcept {
    constexpr std::size_t secret_size = sizeof(kSecret);
    constexpr std::size_t stripes = (secret_size - 64) / 8;
    constexpr std::size_t block = 64 * stripes;
    std::uint64_t acc[8] = {P32_3, P64_1, P64_2, P64_3, P64_4, P32_2, P64_5, P32_1};
    const std::size_t blocks = (len - 1) / block;
    for (std::size_t n = 0; n < blocks; ++n) {
        for (std::size_t s = 0; s < stripes; ++s) accumulate512(acc, in + n * block + s * 64, kSecret + s * 8);
        scramble(acc, kSecret + secret_size - 64);
    }
    const std::size_t last = ((len - 1) - block * blocks) / 64;
    for (std::size_t s = 0; s < last; ++s) accumulate512(acc, in + blocks * block + s * 64, kSecret + s * 8);
    accumulate512(acc, in + len - 64, kSecret + secret_size - 64 - 7);
    std::uint64_t r = len * P64_1;
    for (int i = 0; i < 4; ++i) r += mul_fold(acc[2 * i] ^ r64(kSecret + 11 + 16 * i), acc[2 * i + 1] ^ r64(kSecret + 11 + 16 * i + 8));
    return avalanche(r);
}
}  // namespace xxh3

/// XXH3-64 with seed 0 and the default secret.
[[nodiscard]] inline std::uint64_t xxh3_64(const void* data, std::size_t len) noexcept {
    using namespace xxh3;
    const auto* in = static_cast<const std::uint8_t*>(data);
    const std::uint8_t* s = kSecret;
    if (len == 0) return xxh64_avalanche(r64(s + 56) ^ r64(s + 64));
    if (len <= 3) {
        const std::uint32_t c = (static_cast<std::uint32_t>(in[0]) << 16) | (static_cast<std::uint32_t>(in[len >> 1]) << 24) | in[len - 1] |
                                (static_cast<std::uint32_t>(len) << 8);
        const std::uint64_t flip = r32(s) ^ r32(s + 4);
        return xxh64_avalanche(c ^ flip);
    }
    if (len <= 8) {
        const std::uint64_t flip = r64(s + 8) ^ r64(s + 16);
        const std::uint64_t v = r32(in + len - 4) + (static_cast<std::uint64_t>(r32(in)) << 32);
        return rrmxmx(v ^ flip, len);
    }
    if (len <= 16) {
        const std::uint64_t lo = r64(in) ^ (r64(s + 24) ^ r64(s + 32));
        const std::uint64_t hi = r64(in + len - 8) ^ (r64(s + 40) ^ r64(s + 48));
        return avalanche(len + __builtin_bswap64(lo) + hi + mul_fold(lo, hi));
    }
    if (len <= 128) {
        std::uint64_t acc = len * P64_1;
        if (len > 32) {
            if (len > 64) {
                if (len > 96) {
                    acc += mix16(in + 48, s + 96);
                    acc += mix16(in + len - 64, s + 112);
                }
                acc += mix16(in + 32, s + 64);
                acc += mix16(in + len - 48, s + 80);
            }
            acc += mix16(in + 16, s + 32);
            acc += mix16(in + len - 32, s + 48);
        }
        acc += mix16(in, s);
        acc += mix16(in + len - 16, s + 16);
        return avalanche(acc);
    }
    if (len <= 240) {
        std::uint64_t acc = len * P64_1;
        const std::size_t rounds = len / 16;
        for (std::size_t i = 0; i < 8; ++i) acc += mix16(in + 16 * i, s + 16 * i);
        acc = avalanche(acc);
        for (std::size_t i = 8; i < rounds; ++i) acc += mix16(in + 16 * i, s + 16 * (i - 8) + 3);
        acc += mix16(in + len - 16, s + 136 - 17);
        return avalanche(acc);
    }
    return long_hash(in, len);
}

/// CRC-32C (Castagnoli, reflected, init and final xor 0xFFFFFFFF), continued over several
/// ranges: pass the previous return value as `crc` (start with 0).
[[nodiscard]] inline std::uint32_t crc32c_update(std::uint32_t crc, const void* data, std::size_t len) noexcept {
    const auto* p = static_cast<const std::uint8_t*>(data);
    crc = ~crc;
    for (std::size_t i = 0; i < len; ++i) {
        crc ^= p[i];
        for (int k = 0; k < 8; ++k) crc = (crc >> 1) ^ (0x82F63B78U & (0U - (crc & 1U)));
    }
    return ~crc;
}

}  // namespace scz::inline v1::detail
