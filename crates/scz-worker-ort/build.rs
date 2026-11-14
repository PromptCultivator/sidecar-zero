//! Compile the C++ shim against the pinned ONNX Runtime release, which the build image
//! unpacks at ORT_DIR (scripts/dev.Dockerfile), and link the runtime with an rpath to it.
fn main() {
    let ort = std::env::var("ORT_DIR").unwrap_or_else(|_| "/opt/ort".into());
    println!("cargo:rerun-if-changed=src/shim.cc");
    println!("cargo:rerun-if-env-changed=ORT_DIR");
    cc::Build::new().cpp(true).std("c++17").file("src/shim.cc").include(format!("{ort}/include")).warnings(true).compile("scz_ort_shim");
    println!("cargo:rustc-link-search=native={ort}/lib");
    println!("cargo:rustc-link-lib=dylib=onnxruntime");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{ort}/lib");
}
