//! The C++ output: `sdks/cpp20/include/scz/abi.hpp`. Owned by the C++ SDK.
//!
//! Constants, enumerations, bit fields, region offsets with `static_assert`s on their
//! placement, and one accessor per field of every shared region, with the memory ordering
//! the field's `access` key fixes (shm-abi §8; testing §2.3: "Accessors are generated, with
//! the ordering fixed"). An accessor takes a pointer to the start of its region:
//!
//! | `access` | load | store | also |
//! |---|---|---|---|
//! | `const`, `relaxed` | relaxed | relaxed | |
//! | `acqrel` | acquire | release | |
//! | `cas` | acquire | release | `cas_` (strong) and `cas_weak_`: acq_rel / acquire; `add_`: acq_rel |
//!
//! Regions of the control segment, which a client maps read-only, get loads only (sdk-cpp20
//! §3: a store there would fault). Data-segment regions get the full set. Registry, message,
//! and worker-segment regions get offsets only: the SDK reads the registry from a private
//! copy taken under the seqlock, builds messages in local buffers, and never sees a worker
//! segment.

use crate::*;
use std::fmt::Write as _;

fn cpp_ty(ty: &str) -> &'static str {
    match ty {
        "u8" => "std::uint8_t",
        "u16" => "std::uint16_t",
        "u32" => "std::uint32_t",
        "u64" => "std::uint64_t",
        t => panic!("no C++ type for {t}"),
    }
}

pub fn emit(spec: &Spec) -> String {
    let mut o = String::new();
    writeln!(o, "// {HEADER}").unwrap();
    writeln!(o, "//").unwrap();
    writeln!(o, "// Offsets are bytes from the start of the region. Accessors live in scz::abi::acc; each").unwrap();
    writeln!(o, "// takes a pointer to the start of its region and uses the memory ordering the ABI file").unwrap();
    writeln!(o, "// assigns to its field (const/relaxed: relaxed; acqrel and cas: acquire loads, release").unwrap();
    writeln!(o, "// stores, acq_rel read-modify-writes). Control-segment accessors only load.").unwrap();
    writeln!(o, "#pragma once\n#include <atomic>\n#include <bit>\n#include <cstddef>\n#include <cstdint>\n").unwrap();
    writeln!(o, "static_assert(std::endian::native == std::endian::little, \"the arena is little-endian\");").unwrap();
    writeln!(o, "static_assert(std::atomic_ref<std::uint64_t>::is_always_lock_free && std::atomic_ref<std::uint32_t>::is_always_lock_free);\n").unwrap();
    writeln!(o, "// Everything lives in the SDK's versioned inline namespace (sdk-cpp20 §1).\nnamespace scz::inline v1::abi {{").unwrap();
    writeln!(
        o,
        "inline constexpr std::uint16_t ABI_MAJOR = {};\ninline constexpr std::uint16_t ABI_MINOR = {};\n// The oldest minor this SDK accepts (shm-abi §8).\ninline constexpr std::uint16_t ABI_MINOR_MIN = {};",
        spec.major, spec.minor, spec.min_minor
    )
    .unwrap();
    for (k, v) in &spec.constants {
        writeln!(o, "inline constexpr std::uint64_t {k} = {v:#x}ULL;").unwrap();
    }
    writeln!(o, "\nnamespace defaults {{").unwrap();
    for (k, v) in &spec.defaults {
        writeln!(o, "inline constexpr std::uint64_t {} = {v}ULL;", upper(k)).unwrap();
    }
    writeln!(o, "}}  // namespace defaults").unwrap();
    for (name, vals) in &spec.enums {
        writeln!(o, "\nnamespace {name} {{").unwrap();
        for (k, v) in vals {
            writeln!(o, "inline constexpr std::uint32_t {k} = {v:#x};").unwrap();
        }
        writeln!(o, "[[nodiscard]] constexpr const char* name(std::uint32_t v) noexcept {{\n    switch (v) {{").unwrap();
        for (k, v) in vals {
            writeln!(o, "    case {v:#x}: return \"{k}\";").unwrap();
        }
        writeln!(o, "    default: return \"?\";\n    }}\n}}\n}}  // namespace {name}").unwrap();
    }
    for (name, parts) in &spec.bitfields {
        writeln!(o, "\nnamespace {name} {{").unwrap();
        for (k, shift, bits) in parts {
            let mask: u64 = if *bits == 64 { u64::MAX } else { ((1u64 << bits) - 1) << shift };
            writeln!(o, "inline constexpr unsigned {}_SHIFT = {shift};", upper(k)).unwrap();
            writeln!(o, "inline constexpr std::uint64_t {}_MASK = {mask:#018x}ULL;", upper(k)).unwrap();
        }
        writeln!(o, "}}  // namespace {name}").unwrap();
    }
    for r in &spec.regions {
        writeln!(o, "\n// Region `{}` (segment: {}).", r.name, r.segment).unwrap();
        writeln!(o, "namespace {} {{\ninline constexpr std::size_t SIZE = {:#x};", r.name, r.size).unwrap();
        for f in &r.fields {
            let note = f.note.as_ref().map(|n| format!("; {n}")).unwrap_or_default();
            writeln!(o, "inline constexpr std::size_t {} = {:#x};  // {} {}, written by {}{}", upper(&f.name), f.off, f.ty, f.access, f.writer, note).unwrap();
        }
        for (k, v) in &r.extra {
            writeln!(o, "inline constexpr std::size_t {} = {v:#x};", upper(k)).unwrap();
        }
        for a in &r.arrays {
            writeln!(
                o,
                "namespace {} {{ inline constexpr std::size_t OFF = {:#x}; inline constexpr std::size_t COUNT = {}; inline constexpr std::size_t STRIDE = {:#x}; }}",
                a.name, a.off, a.count, a.stride
            )
            .unwrap();
        }
        if !r.crc_ranges.is_empty() {
            let rs: Vec<String> = r.crc_ranges.iter().map(|(a, b)| format!("{{{a:#x}, {b:#x}}}")).collect();
            writeln!(o, "inline constexpr std::size_t CRC_RANGE_COUNT = {};", r.crc_ranges.len()).unwrap();
            writeln!(o, "inline constexpr std::size_t CRC_RANGES[{}][2] = {{{}}};", r.crc_ranges.len(), rs.join(", ")).unwrap();
        }
        // Drift from the ABI file is a compile error: every field naturally aligned, inside
        // its region.
        for f in &r.fields {
            let w = width(&f.ty);
            let align = if f.ty.starts_with("bytes") { 8 } else { w };
            writeln!(o, "static_assert({c} % {align} == 0 && {c} + {w} <= SIZE, \"{}.{}\");", r.name, f.name, c = upper(&f.name)).unwrap();
        }
        writeln!(o, "}}  // namespace {}", r.name).unwrap();
    }
    writeln!(o, "}}  // namespace scz::inline v1::abi\n").unwrap();

    // ── accessors ──
    writeln!(o, "namespace scz::inline v1::abi::acc {{").unwrap();
    writeln!(o, "namespace detail {{").unwrap();
    writeln!(o, "template <class T>\n[[nodiscard]] inline T load(const std::byte* p, std::memory_order o) noexcept {{").unwrap();
    writeln!(o, "    return std::atomic_ref<T>(*reinterpret_cast<T*>(const_cast<std::byte*>(p))).load(o);\n}}").unwrap();
    writeln!(o, "template <class T>\ninline void store(std::byte* p, T v, std::memory_order o) noexcept {{ std::atomic_ref<T>(*reinterpret_cast<T*>(p)).store(v, o); }}").unwrap();
    writeln!(o, "template <class T>\n[[nodiscard]] inline bool cas(std::byte* p, T& expected, T desired) noexcept {{").unwrap();
    writeln!(o, "    return std::atomic_ref<T>(*reinterpret_cast<T*>(p)).compare_exchange_strong(expected, desired, std::memory_order_acq_rel, std::memory_order_acquire);\n}}").unwrap();
    writeln!(o, "template <class T>\n[[nodiscard]] inline bool cas_weak(std::byte* p, T& expected, T desired) noexcept {{").unwrap();
    writeln!(o, "    return std::atomic_ref<T>(*reinterpret_cast<T*>(p)).compare_exchange_weak(expected, desired, std::memory_order_acq_rel, std::memory_order_acquire);\n}}").unwrap();
    writeln!(o, "template <class T>\ninline T add(std::byte* p, T v) noexcept {{ return std::atomic_ref<T>(*reinterpret_cast<T*>(p)).fetch_add(v, std::memory_order_acq_rel); }}").unwrap();
    writeln!(o, "}}  // namespace detail").unwrap();
    for r in &spec.regions {
        let writable = match r.segment.as_str() {
            "data" => true,
            "control" => false,
            _ => continue, // registry, message, worker: offsets only (see the module comment)
        };
        if r.fields.is_empty() {
            continue;
        }
        writeln!(o, "\n// Region `{}` ({}).", r.name, if writable { "data segment: read-write" } else { "control segment: mapped read-only, loads only" })
            .unwrap();
        writeln!(o, "namespace {} {{", r.name).unwrap();
        for f in &r.fields {
            let (load, store) = match f.access.as_str() {
                "acqrel" | "cas" => ("std::memory_order_acquire", "std::memory_order_release"),
                _ => ("std::memory_order_relaxed", "std::memory_order_relaxed"),
            };
            let c = format!("abi::{}::{}", r.name, upper(&f.name));
            let n = &f.name;
            writeln!(o, "// {} {}, written by {}", f.ty, f.access, f.writer).unwrap();
            if f.ty.starts_with("bytes") {
                let words = width(&f.ty) / 8;
                writeln!(
                    o,
                    "inline void {n}(const std::byte* r, std::uint64_t (&out)[{words}]) noexcept {{ for (std::size_t i = 0; i < {words}; ++i) out[i] = detail::load<std::uint64_t>(r + {c} + i * 8, {load}); }}"
                )
                .unwrap();
                if writable {
                    writeln!(
                        o,
                        "inline void set_{n}(std::byte* r, const std::uint64_t (&v)[{words}]) noexcept {{ for (std::size_t i = 0; i < {words}; ++i) detail::store<std::uint64_t>(r + {c} + i * 8, v[i], {store}); }}"
                    )
                    .unwrap();
                }
                continue;
            }
            let ty = cpp_ty(&f.ty);
            writeln!(o, "[[nodiscard]] inline {ty} {n}(const std::byte* r) noexcept {{ return detail::load<{ty}>(r + {c}, {load}); }}").unwrap();
            if writable {
                writeln!(o, "inline void set_{n}(std::byte* r, {ty} v) noexcept {{ detail::store<{ty}>(r + {c}, v, {store}); }}").unwrap();
                if f.access == "cas" {
                    writeln!(o, "[[nodiscard]] inline bool cas_{n}(std::byte* r, {ty}& expected, {ty} desired) noexcept {{ return detail::cas<{ty}>(r + {c}, expected, desired); }}").unwrap();
                    writeln!(o, "[[nodiscard]] inline bool cas_weak_{n}(std::byte* r, {ty}& expected, {ty} desired) noexcept {{ return detail::cas_weak<{ty}>(r + {c}, expected, desired); }}")
                        .unwrap();
                    writeln!(o, "inline {ty} add_{n}(std::byte* r, {ty} v) noexcept {{ return detail::add<{ty}>(r + {c}, v); }}").unwrap();
                }
            }
        }
        writeln!(o, "}}  // namespace {}", r.name).unwrap();
    }
    writeln!(o, "}}  // namespace scz::inline v1::abi::acc").unwrap();
    o
}
