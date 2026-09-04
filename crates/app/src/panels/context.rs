//! Right-click context menus and the actions they run.

use super::*;

/// One entry in a right-click menu.
pub(super) enum ContextEntry {
    /// Run a registered command.
    Cmd(&'static str),
    /// An app-level action handled inline.
    App(&'static str, ContextAction),
    Sep,
}

#[derive(Clone, Copy)]
pub(super) enum ContextAction {
    LayerProperties(LayerId),
    LayerStyle(LayerId),
    ToggleVisibility(LayerId),
    ZoomFit,
    ZoomActual,
    SwapColors,
    DefaultColors,
    ClearGuides,
}

pub(super) fn context_entries(target: ContextTarget) -> Vec<ContextEntry> {
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

pub(super) fn run_context_action(
    ws: &mut Workspace,
    action: ContextAction,
    cx: &mut Context<Workspace>,
) {
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
                .bg(gpui::rgb(palette().edge))
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
                .bg(gpui::rgb(palette().popup_bg))
                .text_color(gpui::rgb(palette().text))
                .border_1()
                .border_color(gpui::rgb(palette().edge))
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
