//! Link the two Quick Look frameworks even though nothing calls into
//! them.
//!
//! Both provider superclasses are reached by name through the
//! Objective-C runtime, so this binary references no symbol from either
//! framework and a plain `-framework` flag gets dropped from the load
//! commands as unused — leaving the classes unfindable at runtime.
//! `-needed_framework` is the same flag that keeps its dylib regardless.
//!
//! QuickLookUI resolves to Quartz, the umbrella it belongs to; loading
//! that is what puts `QLPreviewProvider` in the runtime.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    for framework in ["QuickLookThumbnailing", "QuickLookUI"] {
        println!("cargo:rustc-link-arg=-Wl,-needed_framework,{framework}");
    }
}
