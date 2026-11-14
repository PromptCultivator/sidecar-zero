//! The Go output: `sdks/go/internal/abi/abi.go`. Owned by the Go SDK.
//!
//! Constants for every offset, enum, bit field, and default, and one accessor per field per
//! region, as methods on `Seg` (testing §2.3: the ordering of every access is fixed by the
//! generator). Go's atomics are sequentially consistent, which is at least as strong as every
//! `access` key asks for, so the only choice left is atomic or not: every accessor is atomic.
//! What the `access` and `writer` keys still decide is which accessors exist:
//!
//! - every field gets a load;
//! - only fields the client may write (`writer` = client or contended) get a store;
//! - `cas` fields the client may write also get a compare-and-swap and an atomic add;
//! - `WorkerSeg` and `SupervisorSeg`, separate view types, get the worker's and the
//!   supervisor's stores: only the in-process double, which plays both, and the SDK's own
//!   tests use them (the lint keeps them out of the client).
//!
//! Go has no 8- or 16-bit atomics. A `u8` or `u16` field is loaded through an atomic load of
//! the aligned 32-bit word that holds it; it gets a store only when no other field shares that
//! word, and the store then writes the whole word, the field's padding as zero.
//!
//! The accessors call `Seg`'s unexported primitives (`seg.go`, hand-written, the only file of
//! the SDK that touches arena memory), so no code outside package `abi` can reach arena memory
//! other than through them. Message regions (the handshake's 64-byte datagrams) are process
//! memory and get offsets only.

use crate::*;
use std::fmt::Write as _;

fn go_ty(ty: &str) -> &'static str {
    match ty {
        "u8" => "uint8",
        "u16" => "uint16",
        "u32" => "uint32",
        "u64" => "uint64",
        t => panic!("no Go type for {t}"),
    }
}

/// Whether any other field of `r` overlaps the aligned 4-byte word holding `f`.
fn shares_word(r: &Region, f: &Field) -> bool {
    let w0 = f.off & !3;
    r.fields.iter().any(|g| g.name != f.name && g.off < w0 + 4 && g.off + width(&g.ty) > w0)
}

fn client_writes(f: &Field) -> bool {
    f.writer == "client" || f.writer == "contended"
}

/// The byte ranges of a region not covered by any field.
fn gaps(r: &Region) -> Vec<(u64, u64)> {
    let mut covered = vec![false; r.size as usize];
    for f in &r.fields {
        for b in f.off..f.off + width(&f.ty) {
            covered[b as usize] = true;
        }
    }
    let mut out = vec![];
    let mut i = 0;
    while i < covered.len() {
        if covered[i] {
            i += 1;
            continue;
        }
        let s = i;
        while i < covered.len() && !covered[i] {
            i += 1;
        }
        out.push((s as u64, i as u64));
    }
    out
}

/// Set (and, for cas fields, Cas and Add) for field `f` on the view type `view`, whose
/// receiver is `recv` and whose segment is reached as `seg`.
fn setters(o: &mut String, r: &Region, f: &Field, view: &str, recv: &str, seg: &str, strict: bool) {
    let rn = camel(&r.name);
    let fname = format!("{rn}{}", camel(&f.name));
    let w = width(&f.ty);
    if f.ty.starts_with("bytes") {
        let words = w / 8;
        writeln!(o, "\n// Set{fname} stores {}.\nfunc ({recv} {view}) Set{fname}(base uintptr, b *[{w}]byte) {{\n\tfor i := uintptr(0); i < {words}; i++ {{\n\t\tvar v uint64\n\t\tfor j := uintptr(0); j < 8; j++ {{\n\t\t\tv |= uint64(b[i*8+j]) << (8 * j)\n\t\t}}\n\t\t{seg}.st64(base+{fname}+i*8, v)\n\t}}\n}}", f.name).unwrap();
        return;
    }
    let gt = go_ty(&f.ty);
    match w {
        8 => writeln!(o, "\n// Set{fname} stores {}.\nfunc ({recv} {view}) Set{fname}(base uintptr, v uint64) {{\n\t{seg}.st64(base+{fname}, v)\n}}", f.name)
            .unwrap(),
        4 => writeln!(o, "\n// Set{fname} stores {}.\nfunc ({recv} {view}) Set{fname}(base uintptr, v uint32) {{\n\t{seg}.st32(base+{fname}, v)\n}}", f.name)
            .unwrap(),
        _ => {
            let shift = (f.off & 3) * 8;
            if !shares_word(r, f) {
                writeln!(
                    o,
                    "\n// Set{fname} stores {}, as the whole 32-bit word that holds it (the rest of it is padding).\nfunc ({recv} {view}) Set{fname}(base uintptr, v {gt}) {{\n\t{seg}.st32(base+{fname}&^3, uint32(v)<<{shift})\n}}",
                    f.name
                )
                .unwrap()
            } else {
                assert!(!strict, "{}.{}: a client-written {} shares its 32-bit word with another field", r.name, f.name, f.ty);
                let mask: u64 = ((1u64 << (w * 8)) - 1) << shift;
                writeln!(
                    o,
                    "\n// Set{fname} stores {}: a load and a store of the 32-bit word that holds it.\nfunc ({recv} {view}) Set{fname}(base uintptr, v {gt}) {{\n\tp := base + {fname}&^3\n\t{seg}.st32(p, {seg}.ld32(p)&^{mask:#x}|uint32(v)<<{shift})\n}}",
                    f.name
                )
                .unwrap()
            }
        }
    }
    if f.access == "cas" {
        let (cas, add) = match w {
            8 => ("cas64", "add64"),
            4 => ("cas32", "add32"),
            _ => panic!("{}.{}: a cas field must be 32 or 64 bits", r.name, f.name),
        };
        writeln!(o, "\n// Cas{fname} compares and swaps {}.\nfunc ({recv} {view}) Cas{fname}(base uintptr, old, new {gt}) bool {{\n\treturn {seg}.{cas}(base+{fname}, old, new)\n}}", f.name).unwrap();
        writeln!(o, "\n// Add{fname} adds to {} and returns the new value.\nfunc ({recv} {view}) Add{fname}(base uintptr, d {gt}) {gt} {{\n\treturn {seg}.{add}(base+{fname}, d)\n}}", f.name).unwrap();
    }
}

pub fn emit(spec: &Spec) -> String {
    let mut o = String::new();
    writeln!(o, "// Code generated by scz-abi-gen from abi/scz_abi.toml. DO NOT EDIT.").unwrap();
    writeln!(o, "// {HEADER}").unwrap();
    writeln!(o, "//").unwrap();
    writeln!(o, "// Offsets are bytes from the start of the region. Each field has a load method on Seg;").unwrap();
    writeln!(o, "// fields the client writes also have Set, and cas fields Cas and Add. Every accessor is").unwrap();
    writeln!(o, "// atomic (Go's atomics are sequentially consistent: at least the ordering the ABI asks for).").unwrap();
    writeln!(o, "\npackage abi\n").unwrap();

    // One declaration per line, each with its comment above it: gofmt aligns nothing, so the
    // output is already gofmt's.
    writeln!(
        o,
        "const AbiMajor = {}\nconst AbiMinor = {}\n\n// AbiMinorMin is the oldest minor this SDK accepts (shm-abi §8).\nconst AbiMinorMin = {}",
        spec.major, spec.minor, spec.min_minor
    )
    .unwrap();
    for (k, v) in &spec.constants {
        writeln!(o, "const {} = {v:#x}", camel(&k.to_ascii_lowercase())).unwrap();
    }
    writeln!(o, "\n// Defaults written into CONFIG; a supervisor may override them per arena.").unwrap();
    for (k, v) in &spec.defaults {
        writeln!(o, "const Default{} = {v}", camel(k)).unwrap();
    }
    for (name, vals) in &spec.enums {
        writeln!(o, "\n// Enum {name}.").unwrap();
        for (k, v) in vals {
            writeln!(o, "const {}{} = {v:#x}", camel(name), camel(&k.to_ascii_lowercase())).unwrap();
        }
        writeln!(o, "\n// {}Name is the ABI file's name for a {name} value, or \"?\".", camel(name)).unwrap();
        writeln!(o, "func {}Name(v uint32) string {{\n\tswitch v {{", camel(name)).unwrap();
        for (k, v) in vals {
            writeln!(o, "\tcase {v:#x}:\n\t\treturn \"{k}\"").unwrap();
        }
        writeln!(o, "\t}}\n\treturn \"?\"\n}}").unwrap();
    }
    for (name, parts) in &spec.bitfields {
        writeln!(o, "\n// Bit field {name}.").unwrap();
        for (k, shift, bits) in parts {
            let mask: u64 = if *bits == 64 { u64::MAX } else { ((1u64 << bits) - 1) << shift };
            writeln!(o, "const {}{}Shift = {shift}", camel(name), camel(k)).unwrap();
            writeln!(o, "const {}{}Mask = {mask:#018x}", camel(name), camel(k)).unwrap();
        }
    }
    for r in &spec.regions {
        let rn = camel(&r.name);
        writeln!(o, "\n// Region {} (segment: {}).\nconst {rn}Size = {:#x}", r.name, r.segment, r.size).unwrap();
        for f in &r.fields {
            let note = f.note.as_ref().map(|n| format!("; {n}")).unwrap_or_default();
            writeln!(
                o,
                "\n// {rn}{} is the offset of {}: {} {}, written by {}{note}.\nconst {rn}{} = {:#x}",
                camel(&f.name),
                f.name,
                f.ty,
                f.access,
                f.writer,
                camel(&f.name),
                f.off
            )
            .unwrap();
        }
        for (k, v) in &r.extra {
            writeln!(o, "const {rn}{} = {v:#x}", camel(k)).unwrap();
        }
        for a in &r.arrays {
            let an = camel(&a.name);
            writeln!(
                o,
                "\n// Array {} of {} regions.\nconst {rn}{an}Off = {:#x}\nconst {rn}{an}Count = {}\nconst {rn}{an}Stride = {:#x}",
                a.name, a.region, a.off, a.count, a.stride
            )
            .unwrap();
        }
        if !r.crc_ranges.is_empty() {
            let rs: Vec<String> = r.crc_ranges.iter().map(|(a, b)| format!("{{{a:#x}, {b:#x}}}")).collect();
            writeln!(o, "\n// {rn}CrcRanges are the byte ranges the layout checksum covers.\nvar {rn}CrcRanges = [...][2]uintptr{{{}}}", rs.join(", "))
                .unwrap();
        }
    }

    writeln!(o, "\n// ── Accessors ───────────────────────────────────────────────────────────────────────").unwrap();
    for r in &spec.regions {
        if r.segment == "message" || r.fields.is_empty() {
            continue;
        }
        let rn = camel(&r.name);
        writeln!(o, "\n// Region {}: `base` is the region's offset in the segment.", r.name).unwrap();
        for f in &r.fields {
            let fname = format!("{rn}{}", camel(&f.name));
            let w = width(&f.ty);
            if f.ty.starts_with("bytes") {
                let words = w / 8;
                writeln!(o, "\n// {fname} loads {} ({}, {}, written by {}).", f.name, f.ty, f.access, f.writer).unwrap();
                writeln!(o, "func (s Seg) {fname}(base uintptr) (b [{w}]byte) {{\n\tfor i := uintptr(0); i < {words}; i++ {{\n\t\tv := s.ld64(base + {fname} + i*8)\n\t\tfor j := uintptr(0); j < 8; j++ {{\n\t\t\tb[i*8+j] = byte(v >> (8 * j))\n\t\t}}\n\t}}\n\treturn b\n}}").unwrap();
                continue;
            }
            let gt = go_ty(&f.ty);
            writeln!(o, "\n// {fname} loads {} ({}, {}, written by {}).", f.name, f.ty, f.access, f.writer).unwrap();
            match w {
                8 => writeln!(o, "func (s Seg) {fname}(base uintptr) uint64 {{\n\treturn s.ld64(base + {fname})\n}}").unwrap(),
                4 => writeln!(o, "func (s Seg) {fname}(base uintptr) uint32 {{\n\treturn s.ld32(base + {fname})\n}}").unwrap(),
                _ => {
                    let shift = (f.off & 3) * 8;
                    writeln!(o, "func (s Seg) {fname}(base uintptr) {gt} {{\n\treturn {gt}(s.ld32(base+{fname}&^3) >> {shift})\n}}").unwrap()
                }
            }
            // The client's view (Seg) is strict: a narrow field it writes must have its 32-bit
            // word to itself. The worker's and supervisor's views stand in for those processes
            // in the in-process double and in tests only, and may write narrow fields that share
            // a word, with a load and a store, because nothing else writes them meanwhile.
            if client_writes(f) {
                setters(&mut o, r, f, "Seg", "s", "s", true);
            }
            if (f.writer == "worker" || f.writer == "contended") && r.segment == "data" {
                setters(&mut o, r, f, "WorkerSeg", "w", "w.s", false);
            }
            if f.writer == "supervisor" && (r.segment == "control" || r.segment == "data") {
                setters(&mut o, r, f, "SupervisorSeg", "sv", "sv.s", false);
            }
        }
        // A region the client fills whole (every field client-written): its padding is zeroed,
        // so the bytes it leaves equal the reference client's.
        if r.fields.iter().all(client_writes) {
            let mut stores = String::new();
            for (a, b) in gaps(r) {
                let mut p = a;
                while p < b {
                    if p % 8 == 0 && p + 8 <= b {
                        writeln!(stores, "\ts.st64(base+{p:#x}, 0)").unwrap();
                        p += 8;
                    } else if p % 4 == 0 && p + 4 <= b {
                        writeln!(stores, "\ts.st32(base+{p:#x}, 0)").unwrap();
                        p += 4;
                    } else {
                        // A gap narrower than a 32-bit word lies beside a narrow field, whose
                        // store writes it as zero.
                        p += 1;
                    }
                }
            }
            writeln!(
                o,
                "\n// Clear{rn}Padding zeroes the bytes of {} that no field covers.\nfunc (s Seg) Clear{rn}Padding(base uintptr) {{\n{stores}}}",
                r.name
            )
            .unwrap();
        }
    }
    o
}
