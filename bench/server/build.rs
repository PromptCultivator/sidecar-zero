//! ONNX Runtime is linked through scz-worker-ort; its rpath must be set on this crate's own
//! binaries too, since a library's link arguments do not reach the programs that use it.
fn main() {
    let ort = std::env::var("ORT_DIR").unwrap_or_else(|_| "/opt/ort".into());
    println!("cargo:rerun-if-env-changed=ORT_DIR");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{ort}/lib");
}
