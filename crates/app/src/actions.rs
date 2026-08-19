//! GPUI action types. Plugin commands and tools are dynamic (registered at
//! runtime), so they route through two data-carrying actions rather than one
//! action type per command.

use gpui::actions;

/// Run a registered plugin command by id (e.g. "edit.undo").
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = photoslop, no_json)]
pub struct RunCommand {
    pub id: String,
}

/// Activate a registered tool by id (e.g. "brush").
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = photoslop, no_json)]
pub struct ActivateTool {
    pub id: String,
}

/// Set tool opacity (digit keys: 1 => 10% … 0 => 100%).
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = photoslop, no_json)]
pub struct SetToolOpacity {
    pub percent: u32,
}

actions!(
    photoslop,
    [
        NewFile,
        OpenFile,
        SaveFileAs,
        ZoomIn,
        ZoomOut,
        ZoomFit,
        ZoomActual,
        BrushSmaller,
        BrushLarger,
        SwapColors,
        DefaultColors,
        CancelGesture,
        Quit,
    ]
);
