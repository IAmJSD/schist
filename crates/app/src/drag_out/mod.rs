//! Dragging photos out of the window, onto the desktop.
//!
//! gpui's drag-and-drop is internal: a drag that leaves the window has
//! nowhere to go. Dropping a photo on Finder, Explorer or a Linux file
//! manager is the platform's own protocol — a pasteboard session on
//! macOS, OLE on Windows, XDND on X11, a data source through our gpui
//! fork on Wayland — so each gets its own small implementation here,
//! behind one call.
//!
//! Every platform advertises *copy* and only copy: the gallery watches
//! folders in place, and a drag that quietly moved someone's photos
//! out of their library would be a poor surprise.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
mod wayland;
#[cfg(all(target_os = "windows", not(target_arch = "wasm32")))]
mod win;
#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
mod x11;

/// Whether the pointer is over some *other* application's window right
/// now — the moment a drag stops being ours and becomes the desktop's.
///
/// "Left our rectangle" is the tempting test and the wrong one: a
/// Finder window sitting on top of the gallery is over us in
/// coordinates and in front of us on screen, so a drop there would
/// never hand off. Each platform can say who owns the pixel under the
/// pointer, so each is asked.
#[allow(unused_variables)]
pub fn over_foreign_window(window: &gpui::Window) -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::over_foreign_window(window)
    }
    #[cfg(all(target_os = "windows", not(target_arch = "wasm32")))]
    {
        win::over_foreign_window(window)
    }
    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    {
        // gpui's own guess, so the two never disagree about which
        // display server is drawing the window.
        if gpui::guess_compositor() == "Wayland" {
            wayland::over_foreign_window(window)
        } else {
            x11::over_foreign_window(window)
        }
    }
    #[cfg(not(any(
        target_os = "macos",
        all(target_os = "windows", not(target_arch = "wasm32")),
        all(target_os = "linux", not(target_arch = "wasm32")),
    )))]
    {
        false
    }
}

/// Hand `paths` to the platform's drag-and-drop, so dropping them on a
/// file manager copies them there. Returns whether a session started —
/// `false` leaves the caller's internal drag alone.
///
/// Called from a mouse handler on the main thread, with the button
/// still down: that is what every platform means by "a drag is in
/// progress", and none of them can start one afterwards.
#[allow(unused_variables)]
pub fn start(paths: &[PathBuf], window: &gpui::Window) -> bool {
    let paths: Vec<PathBuf> = paths.iter().filter(|p| p.exists()).cloned().collect();
    if paths.is_empty() {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        mac::start(&paths, window)
    }
    #[cfg(all(target_os = "windows", not(target_arch = "wasm32")))]
    {
        win::start(&paths, window)
    }
    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    {
        if gpui::guess_compositor() == "Wayland" {
            wayland::start(&paths, window)
        } else {
            x11::start(&paths, window)
        }
    }
    #[cfg(not(any(
        target_os = "macos",
        all(target_os = "windows", not(target_arch = "wasm32")),
        all(target_os = "linux", not(target_arch = "wasm32")),
    )))]
    {
        false
    }
}

/// The `text/uri-list` body for these paths — the payload X11 and most
/// of the world agree on. Absolute, percent-encoded, CRLF-separated.
#[cfg(any(all(target_os = "linux", not(target_arch = "wasm32")), test))]
pub(crate) fn uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        let Some(text) = path.to_str() else { continue };
        out.push_str("file://");
        for byte in text.bytes() {
            // Unreserved characters plus the separators a path is made
            // of; everything else (spaces, '#', '?', non-ASCII) is
            // escaped, or file managers truncate the name.
            if byte.is_ascii_alphanumeric() || b"-_.~/".contains(&byte) {
                out.push(byte as char);
            } else {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_escape_what_file_managers_would_otherwise_cut() {
        let list = uri_list(&[
            PathBuf::from("/photos/a b#c.jpg"),
            PathBuf::from("/photos/caf\u{e9}.png"),
        ]);
        assert_eq!(
            list,
            "file:///photos/a%20b%23c.jpg\r\nfile:///photos/caf%C3%A9.png\r\n"
        );
    }
}
