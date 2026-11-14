//! scz-abi-gen: turns abi/scz_abi.toml into constants and field accessors.
//!
//!   scz-abi-gen            write the generated files
//!   scz-abi-gen --check    exit 1 if any generated file is missing or differs (for CI)
//!
//! Outputs, all marked "@generated":
//!   crates/scz-shm/src/abi.rs                         Rust constants and field accessors
//!   sdks/cpp20/include/scz/abi.hpp                    C++ (cpp.rs)
//!   sdks/go/internal/abi/abi.go                       Go (go.rs)
//!   sdks/java-ffm/src/main/java/io/sidecarzero/internal/Abi.java   Java (java.rs)
//!
//! Every language's accessors fix each field's memory ordering from its `access` key, so code
//! that uses them cannot choose a weaker one (shm-abi §8, testing §2.3). Each language's
//! emitter is its own module, owned by its SDK.

mod cpp;
mod go;
mod java;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use toml::Value;

pub(crate) struct Field {
    pub name: String,
    pub off: u64,
    pub ty: String,
    pub access: String,
    pub writer: String,
    pub note: Option<String>,
}

pub(crate) struct Array {
    pub name: String,
    pub region: String,
    pub off: u64,
    pub count: u64,
    pub stride: u64,
}

pub(crate) struct Region {
    pub name: String,
    pub segment: String,
    pub size: u64,
    pub fields: Vec<Field>,
    pub arrays: Vec<Array>,
    pub extra: Vec<(String, u64)>, // integer keys such as cells_off
    pub crc_ranges: Vec<(u64, u64)>,
}

/// A bit field's parts: (name, shift, bits).
pub(crate) type Parts = Vec<(String, u64, u64)>;

pub(crate) struct Spec {
    pub major: u64,
    pub minor: u64,
    /// The oldest minor a reader generated from this file accepts (shm-abi §8).
    pub min_minor: u64,
    pub constants: Vec<(String, u64)>,
    pub defaults: Vec<(String, u64)>,
    pub enums: Vec<(String, Vec<(String, u64)>)>,
    pub bitfields: Vec<(String, Parts)>,
    pub regions: Vec<Region>,
}

fn int(v: &Value, what: &str) -> u64 {
    match v {
        Value::Integer(i) if *i >= 0 => *i as u64,
        _ => panic!("{what}: expected a non-negative integer, found {v:?}"),
    }
}

fn s(v: &Value, what: &str) -> String {
    v.as_str().unwrap_or_else(|| panic!("{what}: expected a string")).to_string()
}

fn table<'a>(v: &'a Value, what: &str) -> &'a toml::Table {
    v.as_table().unwrap_or_else(|| panic!("{what}: expected a table"))
}

fn ints(t: &toml::Table, what: &str) -> Vec<(String, u64)> {
    t.iter().map(|(k, v)| (k.clone(), int(v, &format!("{what}.{k}")))).collect()
}

pub(crate) fn width(ty: &str) -> u64 {
    match ty {
        "u8" => 1,
        "u16" => 2,
        "u32" => 4,
        "u64" => 8,
        t if t.starts_with("bytes") => t[5..].parse().unwrap_or_else(|_| panic!("bad type {t}")),
        t => panic!("unknown type {t}"),
    }
}

fn parse(text: &str) -> Spec {
    let root: Value = text.parse::<toml::Table>().expect("scz_abi.toml does not parse").into();
    let abi = table(&root["abi"], "abi");
    let mut enums = Vec::new();
    for (name, v) in table(&root["enums"], "enums") {
        enums.push((name.clone(), ints(table(v, name), name)));
    }
    let mut bitfields = Vec::new();
    for (name, v) in table(&root["bitfields"], "bitfields") {
        let mut parts = Vec::new();
        for (k, pair) in table(v, name) {
            let a = pair.as_array().unwrap_or_else(|| panic!("bitfield {name}.{k}: expected [shift, bits]"));
            parts.push((k.clone(), int(&a[0], k), int(&a[1], k)));
        }
        bitfields.push((name.clone(), parts));
    }
    let mut regions = Vec::new();
    for (name, v) in table(&root["regions"], "regions") {
        let t = table(v, name);
        let mut fields = Vec::new();
        for f in t.get("fields").and_then(Value::as_array).cloned().unwrap_or_default() {
            let ft = table(&f, name);
            fields.push(Field {
                name: s(&ft["name"], "field.name"),
                off: int(&ft["off"], "field.off"),
                ty: s(&ft["ty"], "field.ty"),
                access: s(&ft["access"], "field.access"),
                writer: s(&ft["writer"], "field.writer"),
                note: ft.get("note").map(|n| s(n, "field.note")),
            });
        }
        let mut arrays = Vec::new();
        for a in t.get("arrays").and_then(Value::as_array).cloned().unwrap_or_default() {
            let at = table(&a, name);
            arrays.push(Array {
                name: s(&at["name"], "array.name"),
                region: s(&at["region"], "array.region"),
                off: int(&at["off"], "array.off"),
                count: int(&at["count"], "array.count"),
                stride: int(&at["stride"], "array.stride"),
            });
        }
        let mut extra = Vec::new();
        for (k, v) in t {
            if let Value::Integer(_) = v {
                if k != "size" {
                    extra.push((k.clone(), int(v, k)));
                }
            }
        }
        let crc_ranges = t
            .get("crc_ranges")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|r| {
                        let r = r.as_array().unwrap();
                        (int(&r[0], "crc"), int(&r[1], "crc"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        regions.push(Region { name: name.clone(), segment: s(&t["segment"], "segment"), size: int(&t["size"], "size"), fields, arrays, extra, crc_ranges });
    }
    let spec = Spec {
        major: int(&abi["major"], "abi.major"),
        minor: int(&abi["minor"], "abi.minor"),
        min_minor: int(&abi["min_minor"], "abi.min_minor"),
        constants: ints(table(&root["constants"], "constants"), "constants"),
        defaults: ints(table(&root["defaults"], "defaults"), "defaults"),
        enums,
        bitfields,
        regions,
    };
    validate(&spec);
    spec
}

/// Every field naturally aligned, inside its region, and not overlapping another.
fn validate(spec: &Spec) {
    for r in &spec.regions {
        let mut spans: Vec<(u64, u64, &str)> = Vec::new();
        for f in &r.fields {
            let w = width(&f.ty);
            let align = if f.ty.starts_with("bytes") { 8 } else { w };
            assert!(f.off % align == 0, "{}.{} at {:#x} is not {}-byte aligned", r.name, f.name, f.off, align);
            assert!(f.off + w <= r.size, "{}.{} runs past the end of {}", r.name, f.name, r.name);
            assert!(matches!(f.access.as_str(), "const" | "relaxed" | "acqrel" | "cas"), "{}.{}: unknown access {}", r.name, f.name, f.access);
            spans.push((f.off, f.off + w, &f.name));
        }
        for a in &r.arrays {
            let child = spec.regions.iter().find(|c| c.name == a.region).unwrap_or_else(|| panic!("{}: no region {}", r.name, a.region));
            assert!(a.stride >= child.size, "{}.{}: stride below {}'s size", r.name, a.name, child.name);
            assert!(a.off + a.count * a.stride <= r.size, "{}.{} runs past the end of {}", r.name, a.name, r.name);
            spans.push((a.off, a.off + a.count * a.stride, &a.name));
        }
        spans.sort();
        for w in spans.windows(2) {
            assert!(w[0].1 <= w[1].0, "{}: {} and {} overlap", r.name, w[0].2, w[1].2);
        }
    }
}

pub(crate) fn upper(s: &str) -> String {
    s.to_ascii_uppercase()
}

pub(crate) fn camel(s: &str) -> String {
    s.split('_')
        .map(|p| {
            let mut c = p.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect()
}

pub(crate) const HEADER: &str = "@generated by scz-abi-gen from abi/scz_abi.toml. Do not edit; run scz-abi-gen.";

// ── Rust ────────────────────────────────────────────────────────────────────────────────

fn rust(spec: &Spec) -> String {
    let mut o = String::new();
    writeln!(o, "// {HEADER}").unwrap();
    writeln!(o, "//\n// Offsets are bytes from the start of the region. Field accessors live in `acc`; each").unwrap();
    writeln!(o, "// one uses the memory ordering the ABI file assigns to its field.").unwrap();
    writeln!(o, "#![allow(dead_code, unused_imports, clippy::all)]\n").unwrap();
    writeln!(o, "pub const ABI_MAJOR: u16 = {};", spec.major).unwrap();
    writeln!(o, "/// The newest minor this build knows. The `next-minor` feature makes it the next one, for the").unwrap();
    writeln!(o, "/// upgrade test (shm-abi §12.5).").unwrap();
    writeln!(o, "#[cfg(not(feature = \"next-minor\"))]\npub const ABI_MINOR: u16 = {};", spec.minor).unwrap();
    writeln!(o, "#[cfg(feature = \"next-minor\")]\npub const ABI_MINOR: u16 = {};", spec.minor + 1).unwrap();
    writeln!(o, "/// The oldest minor a reader accepts: the low end of HELLO's range and the floor of the").unwrap();
    writeln!(o, "/// control header check.\npub const ABI_MINOR_MIN: u16 = {};\n", spec.min_minor).unwrap();
    for (k, v) in &spec.constants {
        writeln!(o, "pub const {k}: u64 = {v:#x};").unwrap();
    }
    writeln!(o, "\npub mod defaults {{").unwrap();
    for (k, v) in &spec.defaults {
        writeln!(o, "    pub const {}: u64 = {v};", upper(k)).unwrap();
    }
    writeln!(o, "}}\n").unwrap();
    for (name, vals) in &spec.enums {
        writeln!(o, "pub mod {name} {{").unwrap();
        for (k, v) in vals {
            writeln!(o, "    pub const {k}: u32 = {v:#x};").unwrap();
        }
        writeln!(o, "    pub const fn name(v: u32) -> &'static str {{\n        match v {{").unwrap();
        for (k, v) in vals {
            writeln!(o, "            {v:#x} => \"{k}\",").unwrap();
        }
        writeln!(o, "            _ => \"?\",\n        }}\n    }}\n}}\n").unwrap();
    }
    for (name, parts) in &spec.bitfields {
        writeln!(o, "pub mod {name} {{").unwrap();
        for (k, shift, bits) in parts {
            let mask: u64 = if *bits == 64 { u64::MAX } else { ((1u64 << bits) - 1) << shift };
            writeln!(o, "    pub const {}_SHIFT: u32 = {shift};", upper(k)).unwrap();
            writeln!(o, "    pub const {}_MASK: u64 = {mask:#018x};", upper(k)).unwrap();
        }
        writeln!(o, "}}\n").unwrap();
    }
    for r in &spec.regions {
        writeln!(o, "/// Region `{}` (segment: {}).", r.name, r.segment).unwrap();
        writeln!(o, "pub mod {} {{", r.name).unwrap();
        writeln!(o, "    pub const SIZE: usize = {:#x};", r.size).unwrap();
        for f in &r.fields {
            writeln!(o, "    /// {} {}, written by {}{}", f.ty, f.access, f.writer, f.note.as_ref().map(|n| format!("; {n}")).unwrap_or_default()).unwrap();
            writeln!(o, "    pub const {}: usize = {:#x};", upper(&f.name), f.off).unwrap();
        }
        for (k, v) in &r.extra {
            writeln!(o, "    pub const {}: usize = {v:#x};", upper(k)).unwrap();
        }
        for a in &r.arrays {
            writeln!(
                o,
                "    pub mod {} {{ pub const OFF: usize = {:#x}; pub const COUNT: usize = {}; pub const STRIDE: usize = {:#x}; }}",
                a.name, a.off, a.count, a.stride
            )
            .unwrap();
        }
        if !r.crc_ranges.is_empty() {
            let rs: Vec<String> = r.crc_ranges.iter().map(|(a, b)| format!("({a:#x}, {b:#x})")).collect();
            writeln!(o, "    pub const CRC_RANGES: &[(usize, usize)] = &[{}];", rs.join(", ")).unwrap();
        }
        writeln!(o, "}}\n").unwrap();
    }
    // Accessors. Not compiled under loom, whose atomics cannot live in raw shared memory.
    writeln!(o, "/// Field accessors over a mapped segment. `base` is the region's offset in the segment.").unwrap();
    writeln!(o, "#[cfg(not(loom))]\npub mod acc {{").unwrap();
    writeln!(o, "    pub use crate::seg::Seg;\n    use core::sync::atomic::Ordering::{{Acquire, Relaxed, Release, AcqRel}};\n").unwrap();
    for r in &spec.regions {
        if r.segment == "message" || r.fields.is_empty() {
            continue;
        }
        writeln!(o, "    pub mod {} {{\n        use super::*;\n        use crate::abi::{} as r;", r.name, r.name).unwrap();
        for f in &r.fields {
            let (load, store) = match f.access.as_str() {
                "acqrel" | "cas" => ("Acquire", "Release"),
                _ => ("Relaxed", "Relaxed"),
            };
            let c = upper(&f.name);
            let n = &f.name;
            if f.ty.starts_with("bytes") {
                let words = width(&f.ty) / 8;
                writeln!(o, "        pub fn {n}(s: &Seg, base: usize) -> [u8; {w}] {{ let mut b = [0u8; {w}]; for i in 0..{words} {{ b[i * 8..i * 8 + 8].copy_from_slice(&s.u64_at(base + r::{c} + i * 8).load({load}).to_le_bytes()); }} b }}", w = words * 8).unwrap();
                writeln!(o, "        pub fn set_{n}(s: &Seg, base: usize, v: &[u8; {w}]) {{ for i in 0..{words} {{ s.u64_at(base + r::{c} + i * 8).store(u64::from_le_bytes(v[i * 8..i * 8 + 8].try_into().unwrap()), {store}); }} }}", w = words * 8).unwrap();
                continue;
            }
            let ty = &f.ty;
            writeln!(o, "        #[inline] pub fn {n}(s: &Seg, base: usize) -> {ty} {{ s.{ty}_at(base + r::{c}).load({load}) }}").unwrap();
            writeln!(o, "        #[inline] pub fn set_{n}(s: &Seg, base: usize, v: {ty}) {{ s.{ty}_at(base + r::{c}).store(v, {store}) }}").unwrap();
            if f.access == "cas" {
                writeln!(o, "        #[inline] pub fn cas_{n}(s: &Seg, base: usize, old: {ty}, new: {ty}) -> Result<{ty}, {ty}> {{ s.{ty}_at(base + r::{c}).compare_exchange(old, new, AcqRel, Acquire) }}").unwrap();
                writeln!(o, "        #[inline] pub fn add_{n}(s: &Seg, base: usize, v: {ty}) -> {ty} {{ s.{ty}_at(base + r::{c}).fetch_add(v, AcqRel) }}")
                    .unwrap();
            }
        }
        writeln!(o, "    }}").unwrap();
    }
    writeln!(o, "}}").unwrap();
    o
}

fn main() {
    let check = std::env::args().any(|a| a == "--check");
    let root: PathBuf = std::env::var_os("SCZ_ROOT").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    let text = std::fs::read_to_string(root.join("abi/scz_abi.toml")).expect("cannot read abi/scz_abi.toml");
    let spec = parse(&text);
    let outputs = [
        ("crates/scz-shm/src/abi.rs", rust(&spec)),
        ("sdks/cpp20/include/scz/abi.hpp", cpp::emit(&spec)),
        ("sdks/go/internal/abi/abi.go", go::emit(&spec)),
        ("sdks/java-ffm/src/main/java/io/sidecarzero/internal/Abi.java", java::emit(&spec)),
    ];
    let mut stale = 0;
    for (rel, content) in outputs {
        let path = root.join(rel);
        let current = std::fs::read_to_string(&path).ok();
        if check {
            if current.as_deref() != Some(content.as_str()) {
                eprintln!("stale: {rel}");
                stale += 1;
            }
        } else if current.as_deref() != Some(content.as_str()) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            println!("wrote {rel}");
        }
    }
    if stale > 0 {
        eprintln!("{stale} generated file(s) are out of date: run scz-abi-gen");
        std::process::exit(1);
    }
}
