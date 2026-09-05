//! Painting: the canvas element and the window layout around it.

use super::*;

impl Workspace {
    /// Everything the paint closure needs, computed with &mut self.
    pub(super) fn prepare_paint(&mut self, bounds: Bounds<Pixels>, scale_factor: f32) -> PaintJob {
        self.canvas_bounds = bounds;
        // The fit a fresh document owes itself, now that the bounds are
        // real rather than whatever the canvas last knew.
        if self.pending_fit {
            self.pending_fit = false;
            self.fit_to_view();
        }
        let mut job = PaintJob::default();
        // Taken before the no-document return so replaced images still get
        // their atlas slots released while no document is open.
        job.retired.extend(std::mem::take(&mut self.retired_images));
        let Some(doc) = self.doc.as_ref() else {
            return job;
        };
        let canvas_rect = doc.canvas_rect();
        let zoom = self.zoom;
        // Immutable-phase data, captured by value so the mutable phase
        // below (preview/tile-image builds) doesn't fight the borrows.
        let selection_generation = doc.selection.generation();
        let ant_phase = self.ant_phase;
        let has_selection = !doc.selection.is_empty() && !doc.selection.bounds().is_empty();
        let active_note = self.active_note();
        let mut guides = doc.guides.clone();
        if let Some(dragging) = self.dragging_guide {
            guides.push(dragging);
        }
        let tool_id = self.editor.active_tool;
        let mut overlays = self
            .registry
            .tool_mut(tool_id)
            .map(|t| t.overlays(doc, &self.editor))
            .unwrap_or_default();
        self.tool_has_overlay = !overlays.is_empty();
        // The active stored path is always visible, whichever tool is in
        // use -- otherwise a path drawn with the pen would vanish the
        // moment you switched to something else.
        if !PATH_TOOLS.contains(&tool_id) {
            if let Some(path) = doc.active_path.and_then(|i| doc.paths.get(i)) {
                for sub in &schist_tools_vector::paths::flatten(path).subpaths {
                    if sub.len() >= 2 {
                        overlays.push(schist_plugin_api::Overlay::AntsPolygon(sub.clone()));
                    }
                }
            }
        }
        // Notes, under every tool. They are the document's, not the Note
        // tool's, and drawing them from the tool put them on screen only
        // while it happened to be held. View ▸ Notes hides them
        // deliberately, and ⌘H with the rest of the extras.
        //
        // Appended after `tool_has_overlay` is set on purpose: that flag
        // runs the marching-ants ticker, and a static marker has nothing
        // to march.
        if self.view.notes && self.view.extras {
            overlays.extend(schist_tools_doc::note_overlays(doc, active_note));
        }
        let origin = (
            f32::from(bounds.origin.x) + f32::from(self.offset.x),
            f32::from(bounds.origin.y) + f32::from(self.offset.y),
        );
        // Overlays are drawn in window space, so they rotate about the
        // canvas element's centre exactly as the pixels do.
        let rot = self.rotation;
        let rot_centre = (
            f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.0,
            f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0,
        );
        let to_screen = move |x: f32, y: f32| -> Point<Pixels> {
            let (sx, sy) = (origin.0 + x * zoom, origin.1 + y * zoom);
            if rot == 0.0 {
                return point(px(sx), px(sy));
            }
            let (s, c) = rot.sin_cos();
            let (ox, oy) = (sx - rot_centre.0, sy - rot_centre.1);
            point(
                px(ox * c - oy * s + rot_centre.0),
                px(ox * s + oy * c + rot_centre.1),
            )
        };
        // Snap a document-space coordinate to the device-pixel grid.
        // Abutting quads MUST share bit-identical edges or fractional
        // zoom/pan leaves sub-pixel gaps between tiles that show through
        // as hairline seams; computing every tile's edges through this one
        // function (same input -> same output) guarantees they meet.
        let sf = scale_factor.max(0.01);
        let snap_x = move |x: f32| ((origin.0 + x * zoom) * sf).round() / sf;
        let snap_y = move |y: f32| ((origin.1 + y * zoom) * sf).round() / sf;
        let snapped_bounds = move |rect: IntRect| -> Bounds<Pixels> {
            let x0 = snap_x(rect.left as f32);
            let x1 = snap_x(rect.right as f32);
            let y0 = snap_y(rect.top as f32);
            let y1 = snap_y(rect.bottom as f32);
            Bounds {
                origin: point(px(x0), px(y0)),
                size: size(px(x1 - x0), px(y1 - y0)),
            }
        };

        if zoom <= preview_zoom_cutoff(scale_factor) {
            // Far out, compositing every tile would be wasteful; the
            // downscaled preview is already one seamless image.
            if let Some(img) = self.refresh_preview() {
                job.tiles.push((snapped_bounds(canvas_rect), img));
            }
        } else if let Some(quad) = self
            .gesture_viewport_quad(bounds, scale_factor)
            .filter(|_| !self.warm_pan_frame_ready(bounds, scale_factor, canvas_rect))
        {
            // Zooming out or panning can expose ground the stale image
            // never covered; fill it with the surround so those edges
            // don't flash stale pixels or bare window.
            let surround = crate::ui::palette().canvas_bg;
            job.backdrop = Some((bounds, gpui::rgb(surround).into()));
            job.tiles.push(quad);
            // Re-aim the prefetch queue at wherever the view is right
            // now, on-screen tiles first: mid-gesture nothing else
            // composites them, and a queue left pointing at where the
            // gesture started would warm the wrong neighbourhood.
            let sf = scale_factor.max(0.01);
            let w = (f32::from(bounds.size.width) * sf).round().max(1.0) as usize;
            let h = (f32::from(bounds.size.height) * sf).round().max(1.0) as usize;
            let visible = self.visible_doc_rect(w, h, sf, canvas_rect);
            self.rebuild_prefetch_queue(canvas_rect, visible, true);
        } else if let Some(img) = self.assemble_viewport(bounds, scale_factor) {
            // One image covering the whole canvas element, already
            // resampled and checkered, so there are no tile edges to seam.
            job.tiles.push((bounds, img));
        }

        job.retired.extend(std::mem::take(&mut self.retired_images));

        // Document border. An axis-aligned rectangle cannot follow a
        // rotated view, so when the view is turned it is drawn as a path
        // through `to_screen` instead.
        if self.rotation == 0.0 {
            job.outlines
                .push((snapped_bounds(canvas_rect), gpui::rgb(0x000000).into()));
        } else {
            let corners = [
                (canvas_rect.left as f32, canvas_rect.top as f32),
                (canvas_rect.right as f32, canvas_rect.top as f32),
                (canvas_rect.right as f32, canvas_rect.bottom as f32),
                (canvas_rect.left as f32, canvas_rect.bottom as f32),
                (canvas_rect.left as f32, canvas_rect.top as f32),
            ];
            job.polylines.push((
                corners.iter().map(|(x, y)| to_screen(*x, *y)).collect(),
                gpui::rgb(0x000000).into(),
            ));
        }

        // Grid, then guides: both are view chrome, hidden by ⌘H. They are
        // drawn as thin axis-aligned quads, so a rotated view hides them
        // rather than showing them pointing the wrong way.
        if self.view.extras && self.rotation == 0.0 {
            let view = self.view.clone();
            let canvas_w = canvas_rect.width() as f32;
            let canvas_h = canvas_rect.height() as f32;
            let hair = (1.0 / scale_factor.max(0.01)).max(0.5);
            if view.grid && view.grid_spacing > 0.5 {
                let spacing_px = view.grid_spacing * zoom;
                // Skip the grid when it would alias into a solid block.
                if spacing_px >= 4.0 {
                    let mut x = 0.0;
                    while x <= canvas_w {
                        let sx = snap_x(x);
                        job.lines.push((
                            Bounds {
                                origin: point(px(sx), px(snap_y(0.0))),
                                size: size(px(hair), px(snap_y(canvas_h) - snap_y(0.0))),
                            },
                            gpui::rgba(0x8899AA55).into(),
                        ));
                        x += view.grid_spacing;
                    }
                    let mut y = 0.0;
                    while y <= canvas_h {
                        let sy = snap_y(y);
                        job.lines.push((
                            Bounds {
                                origin: point(px(snap_x(0.0)), px(sy)),
                                size: size(px(snap_x(canvas_w) - snap_x(0.0)), px(hair)),
                            },
                            gpui::rgba(0x8899AA55).into(),
                        ));
                        y += view.grid_spacing;
                    }
                }
            }
            if view.guides {
                for guide in guides.iter() {
                    if guide.horizontal {
                        job.lines.push((
                            Bounds {
                                origin: point(px(snap_x(0.0)), px(snap_y(guide.position))),
                                size: size(px(snap_x(canvas_w) - snap_x(0.0)), px(hair.max(1.0))),
                            },
                            gpui::rgb(0x00A0FF).into(),
                        ));
                    } else {
                        job.lines.push((
                            Bounds {
                                origin: point(px(snap_x(guide.position)), px(snap_y(0.0))),
                                size: size(px(hair.max(1.0)), px(snap_y(canvas_h) - snap_y(0.0))),
                            },
                            gpui::rgb(0x00A0FF).into(),
                        ));
                    }
                }
            }
        }

        // Marching ants: the selection's actual boundary, traced from the
        // coverage mask and cached until the selection changes.
        if has_selection && self.view.extras {
            let outline = self.selection_outline(selection_generation);
            for run in outline.iter() {
                let pts: Vec<Point<Pixels>> = run.iter().map(|&(x, y)| to_screen(x, y)).collect();
                push_ants(&mut job.ants, &pts, ant_phase);
            }
        }

        // Active tool overlays.
        for overlay in overlays {
            match overlay {
                Overlay::Rect(r) => {
                    job.outlines.push((
                        Bounds {
                            origin: to_screen(r.left as f32, r.top as f32),
                            size: size(px(r.width() as f32 * zoom), px(r.height() as f32 * zoom)),
                        },
                        gpui::rgb(0x44AAFF).into(),
                    ));
                }
                Overlay::Highlight(r) => {
                    job.highlights.push(Bounds {
                        origin: to_screen(r.left as f32, r.top as f32),
                        size: size(px(r.width() as f32 * zoom), px(r.height() as f32 * zoom)),
                    });
                }
                Overlay::AntsRect(r) => {
                    let (l, t) = (r.left as f32, r.top as f32);
                    let (rt, b) = (r.right as f32, r.bottom as f32);
                    // Closed: the last point repeats the first so the
                    // dashes carry round the final corner.
                    let pts: Vec<Point<Pixels>> = [(l, t), (rt, t), (rt, b), (l, b), (l, t)]
                        .iter()
                        .map(|&(x, y)| to_screen(x, y))
                        .collect();
                    push_ants(&mut job.ants, &pts, ant_phase);
                }
                Overlay::AntsPolygon(points) => {
                    let pts: Vec<Point<Pixels>> =
                        points.iter().map(|&(x, y)| to_screen(x, y)).collect();
                    push_ants(&mut job.ants, &pts, ant_phase);
                }
                Overlay::Line { x1, y1, x2, y2 } => {
                    job.polylines.push((
                        vec![to_screen(x1, y1), to_screen(x2, y2)],
                        gpui::rgb(0xFFFFFF).into(),
                    ));
                }
                Overlay::Caret {
                    x1,
                    y1,
                    x2,
                    y2,
                    color,
                } => {
                    let [r, g, b, _] = color.to_u8();
                    job.carets.push((
                        vec![to_screen(x1, y1), to_screen(x2, y2)],
                        gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into(),
                    ));
                }
                Overlay::Circle { cx: ccx, cy, r } => {
                    let d = r * 2.0 * zoom;
                    job.circles.push(Bounds {
                        origin: to_screen(ccx - r, cy - r),
                        size: size(px(d), px(d)),
                    });
                }
                Overlay::NoteMarker {
                    x,
                    y,
                    color,
                    selected,
                } => {
                    // Sized in screen pixels rather than document ones, so
                    // a note stays the same readable dot at 5% and 1600%.
                    let r = schist_tools_doc::NOTE_MARKER_R;
                    let centre = to_screen(x, y);
                    let [cr, cg, cb, _] = color.to_u8();
                    job.markers.push(Marker {
                        bounds: Bounds {
                            origin: point(centre.x - px(r), centre.y - px(r)),
                            size: size(px(r * 2.0), px(r * 2.0)),
                        },
                        fill: gpui::rgb(((cr as u32) << 16) | ((cg as u32) << 8) | cb as u32)
                            .into(),
                        selected,
                    });
                }
            }
        }
        job
    }

    /// The pointer shape for the active tool, so the canvas says what a
    /// click will do before it happens.
    pub(super) fn canvas_cursor(&self) -> gpui::CursorStyle {
        use gpui::CursorStyle;
        // Space-to-pan overrides whatever tool is active, and a pan in
        // progress grabs.
        if self.space_held || self.pan_last.is_some() {
            return if self.pan_last.is_some() {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
            };
        }
        match self.editor.active_tool {
            "hand" => CursorStyle::OpenHand,
            "zoom" => CursorStyle::Crosshair,
            "type" => CursorStyle::IBeam,
            // Everything that targets a point rather than a region: a
            // crosshair is what Photoshop shows for these.
            "eyedropper" | "crop" | "marquee.rect" | "marquee.ellipse" | "lasso.free"
            | "lasso.polygonal" | "lasso.magnetic" | "wand" | "quick_select" | "object_select"
            | "gradient" | "shape.rect" | "shape.ellipse" | "shape.line" | "shape.polygon"
            | "pen" | "pen.freeform" | "pen.curvature" => CursorStyle::Crosshair,
            _ => CursorStyle::Arrow,
        }
    }

    pub fn render_canvas(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id("canvas")
            .flex_grow()
            .size_full()
            .overflow_hidden()
            .bg(gpui::rgb(crate::ui::palette().canvas_bg))
            // The pointer never changed shape anywhere in the app: the
            // canvas showed an arrow whether the active tool was the
            // hand, the zoom, the eyedropper, a brush or the crop.
            .cursor(self.canvas_cursor())
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_down(ev, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_down(ev, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|ws, ev: &MouseDownEvent, _w, cx| {
                    ws.open_context_menu(ContextTarget::Canvas, ev.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|ws, ev, w, cx| ws.on_mouse_move(ev, w, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_up(ev, w, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|ws, ev, w, cx| ws.on_mouse_up(ev, w, cx)),
            )
            .on_scroll_wheel(cx.listener(|ws, ev, w, cx| ws.on_scroll(ev, w, cx)))
            .on_pinch(cx.listener(|ws, ev, w, cx| ws.on_pinch(ev, w, cx)))
            .on_key_down(cx.listener(|ws, ev: &gpui::KeyDownEvent, window, cx| {
                // An open dropdown is the innermost thing there is: its
                // keys go to it before a modal's fields or a tool.
                if ws.dropdown_key(ev, cx)
                    || ws.layer_rename_key(ev, cx)
                    || ws.note_edit_key(ev, cx)
                    || ws.ai_model_menu_key(ev, cx)
                    || ws.ai_input_key(ev, cx)
                {
                    cx.stop_propagation();
                    return;
                }
                // A modal owns the keyboard while it is up. Without this
                // the key fell through to `tool_key` whenever no field
                // was focused, so opening a dialog on top of a text
                // session typed into the layer behind it.
                if ws.modal.is_some() {
                    match ev.keystroke.key.as_str() {
                        // Enter is the dialog's primary button, which is
                        // the only way to reach OK without the mouse.
                        "enter" => {
                            ws.commit_focused_field();
                            ws.confirm_modal(window, cx);
                        }
                        key => {
                            ws.field_key(key, ev.keystroke.key_char.as_deref());
                        }
                    }
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if ws.field_key(&ev.keystroke.key, ev.keystroke.key_char.as_deref()) {
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if ws.tool_key(ev) {
                    ws.after_change(cx);
                    cx.stop_propagation();
                    return;
                }
                if ev.keystroke.key == "space" && !ev.is_held {
                    ws.space_held = true;
                    cx.notify();
                }
            }))
            .on_key_up(cx.listener(|ws, ev: &gpui::KeyUpEvent, _w, cx| {
                if ev.keystroke.key == "space" {
                    ws.space_held = false;
                    if ws.pan_last.take().is_some() {
                        ws.end_view_gesture(cx);
                    }
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let scale = window.scale_factor();
                        entity.update(cx, |ws, cx| {
                            let job = ws.prepare_paint(bounds, scale);
                            // Idle prefetch rides the paint cycle:
                            // whatever this frame left unrendered starts
                            // warming in the background.
                            ws.kick_prefetch(cx);
                            job
                        })
                    },
                    move |_bounds, job: PaintJob, window, _cx| {
                        if let Some((bounds, color)) = job.backdrop {
                            window.paint_quad(gpui::fill(bounds, color));
                        }
                        for (bounds, img) in job.tiles {
                            let _ =
                                window.paint_image(bounds, gpui::Corners::default(), img, 0, false);
                        }
                        // Release the atlas slots of images this frame
                        // replaced, so repainting doesn't grow the atlas.
                        for old in job.retired {
                            let _ = window.drop_image(old);
                        }
                        // Grid lines and guides sit above the artwork but
                        // below selection ants and tool overlays.
                        for (bounds, color) in job.lines {
                            window.paint_quad(gpui::fill(bounds, color));
                        }
                        for bounds in job.highlights {
                            window.paint_quad(gpui::fill(bounds, gpui::rgba(0x3399FF66)));
                        }
                        for (bounds, color) in job.outlines {
                            window.paint_quad(gpui::outline(
                                bounds,
                                color,
                                gpui::BorderStyle::Solid,
                            ));
                        }
                        for (segments, color) in [
                            (job.ants.light, gpui::rgb(0xFFFFFF)),
                            (job.ants.dark, gpui::rgb(0x000000)),
                        ] {
                            if segments.is_empty() {
                                continue;
                            }
                            // One path per colour: a traced selection can be
                            // thousands of dashes.
                            let mut pb = PathBuilder::stroke(px(1.0));
                            for [a, b] in segments {
                                pb.move_to(a);
                                pb.line_to(b);
                            }
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, color);
                            }
                        }
                        for (pts, color) in job.polylines {
                            if pts.len() < 2 {
                                continue;
                            }
                            let mut pb = PathBuilder::stroke(px(1.0));
                            pb.move_to(pts[0]);
                            for p in &pts[1..] {
                                pb.line_to(*p);
                            }
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, color);
                            }
                        }
                        for (pts, color) in job.carets {
                            let mut pb = PathBuilder::stroke(px(1.0));
                            pb.move_to(pts[0]);
                            for p in &pts[1..] {
                                pb.line_to(*p);
                            }
                            if let Ok(path) = pb.build() {
                                window.paint_path(path, color);
                            }
                        }
                        for bounds in job.circles {
                            let r = bounds.size.width / 2.0;
                            window.paint_quad(gpui::quad(
                                bounds,
                                r,
                                gpui::transparent_black(),
                                px(1.0),
                                gpui::rgb(0xEEEEEE),
                                gpui::BorderStyle::Solid,
                            ));
                        }
                        // Notes last, so a pin is never buried under the
                        // ants or a tool's handles.
                        for marker in job.markers {
                            let r = marker.bounds.size.width / 2.0;
                            // Filled and outlined: the fill is the note's
                            // colour, which the user chose and may well
                            // have matched to the artwork, so a dark rim
                            // is what keeps it visible against it. The
                            // selected note wears a white one instead.
                            window.paint_quad(gpui::quad(
                                marker.bounds,
                                r,
                                marker.fill,
                                px(if marker.selected { 2.0 } else { 1.0 }),
                                if marker.selected {
                                    gpui::rgb(0xFFFFFF)
                                } else {
                                    gpui::rgb(0x202020)
                                },
                                gpui::BorderStyle::Solid,
                            ));
                        }
                    },
                )
                .size_full(),
            )
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Every colour below comes from the palette, so the theme must be
        // selected before any child renders.
        crate::ui::set_light(self.view.theme == Theme::Light);
        // Whichever dropdown renders open this frame registers itself.
        crate::ui::reset_open_dropdown();
        if !self.focused_once {
            // The focus handle only exists in the dispatch tree once we've
            // rendered, so this can't happen at construction time.
            self.focused_once = true;
            window.focus(&self.focus);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((id, enabled)) = self.pending_plugin_toggle.take() {
            self.set_plugin_enabled(id, enabled, cx);
        }
        // Three mutually exclusive input states. A single "is something
        // capturing keys" flag was not enough: the document commands were
        // bound against plain "Workspace", which matches in every state, so
        // only the unmodified single-letter bindings were ever suppressed.
        let key_context = if self.modal.is_some() {
            "Workspace modal"
        } else if self.tool_captures_keys()
            || self.dropdown_open()
            || self.layer_rename.is_some()
            || self.note_edit.is_some()
            || self.ai.input_active
            || self.ai.model_menu
            || self.gallery_typing()
        {
            "Workspace text_entry"
        } else {
            "Workspace editable"
        };
        // A caret somewhere needs the blink timer running; it retires
        // itself once every field lets go of the keyboard.
        #[cfg(not(target_arch = "wasm32"))]
        if self.focused_field.is_some() || self.gallery_search_active() {
            self.ensure_caret_blinker(cx);
        }
        let chrome = self.screen_mode == ScreenMode::Standard;
        // On macOS the menus live in the system bar, not in the window.
        crate::native_menu::sync(self, cx);
        let in_window_menus = chrome && !cfg!(target_os = "macos");
        let modal = crate::dialogs::render(self, cx);
        let context_menu = panels::context_menu(self, window.viewport_size(), cx);
        let tool_flyout = panels::tool_flyout(self, cx);
        // Two bodies share the shell (menu bar, action handlers, modal
        // overlay): the gallery when it is open, otherwise the editor.
        let gallery = self.gallery_open();
        let editor_chrome = !gallery && chrome;
        let body: gpui::AnyElement = if gallery {
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.render_gallery(cx).into_any_element()
            }
            #[cfg(target_arch = "wasm32")]
            {
                unreachable!("the gallery is not compiled into the web build")
            }
        } else {
            div()
                .flex()
                .flex_row()
                .flex_grow()
                .min_h(px(0.0))
                .children(chrome.then(|| panels::toolbar(self, cx)))
                .child(
                    div()
                        .relative()
                        .flex()
                        .flex_grow()
                        .size_full()
                        .child(self.render_canvas(cx))
                        .children((chrome && self.view.rulers).then(|| panels::rulers(self, cx))),
                )
                .children(chrome.then(|| panels::side_panels(self, cx)))
                .children(if chrome {
                    panels::ai_sidebar(self, cx)
                } else {
                    None
                })
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(crate::ui::palette().window_bg))
            .text_color(gpui::rgb(crate::ui::palette().text))
            .text_size(px(12.0))
            // While a tool is capturing typing the context loses "editable",
            // which is what single-letter shortcuts are bound against — so
            // letters reach the tool instead of switching tools.
            .key_context(key_context)
            // Files dragged in from the OS: anywhere in the window works.
            .on_drop(cx.listener(|ws, paths: &ExternalPaths, _w, cx| {
                ws.handle_dropped_paths(paths.paths().to_vec(), cx);
            }))
            .on_action(cx.listener(|ws, action: &RunCommand, _w, cx| {
                ws.run_command(&action.id.clone(), cx);
            }))
            .on_action(cx.listener(|ws, action: &ActivateTool, _w, cx| {
                ws.activate_tool(&action.id.clone(), cx);
            }))
            .on_action(cx.listener(|ws, action: &RunAppItem, window, cx| {
                panels::run_app_item(ws, action.item, window, cx);
            }))
            .on_action(cx.listener(|ws, action: &OpenFilter, _w, cx| {
                // The dialog holds the filter's id for the life of the
                // modal, so it needs the registry's 'static one.
                let id = ws
                    .registry
                    .filters()
                    .find(|f| f.id() == action.id)
                    .map(|f| f.id());
                if let Some(id) = id {
                    ws.open_filter_dialog(id, cx);
                }
            }))
            .on_action(cx.listener(|ws, action: &CycleToolGroup, _w, cx| {
                // The group ids are 'static strings from the registry; find
                // the matching one rather than leaking a new allocation.
                let group = ws
                    .tool_groups
                    .iter()
                    .map(|(g, _)| *g)
                    .find(|g| *g == action.group);
                if let Some(group) = group {
                    ws.cycle_tool_group(group, cx);
                }
            }))
            .on_action(cx.listener(|ws, action: &SetToolOpacity, _w, cx| {
                ws.editor.tool_opacity = action.percent as f32 / 100.0;
                ws.status = format!("Opacity: {}%", action.percent).into();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &NewFile, _w, cx| {
                ws.open_new_file_picker(cx);
            }))
            .on_action(cx.listener(|ws, _: &OpenFile, window, cx| {
                keymap::open_file_dialog(ws, window, cx);
            }))
            .on_action(cx.listener(|ws, _: &SaveFile, window, cx| {
                ws.save_current(window, cx);
            }))
            .on_action(cx.listener(|ws, _: &SaveFileAs, window, cx| {
                keymap::save_file_dialog(ws, window, cx);
            }))
            .on_action(cx.listener(|ws, _: &ZoomIn, _w, cx| {
                ws.zoom_by(1.25, None);
                // Damp like wheel zoom: a burst of key presses stretches the
                // previous image and rebuilds once settled, instead of
                // rebuilding a window-sized atlas image per press.
                ws.view_gesture_event(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomOut, _w, cx| {
                ws.zoom_by(0.8, None);
                ws.view_gesture_event(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomFit, _w, cx| {
                ws.fit_to_view();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ZoomActual, _w, cx| {
                ws.zoom = 1.0;
                ws.editor.zoom = 1.0;
                ws.center();
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &BrushSmaller, _w, cx| {
                ws.editor.brush_size = (ws.editor.brush_size / 1.25).max(1.0);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &BrushLarger, _w, cx| {
                ws.editor.brush_size = (ws.editor.brush_size * 1.25).min(500.0);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &SwapColors, _w, cx| {
                std::mem::swap(&mut ws.editor.foreground, &mut ws.editor.background);
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &DefaultColors, _w, cx| {
                ws.editor.foreground = schist_color::Rgba::BLACK;
                ws.editor.background = schist_color::Rgba::WHITE;
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &CancelGesture, _w, cx| {
                // Escape leaves the gallery's search, a face being
                // named, or the viewer before anything else — they are
                // the innermost things open.
                #[cfg(not(target_arch = "wasm32"))]
                if ws.gallery_open() && ws.gallery_escape(cx) {
                    return;
                }
                ws.cancel_gesture(cx);
            }))
            .on_action(cx.listener(|ws, _: &CommitGesture, _w, cx| {
                // Enter in the gallery opens the selected photo — the
                // binding takes the keystroke before any key listener
                // could, so the branch lives here.
                #[cfg(not(target_arch = "wasm32"))]
                if ws.gallery_open() && !ws.library.search_active {
                    if let Some(path) = ws.library.lead_selected().cloned() {
                        ws.open_from_gallery(path, cx);
                    }
                    return;
                }
                ws.commit_gesture(cx);
            }))
            .on_action(cx.listener(|ws, _: &ShowImageSize, _w, cx| {
                if let Some(doc) = ws.doc.as_ref() {
                    let modal = Modal::ImageSize {
                        width: doc.width,
                        height: doc.height,
                        resample: schist_tools_transform::Resample::Classic(ws.editor.resample),
                        link: true,
                    };
                    ws.open_modal(modal, cx);
                }
            }))
            .on_action(cx.listener(|ws, action: &AddAdjustment, _w, cx| {
                match adjustment_from_id(&action.kind) {
                    Some(kind) => ws.add_adjustment(kind, cx),
                    None => log::warn!("unknown adjustment {}", action.kind),
                }
            }))
            .on_action(cx.listener(|ws, _: &ToggleRulers, _w, cx| ws.toggle_rulers(cx)))
            .on_action(cx.listener(|ws, _: &ToggleGrid, _w, cx| ws.toggle_grid(cx)))
            .on_action(cx.listener(|ws, _: &ToggleGuides, _w, cx| ws.toggle_guides(cx)))
            .on_action(cx.listener(|ws, _: &ToggleNotes, _w, cx| ws.toggle_notes(cx)))
            .on_action(cx.listener(|ws, _: &ToggleExtras, _w, cx| ws.toggle_extras(cx)))
            .on_action(cx.listener(|ws, _: &ToggleSnap, _w, cx| ws.toggle_snap(cx)))
            .on_action(cx.listener(|ws, _: &ClearGuides, _w, cx| ws.clear_guides(cx)))
            .on_action(cx.listener(|ws, _: &CycleScreenMode, _w, cx| ws.cycle_screen_mode(cx)))
            .on_action(cx.listener(|ws, _: &TogglePanels, _w, cx| ws.cycle_screen_mode(cx)))
            .on_action(cx.listener(|ws, _: &ToggleAiPanel, _w, cx| ws.toggle_ai_panel(cx)))
            .on_action(cx.listener(|_ws, _: &ToggleGallery, _w, _cx| {
                #[cfg(not(target_arch = "wasm32"))]
                _ws.toggle_gallery(_cx);
            }))
            .on_action(cx.listener(|ws, _: &ShowLayerStyle, _w, cx| {
                if let Some(id) = ws.doc.as_ref().and_then(|d| d.active_layer) {
                    ws.show_layer_style(id, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &ShowPreferences, _w, cx| {
                ws.snapshot_preferences();
                ws.open_modal(Modal::Preferences, cx);
            }))
            .on_action(cx.listener(|ws, _: &ShowCanvasSize, _w, cx| {
                if let Some(doc) = ws.doc.as_ref() {
                    let modal = Modal::CanvasSize {
                        width: doc.width,
                        height: doc.height,
                        anchor: (0.5, 0.5),
                    };
                    ws.open_modal(modal, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &CloseTab, _w, cx| {
                ws.request_close_tab(ws.active_tab(), cx);
            }))
            .on_action(cx.listener(|ws, _: &NextTab, _w, cx| ws.cycle_tab(1, cx)))
            .on_action(cx.listener(|ws, _: &PrevTab, _w, cx| ws.cycle_tab(-1, cx)))
            // With AppKit's title bar transparent, this row occupies the
            // native drag/traffic-light area and follows the app theme.
            // Other platforms retain their native window decorations.
            .children((chrome && cfg!(target_os = "macos")).then(|| panels::title_bar(self)))
            .children(in_window_menus.then(|| panels::menu_bar(self, cx)))
            .children(editor_chrome.then(|| panels::tool_options_bar(self, cx)))
            .children(editor_chrome.then(|| panels::tab_bar(self, cx)))
            .child(body)
            .children(editor_chrome.then(|| panels::status_bar(self)))
            .children(tool_flyout)
            .children(context_menu)
            .children(modal)
    }
}
