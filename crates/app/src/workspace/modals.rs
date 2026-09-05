//! Modal dialogs, their numeric/text fields, and the colour picker.

use super::*;

impl Workspace {
    // ----- modals and numeric fields -----

    pub fn open_modal(&mut self, modal: Modal, cx: &mut Context<Self>) {
        // A dialog opened from the menus replaces whatever was up,
        // suspended parents included; only `open_color_picker_on` stacks.
        self.modal_stack.clear();
        self.modal = Some(modal);
        self.context_menu = None;
        self.focused_field = None;
        self.field_buffer.clear();
        self.open_popup = None;
        cx.notify();
    }

    /// Open Photoshop's Color Picker on one of the two editor colours.
    pub fn open_color_picker(&mut self, target: ColorTarget, cx: &mut Context<Self>) {
        let original = match target {
            ColorTarget::Foreground => self.editor.foreground,
            ColorTarget::Background => self.editor.background,
            // Not reachable from the editor colour wells: these belong to
            // a dialog, which opens the picker through
            // `open_color_picker_on` and supplies the colour itself.
            ColorTarget::Note => self.editor.note_color,
            ColorTarget::StyleEffect(_) | ColorTarget::ColorRange => return,
        };
        self.open_color_picker_on(target, original, cx);
    }

    /// Open the picker over the dialog that is already up, which is
    /// suspended rather than closed: `close_modal` brings it back whether
    /// the picker is OK'd or cancelled.
    pub fn open_color_picker_on(
        &mut self,
        target: ColorTarget,
        original: Rgba,
        cx: &mut Context<Self>,
    ) {
        let hsv = crate::color_picker::rgb_to_hsv(original.r, original.g, original.b);
        let parent = self.modal.take();
        self.open_modal(
            Modal::ColorPicker {
                target,
                hsv,
                original,
            },
            cx,
        );
        // After `open_modal`, which clears the stack.
        if let Some(parent) = parent {
            self.modal_stack.push(parent);
        }
    }

    /// Take the picker's colour and close it. Cancel does nothing, because
    /// the picker never wrote to the editor while it was open.
    pub fn commit_color_picker(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::ColorPicker { target, hsv, .. }) = self.modal.as_ref() else {
            return;
        };
        let (target, (h, s, v)) = (*target, *hsv);
        let (r, g, b) = crate::color_picker::hsv_to_rgb(h, s, v);
        let colour = Rgba::new(r, g, b, 1.0);
        match target {
            ColorTarget::Foreground => self.editor.foreground = colour,
            ColorTarget::Background => self.editor.background = colour,
            ColorTarget::Note => {
                self.editor.note_color = colour;
                let [r, g, b, _] = colour.to_u8();
                self.view.note_color = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
                self.save_view_options();
            }
            // Written below, once `close_modal` has put the dialog the
            // picker was opened from back in `self.modal`.
            ColorTarget::StyleEffect(_) | ColorTarget::ColorRange => {}
        }
        self.close_modal(cx);
        match target {
            ColorTarget::StyleEffect(effect) => {
                let mut next = None;
                self.update_modal(|m| {
                    if let Modal::LayerStyle { style, layer, .. } = m {
                        crate::style_dialog::set_color(style, effect, colour);
                        next = Some((*layer, **style));
                    }
                });
                if let Some((layer, style)) = next {
                    self.preview_layer_style(layer, style, cx);
                }
            }
            ColorTarget::ColorRange => {
                self.update_modal(|m| {
                    if let Modal::ColorRange { target, .. } = m {
                        *target = colour;
                    }
                });
                cx.notify();
            }
            ColorTarget::Foreground | ColorTarget::Background | ColorTarget::Note => {}
        }
    }

    /// The ± buttons beside a picker component.
    pub fn nudge_color_component(&mut self, id: &'static str, delta: f32) {
        self.update_modal(|m| {
            if let Modal::ColorPicker { hsv, .. } = m {
                crate::color_picker::nudge(hsv, id, delta);
            }
        });
    }

    pub fn close_modal(&mut self, cx: &mut Context<Self>) {
        // Any filter preview still on the canvas belongs to the dialog that
        // is going away, so put the original pixels back. Committing a
        // filter clears the preview first, so this only fires on cancel.
        self.cancel_filter_preview(cx);
        // Same for a cancelled Layer Style session: OK clears the modal
        // itself before it gets here, so reaching this means Cancel.
        self.revert_layer_style();
        // Escape out of Preferences is a cancel, like the button.
        if matches!(self.modal, Some(Modal::Preferences)) {
            self.revert_preferences(cx);
        }
        // Same for escaping the update dialog while its download is
        // running: dismissing the thing that asked must not leave an
        // update to land on its own.
        if matches!(self.modal, Some(Modal::UpdateAvailable { .. })) {
            self.update_progress = None;
        }
        // Closing the picker uncovers the dialog it was opened from.
        self.modal = self.modal_stack.pop();
        self.default_action = None;
        self.focused_field = None;
        self.field_buffer.clear();
        self.open_popup = None;
        cx.notify();
    }

    /// Mutate the open modal's state in place.
    pub fn update_modal(&mut self, f: impl FnOnce(&mut Modal)) {
        if let Some(modal) = &mut self.modal {
            f(modal);
        }
    }

    /// Focus a field, seeded with the text it is currently showing.
    ///
    /// The buffer used to be cleared, and the field falls back to
    /// rendering its committed value while the buffer is empty, so a
    /// freshly clicked field looked full but behaved empty: backspace
    /// popped nothing, and changing 1920 to 1820 meant retyping all four
    /// digits.
    pub fn focus_field(&mut self, id: &'static str, current: impl Into<String>) {
        self.focused_field = Some(id);
        self.field_buffer = current.into();
        self.field_cursor = self.field_buffer.len();
        self.field_fresh = true;
        self.reset_caret_phase();
    }

    /// Whether carets are on this instant of the blink. Solid right
    /// after every keystroke, then 530 ms beats.
    pub fn caret_on(&self) -> bool {
        self.caret_phase
            .is_none_or(|phase| (phase.elapsed().as_millis() / 530) % 2 == 0)
    }

    /// A keystroke or a focus: the caret shows solid from here.
    pub(crate) fn reset_caret_phase(&mut self) {
        // `Instant::now` panics on the web target; its carets simply
        // stay solid.
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.caret_phase = Some(std::time::Instant::now());
        }
    }

    /// Keep a repaint arriving at each caret blink beat while any text
    /// field has the keyboard. One task at a time; it retires itself
    /// when the last field lets go.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn ensure_caret_blinker(&mut self, cx: &mut Context<Self>) {
        if self.caret_blinker {
            return;
        }
        self.caret_blinker = true;
        cx.spawn(async move |this, cx| loop {
            let wait = match this.update(cx, |ws, _| {
                let active = ws.focused_field.is_some() || ws.gallery_search_active();
                if !active {
                    ws.caret_blinker = false;
                }
                active.then(|| {
                    let into = ws
                        .caret_phase
                        .map_or(0, |phase| phase.elapsed().as_millis() as u64 % 530);
                    // A hair past the beat, so the repaint lands on the
                    // caret's other state rather than a boundary tie.
                    530 - into + 5
                })
            }) {
                Ok(Some(ms)) => ms,
                _ => return,
            };
            cx.background_executor()
                .timer(std::time::Duration::from_millis(wait))
                .await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                return;
            }
        })
        .detach();
    }

    /// Fire the open dialog's primary button, as Enter should.
    pub fn confirm_modal(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> bool {
        let Some(action) = self.default_action.clone() else {
            return false;
        };
        action(self, window, cx);
        true
    }

    /// Push whatever is in the focused field into the modal and unfocus.
    pub fn commit_focused_field(&mut self) {
        if let Some(id) = self.focused_field {
            self.commit_field(id);
        }
    }

    /// Feed a keystroke to the focused numeric field. Returns true when the
    /// field consumed it.
    pub(super) fn field_key(&mut self, key: &str, text: Option<&str>) -> bool {
        let Some(id) = self.focused_field else {
            return false;
        };
        let fresh = std::mem::take(&mut self.field_fresh);
        // Text fields (layer and document names) take any printable
        // character; the picker's hex field takes hex digits up to a full
        // triplet; numeric fields only digits.
        let textual = id == "layer-name"
            || id == "new-doc-name"
            || id == "bucket-name"
            || id == "bucket-query"
            || id == "face-name"
            || id == "person-name";
        let hex = id == "cp-hex";
        // The caret belongs to the textual fields; keep it on the rails
        // in case the buffer changed underneath it.
        self.field_cursor = self.field_cursor.min(self.field_buffer.len());
        match key {
            "left" if textual => {
                self.field_cursor = crate::ui::caret_left(&self.field_buffer, self.field_cursor);
                self.reset_caret_phase();
                return true;
            }
            "right" if textual => {
                self.field_cursor = crate::ui::caret_right(&self.field_buffer, self.field_cursor)
                    .min(self.field_buffer.len());
                self.reset_caret_phase();
                return true;
            }
            "home" | "up" if textual => {
                self.field_cursor = 0;
                self.reset_caret_phase();
                return true;
            }
            "end" | "down" if textual => {
                self.field_cursor = self.field_buffer.len();
                self.reset_caret_phase();
                return true;
            }
            "space" if textual => {
                self.field_buffer.insert(self.field_cursor, ' ');
                self.field_cursor += 1;
            }
            "backspace" if textual => {
                if self.field_cursor > 0 {
                    let from = crate::ui::caret_left(&self.field_buffer, self.field_cursor);
                    self.field_buffer.replace_range(from..self.field_cursor, "");
                    self.field_cursor = from;
                }
            }
            "delete" if textual => {
                if self.field_cursor < self.field_buffer.len() {
                    let to = crate::ui::caret_right(&self.field_buffer, self.field_cursor);
                    self.field_buffer.replace_range(self.field_cursor..to, "");
                }
            }
            "backspace" => {
                self.field_buffer.pop();
            }
            // Escape is handled in `cancel_gesture`, which runs first:
            // it is bound to `CancelGesture` in the always-matching
            // "Workspace" context, so nothing escape-shaped ever reaches
            // here. Kept as a fallback for a build with that binding
            // removed rather than left as a dead arm that looks live.
            "escape" => {
                self.focused_field = None;
                self.field_buffer.clear();
                self.field_cursor = 0;
                return true;
            }
            "enter" | "tab" => {
                self.commit_field(id);
                return true;
            }
            _ if hex => match hex_field_after(&self.field_buffer, fresh, text.unwrap_or("")) {
                Some(next) => self.field_buffer = next,
                None => return false,
            },
            _ => match text {
                Some(t) if !t.is_empty() && !t.chars().any(char::is_control) && textual => {
                    self.field_buffer.insert_str(self.field_cursor, t);
                    self.field_cursor += t.len();
                }
                Some(t)
                    if !t.is_empty()
                        && !t.chars().any(char::is_control)
                        && numeric_accepts(&self.field_buffer, t) =>
                {
                    self.field_buffer.push_str(t)
                }
                _ => return false,
            },
        }
        self.reset_caret_phase();
        // Apply as you type so the dialog stays live.
        self.commit_field_value(id);
        true
    }

    pub(super) fn commit_field(&mut self, id: &'static str) {
        self.commit_field_value(id);
        self.focused_field = None;
        self.field_buffer.clear();
        self.field_cursor = 0;
    }

    pub(super) fn commit_field_value(&mut self, id: &'static str) {
        let buffer = self.field_buffer.clone();
        if id == "layer-name" {
            self.update_modal(|m| {
                if let Modal::LayerProperties { name, .. } = m {
                    *name = buffer;
                }
            });
            return;
        }
        if id == "new-doc-name" {
            self.update_modal(|m| {
                if let Modal::NewDocument { name, .. } = m {
                    *name = buffer;
                }
            });
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if id == "face-name" {
            // The viewer's name field: the live text rides on the
            // viewer, so the panel can offer completions as it grows.
            if let Some(viewer) = &mut self.library.viewer {
                viewer.name = buffer;
            }
            return;
        }
        if id == "person-name" {
            self.update_modal(|m| {
                if let Modal::PersonName { name, .. } = m {
                    *name = buffer;
                }
            });
            return;
        }
        if id == "bucket-name" || id == "bucket-query" {
            self.update_modal(|m| {
                if let Modal::BucketName { name, query, .. } = m {
                    if id == "bucket-name" {
                        *name = buffer;
                    } else {
                        *query = buffer;
                    }
                }
            });
            return;
        }
        if id == "cp-hex" {
            if let Some(c) = crate::color_picker::parse_hex(&buffer) {
                let typed = crate::color_picker::rgb_to_hsv(c.r, c.g, c.b);
                self.update_modal(|m| {
                    if let Modal::ColorPicker { hsv, .. } = m {
                        // A grey has no hue to report, so keep the one the
                        // dialog already had rather than snapping to red.
                        if typed.1 > 0.0 {
                            hsv.0 = typed.0;
                        }
                        hsv.1 = typed.1;
                        hsv.2 = typed.2;
                    }
                });
            }
            return;
        }
        if id.starts_with("cp-") {
            if let Ok(value) = buffer.parse::<f32>() {
                self.update_modal(|m| {
                    if let Modal::ColorPicker { hsv, .. } = m {
                        crate::color_picker::set_component(hsv, id, value);
                    }
                });
            }
            return;
        }
        let Ok(value) = self.field_buffer.parse::<f32>() else {
            return;
        };
        // Every remaining field is a dimension, and none of them accept
        // zero.
        let value = value.max(1.0);
        let aspect = self
            .doc
            .as_ref()
            .map(|d| d.width as f32 / d.height.max(1) as f32)
            .unwrap_or(1.0);
        self.update_modal(|m| match m {
            Modal::ImageSize {
                width,
                height,
                link,
                ..
            } => {
                if id == "image-size-w" {
                    *width = value as u32;
                    if *link {
                        *height = (value / aspect).round().max(1.0) as u32;
                    }
                } else if id == "image-size-h" {
                    *height = value as u32;
                    if *link {
                        *width = (value * aspect).round().max(1.0) as u32;
                    }
                }
            }
            Modal::CanvasSize { width, height, .. } => {
                if id == "canvas-size-w" {
                    *width = value as u32;
                } else if id == "canvas-size-h" {
                    *height = value as u32;
                }
            }
            Modal::LayerProperties { name, .. } => {
                if id == "layer-name" {
                    *name = buffer;
                }
            }
            Modal::ContentAwareScale { width, height } => {
                if id == "cas-width" {
                    *width = value as u32;
                } else if id == "cas-height" {
                    *height = value as u32;
                }
            }
            Modal::NewDocument {
                width,
                height,
                resolution,
                ..
            } => {
                if id == "new-doc-w" {
                    *width = (value as u32).min(30000);
                } else if id == "new-doc-h" {
                    *height = (value as u32).min(30000);
                } else if id == "new-doc-dpi" {
                    *resolution = value;
                }
            }
            // These dialogs have no typed fields.
            Modal::DestructiveAdjustment { .. }
            | Modal::Busy { .. }
            | Modal::ConfirmCloseTab
            | Modal::DropImage { .. }
            | Modal::DropFolders { .. }
            | Modal::HeifSupport { .. }
            | Modal::CameraImport { .. }
            | Modal::CameraImportOptions { .. }
            | Modal::CameraImportFailed { .. }
            | Modal::NewFilePicker
            | Modal::MapFilter
            | Modal::SearchModels
            | Modal::PeopleModels
            | Modal::PersonName { .. }
            | Modal::SaveImageAs { .. }
            | Modal::BatchProcess { .. }
            // Handled above, before the numeric parse, like the other
            // text fields.
            | Modal::BucketName { .. }
            | Modal::ModelManager
            | Modal::FilterGallery { .. }
            | Modal::Stroke { .. }
            | Modal::Fill { .. }
            | Modal::SelectModify { .. }
            | Modal::ColorRange { .. }
            | Modal::LayerStyle { .. }
            | Modal::Filter { .. }
            | Modal::Adjustment { .. }
            // Handled above, before the numeric parse: a colour component
            // may legitimately be zero.
            | Modal::ColorPicker { .. }
            | Modal::PluginManager
            | Modal::Preferences
            | Modal::Export { .. }
            | Modal::MissingFonts { .. }
            | Modal::UpdateAvailable { .. }
            | Modal::Profile { .. } => {}
        });
    }

    /// True when the active tool is capturing raw typing.
    pub fn tool_captures_keys(&mut self) -> bool {
        let id = self.editor.active_tool;
        self.registry
            .tool_mut(id)
            .map(|t| t.captures_keys())
            .unwrap_or(false)
    }

    /// Feed a keystroke to the active tool. Returns true if it consumed it.
    pub(super) fn tool_key(&mut self, ev: &gpui::KeyDownEvent) -> bool {
        let tool_id = self.editor.active_tool;
        let key = ev.keystroke.key.clone();
        let text = ev.keystroke.key_char.clone();
        let modifiers = Modifiers {
            shift: ev.keystroke.modifiers.shift,
            alt: ev.keystroke.modifiers.alt,
            ctrl_or_cmd: ev.keystroke.modifiers.control || ev.keystroke.modifiers.platform,
        };
        let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) else {
            return false;
        };
        let mut ctx = ToolCtx {
            doc,
            state: &mut self.editor,
        };
        tool.on_key(&mut ctx, &key, text.as_deref(), modifiers)
    }

    /// Enter: let the active tool commit its pending gesture.
    pub fn commit_gesture(&mut self, cx: &mut Context<Self>) {
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_commit(&mut ctx);
        }
        self.after_change(cx);
    }

    pub fn cancel_gesture(&mut self, cx: &mut Context<Self>) {
        // Escape reaches here as the CancelGesture action, ahead of the
        // canvas key listener the rename normally types through.
        if self.layer_rename.is_some() {
            self.cancel_layer_rename(cx);
            return;
        }
        // Escape ends an open note but keeps what was typed. Unlike a
        // rename there is no draft to throw away: Photoshop's notes save
        // as you write them, so the only question escape answers is
        // whether the keyboard still belongs to the note.
        if self.note_edit.is_some() {
            self.commit_note_edit(cx);
            return;
        }
        // The model picker closes like any popup.
        if self.ai.model_menu {
            self.close_ai_model_menu(cx);
            return;
        }
        // Same shape for the AI prompt box: escape hands the keyboard
        // back and the draft stays put.
        if self.ai.input_active {
            self.ai.input_active = false;
            cx.notify();
            return;
        }
        if self.tool_flyout.is_some() {
            self.close_tool_flyout(cx);
            return;
        }
        if self.context_menu.is_some() {
            self.close_context_menu(cx);
            return;
        }
        // A dropdown is the innermost thing open, inside a dialog or
        // not: escape folds it up and leaves whatever it sits in alone.
        // It used to be checked after the modal, so escape on an open
        // dropdown in a dialog closed the whole dialog.
        if self.open_popup.is_some() {
            self.close_popup(cx);
            return;
        }
        // A focused field takes the escape first: it drops focus and
        // leaves the dialog up, which is what `field_key`'s "escape" arm
        // meant to do before `CancelGesture` -- bound in the
        // always-matching "Workspace" context -- got there ahead of it
        // and closed the whole dialog on the first press.
        if self.modal.is_some() && self.focused_field.is_some() {
            self.focused_field = None;
            self.field_buffer.clear();
            cx.notify();
            return;
        }
        // Not this one: the run is not ours to cancel, and dropping the
        // overlay would let the document be edited underneath something
        // that is about to write to it.
        if matches!(self.modal, Some(Modal::Busy { .. })) {
            return;
        }
        if self.modal.is_some() {
            self.close_modal(cx);
            return;
        }
        self.pointer_down = false;
        self.pan_last = None;
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_cancel(&mut ctx);
        }
        self.after_change(cx);
    }
}
