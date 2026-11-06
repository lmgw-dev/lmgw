//! Drops the wasm `name` section from the debug dashboard at link time (the
//! numbers are at `[profile.dev.package.lmgw-ui]` in the workspace Cargo.toml).
//!
//! `strip` in a profile cannot do it: for wasm32 rustc hands rust-lld
//! `--strip-debug` for `strip = "symbols"` too, which keeps the name section,
//! and a profile cannot carry link arguments on stable. `PROFILE` is `debug`
//! for every profile that inherits from dev, so the release build (whose names
//! wasm-opt removes) links exactly as before.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32");
    let debug = std::env::var("PROFILE").as_deref() == Ok("debug");
    if wasm && debug {
        println!("cargo:rustc-link-arg-bins=--strip-all");
    }
}
