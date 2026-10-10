fn main() {
    // Only needed on macOS with the `au` feature enabled.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        return;
    }

    // Only compile the ObjC shim when the `au` feature is active.
    let au_enabled = std::env::var("CARGO_FEATURE_AU").is_ok();
    if !au_enabled {
        return;
    }

    let shim = "src/wrapper/au/cocoaui.m";
    println!("cargo:rerun-if-changed={shim}");

    cc::Build::new()
        .file(shim)
        .flag("-fobjc-arc")
        .flag("-fmodules")
        .flag("-fvisibility=default")
        .flag("-mmacosx-version-min=11.0")
        .compile("nih_plug_cocoaui");

    // Expose the .a path via the `links` DEP mechanism so downstream crates
    // (e.g. de-artifact-vst3) can pass -force_load at their own link step.
    // (cargo:rustc-link-arg from a dependency's build.rs is not propagated.)
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let lib_path = std::path::Path::new(&out_dir).join("libnih_plug_cocoaui.a");
    println!("cargo:cocoaui_lib={}", lib_path.display());
}
