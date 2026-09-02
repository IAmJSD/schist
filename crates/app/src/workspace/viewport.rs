//! The view transform: zoom, pan, rotation, and gesture settling.

use super::*;

impl Workspace {
    // ----- viewport -----

    pub fn fit_to_view(&mut self) {
        let Some(doc) = &self.doc else { return };
        let avail = self.canvas_bounds.size;
        if avail.width <= px(0.0) || avail.height <= px(0.0) {
            return;
        }
        let margin = 40.0;
        let zx = (f32::from(avail.width) - margin) / doc.width as f32;
        let zy = (f32::from(avail.height) - margin) / doc.height as f32;
        self.zoom = zx.min(zy).clamp(0.005, 32.0);
        self.editor.zoom = self.zoom;
        self.center();
    }

    pub fn center(&mut self) {
        let Some(doc) = &self.doc else { return };
        let avail = self.canvas_bounds.size;
        self.offset = point(
            px((f32::from(avail.width) - doc.width as f32 * self.zoom) / 2.0),
            px((f32::from(avail.height) - doc.height as f32 * self.zoom) / 2.0),
        );
    }

    pub fn zoom_by(&mut self, factor: f32, around: Option<Point<Pixels>>) {
        let old = self.zoom;
        let new = (old * factor).clamp(0.005, 32.0);
        let pivot = around.unwrap_or_else(|| {
            point(
                px(f32::from(self.canvas_bounds.size.width) / 2.0),
                px(f32::from(self.canvas_bounds.size.height) / 2.0),
            )
        });
        // Keep the document point under `pivot` fixed.
        let scale = new / old;
        self.offset = point(
            px(f32::from(pivot.x) - (f32::from(pivot.x) - f32::from(self.offset.x)) * scale),
            px(f32::from(pivot.y) - (f32::from(pivot.y) - f32::from(self.offset.y)) * scale),
        );
        self.zoom = new;
        self.editor.zoom = new;
    }

    /// A continuous zoom/pan event arrived. Marks the gesture live and arms
    /// a settle timer; the timer only ends the gesture if no further event
    /// has bumped the sequence, so a stream of wheel ticks costs one
    /// full-quality rebuild at the end rather than one per tick.
    pub(crate) fn view_gesture_event(&mut self, cx: &mut Context<Self>) {
        self.view_gesture_active = true;
        self.view_gesture_seq += 1;
        let seq = self.view_gesture_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(VIEW_GESTURE_SETTLE_MS))
                .await;
            this.update(cx, |ws, cx| {
                if ws.view_gesture_seq == seq {
                    ws.view_gesture_active = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The input stream has a definite end (pinch ended, pan released):
    /// rebuild crisp now instead of waiting out the settle timer.
    pub(super) fn end_view_gesture(&mut self, cx: &mut Context<Self>) {
        if self.view_gesture_active {
            self.view_gesture_active = false;
            // Orphan any pending settle timer.
            self.view_gesture_seq += 1;
            cx.notify();
        }
    }

    pub(super) fn doc_pos(&self, canvas_local: Point<Pixels>) -> (f32, f32) {
        // The inverse of what `assemble_viewport` draws, so a click lands
        // where the pixel under the cursor actually is, rotation and all.
        let (x, y) = self.unrotate(f32::from(canvas_local.x), f32::from(canvas_local.y));
        (
            (x - f32::from(self.offset.x)) / self.zoom,
            (y - f32::from(self.offset.y)) / self.zoom,
        )
    }

    /// Undo the view rotation for a point in canvas-element coordinates.
    pub(super) fn unrotate(&self, x: f32, y: f32) -> (f32, f32) {
        if self.rotation == 0.0 {
            return (x, y);
        }
        let cx = f32::from(self.canvas_bounds.size.width) / 2.0;
        let cy = f32::from(self.canvas_bounds.size.height) / 2.0;
        let (s, c) = (-self.rotation).sin_cos();
        let (ox, oy) = (x - cx, y - cy);
        (ox * c - oy * s + cx, ox * s + oy * c + cy)
    }

    /// Turn the view by `delta` radians.
    pub fn rotate_view(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.rotation = (self.rotation + delta).rem_euclid(std::f32::consts::TAU);
        self.invalidate_viewport_image();
        self.status = format!("Rotate View {:.0}\u{b0}", self.rotation.to_degrees()).into();
        cx.notify();
    }

    /// Put the view back upright.
    pub fn reset_view_rotation(&mut self, cx: &mut Context<Self>) {
        self.rotation = 0.0;
        self.invalidate_viewport_image();
        self.status = "View reset".into();
        cx.notify();
    }

    pub(super) fn to_local(&self, window_pos: Point<Pixels>) -> Point<Pixels> {
        point(
            window_pos.x - self.canvas_bounds.origin.x,
            window_pos.y - self.canvas_bounds.origin.y,
        )
    }
}
