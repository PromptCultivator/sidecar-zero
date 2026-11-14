//! Compile the C shim against the pinned llama.cpp release, which the build image installs as
//! static CPU-only libraries at LLAMA_DIR (scripts/dev.Dockerfile), and link those libraries
//! with the C++ runtime they need.
fn main() {
    let llama = std::env::var("LLAMA_DIR").unwrap_or_else(|_| "/opt/llama".into());
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-env-changed=LLAMA_DIR");
    cc::Build::new().file("src/shim.c").include(format!("{llama}/include")).std("c11").warnings(true).compile("scz_gguf_shim");
    println!("cargo:rustc-link-search=native={llama}/lib");
    // In dependency order: llama on ggml's backend registry, which registers the CPU backend,
    // and everything on ggml-base.
    for lib in ["llama", "ggml", "ggml-cpu", "ggml-base"] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    for lib in ["stdc++", "m", "pthread"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
