//! Rendering the menu bar and its drop-downs, and running the item a
//! click lands on.

use super::*;

/// Check state for the toggling menu items.
pub(super) fn app_item_checked(ws: &Workspace, item: AppItem) -> Option<bool> {
    Some(match item {
        AppItem::ToggleRulers => ws.view.rulers,
        AppItem::ToggleGrid => ws.view.grid,
        AppItem::ToggleGuides => ws.view.guides,
        AppItem::ToggleNotes => ws.view.notes,
        AppItem::ToggleAi => ws.view.ai_panel,
        AppItem::ToggleExtras => ws.view.extras,
        AppItem::ToggleSnap => ws.view.snap,
        AppItem::ProofColors => ws.color.proof.is_some(),
        _ => return None,
    })
}

pub(crate) fn run_app_item(
    ws: &mut Workspace,
    item: AppItem,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match item {
        AppItem::New => ws.open_new_file_picker(cx),
        AppItem::Open => crate::keymap::open_file_dialog(ws, window, cx),
        AppItem::Close => ws.request_close_tab(ws.active_tab(), cx),
        AppItem::Save => ws.save_current(window, cx),
        AppItem::SaveAs => crate::keymap::save_file_dialog(ws, window, cx),
        AppItem::Quit => ws.request_quit(cx),
        AppItem::ZoomIn => ws.zoom_by(1.25, None),
        AppItem::ZoomOut => ws.zoom_by(0.8, None),
        AppItem::ZoomFit => ws.fit_to_view(),
        AppItem::ZoomActual => {
            ws.zoom = 1.0;
            ws.editor.zoom = 1.0;
            ws.center();
        }
        AppItem::ImageSize => {
            if let Some(doc) = ws.doc.as_ref() {
                let modal = Modal::ImageSize {
                    width: doc.width,
                    height: doc.height,
                    resample: schist_tools_transform::Resample::Classic(ws.editor.resample),
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
        // Unreachable on the web — the entries are filtered out of the
        // menu — but the match must still cover the variants.
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::Plugins => ws.open_modal(Modal::PluginManager, cx),
        #[cfg(target_arch = "wasm32")]
        AppItem::Plugins => {}
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
                        options: schist_plugin_api::ExportOptions::default(),
                    },
                    cx,
                );
            }
        }
        AppItem::AssignProfile => ws.open_modal(
            Modal::Profile {
                convert: false,
                // The document's own profile, not always sRGB: opening on
                // the wrong entry invited an accidental assign to sRGB on
                // a document that was not in it.
                selected: ws.current_profile_index(),
            },
            cx,
        ),
        AppItem::ConvertProfile => ws.open_modal(
            Modal::Profile {
                convert: true,
                selected: ws.current_profile_index(),
            },
            cx,
        ),
        AppItem::ProofColors => {
            let profile = schist_colormgmt::Profile::srgb();
            ws.toggle_proof(profile, cx);
        }
        AppItem::ToggleRulers => ws.toggle_rulers(cx),
        AppItem::ToggleGrid => ws.toggle_grid(cx),
        AppItem::ToggleGuides => ws.toggle_guides(cx),
        AppItem::ToggleNotes => ws.toggle_notes(cx),
        AppItem::ToggleAi => ws.toggle_ai_panel(cx),
        AppItem::ToggleExtras => ws.toggle_extras(cx),
        AppItem::ToggleSnap => ws.toggle_snap(cx),
        AppItem::ClearGuides => ws.clear_guides(cx),
        AppItem::ScreenModeItem => ws.cycle_screen_mode(cx),
        AppItem::Preferences => {
            ws.snapshot_preferences();
            ws.open_modal(Modal::Preferences, cx)
        }
        AppItem::ModeRgb => ws.set_color_mode(schist_color::ColorMode::Rgb, cx),
        AppItem::ModeGrayscale => ws.set_color_mode(schist_color::ColorMode::Grayscale, cx),
        AppItem::ModeCmyk => ws.set_color_mode(schist_color::ColorMode::Cmyk, cx),
        AppItem::ModeLab => ws.set_color_mode(schist_color::ColorMode::Lab, cx),
        AppItem::ModeIndexed => ws.set_color_mode(schist_color::ColorMode::Indexed, cx),
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
                position: schist_core::StrokePosition::Center,
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
        AppItem::ManageFonts => ws.show_missing_fonts(cx),
        AppItem::NewLayerComp => ws.new_layer_comp(cx),
        AppItem::ApplyLayerComp(i) => ws.apply_layer_comp(i, cx),
        AppItem::DeleteLayerComp(i) => ws.delete_layer_comp(i, cx),
        AppItem::ExportArtboards => ws.export_regions(false, window, cx),
        AppItem::ExportSlices => ws.export_regions(true, window, cx),
        AppItem::RotateViewCw => ws.rotate_view(std::f32::consts::FRAC_PI_8, cx),
        AppItem::RotateViewCcw => ws.rotate_view(-std::f32::consts::FRAC_PI_8, cx),
        AppItem::ResetView => ws.reset_view_rotation(cx),
        AppItem::ClearNotes => ws.clear_notes(cx),
        AppItem::ClearCounts => {
            if let Some(doc) = ws.doc.as_mut() {
                if !doc.counts.is_empty() {
                    doc.counts.clear();
                    doc.mark_dirty();
                    doc.damage_all();
                }
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
        // Gallery items. Unreachable on the web — the entries are
        // filtered out of the menus and the view never opens — but the
        // match must still cover the variants.
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::OpenGallery => ws.toggle_gallery(cx),
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::GalleryAddFolder => ws.gallery_add_folder(window, cx),
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::GalleryImportCamera => ws.gallery_import_camera(cx),
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::GalleryRefresh => ws.library_rescan(cx),
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::GalleryEditSelected => {
            if let Some(path) = ws.library.lead_selected().cloned() {
                ws.open_from_gallery(path, cx);
            } else {
                ws.status = "Select a photo to edit".into();
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::GalleryMapFilter => ws.open_map_filter(cx),
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::OpenRecent(i) => {
            if let Some(path) = ws.library.recents.get(i).cloned() {
                ws.load_file(path, cx);
            }
        }
        #[cfg(target_arch = "wasm32")]
        AppItem::OpenGallery
        | AppItem::GalleryAddFolder
        | AppItem::GalleryImportCamera
        | AppItem::GalleryRefresh
        | AppItem::GalleryEditSelected
        | AppItem::GalleryMapFilter
        | AppItem::OpenRecent(_) => {}
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
        #[cfg(not(target_arch = "wasm32"))]
        AppItem::CheckForUpdates => ws.check_for_update(cx),
        #[cfg(target_arch = "wasm32")]
        AppItem::CheckForUpdates => {}
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
                        schist_tools_transform::crop_to(doc, rect);
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

pub(super) fn menu_row_checked(
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
        .hover(|s| {
            s.bg(gpui::rgb(palette().accent))
                .text_color(gpui::rgb(palette().accent_text))
        })
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
                    div().w(px(12.0)).flex_none().children(
                        checked
                            .unwrap_or(false)
                            .then(|| icon("check", 10.0, palette().text)),
                    ),
                )
                .child(div().text_size(px(12.0)).child(label)),
        )
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(palette().text_dim))
                .child(hint),
        )
}

pub(super) fn menu_row(
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
        .cursor_pointer()
        .hover(|s| {
            s.bg(gpui::rgb(palette().accent))
                .text_color(gpui::rgb(palette().accent_text))
        })
        .on_mouse_down(MouseButton::Left, cx.listener(on_click))
        .child(div().text_size(px(12.0)).child(label))
        .child(
            div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(palette().text_dim))
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
        .bg(gpui::rgb(palette().panel_bg))
        .border_b_1()
        .border_color(gpui::rgb(palette().panel_edge))
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
                        .hover(|s| s.bg(gpui::rgb(palette().hover)))
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
pub(super) fn menu_panel(
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
        .bg(gpui::rgb(palette().popup_bg))
        .text_color(gpui::rgb(palette().text))
        .border_1()
        .border_color(gpui::rgb(palette().edge))
        .rounded_sm()
        .shadow_lg()
        .occlude()
        .children(rows)
}

pub(super) fn menu_entry_row(
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
            .bg(gpui::rgb(palette().edge))
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
                .hover(|s| s.bg(gpui::rgb(palette().hover)))
                // Photoshop opens submenus on hover, and the pointer has to
                // cross the parent row to reach the child panel anyway.
                .on_mouse_move(cx.listener(move |ws, _e: &MouseMoveEvent, _w, cx| {
                    if ws.open_submenu != hover_path {
                        ws.open_submenu = hover_path.clone();
                        cx.notify();
                    }
                }))
                .child(div().child(label))
                .child(icon("chevron-right", 10.0, palette().text_dim));
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
