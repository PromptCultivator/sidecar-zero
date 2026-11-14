// Where everything sits in the data segment, read once from the read-only control segment and
// checked (sdk-contract §6: "Geometry is read once"; shm-abi §1: nothing in the data segment
// describes its own layout). Mirrors scz-shm's `Directory::read`.
#pragma once
#include <cstddef>
#include <cstdint>

#include "hash.hpp"
#include "shared.hpp"

namespace scz::inline v1::detail {

struct RingGeo {
    std::uint64_t off = 0;
    std::uint64_t capacity = 0;
};

struct SlabGeo {
    std::uint64_t table_off = 0;
    std::uint64_t data_off = 0;
    std::uint32_t slot_size = 0;
    std::uint32_t slot_count = 0;
    std::uint32_t req_cap = 0;
    std::uint32_t resp_cap = 0;
};

struct Directory {
    std::uint64_t arena_id = 0;
    std::uint64_t data_len = 0;
    std::uint64_t status_off = 0;
    std::uint32_t ring_count = 0;
    std::uint32_t class_count = 0;
    RingGeo rings[abi::MAX_RINGS];
    SlabGeo slabs[abi::MAX_SLAB_CLASSES];
    std::uint64_t hb_interval_ns = 0;
    std::uint64_t hb_stale_ns = 0;
    std::uint64_t spin_ns = 0;
    std::uint64_t default_deadline_ns = 0;
    std::uint64_t max_deadline_ns = 0;
};

/// CRC-32C over the header's IDENT, CONFIG, and DIRECTORY, with the checksum field as zero.
[[nodiscard]] inline std::uint32_t layout_crc(const std::byte* ctl) noexcept {
    alignas(8) std::uint8_t header[abi::CTL_HEADER_LEN];
    for (std::size_t i = 0; i < abi::CTL_HEADER_LEN; i += 8) {
        const std::uint64_t w = fetch_once<std::uint64_t>(ctl + i);
        std::memcpy(header + i, &w, 8);
    }
    std::memset(header + abi::control_header::LAYOUT_CRC32C, 0, 4);
    std::uint32_t crc = 0;
    for (const auto& r : abi::control_header::CRC_RANGES) crc = crc32c_update(crc, header + r[0], r[1] - r[0]);
    return crc;
}

/// Read and check the directory. Returns a static description of the first problem, or
/// nullptr when `out` is valid.
[[nodiscard]] inline const char* read_directory(const std::byte* ctl, std::size_t ctl_len, std::uint64_t data_len_actual, Directory& out) noexcept {
    namespace h = abi::acc::control_header;
    namespace rd = abi::acc::ring_dir_entry;
    namespace sd = abi::acc::slab_dir_entry;
    namespace dm = abi::acc::data_map;
    if (ctl_len < abi::CTL_HEADER_LEN) return "control segment shorter than its header";
    if (h::magic(ctl) != abi::CTL_MAGIC) return "bad control magic";
    if (h::abi_major(ctl) != abi::ABI_MAJOR) return "ABI major differs";
    if (h::abi_minor(ctl) < abi::ABI_MINOR_MIN) return "ABI minor too old";
    if (h::layout_crc32c(ctl) != layout_crc(ctl)) return "layout checksum";
    const std::uint64_t data_len = h::data_len(ctl);
    if (data_len != data_len_actual) return "data segment size";
    if (h::ctl_len(ctl) != ctl_len) return "control segment size";
    auto within = [&](std::uint64_t off, std::uint64_t len) { return off <= data_len && len <= data_len - off && off % 64 == 0; };
    const std::uint32_t nrings = h::ring_count(ctl);
    const std::uint32_t nclasses = h::slab_class_count(ctl);
    if (nrings == 0 || nrings > abi::MAX_RINGS || nclasses == 0 || nclasses > abi::MAX_SLAB_CLASSES) return "ring or slab class count";
    Directory d{};
    for (std::uint32_t i = 0; i < nrings; ++i) {
        const std::byte* b = ctl + abi::control_header::ring_dir::OFF + i * abi::control_header::ring_dir::STRIDE;
        const std::uint64_t off = rd::ring_off(b);
        const std::uint64_t cap = rd::capacity(b);
        if (rd::kind(b) != abi::ring_kind::MPSC || rd::cell_stride(b) != abi::CELL_LEN || cap == 0 || (cap & (cap - 1)) != 0 ||
            cap > (1ULL << 30) || !within(off, abi::RING_HEADER_LEN + cap * abi::CELL_LEN))
            return "ring directory";
        d.rings[i] = {off, cap};
    }
    for (std::uint32_t i = 0; i < nclasses; ++i) {
        const std::byte* b = ctl + abi::control_header::slab_dir::OFF + i * abi::control_header::slab_dir::STRIDE;
        SlabGeo c{sd::table_off(b), sd::data_off(b), sd::slot_size(b), sd::slot_count(b), sd::req_cap(b), sd::resp_cap(b)};
        if (sd::record_stride(b) != abi::SLOT_RECORD_LEN || static_cast<std::uint64_t>(c.req_cap) + c.resp_cap > c.slot_size || c.slot_count == 0 ||
            c.req_cap % 64 != 0 || !within(c.table_off, static_cast<std::uint64_t>(c.slot_count) * abi::SLOT_RECORD_LEN) ||
            !within(c.data_off, static_cast<std::uint64_t>(c.slot_count) * c.slot_size) || c.slot_size % 64 != 0)
            return "slab directory";
        d.slabs[i] = c;
    }
    const std::byte* m = ctl + abi::control_header::data_map::OFF;
    const std::uint64_t status_off = dm::status_off(m);
    if (!within(status_off, abi::STATUS_PAGE_LEN)) return "status page";
    d.arena_id = h::arena_id(ctl);
    d.data_len = data_len;
    d.status_off = status_off;
    d.ring_count = nrings;
    d.class_count = nclasses;
    d.hb_interval_ns = h::hb_interval_ns(ctl);
    d.hb_stale_ns = h::hb_stale_ns(ctl);
    d.spin_ns = h::spin_ns(ctl);
    d.default_deadline_ns = h::default_deadline_ns(ctl);
    d.max_deadline_ns = h::max_deadline_ns(ctl);
    out = d;
    return nullptr;
}

/// One slot: its 64-byte record and its two payload regions.
struct SlotRef {
    std::byte* rec = nullptr;
    std::byte* req = nullptr;
    std::byte* resp = nullptr;
    std::uint32_t req_cap = 0;
    std::uint32_t resp_cap = 0;
};

[[nodiscard]] inline SlotRef slot_at(std::byte* data, const Directory& d, std::uint32_t cls, std::uint32_t idx) noexcept {
    const SlabGeo& c = d.slabs[cls];
    std::byte* region = data + c.data_off + static_cast<std::uint64_t>(idx) * c.slot_size;
    return {data + c.table_off + static_cast<std::uint64_t>(idx) * abi::SLOT_RECORD_LEN, region, region + c.req_cap, c.req_cap, c.resp_cap};
}

/// One ring: its header, and the cell a position maps to.
struct RingRef {
    std::byte* hdr = nullptr;
    std::uint64_t capacity = 0;
    [[nodiscard]] std::byte* cell(std::uint64_t pos) const noexcept { return hdr + abi::ring_header::CELLS_OFF + (pos & (capacity - 1)) * abi::CELL_LEN; }
};

[[nodiscard]] inline RingRef ring_at(std::byte* data, const Directory& d, std::uint32_t i) noexcept { return {data + d.rings[i].off, d.rings[i].capacity}; }

}  // namespace scz::inline v1::detail
