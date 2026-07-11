//! Build script: locate libx265 via pkg-config and generate its FFI bindings.

use std::env;
use std::path::PathBuf;

fn main() {
    // Emits the cargo:rustc-link-* lines for libx265 and its search paths.
    let lib = pkg_config::Config::new()
        .probe("x265")
        .expect("libx265 not found via pkg-config (try `brew install x265`)");

    // libx265 is C++ under its C API; make sure the C++ runtime is linked.
    println!("cargo:rustc-link-lib=c++");

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

    let bindings = builder.generate().expect("bindgen: failed to generate x265 bindings");
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
}
