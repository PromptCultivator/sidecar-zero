//! The Java output: `sdks/java-ffm/src/main/java/io/sidecarzero/internal/Abi.java`. Owned by the
//! Java SDK (testing §2.3: the ordering of every access is fixed by the generator).
//!
//! Constants, enums (with their names), bit fields, region offsets, and one accessor per field
//! per region over a `MemorySegment`, whose `VarHandle` access mode follows the field's
//! `access` key (sdk-java §3, "Access-mode mapping"):
//!
//! | access          | load          | store         | extra                                         |
//! |-----------------|---------------|---------------|-----------------------------------------------|
//! | const, relaxed  | `getOpaque`   | `setOpaque`   |                                               |
//! | acqrel          | `getAcquire`  | `setRelease`  | `*Volatile` load and store (sequentially consistent) |
//! | cas             | `getAcquire`  | `setRelease`  | the same, `cas*` (`compareAndSet`), `add*` (`getAndAdd`) |
//!
//! Relaxed stores use `setOpaque`, not plain `set`: a plain 64-bit `set` is not promised to be
//! a single access, and opaque costs nothing more on x86-64 or aarch64. Every handle is
//! little-endian and has exact invocation behaviour, so a call with the wrong types fails at
//! once instead of converting silently.

use crate::*;
use std::fmt::Write as _;

/// `snake_case` → `lowerCamel`.
fn lower_camel(s: &str) -> String {
    let c = camel(s);
    let mut it = c.chars();
    match it.next() {
        Some(f) => f.to_ascii_lowercase().to_string() + it.as_str(),
        None => c,
    }
}

/// (Java value type, handle, cast-in) for an integer width.
fn jtype(ty: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    // (api type, handle, raw java type, read conversion suffix)
    match ty {
        "u8" => ("int", "U8", "byte", " & 0xFF"),
        "u16" => ("int", "U16", "short", " & 0xFFFF"),
        "u32" => ("int", "U32", "int", ""),
        "u64" => ("long", "U64", "long", ""),
        t => panic!("no Java type for {t}"),
    }
}

/// Zero stores covering `[start, end)`: the widest naturally aligned access at each position.
fn gap_stores(start: u64, end: u64) -> Vec<(u64, &'static str)> {
    let mut v = vec![];
    let mut at = start;
    while at < end {
        let (w, h) = [(8u64, "U64"), (4, "U32"), (2, "U16"), (1, "U8")].into_iter().find(|(w, _)| at.is_multiple_of(*w) && at + w <= end).unwrap();
        v.push((at, h));
        at += w;
    }
    v
}

pub fn emit(spec: &Spec) -> String {
    let mut o = String::new();
    writeln!(o, "// {HEADER}").unwrap();
    writeln!(o, "//").unwrap();
    writeln!(o, "// Offsets are bytes from the start of the region. Each region class carries one accessor per").unwrap();
    writeln!(o, "// field, taking the segment and the region's offset in it; the VarHandle access mode of each").unwrap();
    writeln!(o, "// is fixed by the field's `access` key (const and relaxed: opaque; acqrel: acquire/release,").unwrap();
    writeln!(o, "// with volatile variants; cas: the same plus compareAndSet and getAndAdd).").unwrap();
    writeln!(o, "package io.sidecarzero.internal;\n").unwrap();
    writeln!(
        o,
        "import java.lang.foreign.MemorySegment;\nimport java.lang.foreign.ValueLayout;\nimport java.lang.invoke.VarHandle;\nimport java.nio.ByteOrder;\n"
    )
    .unwrap();
    writeln!(o, "@SuppressWarnings(\"unused\")\npublic final class Abi {{\n    private Abi() {{}}\n").unwrap();
    for (h, l) in [("U8", "JAVA_BYTE"), ("U16", "JAVA_SHORT"), ("U32", "JAVA_INT"), ("U64", "JAVA_LONG")] {
        writeln!(o, "    static final VarHandle {h} = ValueLayout.{l}.withOrder(ByteOrder.LITTLE_ENDIAN).varHandle().withInvokeExactBehavior();").unwrap();
    }
    writeln!(
        o,
        "\n    public static final int ABI_MAJOR = {};\n    public static final int ABI_MINOR = {};\n    /** The oldest minor this SDK accepts (shm-abi §8). */\n    public static final int ABI_MINOR_MIN = {};",
        spec.major, spec.minor, spec.min_minor
    )
    .unwrap();
    for (k, v) in &spec.constants {
        writeln!(o, "    public static final long {k} = {v:#x}L;").unwrap();
    }
    writeln!(o, "\n    public static final class Defaults {{\n        private Defaults() {{}}").unwrap();
    for (k, v) in &spec.defaults {
        writeln!(o, "        public static final long {} = {v}L;", upper(k)).unwrap();
    }
    writeln!(o, "    }}").unwrap();
    for (name, vals) in &spec.enums {
        writeln!(o, "\n    public static final class {} {{\n        private {}() {{}}", camel(name), camel(name)).unwrap();
        for (k, v) in vals {
            writeln!(o, "        public static final int {k} = {v:#x};").unwrap();
        }
        writeln!(o, "        public static String name(int v) {{\n            return switch (v) {{").unwrap();
        for (k, v) in vals {
            writeln!(o, "                case {v:#x} -> \"{k}\";").unwrap();
        }
        writeln!(o, "                default -> \"?\";\n            }};\n        }}\n    }}").unwrap();
    }
    for (name, parts) in &spec.bitfields {
        writeln!(o, "\n    public static final class {} {{\n        private {}() {{}}", camel(name), camel(name)).unwrap();
        for (k, shift, bits) in parts {
            let mask: u64 = if *bits == 64 { u64::MAX } else { ((1u64 << bits) - 1) << shift };
            writeln!(o, "        public static final int {}_SHIFT = {shift};", upper(k)).unwrap();
            writeln!(o, "        public static final long {}_MASK = {mask:#018x}L;", upper(k)).unwrap();
        }
        writeln!(o, "    }}").unwrap();
    }
    for r in &spec.regions {
        let cls = camel(&r.name);
        writeln!(o, "\n    /** Region `{}` (segment: {}). */", r.name, r.segment).unwrap();
        writeln!(o, "    public static final class {cls} {{\n        private {cls}() {{}}\n        public static final long SIZE = {:#x}L;", r.size).unwrap();
        for f in &r.fields {
            writeln!(o, "        /** {} {}, written by {}{}. */", f.ty, f.access, f.writer, f.note.as_ref().map(|n| format!("; {n}")).unwrap_or_default())
                .unwrap();
            writeln!(o, "        public static final long {} = {:#x}L;", upper(&f.name), f.off).unwrap();
        }
        for (k, v) in &r.extra {
            writeln!(o, "        public static final long {} = {v:#x}L;", upper(k)).unwrap();
        }
        for a in &r.arrays {
            let n = upper(&a.name);
            writeln!(o, "        public static final long {n}_OFF = {:#x}L;\n        public static final int {n}_COUNT = {};\n        public static final long {n}_STRIDE = {:#x}L;", a.off, a.count, a.stride).unwrap();
        }
        if !r.crc_ranges.is_empty() {
            let rs: Vec<String> = r.crc_ranges.iter().map(|(a, b)| format!("{{{a:#x}L, {b:#x}L}}")).collect();
            writeln!(o, "        public static final long[][] CRC_RANGES = {{{}}};", rs.join(", ")).unwrap();
        }
        if r.segment != "message" {
            for f in &r.fields {
                emit_accessors(&mut o, f);
            }
            if !r.fields.is_empty() && r.size <= 64 {
                // The bytes between fields, for writers that must leave a region exactly as the
                // reference does (a reused ring cell, for one).
                let mut spans: Vec<(u64, u64)> = r.fields.iter().map(|f| (f.off, f.off + width(&f.ty))).collect();
                spans.sort();
                let mut at = 0;
                let mut stores = vec![];
                for (s, e) in spans.iter().copied().chain([(r.size, r.size)]) {
                    if s > at {
                        stores.extend(gap_stores(at, s));
                    }
                    at = at.max(e);
                }
                writeln!(o, "        /** Zero every byte of the region that no field covers (opaque stores). */").unwrap();
                writeln!(o, "        public static void zeroGaps(MemorySegment s, long base) {{").unwrap();
                for (off, h) in stores {
                    let z = match h {
                        "U64" => "0L",
                        "U32" => "0",
                        "U16" => "(short) 0",
                        _ => "(byte) 0",
                    };
                    writeln!(o, "            {h}.setOpaque(s, base + {off:#x}L, {z});").unwrap();
                }
                writeln!(o, "        }}").unwrap();
            }
        }
        writeln!(o, "    }}").unwrap();
    }
    writeln!(o, "}}").unwrap();
    o
}

fn emit_accessors(o: &mut String, f: &Field) {
    let c = upper(&f.name);
    let get = lower_camel(&f.name);
    let set = format!("set{}", camel(&f.name));
    if f.ty.starts_with("bytes") {
        let words = width(&f.ty) / 8;
        writeln!(o, "        /** Word {{@code i}} (0..{}) of the {} bytes, little-endian. */", words - 1, words * 8).unwrap();
        writeln!(o, "        public static long {get}Word(MemorySegment s, long base, int i) {{ return (long) U64.getOpaque(s, base + {c} + 8L * i); }}")
            .unwrap();
        writeln!(o, "        public static void {set}Word(MemorySegment s, long base, int i, long v) {{ U64.setOpaque(s, base + {c} + 8L * i, v); }}").unwrap();
        return;
    }
    let (api, h, raw, conv) = jtype(&f.ty);
    let (load, store) = match f.access.as_str() {
        "acqrel" | "cas" => ("getAcquire", "setRelease"),
        _ => ("getOpaque", "setOpaque"),
    };
    let rd = |mode: &str| if conv.is_empty() { format!("({raw}) {h}.{mode}(s, base + {c})") } else { format!("(({raw}) {h}.{mode}(s, base + {c})){conv}") };
    let cast_in = if raw == api { "v".to_string() } else { format!("({raw}) v") };
    writeln!(o, "        public static {api} {get}(MemorySegment s, long base) {{ return {}; }}", rd(load)).unwrap();
    writeln!(o, "        public static void {set}(MemorySegment s, long base, {api} v) {{ {h}.{store}(s, base + {c}, {cast_in}); }}").unwrap();
    if f.access == "acqrel" || f.access == "cas" {
        writeln!(o, "        public static {api} {get}Volatile(MemorySegment s, long base) {{ return {}; }}", rd("getVolatile")).unwrap();
        writeln!(o, "        public static void {set}Volatile(MemorySegment s, long base, {api} v) {{ {h}.setVolatile(s, base + {c}, {cast_in}); }}").unwrap();
    }
    if f.access == "cas" {
        let cast = |x: &str| if raw == api { x.to_string() } else { format!("({raw}) {x}") };
        writeln!(
            o,
            "        public static boolean cas{}(MemorySegment s, long base, {api} old, {api} v) {{ return (boolean) {h}.compareAndSet(s, base + {c}, {}, {}); }}",
            camel(&f.name),
            cast("old"),
            cast("v")
        )
        .unwrap();
        if raw == api {
            writeln!(
                o,
                "        public static {api} add{}(MemorySegment s, long base, {api} v) {{ return ({raw}) {h}.getAndAdd(s, base + {c}, v); }}",
                camel(&f.name)
            )
            .unwrap();
        }
    }
}
