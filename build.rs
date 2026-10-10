use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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
    println!("cargo:rerun-if-env-changed=NIH_PLUG_AU_COCOAUI_PACKAGE");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let au_package = std::env::var("NIH_PLUG_AU_COCOAUI_PACKAGE").ok();
    let mut shim_build = cc::Build::new();
    shim_build
        .file(shim)
        .flag("-fobjc-arc")
        .flag("-fmodules")
        .flag("-fvisibility=default")
        .flag("-mmacosx-version-min=11.0");
    if au_package.as_deref() == Some("") {
        // xtask first builds sibling formats without AU-only ObjC classes.
        shim_build.define("NIH_PLUG_AU_COCOAUI_DISABLED", "1");
    } else {
        // Each separately bundled AU package needs its own static metadata.
        // Dynamically registered classes cannot satisfy NSBundle classNamed:.
        let mut h = DefaultHasher::new();
        out_dir.hash(&mut h);
        au_package.hash(&mut h);
        let suffix = h.finish();
        let factory = format!("NihPlugAuViewFactory_{suffix:016x}");
        let container = format!("NihPlugAuContainerView_{suffix:016x}");
        shim_build
            .define("NIH_PLUG_AU_VIEW_CLASS", factory.as_str())
            .define("NIH_PLUG_AU_CONTAINER_CLASS", container.as_str());
    }
    shim_build.compile("nih_plug_cocoaui");

    // Expose the .a path via the `links` DEP mechanism so downstream crates
    // (e.g. de-artifact-vst3) can pass -force_load at their own link step.
    // (cargo:rustc-link-arg from a dependency's build.rs is not propagated.)
    let lib_path = std::path::Path::new(&out_dir).join("libnih_plug_cocoaui.a");
    println!("cargo:cocoaui_lib={}", lib_path.display());
}
