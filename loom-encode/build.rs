//! Build script: locate libx265 via pkg-config and generate its FFI bindings.

use std::env;
use std::path::PathBuf;

fn main() {
    // Emits the cargo:rustc-link-* lines for libx265 and its search paths.
    let lib = pkg_config::Config::new()
        .probe("x265")
        .expect("libx265 not found via pkg-config (try `brew install x265`)");

    // libx265 is C++ under its C API; link the platform C++ runtime (libc++ on
    // macOS/Homebrew, libstdc++ on Linux/GCC).
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let cxx_runtime = if target_os == "macos" {
        "c++"
    } else {
        "stdc++"
    };
    println!("cargo:rustc-link-lib={cxx_runtime}");

    println!("cargo:rerun-if-changed=wrapper.h");

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function("x265_.*")
        .allowlist_type("x265_.*")
        .allowlist_type("X265_RC_METHODS")
        .allowlist_type("NalUnitType")
        .allowlist_var("X265_.*")
        .allowlist_var("NAL_UNIT_.*")
        .layout_tests(false);
    for path in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let bindings = builder
        .generate()
        .expect("bindgen: failed to generate x265 bindings");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("x265_bindings.rs"))
        .expect("write x265 bindings");

    // `x265_encoder_open` is a build-number-versioned macro (ABI-safety guard),
    // so it has no stable symbol to bind. A one-line C shim gives us one.
    println!("cargo:rerun-if-changed=src/x265_shim.c");
    let mut cc = cc::Build::new();
    for path in &lib.include_paths {
        cc.include(path);
    }
    cc.file("src/x265_shim.c").compile("loom_x265_shim");

    if env::var("CARGO_FEATURE_NVENC").is_ok() {
        build_nvenc_bindings();
    }
}

/// Locate libavcodec/libavutil via pkg-config (emitting their link lines) and
/// generate FFI bindings for the small slice of the API the NVENC backend uses.
fn build_nvenc_bindings() {
    let mut include_paths = Vec::new();
    for pkg in ["libavcodec", "libavutil"] {
        let lib = pkg_config::Config::new().probe(pkg).unwrap_or_else(|e| {
            panic!("{pkg} not found via pkg-config (install ffmpeg-devel): {e}")
        });
        include_paths.extend(lib.include_paths);
    }

    println!("cargo:rerun-if-changed=wrapper_av.h");
    let mut builder = bindgen::Builder::default()
        .header("wrapper_av.h")
        // libav headers carry doxygen @code blocks; bindgen would render them as
        // Rust doc-comment code fences that cargo then tries to run as doctests.
        .generate_comments(false)
        .allowlist_function("avcodec_.*")
        .allowlist_function("av_frame_.*")
        .allowlist_function("av_packet_.*")
        .allowlist_function("av_opt_set.*")
        .allowlist_type("AVCodecContext")
        .allowlist_type("AVCodec")
        .allowlist_type("AVFrame")
        .allowlist_type("AVPacket")
        .allowlist_type("AVRational")
        .allowlist_type("AVPixelFormat")
        .allowlist_type("AVPictureType")
        .allowlist_var("AV_PKT_FLAG_.*")
        .layout_tests(false);
    for path in &include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let bindings = builder
        .generate()
        .expect("bindgen: failed to generate libavcodec bindings");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("av_bindings.rs"))
        .expect("write libavcodec bindings");
}
