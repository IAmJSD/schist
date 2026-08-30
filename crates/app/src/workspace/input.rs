//! Routing pointer input to the active tool.

use super::*;

impl Workspace {
    // ----- input routing -----

    pub(super) fn panning_tool(&self) -> bool {
        self.space_held || self.editor.active_tool == "hand"
    }

    /// Build a tool's pointer input.
    ///
    /// `pressure` comes straight from the platform event. It is 1.0 for a
    /// mouse and wherever tablet input is not wired up, so tools multiply
    /// by it unconditionally.
    pub(super) fn tool_input(
        &self,
        local: Point<Pixels>,
        m: gpui::Modifiers,
        pressure: f32,
    ) -> PointerInput {
        let (x, y) = self.doc_pos(local);
        // Snapping is a view affordance, so it happens here rather than in
        // every tool.
        let (x, y) = self.snap_point(x, y);
        PointerInput {
            x,
            y,
            pressure,
            modifiers: Modifiers {
                shift: m.shift,
                alt: m.alt,
                ctrl_or_cmd: m.control || m.platform,
            },
        }
    }

    pub(super) fn on_mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        // Clicking the canvas ends an inline layer rename or an open
        // note, keeping what was typed.
        self.commit_layer_rename(cx);
        self.commit_note_edit(cx);
        let local = self.to_local(ev.position);
        if ev.button == MouseButton::Middle || self.panning_tool() {
            self.pan_last = Some(ev.position);
            return;
        }
        if self.editor.active_tool == "zoom" {
            let factor = if ev.modifiers.alt { 1.0 / 1.5 } else { 1.5 };
            self.zoom_by(factor, Some(local));
            cx.notify();
            return;
        }
        if ev.button != MouseButton::Left {
            return;
        }
        self.pointer_down = true;
        let input = self.tool_input(local, ev.modifiers, ev.pressure);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_down(&mut ctx, input);
        }
        self.after_change(cx);
    }

    pub(super) fn on_mouse_move(
        &mut self,
        ev: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An OS file drag reaches here as synthetic left-button moves;
        // they must not feed the active tool.
        if cx.has_active_drag() {
            return;
        }
        if self.dragging_guide() {
            let horizontal = self.dragging_guide.map(|g| g.horizontal).unwrap_or(false);
            let position = if horizontal {
                self.doc_y_at(f32::from(ev.position.y))
            } else {
                self.doc_x_at(f32::from(ev.position.x))
            };
            self.update_guide(position);
            cx.notify();
            return;
        }
        if let Some(last) = self.pan_last {
            self.offset = point(
                self.offset.x + (ev.position.x - last.x),
                self.offset.y + (ev.position.y - last.y),
            );
            self.pan_last = Some(ev.position);
            self.view_gesture_event(cx);
            cx.notify();
            return;
        }
        let local = self.to_local(ev.position);
        let input = self.tool_input(local, ev.modifiers, ev.pressure);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_move(&mut ctx, input);
        }
        self.after_change(cx);
    }

    pub(super) fn on_mouse_up(
        &mut self,
        ev: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging_guide() {
            self.finish_guide(cx);
            return;
        }
        if self.pan_last.take().is_some() {
            self.end_view_gesture(cx);
            return;
        }
        if !self.pointer_down {
            return;
        }
        self.pointer_down = false;
        let local = self.to_local(ev.position);
        let input = self.tool_input(local, ev.modifiers, ev.pressure);
        let tool_id = self.editor.active_tool;
        if let (Some(doc), Some(tool)) = (self.doc.as_mut(), self.registry.tool_mut(tool_id)) {
            let mut ctx = ToolCtx {
                doc,
                state: &mut self.editor,
            };
            tool.on_pointer_up(&mut ctx, input);
        }
        self.after_change(cx);
    }

    pub(super) fn on_scroll(
        &mut self,
        ev: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Touchpads send many small precise deltas; a mouse wheel sends a
        // few large line-sized ones. Scaling lines to 30px puts both on a
        // comparable footing.
        let delta = ev.delta.pixel_delta(px(30.0));
        // Ctrl (or Cmd, or Alt — Photoshop's Windows binding) flips the
        // gesture's meaning, whichever way round the preference has it.
        let modifier = ev.modifiers.control || ev.modifiers.platform || ev.modifiers.alt;
        let zooming = if self.view.zoom_with_scroll {
            !modifier
        } else {
            modifier
        };
        if zooming {
            // Exponential so the gesture is symmetric: scrolling back up
            // returns to exactly the zoom you started from, and a precise
            // touchpad delta of a couple of pixels still moves it a little
            // rather than rounding away to nothing.
            let steps = f32::from(delta.y) / 240.0;
            if steps.abs() > f32::EPSILON {
                let local = self.to_local(ev.position);
                self.zoom_by(2f32.powf(steps), Some(local));
                self.view_gesture_event(cx);
            }
        } else {
            self.offset = point(self.offset.x + delta.x, self.offset.y + delta.y);
            self.view_gesture_event(cx);
        }
        cx.notify();
    }

    /// Trackpad pinch-to-zoom.
    ///
    /// Unlike the scroll path this is unconditional: a pinch has only one
    /// sensible meaning, so it ignores the zoom-with-scroll preference and
    /// any modifiers. Only macOS and Wayland deliver these, which is why
    /// modifier+scroll zoom stays.
    pub(super) fn on_pinch(
        &mut self,
        ev: &PinchEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `delta` is already the multiplicative change since the previous
        // event of the gesture, so it composes straight into `zoom_by`.
        if ev.phase == TouchPhase::Ended {
            self.end_view_gesture(cx);
            return;
        }
        if ev.phase != TouchPhase::Moved || !(ev.delta.is_finite() && ev.delta > 0.0) {
            return;
        }
        let local = self.to_local(ev.position);
        self.zoom_by(ev.delta, Some(local));
        self.view_gesture_event(cx);
        cx.notify();
    }
}
