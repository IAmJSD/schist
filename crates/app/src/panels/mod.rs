//! UI chrome: menu bar, tool options bar, toolbar, layers/history/color
//! panels, status bar.
//!
//! These render directly from the Workspace (third-party panel plugins get
//! their seam later; the registry shape in plugin-api reserves it). Icons
//! are monochrome SVGs from the embedded asset source, tinted by text
//! color — no emoji.

use crate::actions::AppItem;
use crate::ui;
use crate::ui::palette;
use crate::workspace::{ColorTarget, ContextTarget, LayerDrop, Modal, NoteField, Popup, Workspace};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, deferred, div, img, px, svg, Context, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, RenderImage,
    SharedString, StatefulInteractiveElement as _, Styled, Window,
};
use schist_color::Rgba;
use schist_core::{BlendMode, Layer, LayerId, LayerKind};
use std::sync::Arc;

mod color;
mod context;
mod history;
mod layers;
mod menu_bar;
mod menus;
mod navigator;
mod notes;
mod rulers;
mod sliders;
mod status;
mod tabs;
mod toolbar;

use color::*;
pub use context::*;
use history::*;
use layers::*;
pub use menu_bar::*;
pub(crate) use menus::*;
pub use navigator::*;
use notes::*;
pub use rulers::*;
pub use sliders::*;
pub use status::*;
pub use tabs::*;
pub use toolbar::*;

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
            self.bg(gpui::rgb(palette().accent))
                .text_color(gpui::rgb(palette().accent_text))
        } else {
            self
        }
    }
}
impl<T: Styled> ActiveExt for T {}

// ===== menu bar =====

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

pub fn side_panels(ws: &mut Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w(px(260.0))
        .flex_none()
        .bg(gpui::rgb(palette().panel_bg))
        .border_l_1()
        .border_color(gpui::rgb(palette().panel_edge))
        .child(navigator(ws, cx))
        .child(color_panel(ws, cx))
        .child(layers_panel(ws, cx))
        .children(notes_panel(ws, cx))
        .child(history_panel(ws, cx))
}

fn panel_title(name: &'static str) -> impl IntoElement {
    div()
        .text_size(px(11.0))
        .text_color(gpui::rgb(palette().text_dim))
        .pb_1()
        .child(name.to_uppercase())
}

// ===== layers panel =====

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
        .cursor_pointer()
        .hover(|s| s.bg(gpui::rgb(palette().hover)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, _e, _w, cx| ws.run_command(command, cx)),
        )
        .child(icon(icon_name, 14.0, palette().text))
}
