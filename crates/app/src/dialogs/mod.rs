//! Modal dialogs: Image Size, Canvas Size, filters,
//! adjustments, export options and preferences.

use crate::ui;
#[cfg(not(target_arch = "wasm32"))]
use crate::workspace::UpdateProgress;
use crate::workspace::{ColorTarget, Modal, NewDocBackground, Popup, Workspace};
use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _,
};
use schist_color::{ColorMode, Depth};
use schist_core::Filter;
use schist_tools_transform::Resample;

mod adjust;
#[cfg(not(target_arch = "wasm32"))]
mod batch;
mod close;
mod edit;
mod export;
mod filters;
mod fonts;
mod layer_props;
mod models;
mod new_doc;
mod open;
#[cfg(not(target_arch = "wasm32"))]
mod plugins;
mod prefs;
mod profile;
#[cfg(not(target_arch = "wasm32"))]
mod save_image;
mod size;
#[cfg(not(target_arch = "wasm32"))]
mod update;

use adjust::*;
#[cfg(not(target_arch = "wasm32"))]
use batch::*;
use close::*;
use edit::*;
use export::*;
use filters::*;
use fonts::*;
use layer_props::*;
use models::*;
use new_doc::*;
use open::*;
#[cfg(not(target_arch = "wasm32"))]
use plugins::*;
use prefs::*;
use profile::*;
#[cfg(not(target_arch = "wasm32"))]
use save_image::*;
use size::*;
#[cfg(not(target_arch = "wasm32"))]
use update::*;

/// Apply a stepper delta to a pixel dimension.
///
/// `ui::num_field` hands its callback `+step` or `-step`, never an absolute
/// value, so every dimension stepper adds rather than assigns. Dimensions
/// never go below one pixel.
fn step_dim(current: u32, delta: f32) -> u32 {
    ((current as f32 + delta).max(1.0)) as u32
}

/// Workspace state the dialog widgets read while rendering.
#[derive(Clone)]
struct DialogState {
    open_popup: Option<Popup>,
    focused_field: Option<&'static str>,
    field_buffer: String,
    /// The caret's place in `field_buffer`, and whether this instant
    /// of the blink shows it.
    field_cursor: usize,
    caret_on: bool,
    /// Shares the workspace's dropdown state (it clones as a handle),
    /// so dialog dropdowns open at their current value and follow the
    /// keyboard too.
    dropdown: ui::DropdownState,
}

/// Render whichever modal is open, if any.
pub fn render(ws: &mut Workspace, cx: &mut Context<Workspace>) -> Option<gpui::AnyElement> {
    let modal = ws.modal.clone()?;
    // Snapshot the bits of workspace state the widgets need: they render
    // inside `Workspace::render`, where reading the entity would panic.
    let state = DialogState {
        open_popup: ws.open_popup,
        focused_field: ws.focused_field,
        field_buffer: ws.field_buffer.clone(),
        field_cursor: ws.field_cursor,
        caret_on: ws.caret_on(),
        dropdown: ws.dropdown.clone(),
    };
    // Each dialog's primary button registers itself as the default
    // action while it builds, so Enter can fire it.
    ui::reset_default_action();
    let body = match modal {
        Modal::ImageSize {
            width,
            height,
            resample,
            link,
        } => image_size(ws, &state, width, height, resample, link, cx).into_any_element(),
        Modal::Busy { title, what, note } => busy(&title, &what, &note).into_any_element(),
        Modal::CanvasSize {
            width,
            height,
            anchor,
        } => canvas_size(ws, &state, width, height, anchor, cx).into_any_element(),
        Modal::Filter {
            id,
            values,
            preview,
            map,
        } => filter_dialog(ws, &state, id, values, preview, map, cx).into_any_element(),
        Modal::Adjustment {
            layer,
            params,
            original,
        } => adjustment_dialog(ws, layer, params, original, cx).into_any_element(),
        Modal::LayerStyle {
            layer,
            style,
            active,
            ..
        } => crate::style_dialog::render(ws, layer, *style, active, cx).into_any_element(),
        Modal::SelectModify { kind, amount } => {
            modify_dialog(&state, kind, amount, cx).into_any_element()
        }
        Modal::ColorRange { tolerance, target } => {
            color_range_dialog(&state, tolerance, target, cx).into_any_element()
        }
        Modal::DestructiveAdjustment {
            kind,
            params,
            preview,
        } => {
            destructive_adjustment_dialog(ws, &state, kind, *params, preview, cx).into_any_element()
        }
        Modal::Stroke { width, position } => {
            stroke_dialog(ws, &state, width, position, cx).into_any_element()
        }
        Modal::Fill { source, opacity } => {
            fill_dialog(ws, &state, source, opacity, cx).into_any_element()
        }
        Modal::ContentAwareScale { width, height } => {
            content_aware_scale_dialog(&state, width, height, cx).into_any_element()
        }
        Modal::FilterGallery {
            stack,
            selected,
            preview,
        } => crate::gallery::render(ws, stack, selected, preview, cx).into_any_element(),
        Modal::ColorPicker {
            target,
            hsv,
            original,
        } => crate::color_picker::render(ws, target, hsv, original, cx).into_any_element(),
        Modal::ConfirmCloseTab => confirm_close_tab(ws, cx).into_any_element(),
        Modal::DropImage { path } => drop_image(path, cx).into_any_element(),
        #[cfg(not(target_arch = "wasm32"))]
        Modal::DropFolders { dirs, images } => drop_folders(dirs, images, cx).into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::DropFolders { .. } => return None,
        // Three desktop-only dialogs. Their modals are never opened on
        // the web (the flows that open them are compiled out), so these
        // arms only satisfy the exhaustiveness check there.
        #[cfg(not(target_arch = "wasm32"))]
        Modal::HeifSupport { path } => heif_support(ws, path, cx).into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::HeifSupport { .. } => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::CameraImport { sources } => {
            crate::workspace::camera_import_dialog(&sources, cx).into_any_element()
        }
        #[cfg(target_arch = "wasm32")]
        Modal::CameraImport { .. } => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::CameraImportOptions { source } => {
            crate::workspace::camera_import_options_dialog(ws, source, cx).into_any_element()
        }
        #[cfg(target_arch = "wasm32")]
        Modal::CameraImportOptions { .. } => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::CameraImportFailed {
            source,
            area,
            message,
        } => crate::workspace::camera_import_failed_dialog(source, area, message, cx)
            .into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::CameraImportFailed { .. } => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::UpdateAvailable { update } => update_available(ws, update, cx).into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::UpdateAvailable { .. } => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::PluginManager => plugin_manager(ws, cx).into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::PluginManager => return None,
        Modal::ModelManager => model_manager(ws, cx).into_any_element(),
        Modal::MissingFonts { fonts } => missing_fonts(ws, &fonts, cx).into_any_element(),
        Modal::Preferences => preferences(ws, &state, cx).into_any_element(),
        Modal::LayerProperties { layer, name } => {
            layer_properties(&state, layer, name, cx).into_any_element()
        }
        Modal::Export { codec, options } => {
            export_dialog(ws, &state, codec, options, cx).into_any_element()
        }
        Modal::Profile { convert, selected } => {
            profile_dialog(&state, convert, selected, cx).into_any_element()
        }
        Modal::NewFilePicker => new_file_picker(cx).into_any_element(),
        #[cfg(not(target_arch = "wasm32"))]
        Modal::MapFilter => crate::workspace::map_filter_dialog(ws, cx).into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::MapFilter => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::SearchModels => crate::workspace::search_models_dialog(cx).into_any_element(),
        #[cfg(not(target_arch = "wasm32"))]
        Modal::PeopleModels => crate::workspace::people_models_dialog(cx).into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::PeopleModels => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::PersonName { index, name } => {
            crate::workspace::person_name_dialog(ws, index, name, cx).into_any_element()
        }
        #[cfg(target_arch = "wasm32")]
        Modal::PersonName { .. } => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::SaveImageAs {
            path,
            codec,
            options,
            scale,
            size,
        } => {
            save_image_dialog(ws, &state, path, codec, options, scale, size, cx).into_any_element()
        }
        #[cfg(target_arch = "wasm32")]
        Modal::SaveImageAs { .. } => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::BatchProcess {
            photos,
            recipe,
            target,
            codec,
            options,
        } => {
            batch_dialog(ws, &state, photos, recipe, target, codec, options, cx).into_any_element()
        }
        #[cfg(target_arch = "wasm32")]
        Modal::BatchProcess { .. } => return None,
        #[cfg(target_arch = "wasm32")]
        Modal::SearchModels => return None,
        #[cfg(not(target_arch = "wasm32"))]
        Modal::BucketName {
            name,
            query,
            photos,
            editing,
        } => crate::workspace::bucket_name_dialog(ws, name, query, photos.len(), editing, cx)
            .into_any_element(),
        #[cfg(target_arch = "wasm32")]
        Modal::BucketName { .. } => return None,
        m @ Modal::NewDocument { .. } => new_document_dialog(&state, m, cx).into_any_element(),
    };
    ws.default_action = ui::take_default_action();
    Some(body)
}

/// One slider row's description, shared by the filter and adjustment
/// dialogs (both render the same control from the same shape).
#[derive(Default)]
pub(crate) struct SliderSpec {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) value: f32,
    pub(crate) min: f32,
    pub(crate) max: f32,
    pub(crate) suffix: &'static str,
    /// If set, the value is an index into these and the row shows the
    /// name rather than the number. The slider snaps to whole steps.
    pub(crate) choices: &'static [&'static str],
}

/// A labelled slider row used by the filter and adjustment dialogs.
pub(crate) fn param_slider(
    spec: SliderSpec,
    on_change: impl Fn(&mut Workspace, f32, &mut Context<Workspace>) + Clone + 'static,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let SliderSpec {
        id,
        label,
        value,
        min,
        max,
        suffix,
        choices,
    } = spec;
    let span = (max - min).max(1e-6);
    let ratio = ((value - min) / span).clamp(0.0, 1.0);
    let display = match choices.get((value.round().max(0.0) as usize).min(choices.len())) {
        Some(name) => (*name).to_string(),
        None if max - min > 20.0 => format!("{value:.0}{suffix}"),
        None => format!("{value:.2}{suffix}"),
    };
    let snap = !choices.is_empty();
    let set = on_change.clone();
    ui::field_row(
        label,
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(ui::slider_track(
                id,
                ratio,
                // A choice's name takes the room its track gives up: the
                // track is only there to step through half a dozen
                // options, and the name is the part being read.
                if snap { 92.0 } else { 120.0 },
                move |ws, r, cx| {
                    let v = min + r * span;
                    set(ws, if snap { v.round() } else { v }, cx)
                },
                cx,
            ))
            .child(
                div()
                    // A number needs little room; the name of a choice
                    // needs enough not to wrap "Repeat Edge Pixels" onto
                    // three lines, which is what it did.
                    .w(px(if snap { 146.0 } else { 56.0 }))
                    .flex_none()
                    .text_size(px(11.0))
                    .child(display),
            ),
    )
}

/// What is showing while a plug-in runs in its own process.
///
/// No buttons: the plug-in's dialog is a separate window and answering it
/// is what ends this. A Cancel here would have to kill the helper, which
/// is a bigger promise than "the document is busy".
fn busy(title: &str, what: &str, note: &str) -> impl IntoElement {
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(px(12.0)).child(what.to_string()))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(ui::palette().text_dim))
                .child(note.to_string()),
        );
    ui::modal_frame(
        title.to_string(),
        360.0,
        body,
        div().flex().flex_row().gap_2(),
    )
}

use gpui::prelude::FluentBuilder as _;

#[cfg(test)]
mod tests {
    use super::step_dim;

    #[test]
    fn steppers_add_the_delta_rather_than_assigning_it() {
        // `num_field` passes +step / -step, so a 10 px step on a 1200 px
        // dimension must land on 1210, never on 10.
        assert_eq!(step_dim(1200, 10.0), 1210);
        assert_eq!(step_dim(1200, -10.0), 1190);
    }

    #[test]
    fn dimensions_never_drop_below_one_pixel() {
        assert_eq!(step_dim(5, -10.0), 1);
        assert_eq!(step_dim(1, -1.0), 1);
    }
}
