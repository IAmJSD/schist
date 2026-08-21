//! GPUI action types. Plugin commands and tools are dynamic (registered at
//! runtime), so they route through two data-carrying actions rather than one
//! action type per command.

use gpui::actions;

/// Run a registered plugin command by id (e.g. "edit.undo").
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = schist, no_json)]
pub struct RunCommand {
    pub id: String,
}

/// Activate a registered tool by id (e.g. "brush").
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = schist, no_json)]
pub struct ActivateTool {
    pub id: String,
}

/// Add an adjustment layer of the named kind.
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = schist, no_json)]
pub struct AddAdjustment {
    pub kind: String,
}

/// Step to the next tool in a toolbar group (Shift + the group's key).
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = schist, no_json)]
pub struct CycleToolGroup {
    pub group: String,
}

/// Set tool opacity (digit keys: 1 => 10% … 0 => 100%).
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = schist, no_json)]
pub struct SetToolOpacity {
    pub percent: u32,
}

actions!(
    schist,
    [
        NewFile,
        OpenFile,
        SaveFile,
        SaveFileAs,
        CloseTab,
        NextTab,
        PrevTab,
        ZoomIn,
        ZoomOut,
        ZoomFit,
        ZoomActual,
        BrushSmaller,
        BrushLarger,
        SwapColors,
        DefaultColors,
        CancelGesture,
        CommitGesture,
        ShowImageSize,
        ShowCanvasSize,
        ShowPreferences,
        ShowLayerStyle,
        ToggleRulers,
        ToggleGrid,
        ToggleGuides,
        ToggleExtras,
        ToggleSnap,
        ClearGuides,
        CycleScreenMode,
        TogglePanels,
        Quit,
    ]
);
