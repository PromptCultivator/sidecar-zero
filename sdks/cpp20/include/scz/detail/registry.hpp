// The registry (arrow-data-plane §4) as a client reads it: copied out of the control segment
// under the model seqlock (shm-abi §3), parsed and checked like scz-shm's `registry::parse`,
// and used on the fast path to stamp request frames and check response frames (§7). Also the
// schema fingerprint (§9). Parsing runs on the background thread only.
#pragma once
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <new>
#include <string_view>

#include "hash.hpp"
#include "platform.hpp"
#include "shared.hpp"

namespace scz::inline v1 {

/// The physical type of a column's values (abi `elem_type`).
enum class Elem : std::uint8_t {
    I8 = abi::elem_type::I8,
    I16 = abi::elem_type::I16,
    I32 = abi::elem_type::I32,
    I64 = abi::elem_type::I64,
    U8 = abi::elem_type::U8,
    U16 = abi::elem_type::U16,
    U32 = abi::elem_type::U32,
    U64 = abi::elem_type::U64,
    F16 = abi::elem_type::F16,
    F32 = abi::elem_type::F32,
    F64 = abi::elem_type::F64,
    FixedBinary = abi::elem_type::FIXED_BINARY,
};

/// One column as the fingerprint sees it: name, element type, list size (1 for a scalar, N
/// for FixedSizeList<T>[N], the byte width for FIXED_BINARY), nullability.
struct Column {
    std::string_view name;
    Elem elem = Elem::F32;
    std::uint32_t list_size = 1;
    bool nullable = false;
};

/// A codec's request and response columns.
struct Schema {
    const Column* request = nullptr;
    std::size_t request_count = 0;
    const Column* response = nullptr;
    std::size_t response_count = 0;
};

namespace detail {

[[nodiscard]] inline std::uint32_t elem_width(std::uint8_t e) noexcept {
    namespace et = abi::elem_type;
    switch (e) {
    case et::I8: case et::U8: case et::FIXED_BINARY: return 1;
    case et::I16: case et::U16: case et::F16: return 2;
    case et::I32: case et::U32: case et::F32: return 4;
    case et::I64: case et::U64: case et::F64: return 8;
    default: return 0;
    }
}

/// Serializes the canonical schema bytes of arrow-data-plane §9 into a caller's buffer:
/// "SCZSCHEMA" 0x01, then per direction a u16 column count and per column u16 name length,
/// name, u8 element type, u32 list size, u8 nullable. Returns the length, or 0 if it does not fit.
class SchemaBytes {
public:
    SchemaBytes(std::uint8_t* buf, std::size_t cap) noexcept : b_(buf), cap_(cap) { put("SCZSCHEMA\x01", 10); }
    void count(std::size_t n) noexcept { u16(static_cast<std::uint16_t>(n)); }
    void column(const void* name, std::size_t name_len, std::uint8_t elem, std::uint32_t list_size, bool nullable) noexcept {
        u16(static_cast<std::uint16_t>(name_len));
        put(name, name_len);
        put(&elem, 1);
        std::uint8_t ls[4];
        put32(ls, list_size);
        put(ls, 4);
        const std::uint8_t n = nullable ? 1 : 0;
        put(&n, 1);
    }
    [[nodiscard]] std::size_t len() const noexcept { return ok_ ? len_ : 0; }

private:
    void u16(std::uint16_t v) noexcept {
        std::uint8_t b[2];
        put16(b, v);
        put(b, 2);
    }
    void put(const void* p, std::size_t n) noexcept {
        if (!ok_ || n > cap_ - len_) {
            ok_ = false;
            return;
        }
        std::memcpy(b_ + len_, p, n);
        len_ += n;
    }
    std::uint8_t* b_;
    std::size_t cap_;
    std::size_t len_ = 0;
    bool ok_ = true;
};

}  // namespace detail

/// arrow-data-plane §9: XXH3-64, seed 0, over the canonical schema bytes. 0 if a name is too
/// long for the fingerprint buffer (no real schema is).
[[nodiscard]] inline std::uint64_t fingerprint(const Schema& s) noexcept {
    std::uint8_t buf[8192];
    detail::SchemaBytes b(buf, sizeof buf);
    b.count(s.request_count);
    for (std::size_t i = 0; i < s.request_count; ++i)
        b.column(s.request[i].name.data(), s.request[i].name.size(), static_cast<std::uint8_t>(s.request[i].elem), s.request[i].list_size, s.request[i].nullable);
    b.count(s.response_count);
    for (std::size_t i = 0; i < s.response_count; ++i)
        b.column(s.response[i].name.data(), s.response[i].name.size(), static_cast<std::uint8_t>(s.response[i].elem), s.response[i].list_size,
                 s.response[i].nullable);
    return b.len() == 0 ? 0 : detail::xxh3_64(buf, b.len());
}

namespace detail {

struct ColumnLoc {
    std::uint32_t name_off = 0;  // into the registry copy
    std::uint16_t name_len = 0;
    std::uint8_t elem = 0;
    bool nullable = false;
    std::uint32_t list_size = 0;
    std::uint32_t values_off = 0;  // from the start of the region
    std::uint32_t row_bytes = 0;
};

struct Patch {
    std::uint32_t prefix_off = 0;
    std::uint32_t scale = 0;
    std::uint8_t kind = 0;
    [[nodiscard]] std::uint64_t value(std::uint32_t rows) const noexcept {
        return kind == abi::patch_kind::BITMAP_BYTES ? (static_cast<std::uint64_t>(rows) + 7) / 8 : static_cast<std::uint64_t>(rows) * scale;
    }
};

struct Template {
    std::uint16_t id = 0;
    std::uint8_t direction = 0;
    std::uint16_t cls = 0;
    std::uint32_t max_rows = 0;
    std::uint32_t prefix_off = 0;  // into the registry copy
    std::uint32_t prefix_len = 0;
    std::uint32_t body_off = 0;
    std::uint32_t frame_cap = 0;
    std::uint32_t rows_field_off = 0;
    std::uint16_t column_count = 0;
    std::uint16_t patch_count = 0;
    ColumnLoc columns[abi::MAX_COLUMNS];
    Patch patches[abi::MAX_PATCHES];
    /// `(registry_epoch & 0xFFFF) << 16 | template_id`: what a region holds once stamped.
    [[nodiscard]] std::uint32_t stamp(std::uint32_t epoch) const noexcept { return ((epoch & 0xFFFFU) << 16) | id; }
};

/// A parsed registry, with the copy it was parsed from. Allocated by the background thread.
struct Registry {
    alignas(8) std::uint8_t bytes[abi::CTL_REGISTRY_LEN];
    std::uint32_t len = 0;
    std::uint32_t template_count = 0;
    Template templates[abi::MAX_TEMPLATES];
    std::uint64_t fp = 0;  // of the schema the templates carry; 0 for an empty registry

    [[nodiscard]] const Template* find(std::uint32_t cls, std::uint32_t dir) const noexcept {
        for (std::uint32_t i = 0; i < template_count; ++i)
            if (templates[i].cls == cls && templates[i].direction == dir) return &templates[i];
        return nullptr;
    }
    /// The smallest class whose request template holds `rows` (arrow-data-plane §4); -1 if none.
    [[nodiscard]] int class_for(std::uint32_t rows) const noexcept {
        const Template* best = nullptr;
        for (std::uint32_t i = 0; i < template_count; ++i) {
            const Template& t = templates[i];
            if (t.direction == abi::direction::REQUEST && t.max_rows >= rows && (best == nullptr || t.max_rows < best->max_rows)) best = &t;
        }
        return best == nullptr ? -1 : best->cls;
    }
    [[nodiscard]] const std::uint8_t* prefix(const Template& t) const noexcept { return bytes + t.prefix_off; }
};

/// Bounds-checked little-endian reads over the registry copy.
class Reader {
public:
    Reader(const std::uint8_t* b, std::size_t n) noexcept : b_(b), n_(n) {}
    bool u8(std::size_t at, std::uint8_t& v) const noexcept { return get(at, 1, &v); }
    bool u16(std::size_t at, std::uint16_t& v) const noexcept { return get(at, 2, &v); }
    bool u32(std::size_t at, std::uint32_t& v) const noexcept { return get(at, 4, &v); }
    bool u64(std::size_t at, std::uint64_t& v) const noexcept { return get(at, 8, &v); }
    [[nodiscard]] bool span(std::uint64_t off, std::uint64_t len) const noexcept { return off <= n_ && len <= n_ - off; }
    /// `count` entries of `stride` bytes at `off`, checked to fit and to be aligned.
    [[nodiscard]] bool array(std::uint32_t off, std::size_t count, std::size_t stride, std::size_t min, std::size_t align) const noexcept {
        if (stride < min || stride % (align < 4 ? align : 4) != 0 || off % align != 0) return false;
        return span(off, static_cast<std::uint64_t>(count) * stride);
    }

private:
    bool get(std::size_t at, std::size_t w, void* v) const noexcept {
        if (at > n_ || w > n_ - at) return false;
        std::memcpy(v, b_ + at, w);
        return true;
    }
    const std::uint8_t* b_;
    std::size_t n_;
};

/// Parse `reg.bytes[0..reg.len]` (arrow-data-plane §8, "the fast path has nothing to trust"):
/// every offset, length, and patch position checked against the registry, the prefix, and the
/// slot class's region. `caps` holds each class's (req_cap, resp_cap). Returns nullptr on
/// success, else what was wrong. An empty registry parses to no templates.
[[nodiscard]] inline const char* parse(Registry& reg, const std::uint32_t (*caps)[2], std::uint32_t ncaps) noexcept {
    namespace rh = abi::registry_header;
    namespace td = abi::template_desc;
    namespace cd = abi::column_desc;
    namespace pd = abi::patch_desc;
    reg.template_count = 0;
    reg.fp = 0;
    if (reg.len == 0) return nullptr;
    const Reader r(reg.bytes, reg.len);
    std::uint64_t magic = 0;
    std::uint32_t len = 0;
    if (!r.u64(rh::MAGIC, magic) || magic != abi::REGISTRY_MAGIC) return "bad registry magic";
    if (!r.u32(rh::REGISTRY_LEN, len) || len != reg.len || len > abi::CTL_REGISTRY_LEN) return "registry_len";
    std::uint16_t nt = 0, ts = 0, cs = 0, ps = 0;
    std::uint32_t nc = 0, np = 0, toff = 0, coff = 0, poff = 0, names_off = 0, names_len = 0;
    std::uint32_t in_off = 0, in_len = 0, out_off = 0, out_len = 0;
    if (!r.u16(rh::TEMPLATE_COUNT, nt) || !r.u16(rh::TEMPLATE_STRIDE, ts) || !r.u16(rh::COLUMN_STRIDE, cs) || !r.u16(rh::PATCH_STRIDE, ps) ||
        !r.u32(rh::COLUMN_TOTAL, nc) || !r.u32(rh::PATCH_TOTAL, np) || !r.u32(rh::TEMPLATES_OFF, toff) || !r.u32(rh::COLUMNS_OFF, coff) ||
        !r.u32(rh::PATCHES_OFF, poff) || !r.u32(rh::NAMES_OFF, names_off) || !r.u32(rh::NAMES_LEN, names_len) || !r.u32(rh::IN_SCHEMA_OFF, in_off) ||
        !r.u32(rh::IN_SCHEMA_LEN, in_len) || !r.u32(rh::OUT_SCHEMA_OFF, out_off) || !r.u32(rh::OUT_SCHEMA_LEN, out_len))
        return "registry header";
    if (nt == 0 || nt > abi::MAX_TEMPLATES || nc > nt * abi::MAX_COLUMNS || np > nt * abi::MAX_PATCHES) return "template, column, or patch count";
    if (!r.array(toff, nt, ts, td::SIZE, 8) || !r.array(coff, nc, cs, cd::SIZE, 4) || !r.array(poff, np, ps, pd::SIZE, 4)) return "descriptor arrays";
    if (!r.span(names_off, names_len) || !r.span(in_off, in_len) || !r.span(out_off, out_len)) return "names or schema messages";
    for (std::uint32_t k = 0; k < nt; ++k) {
        const std::size_t at = toff + static_cast<std::size_t>(k) * ts;
        Template& t = reg.templates[k];
        t = Template{};
        std::uint8_t dir = 0;
        std::uint32_t first_col = 0, first_patch = 0;
        std::uint16_t ncol = 0, npat = 0;
        if (!r.u16(at + td::TEMPLATE_ID, t.id) || !r.u8(at + td::DIRECTION, dir) || !r.u16(at + td::SLAB_CLASS, t.cls) ||
            !r.u32(at + td::MAX_ROWS, t.max_rows) || !r.u32(at + td::PREFIX_OFF, t.prefix_off) || !r.u32(at + td::PREFIX_LEN, t.prefix_len) ||
            !r.u32(at + td::BODY_OFF, t.body_off) || !r.u32(at + td::FRAME_CAP, t.frame_cap) || !r.u32(at + td::ROWS_FIELD_OFF, t.rows_field_off) ||
            !r.u32(at + td::FIRST_COLUMN, first_col) || !r.u16(at + td::COLUMN_COUNT, ncol) || !r.u32(at + td::FIRST_PATCH, first_patch) ||
            !r.u16(at + td::PATCH_COUNT, npat))
            return "template descriptor";
        t.direction = dir;
        if (t.id == 0) return "template id 0";
        for (std::uint32_t j = 0; j < k; ++j)
            if (reg.templates[j].id == t.id) return "repeated template id";
        if (t.cls >= ncaps) return "template for a slot class that does not exist";
        std::uint32_t cap = 0;
        if (dir == abi::direction::REQUEST) cap = caps[t.cls][0];
        else if (dir == abi::direction::RESPONSE) cap = caps[t.cls][1];
        else return "template direction";
        for (std::uint32_t j = 0; j < k; ++j)
            if (reg.templates[j].cls == t.cls && reg.templates[j].direction == dir) return "a second template for a class and direction";
        if (!r.span(t.prefix_off, t.prefix_len)) return "template prefix outside the registry";
        if (t.max_rows == 0 || t.prefix_len < 8 || t.body_off != t.prefix_len || t.body_off % 8 != 0 || t.frame_cap < t.body_off || t.frame_cap > cap)
            return "template frame geometry";
        auto in_prefix = [&](std::uint32_t off) { return off % 8 == 0 && static_cast<std::uint64_t>(off) + 8 <= t.prefix_len; };
        if (!in_prefix(t.rows_field_off)) return "row count field outside the prefix";
        if (static_cast<std::uint64_t>(first_col) + ncol > nc || static_cast<std::uint64_t>(first_patch) + npat > np || ncol > abi::MAX_COLUMNS ||
            npat > abi::MAX_PATCHES)
            return "column or patch range";
        t.column_count = ncol;
        t.patch_count = npat;
        for (std::uint32_t i = 0; i < ncol; ++i) {
            const std::size_t c = coff + static_cast<std::size_t>(first_col + i) * cs;
            ColumnLoc& col = t.columns[i];
            std::uint32_t validity = 0, null_count = 0;
            std::uint8_t nullable = 0;
            if (!r.u32(c + cd::NAME_OFF, col.name_off) || !r.u16(c + cd::NAME_LEN, col.name_len) || !r.u8(c + cd::ELEM_TYPE, col.elem) ||
                !r.u32(c + cd::LIST_SIZE, col.list_size) || !r.u8(c + cd::NULLABLE, nullable) || !r.u32(c + cd::VALUES_OFF, col.values_off) ||
                !r.u32(c + cd::ROW_BYTES, col.row_bytes) || !r.u32(c + cd::VALIDITY_OFF, validity) || !r.u32(c + cd::NULL_COUNT_OFF, null_count))
                return "column descriptor";
            if (static_cast<std::uint64_t>(col.name_off) + col.name_len > names_len) return "a column name outside the names";
            col.name_off += names_off;
            col.nullable = nullable != 0;
            if (col.nullable || validity != 0 || null_count != 0) return "a nullable column, which version 1 does not carry";
            const std::uint64_t w = elem_width(col.elem);
            if (col.list_size == 0 || w == 0 || w * col.list_size != col.row_bytes || col.row_bytes == 0) return "column row bytes";
            const std::uint64_t end = col.values_off + static_cast<std::uint64_t>(t.max_rows) * col.row_bytes;
            if (col.values_off < t.body_off || col.values_off % 8 != 0 || end > t.frame_cap) return "column values outside the frame";
        }
        bool rows_patched = false;
        for (std::uint32_t i = 0; i < npat; ++i) {
            const std::size_t p = poff + static_cast<std::size_t>(first_patch + i) * ps;
            Patch& pt = t.patches[i];
            if (!r.u32(p + pd::PREFIX_OFF, pt.prefix_off) || !r.u32(p + pd::SCALE, pt.scale) || !r.u8(p + pd::KIND, pt.kind)) return "patch descriptor";
            const bool ok_kind = (pt.kind == abi::patch_kind::ROWS_SCALED && pt.scale > 0) || pt.kind == abi::patch_kind::BITMAP_BYTES;
            if (!ok_kind || !in_prefix(pt.prefix_off) || pt.prefix_off < 8) return "patch outside the prefix";
            if (pt.prefix_off == t.rows_field_off && pt.kind == abi::patch_kind::ROWS_SCALED && pt.scale == 1) rows_patched = true;
        }
        if (!rows_patched) return "no patch rewrites the row count";
    }
    // Every class has both templates or neither, with the same row capacity, and every
    // template of a direction carries the same columns.
    const Template* req0 = nullptr;
    const Template* resp0 = nullptr;
    auto same_columns = [&](const Template& a, const Template& b) {
        if (a.column_count != b.column_count) return false;
        for (std::uint32_t i = 0; i < a.column_count; ++i) {
            const ColumnLoc& x = a.columns[i];
            const ColumnLoc& y = b.columns[i];
            if (x.elem != y.elem || x.list_size != y.list_size || x.nullable != y.nullable || x.name_len != y.name_len ||
                std::memcmp(reg.bytes + x.name_off, reg.bytes + y.name_off, x.name_len) != 0)
                return false;
        }
        return true;
    };
    for (std::uint32_t k = 0; k < nt; ++k) {
        const Template& t = reg.templates[k];
        const std::uint32_t other = t.direction == abi::direction::REQUEST ? abi::direction::RESPONSE : abi::direction::REQUEST;
        bool matched = false;
        for (std::uint32_t j = 0; j < nt; ++j)
            if (reg.templates[j].cls == t.cls && reg.templates[j].direction == other && reg.templates[j].max_rows == t.max_rows) matched = true;
        if (!matched) return "a class lacks a matching template of the other direction";
        const Template*& first = t.direction == abi::direction::REQUEST ? req0 : resp0;
        if (first == nullptr) first = &t;
        else if (!same_columns(*first, t)) return "columns differ between templates of one direction";
    }
    if (req0 == nullptr || resp0 == nullptr || req0->column_count == 0 || resp0->column_count == 0) return "a direction without columns";
    // The fingerprint of what the templates carry.
    std::uint8_t* buf = new (std::nothrow) std::uint8_t[abi::CTL_REGISTRY_LEN];
    if (buf == nullptr) return "out of memory";
    SchemaBytes b(buf, abi::CTL_REGISTRY_LEN);
    for (const Template* t : {req0, resp0}) {
        b.count(t->column_count);
        for (std::uint32_t i = 0; i < t->column_count; ++i) {
            const ColumnLoc& c = t->columns[i];
            b.column(reg.bytes + c.name_off, c.name_len, c.elem, c.list_size, c.nullable);
        }
    }
    reg.fp = b.len() == 0 ? 0 : xxh3_64(buf, b.len());
    delete[] buf;
    reg.template_count = nt;
    return nullptr;
}

/// The model block under the seqlock (shm-abi §3): the registry epoch, the schema
/// fingerprint, and a copy of the registry bytes into `reg`. False if the writer stayed busy
/// for `attempts` tries or the block names a registry outside the segment.
struct ModelBlock {
    std::uint32_t model_gen = 0;
    std::uint32_t registry_epoch = 0;
    std::uint64_t schema_fingerprint = 0;
    std::uint32_t registry_len = 0;
};

[[nodiscard]] inline bool read_model(const std::byte* ctl, std::size_t ctl_len, std::uint32_t attempts, ModelBlock& out, Registry& reg) noexcept {
    namespace h = abi::acc::control_header;
    constexpr std::uint32_t REGISTRY_OFF = static_cast<std::uint32_t>(abi::CTL_HEADER_LEN);
    for (std::uint32_t a = 0; a < attempts; ++a) {
        const std::uint64_t s1 = h::model_seq(ctl);
        if (s1 & 1) {
            cpu_relax();
            continue;
        }
        ModelBlock b{h::model_gen(ctl), h::registry_epoch(ctl), h::schema_fingerprint(ctl), h::registry_len(ctl)};
        const std::uint32_t off = h::registry_off(ctl);
        const std::size_t len = b.registry_len;
        const bool fits = len == 0 || (off == REGISTRY_OFF && len <= abi::CTL_REGISTRY_LEN && off + len <= ctl_len);
        if (fits && len > 0) {
            for (std::size_t i = 0; i < (len + 7) / 8; ++i) {
                const std::uint64_t w = fetch_once<std::uint64_t>(ctl + off + i * 8);
                const std::size_t n = (i * 8 + 8 <= len) ? 8 : len - i * 8;
                std::memcpy(reg.bytes + i * 8, &w, n);
            }
        }
        std::atomic_thread_fence(std::memory_order_acquire);
        if (fetch_once<std::uint64_t>(ctl + abi::control_header::MODEL_SEQ) == s1) {
            if (!fits) return false;
            out = b;
            reg.len = static_cast<std::uint32_t>(len);
            return true;
        }
    }
    return false;
}

/// The registry epoch alone, under the seqlock: what a request thread re-reads when the gate
/// shows a new worker generation (arrow-data-plane §9).
[[nodiscard]] inline bool read_epoch(const std::byte* ctl, std::uint32_t attempts, std::uint32_t& out) noexcept {
    namespace h = abi::acc::control_header;
    for (std::uint32_t a = 0; a < attempts; ++a) {
        const std::uint64_t s1 = h::model_seq(ctl);
        if ((s1 & 1) == 0) {
            const std::uint32_t e = h::registry_epoch(ctl);
            std::atomic_thread_fence(std::memory_order_acquire);
            if (fetch_once<std::uint64_t>(ctl + abi::control_header::MODEL_SEQ) == s1) {
                out = e;
                return true;
            }
        }
        cpu_relax();
    }
    return false;
}

/// Make the request region a frame of `t` carrying `rows` rows (arrow-data-plane §7.1): copy
/// the prefix in if the slot's request stamp is not `want`, then rewrite the length fields if
/// the row count differs from the one the frame last held.
inline void prepare(const Registry& reg, std::byte* region, std::byte* rec, const Template& t, std::uint32_t want, std::uint32_t rows) noexcept {
    namespace sr = abi::acc::slot_record;
    if (sr::req_stamp(rec) != want) {
        copy_in(region, reg.prefix(t), t.prefix_len);
        sr::set_req_stamp(rec, want);
    }
    if (fetch_once<std::uint64_t>(region + t.rows_field_off) != rows) {
        for (std::uint32_t i = 0; i < t.patch_count; ++i) store_relaxed<std::uint64_t>(region + t.patches[i].prefix_off, t.patches[i].value(rows));
    }
}

/// The row count a frame of `t` holds, read once.
[[nodiscard]] inline std::uint64_t frame_rows(const std::byte* region, const Template& t) noexcept { return fetch_once<std::uint64_t>(region + t.rows_field_off); }

}  // namespace detail
}  // namespace scz::inline v1
