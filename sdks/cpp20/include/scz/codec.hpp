// Codecs: how an input becomes a request and a response becomes an output (sdk-cpp20 §2).
//
// A codec either carries opaque bytes (OpaqueCodec: the echo and chaos workers) or declares
// the model's columns and writes and reads Arrow frames laid out by the registry's templates
// (FrameCodec, arrow-data-plane §7). The client binds a frame codec only to a registry with the
// same schema fingerprint, and an opaque one only to an empty registry (§9, strict binding).
//
// Codecs report failure by return value: the library throws nothing. A `false` releases the
// slot and resolves the call to the fallback with CODEC_ERROR. Every view is bounds-checked
// against the region, and no reference to slot memory outlives the encode or decode call.
#pragma once
#include <array>
#include <concepts>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <span>
#include <type_traits>

#include "detail/registry.hpp"
#include "detail/shared.hpp"

namespace scz::inline v1 {

/// Writes opaque bytes into a slot's request region.
class SlotWriter {
public:
    SlotWriter(std::byte* base, std::size_t cap) noexcept : base_(base), cap_(cap) {}
    [[nodiscard]] std::size_t capacity() const noexcept { return cap_; }
    /// One past the highest byte written.
    [[nodiscard]] std::size_t size() const noexcept { return len_; }
    [[nodiscard]] bool write_at(std::size_t at, const void* src, std::size_t n) noexcept {
        if (at > cap_ || n > cap_ - at) return false;
        detail::copy_in(base_ + at, src, n);
        if (at + n > len_) len_ = at + n;
        return true;
    }
    /// Append after the highest byte written so far.
    [[nodiscard]] bool write(const void* src, std::size_t n) noexcept { return write_at(len_, src, n); }
    [[nodiscard]] bool write(std::span<const std::byte> b) noexcept { return write(b.data(), b.size()); }

private:
    std::byte* base_;
    std::size_t cap_;
    std::size_t len_ = 0;
};

/// Reads opaque bytes from a response region whose length was read once and checked.
class SlotReader {
public:
    SlotReader(const std::byte* base, std::size_t len) noexcept : base_(base), len_(len) {}
    [[nodiscard]] std::size_t size() const noexcept { return len_; }
    [[nodiscard]] bool copy_to(std::size_t at, void* dst, std::size_t n) const noexcept {
        if (at > len_ || n > len_ - at) return false;
        detail::copy_out(dst, base_ + at, n);
        return true;
    }
    [[nodiscard]] bool read_u64(std::size_t at, std::uint64_t& v) const noexcept { return copy_to(at, &v, 8); }

private:
    const std::byte* base_;
    std::size_t len_;
};

/// The request frame's columns, writable (arrow-data-plane §7.1). The frame's metadata is
/// already in place; each write is checked against the column's type and the row count.
class FrameWriter {
public:
    FrameWriter(std::byte* region, const detail::Template& t, std::uint32_t rows) noexcept : region_(region), t_(&t), rows_(rows) {}
    [[nodiscard]] std::uint32_t rows() const noexcept { return rows_; }
    /// Column `col`'s values for every row, row-major: rows × list_size floats.
    [[nodiscard]] bool write_f32(std::size_t col, std::span<const float> v) noexcept {
        const detail::ColumnLoc* c = f32_column(col);
        if (c == nullptr || v.size() != static_cast<std::size_t>(rows_) * c->list_size) return false;
        detail::copy_in(region_ + c->values_off, v.data(), v.size_bytes());
        return true;
    }
    /// The values of row `row` alone.
    [[nodiscard]] bool write_f32_row(std::size_t col, std::uint32_t row, std::span<const float> v) noexcept {
        const detail::ColumnLoc* c = f32_column(col);
        if (c == nullptr || v.size() != c->list_size || row >= rows_) return false;
        detail::copy_in(region_ + c->values_off + static_cast<std::size_t>(row) * c->row_bytes, v.data(), v.size_bytes());
        return true;
    }
    /// Raw bytes for every row, for any fixed-width type.
    [[nodiscard]] bool write_bytes(std::size_t col, std::span<const std::byte> b) noexcept {
        if (col >= t_->column_count) return false;
        const detail::ColumnLoc& c = t_->columns[col];
        if (b.size() != static_cast<std::size_t>(rows_) * c.row_bytes) return false;
        detail::copy_in(region_ + c.values_off, b.data(), b.size());
        return true;
    }

private:
    [[nodiscard]] const detail::ColumnLoc* f32_column(std::size_t col) const noexcept {
        if (col >= t_->column_count || t_->columns[col].elem != abi::elem_type::F32) return nullptr;
        return &t_->columns[col];
    }
    std::byte* region_;
    const detail::Template* t_;
    std::uint32_t rows_;
};

/// The response frame's columns (arrow-data-plane §7.3), valid only inside `decode`. The
/// frame's stamp, length, and row count were checked before decode is called.
class FrameReader {
public:
    FrameReader(const std::byte* region, const detail::Template& t, std::uint32_t rows) noexcept : region_(region), t_(&t), rows_(rows) {}
    [[nodiscard]] std::uint32_t rows() const noexcept { return rows_; }
    [[nodiscard]] bool read_f32(std::size_t col, std::span<float> out) const noexcept {
        const detail::ColumnLoc* c = f32_column(col);
        if (c == nullptr || out.size() != static_cast<std::size_t>(rows_) * c->list_size) return false;
        detail::copy_out(out.data(), region_ + c->values_off, out.size_bytes());
        return true;
    }
    /// One f32 value: element `i` of column `col` (row-major), read once.
    [[nodiscard]] bool f32(std::size_t col, std::size_t i, float& out) const noexcept {
        const detail::ColumnLoc* c = f32_column(col);
        if (c == nullptr || i >= static_cast<std::size_t>(rows_) * c->list_size) return false;
        const std::uint32_t bits = detail::fetch_once<std::uint32_t>(region_ + c->values_off + i * 4);
        std::memcpy(&out, &bits, 4);
        return true;
    }
    [[nodiscard]] bool read_bytes(std::size_t col, std::span<std::byte> out) const noexcept {
        if (col >= t_->column_count) return false;
        const detail::ColumnLoc& c = t_->columns[col];
        if (out.size() != static_cast<std::size_t>(rows_) * c.row_bytes) return false;
        detail::copy_out(out.data(), region_ + c.values_off, out.size());
        return true;
    }

private:
    [[nodiscard]] const detail::ColumnLoc* f32_column(std::size_t col) const noexcept {
        if (col >= t_->column_count || t_->columns[col].elem != abi::elem_type::F32) return nullptr;
        return &t_->columns[col];
    }
    const std::byte* region_;
    const detail::Template* t_;
    std::uint32_t rows_;
};

/// Opaque bytes: `request_len` picks the smallest slot class whose request region holds them.
template <class C>
concept OpaqueCodec = requires(const C& c, const typename C::input_type& in, typename C::output_type& out, SlotWriter& w, const SlotReader& r) {
    { c.request_len(in) } noexcept -> std::convertible_to<std::size_t>;
    { c.encode(in, w) } noexcept -> std::same_as<bool>;  // false: this input cannot be encoded
    { c.decode(r, out) } noexcept -> std::same_as<bool>;  // false: the response cannot be decoded
};

/// Arrow frames: `schema` declares the columns (the fingerprint is computed from it), `rows`
/// picks the smallest class whose template holds them.
template <class C>
concept FrameCodec = requires(const C& c, const typename C::input_type& in, typename C::output_type& out, FrameWriter& w, const FrameReader& r) {
    { c.schema() } noexcept -> std::same_as<Schema>;
    { c.rows(in) } noexcept -> std::convertible_to<std::uint32_t>;
    { c.encode(in, w) } noexcept -> std::same_as<bool>;
    { c.decode(r, out) } noexcept -> std::same_as<bool>;
};

template <class C>
concept Codec = (OpaqueCodec<C> != FrameCodec<C>) && std::default_initializable<typename C::output_type> && std::is_nothrow_move_constructible_v<C> &&
                std::is_nothrow_move_constructible_v<typename C::output_type>;

/// The codec's schema fingerprint (arrow-data-plane §9); 0 for opaque bytes, which bind only
/// to an empty registry.
template <Codec C>
[[nodiscard]] std::uint64_t schema_fingerprint(const C& c) noexcept {
    if constexpr (FrameCodec<C>) return fingerprint(c.schema());
    else return 0;
}

// ── prebuilt codecs ──────────────────────────────────────────────────────────────────────
namespace codecs {

/// One row of N float features in, one float score out: a model with input
/// `features: float32[rows, N]` and output `score: float32[rows, 1]`.
template <std::size_t N>
struct DenseF32ToScore {
    using input_type = std::array<float, N>;
    using output_type = float;
    std::array<Column, 1> request{{{"features", Elem::F32, static_cast<std::uint32_t>(N), false}}};
    std::array<Column, 1> response{{{"score", Elem::F32, 1, false}}};
    [[nodiscard]] Schema schema() const noexcept { return {request.data(), request.size(), response.data(), response.size()}; }
    [[nodiscard]] std::uint32_t rows(const input_type&) const noexcept { return 1; }
    [[nodiscard]] bool encode(const input_type& in, FrameWriter& w) const noexcept { return w.write_f32(0, in); }
    [[nodiscard]] bool decode(const FrameReader& r, output_type& out) const noexcept { return r.f32(0, 0, out); }
};

/// Scores for up to MaxRows rows, by value.
template <std::size_t MaxRows>
struct Scores {
    std::array<float, MaxRows> v{};
    std::uint32_t n = 0;
};

/// Any number of rows (up to MaxRows) of N features in one call, a score per row out: the
/// batch slot class. Input: a span of rows.
template <std::size_t N, std::size_t MaxRows>
struct DenseF32Batch {
    using input_type = std::span<const std::array<float, N>>;
    using output_type = Scores<MaxRows>;
    std::array<Column, 1> request{{{"features", Elem::F32, static_cast<std::uint32_t>(N), false}}};
    std::array<Column, 1> response{{{"score", Elem::F32, 1, false}}};
    [[nodiscard]] Schema schema() const noexcept { return {request.data(), request.size(), response.data(), response.size()}; }
    [[nodiscard]] std::uint32_t rows(const input_type& in) const noexcept { return static_cast<std::uint32_t>(in.size()); }
    [[nodiscard]] bool encode(const input_type& in, FrameWriter& w) const noexcept {
        if (in.size() > MaxRows || in.size() != w.rows()) return false;
        for (std::size_t r = 0; r < in.size(); ++r)
            if (!w.write_f32_row(0, static_cast<std::uint32_t>(r), in[r])) return false;
        return true;
    }
    [[nodiscard]] bool decode(const FrameReader& r, output_type& out) const noexcept {
        const std::uint32_t rows = r.rows();
        if (rows > MaxRows) return false;
        out.n = rows;
        return r.read_f32(0, std::span<float>(out.v.data(), rows));
    }
};

/// N floats in, N floats out, as opaque bytes: what the echo worker answers.
template <std::size_t N>
struct F32Echo {
    using input_type = std::array<float, N>;
    using output_type = std::array<float, N>;
    [[nodiscard]] std::size_t request_len(const input_type&) const noexcept { return N * 4; }
    [[nodiscard]] bool encode(const input_type& in, SlotWriter& w) const noexcept { return w.write(in.data(), N * 4); }
    [[nodiscard]] bool decode(const SlotReader& r, output_type& out) const noexcept { return r.size() == N * 4 && r.copy_to(0, out.data(), N * 4); }
};

}  // namespace codecs
}  // namespace scz::inline v1
