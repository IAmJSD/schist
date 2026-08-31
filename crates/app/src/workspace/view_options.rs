//! View options, preferences, screen mode, rulers, guides and snapping.

use super::*;

impl Workspace {
    // ----- view options, guides and snapping -----

    /// Remember the current preferences so Cancel can restore them.
    pub fn snapshot_preferences(&mut self) {
        self.preferences_snapshot = Some(Box::new((self.view.clone(), self.color.intent)));
    }

    /// Accept whatever Preferences changed.
    pub fn keep_preferences(&mut self) {
        self.preferences_snapshot = None;
        self.save_view_options();
    }

    /// Put back the preferences as they were when the dialog opened.
    pub fn revert_preferences(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.preferences_snapshot.take() else {
            return;
        };
        let (view, intent) = *snapshot;
        let gpu_changed = view.gpu_compositing != self.view.gpu_compositing;
        let theme_changed = view.theme != self.view.theme;
        self.view = view;
        self.color.intent = intent;
        if gpu_changed {
            init_compositor_backend(self.view.gpu_compositing);
            // The caches hold tiles composited by the other backend; the
            // dialog's own toggle drops them and reverting has to as well.
            self.rebuild_after_backend_change(cx);
        }
        if theme_changed {
            self.set_theme_quiet(self.view.theme);
        }
        self.rebuild_color_transforms();
        self.save_view_options();
        cx.notify();
    }

    /// Persist view options so they survive a restart.
    pub fn save_view_options(&self) {
        #[cfg(target_arch = "wasm32")]
        {
            if let Ok(json) = serde_json::to_string(&self.view) {
                crate::web::local_set(crate::web::PREFS_KEY, &json);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(path) = prefs_path() else { return };
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string_pretty(&self.view) {
                let _ = std::fs::write(path, json);
            }
        }
    }

    /// Drop everything composited by the previous backend and repaint.
    pub fn rebuild_after_backend_change(&mut self, cx: &mut Context<Self>) {
        self.cache.invalidate_all();
        self.display_tiles.clear();
        if let Some((_, old)) = self.viewport_image.take() {
            self.retired_images.push(old);
        }
        cx.notify();
    }

    pub fn toggle_rulers(&mut self, cx: &mut Context<Self>) {
        self.view.rulers = !self.view.rulers;
        self.status = format!("Rulers {}", if self.view.rulers { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_grid(&mut self, cx: &mut Context<Self>) {
        self.view.grid = !self.view.grid;
        self.status = format!("Grid {}", if self.view.grid { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_guides(&mut self, cx: &mut Context<Self>) {
        self.view.guides = !self.view.guides;
        self.status = format!("Guides {}", if self.view.guides { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_extras(&mut self, cx: &mut Context<Self>) {
        self.view.extras = !self.view.extras;
        self.status = format!("Extras {}", if self.view.extras { "on" } else { "off" }).into();
        self.save_view_options();
        cx.notify();
    }

    pub fn toggle_snap(&mut self, cx: &mut Context<Self>) {
        self.view.snap = !self.view.snap;
        self.status = if self.view.snap {
            "Snap on"
        } else {
            "Snap off"
        }
        .into();
        self.save_view_options();
        cx.notify();
    }

    /// Change the chrome theme and persist it. Callers repaint themselves
    /// (dialog dropdowns already notify).
    pub fn set_theme_quiet(&mut self, theme: Theme) {
        self.view.theme = theme;
        crate::ui::set_light(theme == Theme::Light);
        self.save_view_options();
    }

    pub fn cycle_screen_mode(&mut self, cx: &mut Context<Self>) {
        self.screen_mode = match self.screen_mode {
            ScreenMode::Standard => ScreenMode::FullCanvas,
            ScreenMode::FullCanvas => ScreenMode::Standard,
        };
        cx.notify();
    }

    /// Thickness of the rulers in screen pixels.
    pub const RULER_SIZE: f32 = 18.0;

    /// Document x for a window x (used by the rulers).
    pub fn doc_x_at(&self, window_x: f32) -> f32 {
        (window_x - f32::from(self.canvas_bounds.origin.x) - f32::from(self.offset.x)) / self.zoom
    }

    pub fn doc_y_at(&self, window_y: f32) -> f32 {
        (window_y - f32::from(self.canvas_bounds.origin.y) - f32::from(self.offset.y)) / self.zoom
    }

    /// Screen x for a document x, inside the canvas element.
    pub fn screen_x(&self, doc_x: f32) -> f32 {
        f32::from(self.canvas_bounds.origin.x) + f32::from(self.offset.x) + doc_x * self.zoom
    }

    pub fn screen_y(&self, doc_y: f32) -> f32 {
        f32::from(self.canvas_bounds.origin.y) + f32::from(self.offset.y) + doc_y * self.zoom
    }

    pub fn canvas_bounds(&self) -> Bounds<Pixels> {
        self.canvas_bounds
    }

    /// Start dragging a new guide out of a ruler.
    pub fn begin_guide(&mut self, horizontal: bool, position: f32) {
        self.dragging_guide = Some(schist_core::Guide {
            horizontal,
            position,
        });
    }

    pub fn update_guide(&mut self, position: f32) {
        if let Some(guide) = &mut self.dragging_guide {
            guide.position = position;
        }
    }

    /// Drop the dragged guide onto the document (or discard it if it landed
    /// outside the canvas).
    pub fn finish_guide(&mut self, cx: &mut Context<Self>) {
        let Some(guide) = self.dragging_guide.take() else {
            return;
        };
        if let Some(doc) = self.doc.as_mut() {
            let limit = if guide.horizontal {
                doc.height as f32
            } else {
                doc.width as f32
            };
            if guide.position >= 0.0 && guide.position <= limit {
                doc.guides.push(guide);
                doc.mark_dirty();
                doc.damage_all();
            }
        }
        self.after_change(cx);
    }

    pub fn dragging_guide(&self) -> bool {
        self.dragging_guide.is_some()
    }

    /// Remove every guide.
    pub fn clear_guides(&mut self, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.as_mut() {
            // Clearing an already-empty list changes nothing, and marking
            // dirty for it means a spurious close prompt plus a full
            // export every 30 seconds from autosave until the next save.
            if !doc.guides.is_empty() {
                doc.guides.clear();
                doc.mark_dirty();
                doc.damage_all();
            }
        }
        self.status = "Guides cleared".into();
        self.after_change(cx);
    }

    /// Snap a document-space coordinate to nearby guides and grid lines.
    pub(super) fn snap_point(&self, x: f32, y: f32) -> (f32, f32) {
        if !self.view.snap || !self.view.extras {
            return (x, y);
        }
        // Snap within 6 screen pixels, so the pull feels the same at any
        // zoom rather than growing with it.
        let threshold = 6.0 / self.zoom.max(0.01);
        let mut best = (x, y);
        let mut best_dist = (threshold, threshold);
        if let Some(doc) = &self.doc {
            if self.view.guides {
                for guide in &doc.guides {
                    if guide.horizontal {
                        let d = (guide.position - y).abs();
                        if d < best_dist.1 {
                            best_dist.1 = d;
                            best.1 = guide.position;
                        }
                    } else {
                        let d = (guide.position - x).abs();
                        if d < best_dist.0 {
                            best_dist.0 = d;
                            best.0 = guide.position;
                        }
                    }
                }
            }
            // Canvas edges always attract.
            for edge in [0.0, doc.width as f32] {
                let d = (edge - x).abs();
                if d < best_dist.0 {
                    best_dist.0 = d;
                    best.0 = edge;
                }
            }
            for edge in [0.0, doc.height as f32] {
                let d = (edge - y).abs();
                if d < best_dist.1 {
                    best_dist.1 = d;
                    best.1 = edge;
                }
            }
        }
        if self.view.grid && self.view.grid_spacing > 0.5 {
            let g = self.view.grid_spacing;
            let gx = (x / g).round() * g;
            let gy = (y / g).round() * g;
            if (gx - x).abs() < best_dist.0 {
                best.0 = gx;
            }
            if (gy - y).abs() < best_dist.1 {
                best.1 = gy;
            }
        }
        best
    }
}
