//! The menu model: what each menu holds, and the filter, plug-in and
//! layer-comp entries built from the registry.

use super::*;

pub(crate) enum MenuEntry {
    /// A registered plugin command (label + keybind resolved from registry).
    Cmd(&'static str),
    /// An app-level item handled by the shell.
    App(&'static str, AppItem, Option<&'static str>),
    /// Create an adjustment layer of this kind.
    Adjustment(schist_core::AdjustmentKind),
    /// Open a registered filter's dialog.
    Filter(&'static str),
    /// A nested menu, opened by hovering its row.
    Sub(&'static str, Vec<MenuEntry>),
    /// An app item whose label is not known at compile time -- the names
    /// of layer comps, for instance.
    Dynamic(String, AppItem),
    Sep,
}

pub(crate) fn menus(ws: &Workspace) -> Vec<(&'static str, Vec<MenuEntry>)> {
    use AppItem::*;
    use MenuEntry::*;
    // The gallery is a different room with different furniture: while it
    // is showing, the bar holds its menus instead of the editor's.
    #[cfg(not(target_arch = "wasm32"))]
    if ws.gallery_open() {
        return gallery_menus(ws);
    }
    // `mut` for the desktop-only recents insertion below.
    #[allow(unused_mut)]
    let mut menus = vec![
        (
            "File",
            vec![
                App("New", New, Some("cmd-n")),
                App("Open…", Open, Some("cmd-o")),
                App("Browse Gallery…", OpenGallery, Some("cmd-shift-g")),
                App("Close", Close, Some("cmd-w")),
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
                App("Missing Fonts…", ManageFonts, None),
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
            schist_adjustments::Params::creatable()
                .iter()
                .map(|&k| Adjustment(k))
                .collect(),
        ),
        ("Filter", {
            // Liquify and Vanishing Point sit above the categories, as
            // they do in Photoshop's Filter menu.
            let mut out = vec![
                App("Filter Gallery…", FilterGalleryItem, None),
                Filter("filter.adaptive_wide_angle"),
                Filter("filter.camera_raw"),
                Filter("filter.lens_correction"),
                App("Liquify", LiquifyItem, None),
                App("Vanishing Point", VanishingPointItem, None),
                Sep,
            ];
            out.extend(filter_menu_entries(ws));
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
                App("Notes", ToggleNotes, None),
                App("AI Panel", ToggleAi, Some("cmd-shift-a")),
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
    ];
    // Open Recent, after Open…. Desktop only: browser paths are invented
    // per session, so a recents list would be a list of nothing.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let recents = recent_entries(ws);
        if !recents.is_empty() {
            menus[0].1.insert(2, Sub("Open Recent", recents));
        }
    }
    // Items whose whole subsystem is compiled out on the web: plug-in
    // hosts (no subprocesses or JITs in a tab), the self-updater (a web
    // deployment updates by serving newer files), and the AI panel
    // (drives locally installed CLIs). A menu item that answers "this
    // does nothing in a browser" is worse than no item.
    #[cfg(target_arch = "wasm32")]
    let menus = {
        let mut menus = menus;
        for (_, entries) in &mut menus {
            entries.retain(|e| {
                !matches!(
                    e,
                    App(
                        _,
                        Plugins | CheckForUpdates | ToggleAi | Quit | OpenGallery,
                        _
                    )
                )
            });
        }
        menus
    };
    menus
}

/// The n-th recent files as menu rows.
#[cfg(not(target_arch = "wasm32"))]
fn recent_entries(ws: &Workspace) -> Vec<MenuEntry> {
    ws.library
        .recents
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            MenuEntry::Dynamic(label, AppItem::OpenRecent(i))
        })
        .collect()
}

/// The menu bar while the gallery is showing. Small on purpose: the
/// gallery browses and hands photos to the editor, it does not edit.
#[cfg(not(target_arch = "wasm32"))]
fn gallery_menus(ws: &Workspace) -> Vec<(&'static str, Vec<MenuEntry>)> {
    use AppItem::*;
    use MenuEntry::*;
    let mut file = vec![
        App("New", New, Some("cmd-n")),
        App("Open…", Open, Some("cmd-o")),
    ];
    let recents = recent_entries(ws);
    if !recents.is_empty() {
        file.push(Sub("Open Recent", recents));
    }
    file.extend([
        Sep,
        App("Add Folder to Gallery…", GalleryAddFolder, None),
        App("Import from Camera…", GalleryImportCamera, None),
        Sep,
        App("Quit", Quit, Some("cmd-q")),
    ]);
    vec![
        ("File", file),
        (
            "Gallery",
            vec![
                App("Edit Selected", GalleryEditSelected, None),
                App("Refresh", GalleryRefresh, None),
                App("Map Filter…", GalleryMapFilter, None),
                Sep,
                // The content filter's model downloads live here too, so
                // turning the filter on never requires leaving the room.
                App("Manage Models…", ManageModels, None),
                Sep,
                App("Back to Editor", OpenGallery, Some("cmd-shift-g")),
            ],
        ),
        // On macOS Preferences sits in the application menu instead and
        // this menu converts to nothing; the native bar drops menus that
        // end up empty.
        (
            "View",
            vec![
                App("AI Panel", ToggleAi, Some("cmd-shift-a")),
                Sep,
                App("Preferences…", Preferences, Some("cmd-k")),
            ],
        ),
    ]
}

/// Filters grouped by category, in registration order.
pub(super) fn filter_menu_entries(ws: &Workspace) -> Vec<MenuEntry> {
    // The ids are static strings owned by the plugins; the menu resolves
    // names from the registry at render time. Categories nest, as in
    // Photoshop's Filter menu.
    let mut groups: Vec<MenuEntry> = FILTER_GROUPS
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
        .collect();
    add_photoshop_plugins(ws, &mut groups);
    groups
}

/// Fold Photoshop plug-ins into the category submenus, by the category
/// their own PiPL declares.
///
/// Straight into the Filter menu rather than under a "Photoshop" branch,
/// for two reasons. It is what Photoshop does — a plug-in declaring
/// "Blur" belongs beside the other blurs, and vendors choose their
/// category expecting exactly that. And the menu only nests one level:
/// a submenu inside a submenu cannot be reached with the mouse, so
/// grouping them under a wrapper would have put every plug-in one level
/// past where the pointer can go.
pub(super) fn add_photoshop_plugins(ws: &Workspace, groups: &mut Vec<MenuEntry>) {
    for filter in ws.registry.filters().filter(|f| f.runs_out_of_process()) {
        let category = filter.category();
        let existing = groups.iter_mut().find_map(|g| match g {
            MenuEntry::Sub(name, entries) if *name == category => Some(entries),
            _ => None,
        });
        match existing {
            Some(entries) => entries.push(MenuEntry::Filter(filter.id())),
            None => groups.push(MenuEntry::Sub(
                category,
                vec![MenuEntry::Filter(filter.id())],
            )),
        }
    }
}

/// The Layer Comps submenu: capture a new one, then the existing comps,
/// each of which applies on click and can be deleted from beside it.
pub(super) fn layer_comp_entries(ws: &Workspace) -> Vec<MenuEntry> {
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
pub(super) fn destructive_adjustment_entries() -> Vec<MenuEntry> {
    schist_adjustments::Params::creatable()
        .iter()
        .filter(|k| !matches!(k, schist_core::AdjustmentKind::SolidColor))
        .map(|&k| MenuEntry::App(k.display_name(), AppItem::ApplyAdjustment(k), None))
        .collect()
}

/// Menu grouping for the built-in filters.
pub(super) const FILTER_GROUPS: &[(&str, &[&str])] = &[
    // Photoshop's own order, which starts with 3D and puts Other last
    // before the Neural Filters.
    ("3D", &["filter.bump_map", "filter.normal_map"]),
    (
        "Artistic",
        &[
            "filter.colored_pencil",
            "filter.cutout",
            "filter.dry_brush",
            "filter.film_grain",
            "filter.fresco",
            "filter.neon_glow",
            "filter.paint_daubs",
            "filter.palette_knife",
            "filter.plastic_wrap",
            "filter.poster_edges",
            "filter.rough_pastels",
            "filter.smudge_stick",
            "filter.sponge",
            "filter.underpainting",
            "filter.watercolor",
        ],
    ),
    (
        "Blur",
        &[
            "filter.average",
            "filter.blur",
            "filter.blur_more",
            "filter.box_blur",
            "filter.gaussian_blur",
            "filter.lens_blur",
            "filter.motion_blur",
            "filter.radial_blur",
            "filter.shape_blur",
            "filter.smart_blur",
            "filter.surface_blur",
        ],
    ),
    (
        "Blur Gallery",
        &[
            "filter.field_blur",
            "filter.iris_blur",
            "filter.tilt_shift",
            "filter.path_blur",
            "filter.spin_blur",
        ],
    ),
    (
        "Brush Strokes",
        &[
            "filter.accented_edges",
            "filter.angled_strokes",
            "filter.crosshatch",
            "filter.dark_strokes",
            "filter.ink_outlines",
            "filter.spatter",
            "filter.sprayed_strokes",
            "filter.sumi_e",
        ],
    ),
    (
        "Distort",
        &[
            "filter.diffuse_glow",
            "filter.displace",
            "filter.glass",
            "filter.ocean_ripple",
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
            "filter.flame",
            "filter.picture_frame",
            "filter.tree",
            "filter.clouds",
            "filter.difference_clouds",
            "filter.fibers",
            "filter.lens_flare",
            "filter.lighting_effects",
        ],
    ),
    (
        "Sharpen",
        &[
            "filter.sharpen",
            "filter.sharpen_edges",
            "filter.sharpen_more",
            "filter.smart_sharpen",
            "filter.unsharp_mask",
        ],
    ),
    (
        "Sketch",
        &[
            "filter.bas_relief",
            "filter.chalk_charcoal",
            "filter.charcoal",
            "filter.chrome",
            "filter.conte_crayon",
            "filter.graphic_pen",
            "filter.halftone_pattern",
            "filter.note_paper",
            "filter.photocopy",
            "filter.plaster",
            "filter.reticulation",
            "filter.stamp",
            "filter.torn_edges",
            "filter.water_paper",
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
        "Texture",
        &[
            "filter.craquelure",
            "filter.grain",
            "filter.mosaic_tiles",
            "filter.patchwork",
            "filter.stained_glass",
            "filter.texturizer",
        ],
    ),
    ("Video", &["filter.deinterlace", "filter.ntsc_colors"]),
    (
        "Other",
        &[
            "filter.custom",
            "filter.high_pass",
            "filter.hsb_hsl",
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
            "filter.neural.harmonization",
            "filter.neural.landscape_mixer",
            "filter.neural.photo_restoration",
            "filter.neural.photo_to_sketch",
            "filter.neural.face_to_caricature",
            "filter.neural.smart_portrait",
            "filter.neural.makeup_transfer",
            "filter.neural.sketch_to_portrait",
        ],
    ),
];
