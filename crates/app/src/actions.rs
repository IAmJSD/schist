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

/// Open a registered filter's dialog by id (e.g. "filter.gaussian_blur").
#[derive(Clone, PartialEq, Debug, serde::Deserialize, gpui::Action)]
#[action(namespace = schist, no_json)]
pub struct OpenFilter {
    pub id: String,
}

/// Run one of the shell's own menu items. The in-window menu bar calls
/// `panels::run_app_item` directly; the macOS menu bar can only carry
/// actions, so it dispatches this.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = schist, no_json)]
pub struct RunAppItem {
    pub item: AppItem,
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
        HideApp,
        HideOthers,
        ShowAll,
        Quit,
    ]
);

/// An item in the menu bar that the shell itself handles, as opposed to a
/// plugin command, a filter or an adjustment. Named here rather than in
/// `panels` because [`RunAppItem`] carries one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AppItem {
    New,
    Open,
    Close,
    Save,
    SaveAs,
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    ZoomActual,
    ImageSize,
    CanvasSize,
    FreeTransform,
    Crop,
    Plugins,
    Export,
    AssignProfile,
    ConvertProfile,
    ProofColors,
    ToggleRulers,
    ToggleGrid,
    ToggleGuides,
    ToggleExtras,
    ToggleSnap,
    ClearGuides,
    ScreenModeItem,
    Preferences,
    CheckForUpdates,
    LayerStyleItem,
    SelectExpand,
    SelectContract,
    SelectBorder,
    SelectSmooth,
    SelectFeatherItem,
    ColorRangeItem,
    ModeRgb,
    ModeGrayscale,
    ModeCmyk,
    ModeLab,
    ModeIndexed,
    AutoTone,
    AutoContrast,
    AutoColor,
    RotateCw,
    RotateCcw,
    Rotate180,
    FlipCanvasH,
    FlipCanvasV,
    Trim,
    /// Apply an adjustment to the pixels rather than adding a layer.
    ApplyAdjustment(schist_core::AdjustmentKind),
    StrokeItem,
    FillItem,
    ContentAwareFill,
    TransformSelection,
    ContentAwareScaleItem,
    FilterGalleryItem,
    ManageModels,
    NewLayerComp,
    ExportArtboards,
    ExportSlices,
    RotateViewCw,
    RotateViewCcw,
    ResetView,
    ClearNotes,
    ClearCounts,
    ApplyLayerComp(usize),
    DeleteLayerComp(usize),
    LiquifyItem,
    PuppetWarpItem,
    VanishingPointItem,
    PathFill,
    PathStroke,
    PathToSelection,
    PathDelete,
}

/// The string [`AddAdjustment`] carries for each adjustment kind. Kept
/// stable: these ids appear in the default keymap.
pub fn adjustment_id(kind: schist_core::AdjustmentKind) -> Option<&'static str> {
    use schist_core::AdjustmentKind::*;
    Some(match kind {
        Levels => "levels",
        Curves => "curves",
        HueSaturation => "hue_saturation",
        BrightnessContrast => "brightness_contrast",
        BlackWhite => "black_white",
        SolidColor => "solid_color",
        GradientFill => "gradient_fill",
        PatternFill => "pattern_fill",
        Invert => "invert",
        Posterize => "posterize",
        Threshold => "threshold",
        ColorBalance => "color_balance",
        Vibrance => "vibrance",
        Exposure => "exposure",
        PhotoFilter => "photo_filter",
        GradientMap => "gradient_map",
        SelectiveColor => "selective_color",
        ChannelMixer => "channel_mixer",
        // A kind read from a PSD we have no editor for; there is nothing
        // to create one from.
        Other(_) => return None,
    })
}

/// Inverse of [`adjustment_id`].
pub fn adjustment_from_id(id: &str) -> Option<schist_core::AdjustmentKind> {
    schist_adjustments::Params::creatable()
        .iter()
        .copied()
        .find(|&kind| adjustment_id(kind) == Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_creatable_adjustment_has_an_id() {
        // A kind with no id is one the Adjust menu would drop on macOS,
        // where the item has to be carried by an action rather than a
        // closure.
        for &kind in schist_adjustments::Params::creatable() {
            let id =
                adjustment_id(kind).unwrap_or_else(|| panic!("{} has no id", kind.display_name()));
            assert_eq!(adjustment_from_id(id), Some(kind));
        }
    }

    #[test]
    fn default_keymap_adjustment_ids_resolve() {
        // The ids the built-in ⌘L/⌘M/⌘U/⌘I bindings are written with.
        for id in ["levels", "curves", "hue_saturation", "invert"] {
            assert!(adjustment_from_id(id).is_some(), "{id} no longer resolves");
        }
    }
}
