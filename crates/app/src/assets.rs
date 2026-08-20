//! Embedded asset source: monochrome SVG line icons, tinted by the
//! element's text color at render time.

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $((concat!("icons/", $name, ".svg"),
               include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*
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
