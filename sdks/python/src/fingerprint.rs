//! The schema fingerprint (arrow-data-plane §9): xxHash3-64, seed 0, over the logical schema.
//! On Linux it is the Rust core's own function, the one the client binds by; elsewhere the
//! same bytes are hashed here. Both are checked against `abi/golden/fingerprints.txt`.

use pyo3::prelude::*;

/// `elem_type` F32 (abi/scz_abi.toml).
pub const F32: u8 = 0x0a;

/// (name, elem_type, list_size, nullable)
pub type Col<'a> = (&'a str, u8, u32, bool);

#[cfg(target_os = "linux")]
pub fn of(request: &[Col<'_>], response: &[Col<'_>]) -> u64 {
    use scz_shm::registry::{fingerprint, Column, Schema};
    let cols = |c: &[Col<'_>]| c.iter().map(|&(name, elem, list_size, nullable)| Column { name: name.to_string(), elem, list_size, nullable }).collect();
    fingerprint(&Schema { request: cols(request), response: cols(response) })
}

#[cfg(not(target_os = "linux"))]
pub fn of(request: &[Col<'_>], response: &[Col<'_>]) -> u64 {
    let mut b = b"SCZSCHEMA\x01".to_vec();
    for cols in [request, response] {
        b.extend_from_slice(&(cols.len() as u16).to_le_bytes());
        for &(name, elem, list_size, nullable) in cols {
            b.extend_from_slice(&(name.len() as u16).to_le_bytes());
            b.extend_from_slice(name.as_bytes());
            b.push(elem);
            b.extend_from_slice(&list_size.to_le_bytes());
            b.push(u8::from(nullable));
        }
    }
    xxhash_rust::xxh3::xxh3_64(&b)
}

/// `fingerprint(request, response)`: each a list of `(name, elem_type, list_size, nullable)`.
#[pyfunction]
pub fn fingerprint(request: Vec<(String, u8, u32, bool)>, response: Vec<(String, u8, u32, bool)>) -> u64 {
    let r: Vec<Col<'_>> = request.iter().map(|(n, e, l, x)| (n.as_str(), *e, *l, *x)).collect();
    let s: Vec<Col<'_>> = response.iter().map(|(n, e, l, x)| (n.as_str(), *e, *l, *x)).collect();
    of(&r, &s)
}
