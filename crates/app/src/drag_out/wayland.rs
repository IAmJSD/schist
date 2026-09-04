//! Dragging photos out of the window under native Wayland.
//!
//! The one platform where the toolkit has to do it: a Wayland client
//! can only begin a drag with the serial of the button press that
//! started it, and only gpui's client holds that. So this leans on
//! `Window::start_native_drag`, added to our gpui fork — a data source
//! offering `text/uri-list`, copy only — and the compositor runs the
//! drag from there, delivering it to whichever surface the pointer
//! ends on.
//!
//! The trigger differs from the other platforms too. Nothing on
//! Wayland will say whose window is under the pointer, but the
//! implicit grab of a held button keeps motion flowing to us with
//! coordinates past our own edges, so "left our bounds" is what a
//! drag to another window looks like from here. A file manager laid
//! *over* the gallery is the one case this cannot see.

use std::path::PathBuf;

/// Whether the pointer has left the window's own rectangle.
pub(super) fn over_foreign_window(window: &gpui::Window) -> bool {
    let at = window.mouse_position();
    let size = window.viewport_size();
    at.x < gpui::px(0.0) || at.y < gpui::px(0.0) || at.x > size.width || at.y > size.height
}

pub(super) fn start(paths: &[PathBuf], window: &gpui::Window) -> bool {
    let uris = super::uri_list(paths);
    if uris.is_empty() {
        return false;
    }
    window.start_native_drag("text/uri-list", uris.into_bytes())
}
