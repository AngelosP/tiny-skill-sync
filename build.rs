//! Embeds `app.manifest` (visual styles, per-monitor DPI, UTF-8, long-path opt-in, no elevation)
//! into the executables with the MSVC linker. No resource compiler or extra crate is needed.

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    for kind in ["bins", "examples"] {
        println!("cargo:rustc-link-arg-{kind}=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-{kind}=/MANIFESTUAC:NO");
        println!("cargo:rustc-link-arg-{kind}=/MANIFESTINPUT:{dir}\\app.manifest");
    }
}
