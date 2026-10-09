//! Builds Loom's MLX engine on macOS when the prebuilt MLX runtime is present
//! (`scripts/fetch-mlx`, or `LOOM_MLX_DIR`). Without it the crate builds unchanged and
//! the MLX path is simply absent (`cfg(loom_mlx)` unset).
use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(loom_mlx)");
    println!("cargo::rustc-check-cfg=cfg(loom_lmx_engine)");
    build_lmx_engine();
    println!("cargo:rerun-if-changed=native/mlx/engine.cpp");
    println!("cargo:rerun-if-env-changed=LOOM_MLX_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let dir = std::env::var_os("LOOM_MLX_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("vendor/mlx")
    });
    let library = dir.join("lib/libmlx.dylib");
    println!("cargo:rerun-if-changed={}", library.display());
    if !library.is_file() || !dir.join("include/mlx/mlx.h").is_file() {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .include(dir.join("include"))
        .file("native/mlx/engine.cpp")
        .opt_level(2)
        .warnings(false)
        .compile("loom_mlx");
    println!("cargo:rustc-link-search=native={}", dir.join("lib").display());
    println!("cargo:rustc-link-lib=dylib=mlx");
    println!("cargo:rustc-cfg=loom_mlx");
}

/// Loom's native LMX engine runs on ggml (CPU, Metal, CUDA, Vulkan: whatever backends
/// llama-cpp-sys-2 was built with). It compiles against the ggml that crate installs, so
/// there is exactly one ggml in the program.
fn build_lmx_engine() {
    println!("cargo:rerun-if-changed=native/lmx/engine.cpp");
    let Some(cmake) = std::env::var_os("DEP_LLAMA_GGML_CMAKE_DIR").map(PathBuf::from) else {
        println!("cargo:warning=llama-cpp-sys-2 did not export its ggml; native LMX engine disabled");
        return;
    };
    // <out>/lib/cmake -> <out>/include
    let include = cmake.join("../../include");
    if !include.join("ggml-backend.h").is_file() {
        println!("cargo:warning=ggml headers missing at {}; native LMX engine disabled", include.display());
        return;
    }
    let mut build = cc::Build::new();
    build.cpp(true).include(&include).file("native/lmx/engine.cpp").opt_level(2).warnings(false);
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        build.flag("/std:c++17").flag("/EHsc");
    } else {
        build.std("c++17");
    }
    build.compile("loom_lmx_engine");
    println!("cargo:rustc-cfg=loom_lmx_engine");
}
