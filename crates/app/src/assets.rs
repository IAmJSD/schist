//! Embedded asset source: monochrome SVG line icons, tinted by the
//! element's text color at render time.
//!
//! Embedded natively, that is. The web build keeps nothing in the binary
//! it can fetch instead: the loading page downloads the same files (the
//! build script copies them beside the wasm) and this source serves them
//! from the map it left behind — see `crate::web`.

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

#[cfg(not(target_arch = "wasm32"))]
macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $((concat!("icons/", $name, ".svg"),
               include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*
        ];
    };
}
#[cfg(target_arch = "wasm32")]
macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[&str] = &[
            $(concat!("icons/", $name, ".svg")),*
        ];
    };
}

icons!(
    "move",
    "swap",
    "eyedropper",
    "hand",
    "zoom",
    "brush",
    "pencil",
    "eraser",
    "marquee-rect",
    "marquee-ellipse",
    "lasso",
    "wand",
    "eye",
    "eye-off",
    "chevron-right",
    "chevron-down",
    "folder",
    "layer-new",
    "duplicate",
    "trash",
    "group-new",
    "merge-down",
    "undo",
    "redo",
    "minus",
    "plus",
    "transform",
    "crop",
    "pen",
    "type",
    "clone",
    "dodge",
    "burn",
    "sponge",
    "gradient",
    "bucket",
    "shape-rect",
    "shape-ellipse",
    "shape-line",
    "shape-polygon",
    "check",
    "close",
    "filter",
    "adjust",
    "image-size",
    "settings",
    "search",
    "ai-claude",
    "ai-codex",
    "plugin",
    "navigator",
    "blur",
    "content-move",
    "direct-select",
    "eraser-background",
    "eraser-magic",
    "heal",
    "history-brush",
    "lasso-magnetic",
    "lasso-poly",
    "object-select",
    "patch",
    "path-select",
    "pen-curvature",
    "pen-freeform",
    "quick-select",
    "red-eye",
    "shape-custom",
    "sharpen",
    "smudge",
    "liquify",
    "puppet",
    "vanishing-point",
    "artboard",
    "count",
    "frame",
    "note",
    "slice",
);

pub struct Assets;

#[cfg(not(target_arch = "wasm32"))]
impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(target_arch = "wasm32")]
impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(crate::web::asset(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        // The name list still compiles in (it is how a missing fetch is
        // told apart from a name nothing ever shipped), so listing does
        // not depend on what arrived.
        Ok(ICONS
            .iter()
            .filter(|name| name.starts_with(path))
            .map(|name| SharedString::from(*name))
            .collect())
    }
}
