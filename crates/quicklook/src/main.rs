//! Schist's macOS Quick Look extensions.
//!
//! One binary, two app extensions: Finder's thumbnails
//! (`QLThumbnailProvider`) and the space-bar preview panel
//! (`QLPreviewProvider`). macOS allows one extension point per bundle,
//! so `packaging/macos/quicklook` wraps this same executable in two
//! `.appex` bundles whose Info.plists name different principal classes;
//! both classes are registered at startup, and `NSExtensionMain` picks
//! whichever the host asked for.
//!
//! Both providers answer the same way: render the file through
//! [`schist_preview`], write a PNG to this process's temporary
//! directory, and hand Quick Look its URL.
//!
//! Run it directly to see what Quick Look would see:
//!
//! ```sh
//! schist-quicklook --render file.afphoto out.png [max-edge]
//! ```

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("schist-quicklook is a macOS app extension; there is nothing to run here");
    std::process::exit(1);
}
