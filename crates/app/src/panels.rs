//! UI chrome: menu bar, tool options bar, toolbar, layers/history/color
//! panels, status bar.
//!
//! These render directly from the Workspace (third-party panel plugins get
//! their seam in M9; the registry shape in plugin-api reserves it). Icons
//! are monochrome SVGs from the embedded asset source, tinted by text
//! color — no emoji.

use crate::ui;
use crate::workspace::{ColorTarget, ContextTarget, Modal, Popup, Workspace};
use gpui::{
    canvas, deferred, div, img, px, svg, Context, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, RenderImage,
    SharedString, StatefulInteractiveElement as _, Styled, Window,
};
use photoslop_color::Rgba;
use photoslop_core::{BlendMode, Layer, LayerId, LayerKind};
use std::sync::Arc;

const PANEL_BG: u32 = 0x1A1A1A;
const PANEL_EDGE: u32 = 0x111111;
const TEXT_DIM: u32 = 0x9A9A9A;
const TEXT: u32 = 0xD8D8D8;
const ACCENT: u32 = 0x3A6EA5;
const HOVER: u32 = 0x2E2E2E;
const POPUP_BG: u32 = 0x242424;

fn swatch_hex(c: Rgba) -> gpui::Rgba {
    let [r, g, b, _] = c.to_u8();
    gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
}

pub fn icon(name: &str, size: f32, color: u32) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(size))
        .text_color(gpui::rgb(color))
}

trait ActiveExt: Styled + Sized {
    fn when_active(self, active: bool) -> Self {
        if active {
            self.bg(gpui::rgb(ACCENT))
        } else {
            self
        }
    }
}
impl<T: Styled> ActiveExt for T {}

// ===== menu bar =====

enum MenuEntry {
    /// A registered plugin command (label + keybind resolved from registry).
    Cmd(&'static str),
    /// An app-level item handled by the shell.
    App(&'static str, AppItem, Option<&'static str>),
    /// Create an adjustment layer of this kind.
    Adjustment(photoslop_core::AdjustmentKind),
    /// Open a registered filter's dialog.
    Filter(&'static str),
    /// A nested menu, opened by hovering its row.
    Sub(&'static str, Vec<MenuEntry>),
    /// An app item whose label is not known at compile time -- the names
    /// of layer comps, for instance.
    Dynamic(String, AppItem),
    Sep,
}

#[derive(Clone, Copy)]
enum AppItem {
    New,
    Open,
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
    ApplyAdjustment(photoslop_core::AdjustmentKind),
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

fn menus(ws: &Workspace) -> Vec<(&'static str, Vec<MenuEntry>)> {
    use AppItem::*;
    use MenuEntry::*;
    vec![
        (
            "File",
            vec![
                App("New", New, Some("cmd-n")),
                App("Open…", Open, Some("cmd-o")),
                App("Save", Save, Some("cmd-s")),
                App("Save As…", SaveAs, Some("cmd-shift-s")),
                App("Export…", Export, Some("cmd-shift-alt-s")),
                Sep,
                Sub(
                    "Export",
                    vec![
                        App("Artboards to PNG…", ExportArtboards, None),
                        App("Slices to PNG…", ExportSlices, None),
                    ],
                ),
                Sep,
                App("Plugins…", Plugins, None),
                App("Check for Updates…", CheckForUpdates, None),
                Sep,
                App("Quit", Quit, Some("cmd-q")),
            ],
        ),
        (
            "Edit",
            vec![
                Cmd("edit.undo"),
                Cmd("edit.redo"),
                Sep,
                Cmd("edit.cut"),
                Cmd("edit.copy"),
                Cmd("edit.copy_merged"),
                Cmd("edit.paste"),
                Cmd("edit.paste_in_place"),
                Sep,
                Cmd("edit.fill_foreground"),
                Cmd("edit.fill_background"),
                App("Fill…", FillItem, Some("shift-f5")),
                App("Stroke…", StrokeItem, None),
                App("Content-Aware Fill", ContentAwareFill, None),
                App("Content-Aware Scale…", ContentAwareScaleItem, None),
                App("Puppet Warp", PuppetWarpItem, None),
                Sep,
                App("Free Transform", FreeTransform, Some("cmd-t")),
                Sub(
                    "Transform",
                    vec![
                        App("Rotate 180°", Rotate180, None),
                        App("Rotate 90° Clockwise", RotateCw, None),
                        App("Rotate 90° Counter Clockwise", RotateCcw, None),
                        Sep,
                        App("Flip Horizontal", FlipCanvasH, None),
                        App("Flip Vertical", FlipCanvasV, None),
                    ],
                ),
            ],
        ),
        (
            "Image",
            vec![
                Sub(
                    "Mode",
                    vec![
                        App("RGB Color", ModeRgb, None),
                        App("Grayscale", ModeGrayscale, None),
                        App("CMYK Color", ModeCmyk, None),
                        App("Lab Color", ModeLab, None),
                        App("Indexed Color", ModeIndexed, None),
                    ],
                ),
                Sub("Adjustments", destructive_adjustment_entries()),
                Sep,
                App("Auto Tone", AutoTone, None),
                App("Auto Contrast", AutoContrast, None),
                App("Auto Color", AutoColor, None),
                Sep,
                App("Image Size…", ImageSize, Some("cmd-alt-i")),
                App("Canvas Size…", CanvasSize, Some("cmd-alt-c")),
                Sub(
                    "Image Rotation",
                    vec![
                        App("180°", Rotate180, None),
                        App("90° Clockwise", RotateCw, None),
                        App("90° Counter Clockwise", RotateCcw, None),
                        Sep,
                        App("Flip Canvas Horizontal", FlipCanvasH, None),
                        App("Flip Canvas Vertical", FlipCanvasV, None),
                    ],
                ),
                App("Crop to Selection", Crop, None),
                App("Trim", Trim, None),
                Sep,
                App("Assign Profile…", AssignProfile, None),
                App("Convert to Profile…", ConvertProfile, None),
            ],
        ),
        (
            "Select",
            vec![
                Cmd("select.all"),
                Cmd("select.deselect"),
                Cmd("select.reselect"),
                Cmd("select.inverse"),
                Sep,
                App("Color Range…", ColorRangeItem, None),
                Sep,
                Sub(
                    "Modify",
                    vec![
                        App("Border…", SelectBorder, None),
                        App("Smooth…", SelectSmooth, None),
                        App("Expand…", SelectExpand, None),
                        App("Contract…", SelectContract, None),
                        App("Feather…", SelectFeatherItem, None),
                    ],
                ),
                Sep,
                App("Transform Selection", TransformSelection, None),
                Sep,
                Cmd("select.grow"),
                Cmd("select.similar"),
                Sep,
                Cmd("select.save"),
                Cmd("select.load"),
            ],
        ),
        (
            "Layer",
            vec![
                Cmd("layer.new"),
                Cmd("layer.duplicate"),
                Cmd("layer.delete"),
                Sep,
                Cmd("layer.smart_object"),
                Cmd("layer.rasterize"),
                Sep,
                App("Layer Style…", LayerStyleItem, None),
                Sep,
                Sub("Layer Comps", layer_comp_entries(ws)),
                Sep,
                Sub(
                    "Path",
                    vec![
                        App("Fill Path", PathFill, None),
                        App("Stroke Path", PathStroke, None),
                        App("Make Selection", PathToSelection, None),
                        Sep,
                        App("Delete Path", PathDelete, None),
                    ],
                ),
                Sep,
                Cmd("layer.group"),
                Cmd("layer.merge_down"),
                Cmd("layer.merge_visible"),
            ],
        ),
        (
            "Adjust",
            photoslop_adjustments::Params::creatable()
                .iter()
                .map(|&k| Adjustment(k))
                .collect(),
        ),
        ("Filter", {
            // Liquify and Vanishing Point sit above the categories, as
            // they do in Photoshop's Filter menu.
            let mut out = vec![
                App("Filter Gallery…", FilterGalleryItem, None),
                Filter("filter.camera_raw"),
                App("Liquify", LiquifyItem, None),
                App("Vanishing Point", VanishingPointItem, None),
                Sep,
            ];
            out.extend(filter_menu_entries());
            out
        }),
        (
            "View",
            vec![
                App("Rotate View Clockwise", RotateViewCw, None),
                App("Rotate View Counter Clockwise", RotateViewCcw, None),
                App("Reset View", ResetView, None),
                Sep,
                App("Zoom In", ZoomIn, Some("cmd-=")),
                App("Zoom Out", ZoomOut, Some("cmd--")),
                App("Fit on Screen", ZoomFit, Some("cmd-0")),
                App("100%", ZoomActual, Some("cmd-1")),
                Sep,
                App("Rulers", ToggleRulers, Some("cmd-r")),
                App("Grid", ToggleGrid, Some("cmd-'")),
                App("Guides", ToggleGuides, Some("cmd-;")),
                App("Extras", ToggleExtras, Some("cmd-h")),
                App("Snap", ToggleSnap, Some("cmd-shift-;")),
                App("Clear Guides", ClearGuides, Some("cmd-alt-;")),
                App("Clear Notes", ClearNotes, None),
                App("Clear Count", ClearCounts, None),
                Sep,
                App("Screen Mode", ScreenModeItem, Some("f")),
                App("Proof Colors", ProofColors, None),
                Sep,
                App("Preferences…", Preferences, Some("cmd-k")),
            ],
        ),
    ]
}

/// Filters grouped by category, in registration order.
fn filter_menu_entries() -> Vec<MenuEntry> {
    // The ids are static strings owned by the plugins; the menu resolves
    // names from the registry at render time. Categories nest, as in
    // Photoshop's Filter menu.
    FILTER_GROUPS
        .iter()
        .map(|(name, ids)| {
            let mut entries: Vec<MenuEntry> = ids.iter().map(|id| MenuEntry::Filter(id)).collect();
            // The Neural Filters need somewhere to fetch their models.
            if *name == "Neural Filters" {
                entries.push(MenuEntry::Sep);
                entries.push(MenuEntry::App(
                    "Manage Models…",
                    AppItem::ManageModels,
                    None,
                ));
            }
            MenuEntry::Sub(name, entries)
        })
        .collect()
}

/// The Layer Comps submenu: capture a new one, then the existing comps,
/// each of which applies on click and can be deleted from beside it.
fn layer_comp_entries(ws: &Workspace) -> Vec<MenuEntry> {
    let mut out = vec![MenuEntry::App(
        "New Layer Comp",
        AppItem::NewLayerComp,
        None,
    )];
    let comps: Vec<String> = ws
        .doc
        .as_ref()
        .map(|d| d.layer_comps.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default();
    if !comps.is_empty() {
        out.push(MenuEntry::Sep);
        for (i, name) in comps.iter().enumerate() {
            out.push(MenuEntry::Dynamic(name.clone(), AppItem::ApplyLayerComp(i)));
        }
        out.push(MenuEntry::Sep);
        for (i, name) in comps.iter().enumerate() {
            out.push(MenuEntry::Dynamic(
                format!("Delete {name}"),
                AppItem::DeleteLayerComp(i),
            ));
        }
    }
    out
}

/// Image ▸ Adjustments: the same list as the Adjust menu, but applied to
/// the pixels rather than as a layer.
fn destructive_adjustment_entries() -> Vec<MenuEntry> {
    photoslop_adjustments::Params::creatable()
        .iter()
        .filter(|k| !matches!(k, photoslop_core::AdjustmentKind::SolidColor))
        .map(|&k| MenuEntry::App(k.display_name(), AppItem::ApplyAdjustment(k), None))
        .collect()
}

/// Menu grouping for the built-in filters.
const FILTER_GROUPS: &[(&str, &[&str])] = &[
    (
        "Blur",
        &[
            "filter.average",
            "filter.box_blur",
            "filter.gaussian_blur",
            "filter.lens_blur",
            "filter.motion_blur",
            "filter.radial_blur",
            "filter.surface_blur",
        ],
    ),
    (
        "Distort",
        &[
            "filter.displace",
            "filter.pinch",
            "filter.polar",
            "filter.ripple",
            "filter.shear",
            "filter.spherize",
            "filter.twirl",
            "filter.wave",
            "filter.zigzag",
        ],
    ),
    (
        "Noise",
        &[
            "filter.add_noise",
            "filter.despeckle",
            "filter.dust_scratches",
            "filter.median",
            "filter.reduce_noise",
        ],
    ),
    (
        "Pixelate",
        &[
            "filter.color_halftone",
            "filter.crystallize",
            "filter.facet",
            "filter.fragment",
            "filter.mezzotint",
            "filter.mosaic",
            "filter.pointillize",
        ],
    ),
    (
        "Render",
        &[
            "filter.clouds",
            "filter.difference_clouds",
            "filter.fibers",
            "filter.lens_flare",
        ],
    ),
    (
        "Sharpen",
        &[
            "filter.sharpen",
            "filter.sharpen_edges",
            "filter.smart_sharpen",
            "filter.unsharp_mask",
        ],
    ),
    (
        "Stylize",
        &[
            "filter.diffuse",
            "filter.emboss",
            "filter.extrude",
            "filter.find_edges",
            "filter.glowing_edges",
            "filter.oil_paint",
            "filter.solarize",
            "filter.tiles",
            "filter.trace_contour",
            "filter.wind",
        ],
    ),
    (
        "Other",
        &[
            "filter.high_pass",
            "filter.maximum",
            "filter.minimum",
            "filter.offset",
        ],
    ),
    (
        "Neural Filters",
        &[
            "filter.neural.style_transfer",
            "filter.neural.skin_smoothing",
            "filter.neural.jpeg_artifacts",
            "filter.neural.colorize",
            "filter.neural.super_zoom",
            "filter.neural.color_transfer",
            "filter.neural.depth_blur",
        ],
    ),
];

fn keybind_hint(kb: Option<&str>) -> String {
    let Some(kb) = kb else { return String::new() };
    let kb = if cfg!(target_os = "macos") {
        kb.to_string()
    } else {
        kb.replace("cmd-", "ctrl-")
    };
    kb.split('-')
        .map(|part| match part {
            "cmd" => "Cmd".to_string(),
            "ctrl" => "Ctrl".to_string(),
            "shift" => "Shift".to_string(),
            "alt" => "Alt".to_string(),
            other if other.len() == 1 => other.to_uppercase(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Check state for the toggling menu items.
fn app_item_checked(ws: &Workspace, item: AppItem) -> Option<bool> {
    Some(match item {
        AppItem::ToggleRulers => ws.view.rulers,
        AppItem::ToggleGrid => ws.view.grid,
        AppItem::ToggleGuides => ws.view.guides,
        AppItem::ToggleExtras => ws.view.extras,
        AppItem::ToggleSnap => ws.view.snap,
        AppItem::ProofColors => ws.color.proof.is_some(),
        _ => return None,
    })
}

fn run_app_item(
    ws: &mut Workspace,
    item: AppItem,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match item {
        AppItem::New => ws.new_document(),
        AppItem::Open => crate::keymap::open_file_dialog(ws, window, cx),
        AppItem::Save => ws.save_current(window, cx),
        AppItem::SaveAs => crate::keymap::save_file_dialog(ws, window, cx),
        AppItem::Quit => cx.quit(),
        AppItem::ZoomIn => ws.zoom_by(1.25, None),
        AppItem::ZoomOut => ws.zoom_by(0.8, None),
        AppItem::ZoomFit => ws.fit_to_view(),
        AppItem::ZoomActual => {
            ws.zoom = 1.0;
            ws.editor.zoom = 1.0;
        }
        AppItem::ImageSize => {
            if let Some(doc) = ws.doc.as_ref() {
                let modal = Modal::ImageSize {
                    width: doc.width,
                    height: doc.height,
                    filter: ws.editor.resample,
                    link: true,
                };
                ws.open_modal(modal, cx);
            }
        }
        AppItem::CanvasSize => {
            if let Some(doc) = ws.doc.as_ref() {
                let modal = Modal::CanvasSize {
                    width: doc.width,
                    height: doc.height,
                    anchor: (0.5, 0.5),
                };
                ws.open_modal(modal, cx);
            }
        }
        AppItem::Plugins => ws.open_modal(Modal::PluginManager, cx),
        AppItem::Export => {
            let codec = ws
                .registry
                .codecs()
                .find(|c| c.can_export() && c.extensions().contains(&"png"))
                .or_else(|| ws.registry.codecs().find(|c| c.can_export()))
                .map(|c| c.id());
            if let Some(codec) = codec {
                ws.open_modal(
                    Modal::Export {
                        codec,
                        options: photoslop_plugin_api::ExportOptions::default(),
                    },
                    cx,
                );
            }
        }
        AppItem::AssignProfile => ws.open_modal(
            Modal::Profile {
                convert: false,
                selected: 0,
            },
            cx,
        ),
        AppItem::ConvertProfile => ws.open_modal(
            Modal::Profile {
                convert: true,
                selected: 0,
            },
            cx,
        ),
        AppItem::ProofColors => {
            let profile = photoslop_colormgmt::Profile::srgb();
            ws.toggle_proof(profile, cx);
        }
        AppItem::ToggleRulers => ws.toggle_rulers(cx),
        AppItem::ToggleGrid => ws.toggle_grid(cx),
        AppItem::ToggleGuides => ws.toggle_guides(cx),
        AppItem::ToggleExtras => ws.toggle_extras(cx),
        AppItem::ToggleSnap => ws.toggle_snap(cx),
        AppItem::ClearGuides => ws.clear_guides(cx),
        AppItem::ScreenModeItem => ws.cycle_screen_mode(cx),
        AppItem::Preferences => ws.open_modal(Modal::Preferences, cx),
        AppItem::ModeRgb => ws.set_color_mode(photoslop_color::ColorMode::Rgb, cx),
        AppItem::ModeGrayscale => ws.set_color_mode(photoslop_color::ColorMode::Grayscale, cx),
        AppItem::ModeCmyk => ws.set_color_mode(photoslop_color::ColorMode::Cmyk, cx),
        AppItem::ModeLab => ws.set_color_mode(photoslop_color::ColorMode::Lab, cx),
        AppItem::ModeIndexed => ws.set_color_mode(photoslop_color::ColorMode::Indexed, cx),
        AppItem::AutoTone => ws.auto_adjust(crate::workspace::AutoMode::Tone, cx),
        AppItem::AutoContrast => ws.auto_adjust(crate::workspace::AutoMode::Contrast, cx),
        AppItem::AutoColor => ws.auto_adjust(crate::workspace::AutoMode::Color, cx),
        AppItem::RotateCw => ws.transform_canvas(crate::workspace::CanvasTransform::Cw90, cx),
        AppItem::RotateCcw => ws.transform_canvas(crate::workspace::CanvasTransform::Ccw90, cx),
        AppItem::Rotate180 => ws.transform_canvas(crate::workspace::CanvasTransform::Rotate180, cx),
        AppItem::FlipCanvasH => ws.transform_canvas(crate::workspace::CanvasTransform::FlipH, cx),
        AppItem::FlipCanvasV => ws.transform_canvas(crate::workspace::CanvasTransform::FlipV, cx),
        AppItem::Trim => ws.trim(cx),
        AppItem::ApplyAdjustment(kind) => ws.apply_adjustment_destructive(kind, cx),
        AppItem::StrokeItem => ws.open_modal(
            Modal::Stroke {
                width: 3.0,
                position: photoslop_core::StrokePosition::Center,
            },
            cx,
        ),
        AppItem::FillItem => ws.open_modal(
            Modal::Fill {
                source: crate::workspace::FillSource::Foreground,
                opacity: 1.0,
            },
            cx,
        ),
        AppItem::ContentAwareFill => ws.content_aware_fill(cx),
        AppItem::ContentAwareScaleItem => {
            let (w, h) = ws
                .doc
                .as_ref()
                .map(|d| (d.width, d.height))
                .unwrap_or((1, 1));
            ws.open_modal(
                Modal::ContentAwareScale {
                    width: w,
                    height: h,
                },
                cx,
            )
        }
        AppItem::FilterGalleryItem => ws.show_filter_gallery(cx),
        AppItem::ManageModels => ws.open_modal(Modal::ModelManager, cx),
        AppItem::NewLayerComp => ws.new_layer_comp(cx),
        AppItem::ApplyLayerComp(i) => ws.apply_layer_comp(i, cx),
        AppItem::DeleteLayerComp(i) => ws.delete_layer_comp(i, cx),
        AppItem::ExportArtboards => ws.export_regions(false, window, cx),
        AppItem::ExportSlices => ws.export_regions(true, window, cx),
        AppItem::RotateViewCw => ws.rotate_view(std::f32::consts::FRAC_PI_8, cx),
        AppItem::RotateViewCcw => ws.rotate_view(-std::f32::consts::FRAC_PI_8, cx),
        AppItem::ResetView => ws.reset_view_rotation(cx),
        AppItem::ClearNotes => {
            if let Some(doc) = ws.doc.as_mut() {
                doc.notes.clear();
                doc.damage_all();
            }
            ws.after_change(cx);
        }
        AppItem::ClearCounts => {
            if let Some(doc) = ws.doc.as_mut() {
                doc.counts.clear();
                doc.damage_all();
            }
            ws.after_change(cx);
        }
        AppItem::LiquifyItem => ws.activate_tool("liquify", cx),
        AppItem::PuppetWarpItem => ws.activate_tool("puppet_warp", cx),
        AppItem::VanishingPointItem => ws.activate_tool("vanishing_point", cx),
        AppItem::TransformSelection => {
            if ws.doc.as_ref().is_some_and(|d| d.selection.is_empty()) {
                ws.status = "Transform Selection needs a selection".into();
                cx.notify();
            } else {
                ws.activate_tool("transform.selection", cx);
            }
        }
        AppItem::PathFill => ws.use_active_path(crate::workspace::PathOp::Fill, cx),
        AppItem::PathStroke => ws.use_active_path(crate::workspace::PathOp::Stroke, cx),
        AppItem::PathToSelection => ws.use_active_path(crate::workspace::PathOp::Select, cx),
        AppItem::PathDelete => ws.use_active_path(crate::workspace::PathOp::Delete, cx),
        AppItem::SelectExpand => ws.open_modal(
            Modal::SelectModify {
                kind: crate::workspace::ModifyKind::Expand,
                amount: 4.0,
            },
            cx,
        ),
        AppItem::SelectContract => ws.open_modal(
            Modal::SelectModify {
                kind: crate::workspace::ModifyKind::Contract,
                amount: 4.0,
            },
            cx,
        ),
        AppItem::SelectBorder => ws.open_modal(
            Modal::SelectModify {
                kind: crate::workspace::ModifyKind::Border,
                amount: 6.0,
            },
            cx,
        ),
        AppItem::SelectSmooth => ws.open_modal(
            Modal::SelectModify {
                kind: crate::workspace::ModifyKind::Smooth,
                amount: 4.0,
            },
            cx,
        ),
        AppItem::SelectFeatherItem => ws.open_modal(
            Modal::SelectModify {
                kind: crate::workspace::ModifyKind::Feather,
                amount: 2.0,
            },
            cx,
        ),
        AppItem::ColorRangeItem => {
            let fg = ws.editor.foreground;
            ws.open_modal(
                Modal::ColorRange {
                    tolerance: 40.0,
                    target: fg,
                },
                cx,
            )
        }
        AppItem::LayerStyleItem => {
            if let Some(id) = ws.doc.as_ref().and_then(|d| d.active_layer) {
                ws.show_layer_style(id, cx);
            }
        }
        AppItem::CheckForUpdates => ws.check_for_update(cx),
        AppItem::FreeTransform => ws.activate_tool("transform", cx),
        AppItem::Crop => {
            let rect = ws
                .doc
                .as_ref()
                .filter(|d| !d.selection.is_empty())
                .map(|d| d.selection.bounds().intersect(&d.canvas_rect()));
            match rect {
                Some(rect) if !rect.is_empty() => {
                    if let Some(doc) = ws.doc.as_mut() {
                        photoslop_tools_transform::crop_to(doc, rect);
                    }
                    ws.after_change(cx);
                    ws.fit_to_view();
                }
                _ => ws.status = "Crop to Selection needs a selection".into(),
            }
        }
    }
    cx.notify();
}

fn menu_row_checked(
    label: String,
    hint: String,
    checked: Option<bool>,
    on_click: impl Fn(&mut Workspace, &MouseDownEvent, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_2()
        .h(px(24.0))
        .hover(|s| s.bg(gpui::rgb(ACCENT)))
        .on_mouse_down(MouseButton::Left, cx.listener(on_click))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    // Fixed-width gutter so labels line up whether or not
                    // the item is checkable.
                    div()
                        .w(px(12.0))
                        .flex_none()
                        .children(checked.unwrap_or(false).then(|| icon("check", 10.0, TEXT))),
                )
                .child(div().text_size(px(12.0)).child(label)),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(hint),
        )
}

fn menu_row(
    label: String,
    hint: String,
    on_click: impl Fn(&mut Workspace, &MouseDownEvent, &mut Window, &mut Context<Workspace>) + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px_2()
        .h(px(24.0))
        .hover(|s| s.bg(gpui::rgb(ACCENT)))
        .on_mouse_down(MouseButton::Left, cx.listener(on_click))
        .child(div().text_size(px(12.0)).child(label))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(hint),
        )
}

pub fn menu_bar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let open = ws.open_popup;
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(28.0))
        .flex_none()
        .px_1()
        .bg(gpui::rgb(PANEL_BG))
        .border_b_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .children(
            menus(ws)
                .into_iter()
                .enumerate()
                .map(|(i, (title, entries))| {
                    let is_open = open == Some(Popup::Menu(i));
                    let mut button = div()
                        .relative()
                        .flex()
                        .items_center()
                        .px_2()
                        .h(px(22.0))
                        .rounded_sm()
                        .text_size(px(12.0))
                        .when_active(is_open)
                        .hover(|s| s.bg(gpui::rgb(HOVER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.toggle_popup(Popup::Menu(i), cx)),
                        )
                        .child(title);
                    if is_open {
                        button = button.child(deferred(
                            menu_panel(ws, entries, &[i], 24.0, 0.0, cx).on_mouse_down_out(
                                cx.listener(|ws, _e, _w, cx| ws.close_popup(cx)),
                            ),
                        ));
                    }
                    button
                }),
        )
}

/// One menu panel: the rows of `entries`, plus any submenu that is open
/// beneath them.
///
/// `path` identifies this panel in the menu tree, so hovering a row can
/// name the submenu to open without the panels needing to know about each
/// other. `top`/`left` place the panel relative to whatever opened it.
fn menu_panel(
    ws: &mut Workspace,
    entries: Vec<MenuEntry>,
    path: &[usize],
    top: f32,
    left: f32,
    cx: &mut Context<Workspace>,
) -> gpui::Div {
    let rows: Vec<gpui::AnyElement> = entries
        .into_iter()
        .enumerate()
        .map(|(row, entry)| menu_entry_row(ws, entry, path, row, cx))
        .collect();
    div()
        .absolute()
        .top(px(top))
        .left(px(left))
        .w(px(230.0))
        .py_1()
        .bg(gpui::rgb(POPUP_BG))
        .border_1()
        .border_color(gpui::rgb(0x3A3A3A))
        .rounded_sm()
        .shadow_lg()
        .occlude()
        .children(rows)
}

fn menu_entry_row(
    ws: &mut Workspace,
    entry: MenuEntry,
    path: &[usize],
    row: usize,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    match entry {
        MenuEntry::Sep => div()
            .h(px(1.0))
            .my_1()
            .bg(gpui::rgb(0x3A3A3A))
            .into_any_element(),
        MenuEntry::Cmd(id) => {
            let (label, hint) = ws
                .registry
                .command(id)
                .map(|c| (c.title.to_string(), keybind_hint(c.keybind)))
                .unwrap_or_else(|| (id.to_string(), String::new()));
            menu_row(
                label,
                hint,
                move |ws, _e, _w, cx| {
                    ws.close_popup(cx);
                    ws.run_command(id, cx);
                },
                cx,
            )
            .into_any_element()
        }
        MenuEntry::Adjustment(kind) => menu_row(
            kind.display_name().to_string(),
            String::new(),
            move |ws, _e, _w, cx| {
                ws.close_popup(cx);
                ws.add_adjustment(kind, cx);
            },
            cx,
        )
        .into_any_element(),
        MenuEntry::Filter(id) => {
            let name = ws
                .registry
                .filters()
                .find(|f| f.id() == id)
                .map(|f| format!("{}…", f.name()))
                .unwrap_or_else(|| id.to_string());
            menu_row(
                name,
                String::new(),
                move |ws, _e, _w, cx| {
                    ws.close_popup(cx);
                    ws.open_filter_dialog(id, cx);
                },
                cx,
            )
            .into_any_element()
        }
        MenuEntry::Dynamic(label, item) => menu_row(
            label,
            String::new(),
            move |ws, _e, window, cx| {
                ws.close_popup(cx);
                run_app_item(ws, item, window, cx);
            },
            cx,
        )
        .into_any_element(),
        MenuEntry::App(label, item, kb) => menu_row_checked(
            label.to_string(),
            keybind_hint(kb),
            app_item_checked(ws, item),
            move |ws, _e, window, cx| {
                ws.close_popup(cx);
                run_app_item(ws, item, window, cx);
            },
            cx,
        )
        .into_any_element(),
        MenuEntry::Sub(label, children) => {
            let mut here = path.to_vec();
            here.push(row);
            let open = ws.open_submenu == here;
            let hover_path = here.clone();
            let mut root = div()
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_2()
                .h(px(22.0))
                .text_size(px(12.0))
                .hover(|s| s.bg(gpui::rgb(HOVER)))
                // Photoshop opens submenus on hover, and the pointer has to
                // cross the parent row to reach the child panel anyway.
                .on_mouse_move(cx.listener(move |ws, _e: &MouseMoveEvent, _w, cx| {
                    if ws.open_submenu != hover_path {
                        ws.open_submenu = hover_path.clone();
                        cx.notify();
                    }
                }))
                .child(div().child(label))
                .child(icon("chevron-right", 10.0, TEXT_DIM));
            if open {
                // Sits alongside its own row, clear of this panel's width.
                // Not wrapped in `deferred`: the panel containing this row
                // already is, and GPUI does not allow nesting them.
                root = root.child(menu_panel(ws, children, &here, -4.0, 224.0, cx));
            }
            root.into_any_element()
        }
    }
}

// ===== sliders =====

#[derive(Clone, Copy, PartialEq)]
pub enum SliderTarget {
    /// A control the active tool declared for itself, mapped from the
    /// slider's 0..=1 ratio into the option's own range.
    ToolOption {
        key: &'static str,
        min: f32,
        max: f32,
    },
    BrushSize,
    BrushHardness,
    ToolOpacity,
    LayerOpacity(LayerId),
    ForegroundR,
    ForegroundG,
    ForegroundB,
}

fn slider_get(ws: &Workspace, target: SliderTarget) -> f32 {
    match target {
        SliderTarget::ToolOption { key, min, max } => {
            let v = ws
                .registry
                .tools()
                .find(|t| t.id() == ws.editor.active_tool)
                .and_then(|t| t.options().into_iter().find(|o| o.key == key))
                .map(|o| o.value.num())
                .unwrap_or(min);
            ((v - min) / (max - min).max(1e-6)).clamp(0.0, 1.0)
        }
        SliderTarget::BrushSize => ((ws.editor.brush_size - 1.0) / 299.0).clamp(0.0, 1.0),
        SliderTarget::BrushHardness => ws.editor.brush_hardness,
        SliderTarget::ToolOpacity => ws.editor.tool_opacity,
        SliderTarget::LayerOpacity(id) => ws
            .doc
            .as_ref()
            .and_then(|d| d.tree.find(id))
            .map(|l| l.opacity)
            .unwrap_or(1.0),
        SliderTarget::ForegroundR => ws.editor.foreground.r,
        SliderTarget::ForegroundG => ws.editor.foreground.g,
        SliderTarget::ForegroundB => ws.editor.foreground.b,
    }
}

fn slider_set(ws: &mut Workspace, target: SliderTarget, ratio: f32, cx: &mut Context<Workspace>) {
    match target {
        SliderTarget::ToolOption { key, min, max } => ws.set_tool_option(
            key,
            photoslop_plugin_api::OptionValue::Num(min + ratio * (max - min)),
            cx,
        ),
        SliderTarget::BrushSize => ws.editor.brush_size = 1.0 + ratio * 299.0,
        SliderTarget::BrushHardness => ws.editor.brush_hardness = ratio,
        SliderTarget::ToolOpacity => ws.editor.tool_opacity = ratio,
        SliderTarget::LayerOpacity(id) => ws.set_layer_opacity_live(id, ratio),
        SliderTarget::ForegroundR => ws.editor.foreground.r = ratio,
        SliderTarget::ForegroundG => ws.editor.foreground.g = ratio,
        SliderTarget::ForegroundB => ws.editor.foreground.b = ratio,
    }
    if matches!(target, SliderTarget::LayerOpacity(_)) {
        ws.after_change(cx);
    } else {
        cx.notify();
    }
}

/// A horizontal slider. The track's live bounds are recorded via a nested
/// canvas so mouse positions can be mapped back to a 0..=1 ratio.
fn slider(
    id: &'static str,
    label: &'static str,
    display: String,
    target: SliderTarget,
    ws: &Workspace,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let ratio = slider_get(ws, target);
    let entity = cx.entity();
    let track = div()
        .relative()
        .w(px(72.0))
        .h(px(12.0))
        .flex_none()
        .rounded_sm()
        .bg(gpui::rgb(0x0E0E0E))
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(72.0 * ratio))
                .rounded_sm()
                .bg(gpui::rgb(ACCENT)),
        )
        .child(
            canvas(
                move |bounds, _window, cx| {
                    entity.update(cx, |ws, _| ws.record_slider_bounds(id, bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                ws.begin_slider(id, slider_get(ws, target));
                if let Some(r) = ws.slider_ratio(id, ev.position) {
                    slider_set(ws, target, r, cx);
                }
            }),
        )
        .on_mouse_move(cx.listener(move |ws, ev: &MouseMoveEvent, _w, cx| {
            if ev.pressed_button == Some(MouseButton::Left) && ws.dragging_slider(id) {
                if let Some(r) = ws.slider_ratio(id, ev.position) {
                    slider_set(ws, target, r, cx);
                }
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |ws, _ev: &MouseUpEvent, _w, cx| {
                if let Some(before) = ws.end_slider(id) {
                    if let SliderTarget::LayerOpacity(layer) = target {
                        ws.commit_layer_opacity(layer, before, cx);
                    }
                }
            }),
        );
    let mut row = div().flex().flex_row().items_center().gap_1();
    if !label.is_empty() {
        row = row.child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(TEXT_DIM))
                .child(label),
        );
    }
    row.child(track).child(
        div()
            .w(px(34.0))
            .flex_none()
            .text_size(px(11.0))
            .child(display),
    )
}

// ===== tool options bar =====

pub fn tool_options_bar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let tool_id = ws.editor.active_tool;
    let (tool_icon, tool_name) = ws
        .registry
        .tools()
        .find(|t| t.id() == tool_id)
        .map(|t| (t.icon(), t.name()))
        .unwrap_or(("move", "Move"));
    let is_paint = matches!(tool_id, "brush" | "pencil" | "eraser");

    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .h(px(32.0))
        .flex_none()
        .px_3()
        .bg(gpui::rgb(PANEL_BG))
        .border_b_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w(px(130.0))
                .flex_none()
                .child(icon(tool_icon, 15.0, TEXT))
                .child(div().text_size(px(12.0)).child(tool_name)),
        );
    if is_paint {
        bar = bar
            .child(slider(
                "opt-size",
                "Size",
                format!("{:.0}px", ws.editor.brush_size),
                SliderTarget::BrushSize,
                ws,
                cx,
            ))
            .child(slider(
                "opt-hard",
                "Hardness",
                format!("{:.0}%", ws.editor.brush_hardness * 100.0),
                SliderTarget::BrushHardness,
                ws,
                cx,
            ));
    }
    bar = bar.child(slider(
        "opt-opacity",
        "Opacity",
        format!("{:.0}%", ws.editor.tool_opacity * 100.0),
        SliderTarget::ToolOpacity,
        ws,
        cx,
    ));
    // Whatever else the active tool asked for.
    for opt in ws
        .registry
        .tools()
        .find(|t| t.id() == tool_id)
        .map(|t| t.options())
        .unwrap_or_default()
    {
        bar = bar.child(tool_option_control(ws, opt, cx));
    }
    bar
}

/// Render one plugin-declared option. The shell knows the three kinds, not
/// the tools.
fn tool_option_control(
    ws: &Workspace,
    opt: photoslop_plugin_api::ToolOption,
    cx: &mut Context<Workspace>,
) -> gpui::AnyElement {
    use photoslop_plugin_api::OptionKind;
    let key = opt.key;
    match opt.kind {
        OptionKind::Slider { min, max, suffix } => {
            let v = opt.value.num();
            // Coarse ranges read better without decimals.
            let display = if max - min > 20.0 {
                format!("{v:.0}{suffix}")
            } else {
                format!("{v:.1}{suffix}")
            };
            slider(
                key,
                opt.label,
                display,
                SliderTarget::ToolOption { key, min, max },
                ws,
                cx,
            )
            .into_any_element()
        }
        OptionKind::Toggle => {
            let on = opt.value.bool();
            ui::checkbox(
                opt.label,
                on,
                move |ws, cx| {
                    ws.set_tool_option(key, photoslop_plugin_api::OptionValue::Bool(!on), cx)
                },
                cx,
            )
            .into_any_element()
        }
        OptionKind::Choice(labels) => {
            let current = opt.value.index().min(labels.len().saturating_sub(1));
            // Wide enough for the longest thing it can say, so a dropdown
            // never has to truncate its own value.
            let longest = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            let width = (longest as f32 * 6.2 + 34.0).clamp(80.0, 210.0);
            let control = ui::dropdown(
                ui::Dropdown {
                    popup: Popup::Field(key),
                    is_open: ws.open_popup == Some(Popup::Field(key)),
                    current,
                    label: labels.get(current).copied().unwrap_or("").into(),
                    width,
                    options: labels
                        .iter()
                        .enumerate()
                        .map(|(i, l)| (SharedString::from(*l), i))
                        .collect(),
                },
                move |ws, i, cx| {
                    ws.set_tool_option(key, photoslop_plugin_api::OptionValue::Choice(i), cx)
                },
                cx,
            );
            // Sliders carry their own label; a dropdown does not, and an
            // unlabelled one reading "Point Sample" does not say what it
            // is choosing.
            if opt.label.is_empty() {
                return control.into_any_element();
            }
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(TEXT_DIM))
                        .child(opt.label),
                )
                .child(control)
                .into_any_element()
        }
    }
}

// ===== toolbar =====

pub fn toolbar(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active = ws.editor.active_tool;
    // One slot per group, showing whichever tool that group last used —
    // Photoshop's nested tools, so twenty tools take eleven slots.
    let slots: Vec<(&'static str, &'static str, bool, bool)> = ws
        .tool_groups
        .clone()
        .into_iter()
        .map(|(group, tools)| {
            let shown = ws.group_tool(group);
            let icon = ws
                .registry
                .tool_mut(shown)
                .map(|t| t.icon())
                .unwrap_or("move");
            (group, icon, tools.contains(&active), tools.len() > 1)
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .w(px(40.0))
        .flex_none()
        .items_center()
        .bg(gpui::rgb(PANEL_BG))
        .border_r_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .pt_1()
        .children(
            slots
                .into_iter()
                .map(|(group, icon_name, is_active, has_siblings)| {
                    div()
                        .relative()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(30.0))
                        .my(px(1.0))
                        .rounded_sm()
                        .when_active(is_active)
                        .hover(move |s| if is_active { s } else { s.bg(gpui::rgb(HOVER)) })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                                ws.press_tool_group(group, ev.position, cx);
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |ws, _ev, _w, cx| {
                                ws.release_tool_group(group, cx);
                            }),
                        )
                        // Right-click opens the flyout immediately, for
                        // people who don't want to wait out the hold.
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                                ws.open_tool_flyout(group, ev.position, cx);
                            }),
                        )
                        .child(icon(
                            icon_name,
                            16.0,
                            if is_active { 0xFFFFFF } else { TEXT },
                        ))
                        .children(has_siblings.then(|| {
                            // The corner mark that means "more tools here".
                            div()
                                .absolute()
                                .right(px(2.0))
                                .bottom(px(2.0))
                                .size(px(4.0))
                                .bg(gpui::rgb(if is_active { 0xFFFFFF } else { TEXT_DIM }))
                        }))
                }),
        )
        .child(color_wells(ws, cx))
}

/// The flyout listing a group's tools, opened by holding or right-clicking
/// its toolbar slot.
pub fn tool_flyout(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    let (group, position) = ws.tool_flyout?;
    let active = ws.editor.active_tool;
    let shortcut = ws
        .group_shortcut(group)
        .map(|s| s.to_uppercase())
        .unwrap_or_default();
    let tools: Vec<&'static str> = ws
        .tool_groups
        .iter()
        .find(|(g, _)| *g == group)
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    let rows: Vec<gpui::AnyElement> = tools
        .into_iter()
        .map(|id| {
            let (name, icon_name) = ws
                .registry
                .tool_mut(id)
                .map(|t| (t.name(), t.icon()))
                .unwrap_or((id, "move"));
            let selected = id == active;
            let shortcut = shortcut.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(24.0))
                .when_active(selected)
                .hover(move |s| if selected { s } else { s.bg(gpui::rgb(HOVER)) })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |ws, _e, _w, cx| {
                        ws.close_tool_flyout(cx);
                        ws.activate_tool(id, cx);
                    }),
                )
                .child(icon(icon_name, 14.0, TEXT))
                .child(div().flex_grow().text_size(px(12.0)).child(name))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(TEXT_DIM))
                        .child(shortcut),
                )
                .into_any_element()
        })
        .collect();

    Some(
        deferred(
            div()
                .absolute()
                // Sits just right of the toolbar, level with the slot.
                .left(px(42.0))
                .top(px(f32::from(position.y) - 12.0))
                .w(px(200.0))
                .py_1()
                .bg(gpui::rgb(POPUP_BG))
                .border_1()
                .border_color(gpui::rgb(0x3A3A3A))
                .rounded_sm()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_tool_flyout(cx)))
                .children(rows),
        )
        .into_any_element(),
    )
}

fn color_wells(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .relative()
        .size(px(30.0))
        .mt_2()
        .child(
            div()
                .absolute()
                .bottom_0()
                .right_0()
                .size(px(18.0))
                .bg(swatch_hex(ws.editor.background))
                .border_1()
                .border_color(gpui::rgb(0x777777))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _ev, _w, cx| {
                        ws.open_color_picker(ColorTarget::Background, cx)
                    }),
                ),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size(px(18.0))
                .bg(swatch_hex(ws.editor.foreground))
                .border_1()
                .border_color(gpui::rgb(0xEEEEEE))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _ev, _w, cx| {
                        ws.open_color_picker(ColorTarget::Foreground, cx)
                    }),
                ),
        )
        // The empty corner between the two wells, which is where
        // Photoshop puts the swap arrows too.
        .child(
            div()
                .absolute()
                .top(px(-1.0))
                .right(px(-1.0))
                .size(px(11.0))
                .child(icon("swap", 11.0, TEXT_DIM))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _ev, _w, cx| {
                        std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
                        cx.notify();
                    }),
                ),
        )
}

// ===== side panels =====

pub fn side_panels(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w(px(260.0))
        .flex_none()
        .bg(gpui::rgb(PANEL_BG))
        .border_l_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(navigator(ws, cx))
        .child(color_panel(ws, cx))
        .child(layers_panel(ws, cx))
        .child(history_panel(ws, cx))
}

const PALETTE: [u32; 16] = [
    0x000000, 0xFFFFFF, 0x808080, 0xC0C0C0, 0xE81E25, 0xFF7F27, 0xFFF200, 0x22B14C, 0x00A2E8,
    0x3F48CC, 0xA349A4, 0xB97A57, 0xFFAEC9, 0xFFC90E, 0xB5E61D, 0x99D9EA,
];

fn color_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let fg = ws.editor.foreground.to_u8();
    div()
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        .child(panel_title("Color"))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.open_context_menu(ContextTarget::Color, ev.position, cx);
            }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_1()
                .children(PALETTE.map(|hex| {
                    div()
                        .size(px(18.0))
                        .bg(gpui::rgb(hex))
                        .border_1()
                        .border_color(gpui::rgb(0x333333))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                                let color = Rgba::from_u8(
                                    ((hex >> 16) & 0xFF) as u8,
                                    ((hex >> 8) & 0xFF) as u8,
                                    (hex & 0xFF) as u8,
                                    255,
                                );
                                if ev.modifiers.alt {
                                    ws.editor.background = color;
                                } else {
                                    ws.editor.foreground = color;
                                }
                                cx.notify();
                            }),
                        )
                })),
        )
        .child(slider(
            "col-r",
            "R",
            format!("{}", fg[0]),
            SliderTarget::ForegroundR,
            ws,
            cx,
        ))
        .child(slider(
            "col-g",
            "G",
            format!("{}", fg[1]),
            SliderTarget::ForegroundG,
            ws,
            cx,
        ))
        .child(slider(
            "col-b",
            "B",
            format!("{}", fg[2]),
            SliderTarget::ForegroundB,
            ws,
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(TEXT_DIM))
                        .child(format!("#{:02X}{:02X}{:02X}", fg[0], fg[1], fg[2])),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(gpui::rgb(TEXT_DIM))
                        .hover(|s| s.text_color(gpui::rgb(TEXT)))
                        .child("Picker\u{2026}")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|ws, _e, _w, cx| {
                                ws.open_color_picker(ColorTarget::Foreground, cx)
                            }),
                        ),
                ),
        )
        // Photoshop's spectrum bar: drag along it to take a hue directly.
        .child(crate::color_picker::hue_ramp(ws, cx))
}

fn panel_title(name: &'static str) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(gpui::rgb(TEXT_DIM))
        .pb_1()
        .child(name.to_uppercase())
}

// ===== layers panel =====

struct LayerRow {
    id: LayerId,
    depth: usize,
    kind: RowKind,
    name: String,
    visible: bool,
    active: bool,
    open: bool,
    /// The layer has effects switched on, shown as Photoshop's fx badge.
    fx: bool,
    /// The layer is a smart object.
    smart: bool,
}

enum RowKind {
    Raster,
    Group,
    Adjustment,
}

fn flatten_layers(
    layers: &[Layer],
    depth: usize,
    active: Option<LayerId>,
    out: &mut Vec<LayerRow>,
) {
    for layer in layers.iter().rev() {
        let (kind, open) = match &layer.kind {
            LayerKind::Group(g) => (RowKind::Group, g.open),
            LayerKind::Adjustment(_) => (RowKind::Adjustment, false),
            LayerKind::Raster(_) => (RowKind::Raster, false),
        };
        out.push(LayerRow {
            id: layer.id,
            depth,
            kind,
            name: layer.name.clone(),
            visible: layer.visible,
            active: Some(layer.id) == active,
            open,
            fx: !layer.style.is_empty(),
            smart: layer.smart.is_some(),
        });
        if let LayerKind::Group(g) = &layer.kind {
            if g.open {
                flatten_layers(&g.children, depth + 1, active, out);
            }
        }
    }
}

fn icon_button(
    icon_name: &'static str,
    command: &'static str,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.0))
        .rounded_sm()
        .hover(|s| s.bg(gpui::rgb(HOVER)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| ws.run_command(command, cx)),
        )
        .child(icon(icon_name, 14.0, TEXT))
}

fn blend_mode_control(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let active_layer = ws.doc.as_ref().and_then(|d| d.active_layer);
    let current = active_layer
        .and_then(|id| ws.doc.as_ref().and_then(|d| d.tree.find(id)))
        .map(|l| l.blend)
        .unwrap_or(BlendMode::Normal);
    let is_open = ws.open_popup == Some(Popup::BlendModes);
    let mut button = div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_grow()
        .h(px(20.0))
        .px_1()
        .rounded_sm()
        .bg(gpui::rgb(0x0E0E0E))
        .text_size(px(11.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|ws, _e, _w, cx| ws.toggle_popup(Popup::BlendModes, cx)),
        )
        .child(current.display_name())
        .child(icon("chevron-down", 11.0, TEXT_DIM));
    if is_open {
        if let Some(layer_id) = active_layer {
            let rows: Vec<gpui::AnyElement> = BlendMode::layer_modes()
                .iter()
                .map(|&mode| {
                    let selected = mode == current;
                    div()
                        .px_2()
                        .h(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .text_size(px(11.0))
                        .when_active(selected)
                        .hover(move |s| if selected { s } else { s.bg(gpui::rgb(HOVER)) })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| {
                                ws.close_popup(cx);
                                ws.set_blend_mode(layer_id, mode, cx);
                            }),
                        )
                        .child(mode.display_name())
                        .into_any_element()
                })
                .collect();
            button = button.child(deferred(
                div()
                    .id("blend-modes")
                    .absolute()
                    .top(px(22.0))
                    .left_0()
                    .w(px(150.0))
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    .py_1()
                    .bg(gpui::rgb(POPUP_BG))
                    .border_1()
                    .border_color(gpui::rgb(0x3A3A3A))
                    .rounded_sm()
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_popup(cx)))
                    .children(rows),
            ));
        }
    }
    button
}

fn layers_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let mut rows = Vec::new();
    let active_layer = ws.doc.as_ref().and_then(|d| d.active_layer);
    if let Some(doc) = &ws.doc {
        flatten_layers(&doc.tree.layers, 0, doc.active_layer, &mut rows);
    }
    let thumbs: Vec<Option<Arc<RenderImage>>> =
        rows.iter().map(|r| ws.layer_thumbnail(r.id)).collect();
    let opacity_display = active_layer
        .map(|id| slider_get(ws, SliderTarget::LayerOpacity(id)))
        .map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_default();

    div()
        .flex()
        .flex_col()
        .flex_grow()
        .min_h(px(0.0))
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(panel_title("Layers"))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(blend_mode_control(ws, cx))
                .child(match active_layer {
                    Some(id) => slider(
                        "layer-opacity",
                        "",
                        opacity_display,
                        SliderTarget::LayerOpacity(id),
                        ws,
                        cx,
                    )
                    .into_any_element(),
                    None => div().into_any_element(),
                }),
        )
        .child(
            div()
                .id("layers-scroll")
                .flex()
                .flex_col()
                .flex_grow()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .children(rows.into_iter().zip(thumbs).map(|(row, thumb)| {
                    let id = row.id;
                    let is_active_row = row.active;
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .h(px(34.0))
                        .flex_none()
                        .rounded_sm()
                        .when_active(row.active)
                        .hover(move |s| {
                            if is_active_row {
                                s
                            } else {
                                s.bg(gpui::rgb(HOVER))
                            }
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| {
                                if let Some(doc) = &mut ws.doc {
                                    doc.active_layer = Some(id);
                                }
                                cx.notify();
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |ws, ev: &MouseDownEvent, _w, cx| {
                                ws.open_context_menu(ContextTarget::Layer(id), ev.position, cx);
                            }),
                        )
                        .child(
                            // Visibility eye.
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(20.0))
                                .flex_none()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| {
                                        if let Some(doc) = &mut ws.doc {
                                            let mut edit = doc.begin_edit("Toggle Visibility");
                                            edit.change_props(id, |l| l.visible = !l.visible);
                                            edit.commit();
                                        }
                                        ws.after_change(cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(icon(
                                    if row.visible { "eye" } else { "eye-off" },
                                    13.0,
                                    if row.visible { TEXT } else { TEXT_DIM },
                                )),
                        )
                        .child(div().w(px(row.depth as f32 * 12.0)).flex_none())
                        .child(match &row.kind {
                            RowKind::Group => div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(16.0))
                                .flex_none()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| {
                                        ws.toggle_group_open(id, cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(icon(
                                    if row.open {
                                        "chevron-down"
                                    } else {
                                        "chevron-right"
                                    },
                                    11.0,
                                    TEXT_DIM,
                                ))
                                .into_any_element(),
                            _ => div().w(px(0.0)).into_any_element(),
                        })
                        .child(
                            // Thumbnail (raster) or type icon.
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .w(px(38.0))
                                .h(px(30.0))
                                .flex_none()
                                .bg(gpui::rgb(0x0E0E0E))
                                .rounded_sm()
                                // Adjustment layers open their settings
                                // from the thumbnail, like Photoshop.
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |ws, _e, _w, cx| ws.edit_adjustment(id, cx)),
                                )
                                .child(match (&row.kind, thumb) {
                                    (RowKind::Raster, Some(t)) => {
                                        img(t).max_w(px(36.0)).max_h(px(28.0)).into_any_element()
                                    }
                                    (RowKind::Group, _) => {
                                        icon("folder", 14.0, TEXT_DIM).into_any_element()
                                    }
                                    _ => icon("adjust", 13.0, TEXT_DIM).into_any_element(),
                                }),
                        )
                        .child(
                            div()
                                .flex_grow()
                                .text_size(px(12.0))
                                .overflow_hidden()
                                .child(row.name),
                        )
                        .children(row.smart.then(|| {
                            div()
                                .flex_none()
                                .px_1()
                                .rounded_sm()
                                .text_size(px(10.0))
                                .text_color(gpui::rgb(TEXT_DIM))
                                .bg(gpui::rgb(0x2A2A2A))
                                .child("SO")
                        }))
                        .children(row.fx.then(|| {
                            div()
                                .flex_none()
                                .px_1()
                                .rounded_sm()
                                .text_size(px(10.0))
                                .text_color(gpui::rgb(TEXT_DIM))
                                .bg(gpui::rgb(0x2A2A2A))
                                .child("fx")
                        }))
                })),
        )
        .child(
            // Action buttons.
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_1()
                .pt_1()
                .border_t_1()
                .border_color(gpui::rgb(0x2A2A2A))
                .child(icon_button("layer-new", "layer.new", cx))
                .child(icon_button("group-new", "layer.group", cx))
                .child(icon_button("duplicate", "layer.duplicate", cx))
                .child(icon_button("merge-down", "layer.merge_down", cx))
                .child(icon_button("trash", "layer.delete", cx)),
        )
}

// ===== history panel =====

fn history_panel(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let (undo_entries, redo_entries): (Vec<String>, Vec<String>) = ws
        .doc
        .as_ref()
        .map(|d| {
            (
                d.history.entries().iter().map(|e| e.name.clone()).collect(),
                // Most-recently-undone first == next redo first.
                d.history
                    .redo_entries()
                    .iter()
                    .rev()
                    .map(|e| e.name.clone())
                    .collect(),
            )
        })
        .unwrap_or_default();
    let n_undo = undo_entries.len() as i32;

    div()
        .flex()
        .flex_col()
        .h(px(150.0))
        .flex_none()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(panel_title("History"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(icon_button("undo", "edit.undo", cx))
                        .child(icon_button("redo", "edit.redo", cx)),
                ),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.open_context_menu(ContextTarget::History, ev.position, cx);
            }),
        )
        .child(
            div()
                .id("history-scroll")
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .flex_grow()
                .min_h(px(0.0))
                .children(undo_entries.into_iter().enumerate().map(|(i, name)| {
                    // Jump so entry i becomes the last applied edit.
                    let steps = (i as i32 + 1) - n_undo;
                    let is_current = i as i32 + 1 == n_undo;
                    div()
                        .px_1()
                        .h(px(19.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .rounded_sm()
                        .when_active(is_current)
                        .hover(move |s| {
                            if is_current {
                                s
                            } else {
                                s.bg(gpui::rgb(HOVER))
                            }
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.history_jump(steps, cx)),
                        )
                        .child(name)
                }))
                .children(redo_entries.into_iter().enumerate().map(|(j, name)| {
                    let steps = j as i32 + 1;
                    div()
                        .px_1()
                        .h(px(19.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .rounded_sm()
                        .text_color(gpui::rgb(0x666666))
                        .hover(|s| s.bg(gpui::rgb(HOVER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |ws, _e, _w, cx| ws.history_jump(steps, cx)),
                        )
                        .child(name)
                })),
        )
}

// ===== status bar =====

pub fn status_bar(ws: &Workspace) -> impl IntoElement {
    let title = ws
        .doc
        .as_ref()
        .map(|d| {
            format!(
                "{}{}  {}×{}",
                d.title,
                if d.dirty { " •" } else { "" },
                d.width,
                d.height
            )
        })
        .unwrap_or_else(|| "No document".into());
    let zoom = format!("{:.0}%", ws.zoom * 100.0);
    let brush = format!("{:.0}px", ws.editor.brush_size);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .h(px(24.0))
        .flex_none()
        .px_2()
        .bg(gpui::rgb(0x161616))
        .border_t_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .text_size(px(11.0))
        .text_color(gpui::rgb(TEXT_DIM))
        .child(title)
        .child(zoom)
        .child(brush)
        .child(div().flex_grow())
        .child(ws.status.clone())
}

// ===== context menus =====

/// One entry in a right-click menu.
enum ContextEntry {
    /// Run a registered command.
    Cmd(&'static str),
    /// An app-level action handled inline.
    App(&'static str, ContextAction),
    Sep,
}

#[derive(Clone, Copy)]
enum ContextAction {
    LayerProperties(LayerId),
    LayerStyle(LayerId),
    ToggleVisibility(LayerId),
    ZoomFit,
    ZoomActual,
    SwapColors,
    DefaultColors,
    ClearGuides,
}

fn context_entries(target: ContextTarget) -> Vec<ContextEntry> {
    use ContextAction::*;
    use ContextEntry::*;
    match target {
        ContextTarget::Layer(id) => vec![
            App("Blending Options…", LayerStyle(id)),
            App("Layer Properties…", LayerProperties(id)),
            App("Show/Hide Layer", ToggleVisibility(id)),
            Sep,
            Cmd("layer.duplicate"),
            Cmd("layer.delete"),
            Sep,
            Cmd("layer.smart_object"),
            Cmd("layer.rasterize"),
            Sep,
            Cmd("layer.group"),
            Cmd("layer.clipping_mask"),
            Cmd("layer.add_mask"),
            Sep,
            Cmd("layer.raise"),
            Cmd("layer.lower"),
            Cmd("layer.to_front"),
            Cmd("layer.to_back"),
            Sep,
            Cmd("layer.merge_down"),
            Cmd("layer.merge_visible"),
            Cmd("layer.flatten"),
        ],
        ContextTarget::History => vec![Cmd("edit.undo"), Cmd("edit.redo")],
        ContextTarget::Color => vec![
            App("Swap Colors", SwapColors),
            App("Reset to Black and White", DefaultColors),
        ],
        ContextTarget::Navigator => vec![
            App("Fit on Screen", ZoomFit),
            App("Actual Pixels", ZoomActual),
        ],
        ContextTarget::Canvas => vec![
            Cmd("select.all"),
            Cmd("select.deselect"),
            Cmd("select.inverse"),
            Sep,
            Cmd("edit.copy"),
            Cmd("edit.paste"),
            Sep,
            App("Clear Guides", ClearGuides),
        ],
    }
}

fn run_context_action(ws: &mut Workspace, action: ContextAction, cx: &mut Context<Workspace>) {
    match action {
        ContextAction::LayerProperties(id) => ws.open_layer_properties(id, cx),
        ContextAction::LayerStyle(id) => ws.show_layer_style(id, cx),
        ContextAction::ToggleVisibility(id) => {
            if let Some(doc) = &mut ws.doc {
                let mut edit = doc.begin_edit("Toggle Visibility");
                edit.change_props(id, |l| l.visible = !l.visible);
                edit.commit();
            }
            ws.after_change(cx);
        }
        ContextAction::ZoomFit => {
            ws.fit_to_view();
            cx.notify();
        }
        ContextAction::ZoomActual => {
            ws.set_zoom(1.0);
            cx.notify();
        }
        ContextAction::SwapColors => {
            std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
            cx.notify();
        }
        ContextAction::DefaultColors => {
            ws.editor.foreground = Rgba::BLACK;
            ws.editor.background = Rgba::WHITE;
            cx.notify();
        }
        ContextAction::ClearGuides => ws.clear_guides(cx),
    }
}

/// The open right-click menu, positioned at the cursor.
pub fn context_menu(
    ws: &mut Workspace,
    viewport: gpui::Size<gpui::Pixels>,
    cx: &mut Context<Workspace>,
) -> Option<gpui::AnyElement> {
    let menu = ws.context_menu?;
    let entries = context_entries(menu.target);
    let rows: Vec<gpui::AnyElement> = entries
        .into_iter()
        .map(|entry| match entry {
            ContextEntry::Sep => div()
                .h(px(1.0))
                .my_1()
                .bg(gpui::rgb(0x3A3A3A))
                .into_any_element(),
            ContextEntry::Cmd(id) => {
                let (label, hint) = ws
                    .registry
                    .command(id)
                    .map(|c| (c.title.to_string(), keybind_hint(c.keybind)))
                    .unwrap_or_else(|| (id.to_string(), String::new()));
                menu_row(
                    label,
                    hint,
                    move |ws, _e, _w, cx| {
                        ws.close_context_menu(cx);
                        ws.run_command(id, cx);
                    },
                    cx,
                )
                .into_any_element()
            }
            ContextEntry::App(label, action) => menu_row(
                label.to_string(),
                String::new(),
                move |ws, _e, _w, cx| {
                    ws.close_context_menu(cx);
                    run_context_action(ws, action, cx);
                },
                cx,
            )
            .into_any_element(),
        })
        .collect();

    // Keep the menu on screen: flip it back from the right/bottom edges
    // instead of letting it clip, which is what Photoshop does.
    const WIDTH: f32 = 240.0;
    let height = rows.len() as f32 * 24.0 + 8.0;
    let left = f32::from(menu.position.x)
        .min(f32::from(viewport.width) - WIDTH - 4.0)
        .max(0.0);
    let top = f32::from(menu.position.y)
        .min(f32::from(viewport.height) - height - 4.0)
        .max(0.0);

    Some(
        deferred(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(WIDTH))
                .py_1()
                .bg(gpui::rgb(POPUP_BG))
                .border_1()
                .border_color(gpui::rgb(0x3A3A3A))
                .rounded_sm()
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|ws, _e, _w, cx| ws.close_context_menu(cx)))
                .children(rows),
        )
        .into_any_element(),
    )
}

// ===== rulers =====

/// Horizontal and vertical rulers around the canvas. Dragging from a ruler
/// pulls out a guide.
pub fn rulers(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let size = Workspace::RULER_SIZE;
    let zoom = ws.zoom;
    // Choose a tick spacing that stays legible at any zoom.
    let step = [
        1.0f32, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0,
    ]
    .into_iter()
    .find(|s| s * zoom >= 60.0)
    .unwrap_or(5000.0);
    let bounds = ws.canvas_bounds();
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));

    let mut h_ticks = Vec::new();
    let mut v_ticks = Vec::new();
    if w > 0.0 && zoom > 0.0 {
        let first = (ws.doc_x_at(f32::from(bounds.origin.x)) / step).floor() * step;
        let last = ws.doc_x_at(f32::from(bounds.origin.x) + w);
        let mut x = first;
        while x <= last && h_ticks.len() < 200 {
            let sx = ws.screen_x(x) - f32::from(bounds.origin.x);
            if sx >= 0.0 {
                h_ticks.push((sx, x));
            }
            x += step;
        }
        let first = (ws.doc_y_at(f32::from(bounds.origin.y)) / step).floor() * step;
        let last = ws.doc_y_at(f32::from(bounds.origin.y) + h);
        let mut y = first;
        while y <= last && v_ticks.len() < 200 {
            let sy = ws.screen_y(y) - f32::from(bounds.origin.y);
            if sy >= 0.0 {
                v_ticks.push((sy, y));
            }
            y += step;
        }
    }

    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .child(
            // Top ruler.
            div()
                .absolute()
                .top_0()
                .left(px(size))
                .right_0()
                .h(px(size))
                .bg(gpui::rgb(0x202020))
                .border_b_1()
                .border_color(gpui::rgb(PANEL_EDGE))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                        let y = ws.doc_y_at(f32::from(ev.position.y));
                        ws.begin_guide(true, y);
                        cx.notify();
                    }),
                )
                .children(h_ticks.into_iter().map(|(sx, value)| {
                    div().absolute().top_0().left(px(sx)).h(px(size)).child(
                        div()
                            .text_size(px(9.0))
                            .text_color(gpui::rgb(TEXT_DIM))
                            .pl(px(2.0))
                            .child(format!("{value:.0}")),
                    )
                })),
        )
        .child(
            // Left ruler.
            div()
                .absolute()
                .top(px(size))
                .left_0()
                .bottom_0()
                .w(px(size))
                .bg(gpui::rgb(0x202020))
                .border_r_1()
                .border_color(gpui::rgb(PANEL_EDGE))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                        let x = ws.doc_x_at(f32::from(ev.position.x));
                        ws.begin_guide(false, x);
                        cx.notify();
                    }),
                )
                .children(v_ticks.into_iter().map(|(sy, value)| {
                    div()
                        .absolute()
                        .left_0()
                        .top(px(sy))
                        .w(px(size))
                        .text_size(px(9.0))
                        .text_color(gpui::rgb(TEXT_DIM))
                        .child(format!("{value:.0}"))
                })),
        )
        .child(
            // Corner square.
            div()
                .absolute()
                .top_0()
                .left_0()
                .size(px(size))
                .bg(gpui::rgb(0x1A1A1A)),
        )
}

// ===== navigator =====

/// A thumbnail of the whole document with the viewport marked, plus a zoom
/// slider.
pub fn navigator(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let zoom_ratio = ((ws.zoom.log2() + 7.0) / 12.0).clamp(0.0, 1.0);
    let zoom_label = format!("{:.0}%", ws.zoom * 100.0);
    let thumb = ws.document_thumbnail();
    div()
        .flex()
        .flex_col()
        .p_2()
        .gap_1()
        .border_t_1()
        .border_color(gpui::rgb(PANEL_EDGE))
        .child(panel_title("Navigator"))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                ws.open_context_menu(ContextTarget::Navigator, ev.position, cx);
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(90.0))
                .bg(gpui::rgb(0x0E0E0E))
                .rounded_sm()
                .children(thumb.map(|t| img(t).max_w(px(220.0)).max_h(px(84.0)))),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(crate::ui::slider_track(
                    "nav-zoom",
                    zoom_ratio,
                    150.0,
                    |ws, r, _cx| {
                        // Log scale: 0.8% .. 3200%.
                        let zoom = 2f32.powf(r * 12.0 - 7.0);
                        ws.set_zoom(zoom);
                    },
                    cx,
                ))
                .child(
                    div()
                        .w(px(48.0))
                        .flex_none()
                        .text_size(px(11.0))
                        .child(zoom_label),
                ),
        )
}
