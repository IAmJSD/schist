//! The People album's moving parts: the viewer (one photo, big, with
//! its faces to name), the face avatars the sidebar wears, and the
//! keys and clicks that drive them. What the album *is* — the
//! detections, the tags, the suggestions — lives on `Library`; this is
//! what the window does with it.
//!
//! Picasa's flow, kept: open a photo, the faces it found are boxed;
//! click one, type a name, Enter. A face the detector missed is drawn
//! by dragging a box. Once a person has a few faces the recogniser
//! starts offering "Is this …?" on the rest.

use super::library::{PersonFilter, ViewImage, Viewer, AVATAR_PX, VIEW_EDGE};
use super::*;
use schist_gallery::*;
use std::path::Path;

/// The two models the People album runs on — the detector that finds
/// faces and the recogniser that tells them apart — offered as a pair.
pub(crate) const PEOPLE_MODELS: [&str; 2] = ["face", "face-embed"];

impl Workspace {
    /// Show one photo instead of the grid, its faces ready to name.
    pub fn open_viewer(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(entry) = self.library.entry_of(&path).cloned() else {
            return;
        };
        self.library.select_single(path.clone());
        self.library.context = None;
        // The viewer takes the grid's place; the world map would hide it.
        self.library.map_view = false;
        self.viewer_unpick();
        self.library.viewer = Some(Viewer {
            path: path.clone(),
            image: None,
            loading: false,
            image_bounds: Bounds::default(),
            area: Bounds::default(),
            drawing: None,
            pick: None,
            name: String::new(),
            crops: FxHashMap::default(),
        });
        // Its faces first, if the detector has not been this way yet:
        // the person is looking at this photo now.
        if self.library.prioritize_index(&path) {
            self.kick_thumb_loader(cx);
        }
        self.load_view_image(entry, cx);
        cx.notify();
    }

    /// Back to the grid.
    pub fn close_viewer(&mut self, cx: &mut Context<Self>) {
        self.viewer_unpick();
        self.library.viewer = None;
        cx.notify();
    }

    /// The next (or previous) photo in the grid's order.
    pub fn viewer_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(current) = self.library.viewer.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        let flat = self.gallery_flat_order();
        let Some(at) = flat.iter().position(|p| p == &current) else {
            return;
        };
        let next = (at as isize + delta).clamp(0, flat.len() as isize - 1) as usize;
        if next != at {
            self.open_viewer(flat[next].clone(), cx);
        }
    }

    /// Decode the photo for the viewer off the UI thread — the edit
    /// sidecar when there is one, as the thumbnail does.
    fn load_view_image(&mut self, entry: Entry, cx: &mut Context<Self>) {
        if let Some(viewer) = &mut self.library.viewer {
            viewer.loading = true;
        }
        let source = thumb_source(&entry.path, entry.edited);
        let key = entry.path.clone();
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    match schist_preview::render_file(&source, VIEW_EDGE) {
                        Ok(preview) => Some((preview.width, preview.height, preview.rgba)),
                        Err(err) => {
                            log::warn!("viewer decode failed for {}: {err:#}", source.display());
                            None
                        }
                    }
                })
                .await;
            this.update(cx, |ws, cx| {
                let Some(viewer) = &mut ws.library.viewer else {
                    return;
                };
                // The viewer moved on while this decoded.
                if viewer.path != key {
                    return;
                }
                viewer.loading = false;
                if let Some((width, height, rgba)) = decoded {
                    if let Some(render) =
                        super::library::rgba_to_render_image(width, height, rgba.clone())
                    {
                        viewer.image = Some(ViewImage {
                            width,
                            height,
                            rgba: Arc::new(rgba),
                            render,
                        });
                        viewer.crops.clear();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A face cut from the viewer's own decode, cached per face for the
    /// people panel. `None` until the decode lands.
    pub(super) fn viewer_crop(&mut self, rect: &FaceRect) -> Option<Arc<RenderImage>> {
        let viewer = self.library.viewer.as_mut()?;
        let key = rect.key();
        if let Some(img) = viewer.crops.get(&key) {
            return Some(img.clone());
        }
        let image = viewer.image.as_ref()?;
        let rgba = face_crop_rgba(
            &image.rgba,
            image.width,
            image.height,
            rect,
            AVATAR_GROW,
            AVATAR_PX,
        )?;
        let img = super::library::rgba_to_render_image(AVATAR_PX, AVATAR_PX, rgba)?;
        viewer.crops.insert(key, img.clone());
        Some(img)
    }

    /// Choose a face to name: the name field takes the keyboard,
    /// seeded with whatever it is called now.
    pub(super) fn viewer_pick(&mut self, rect: FaceRect) {
        let Some(path) = self.library.viewer.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        let current = self
            .library
            .person_of(&path, &rect)
            .and_then(|i| self.library.people.get(i))
            .map(|p| p.name.clone())
            .unwrap_or_default();
        if let Some(viewer) = &mut self.library.viewer {
            viewer.pick = Some(rect);
            viewer.name = current.clone();
            viewer.drawing = None;
        }
        self.focus_field("face-name", current);
    }

    /// Let go of the face being named, keyboard included.
    pub(super) fn viewer_unpick(&mut self) {
        if let Some(viewer) = &mut self.library.viewer {
            viewer.pick = None;
            viewer.name.clear();
            viewer.drawing = None;
        }
        if self.focused_field == Some("face-name") {
            self.focused_field = None;
            self.field_buffer.clear();
            self.field_cursor = 0;
        }
    }

    /// Enter in the name field: the typed name goes on the picked face.
    /// An empty name just lets go.
    pub(super) fn viewer_commit_name(&mut self, cx: &mut Context<Self>) {
        let typed = if self.focused_field == Some("face-name") {
            self.field_buffer.clone()
        } else {
            self.library
                .viewer
                .as_ref()
                .map(|v| v.name.clone())
                .unwrap_or_default()
        };
        let Some((path, rect)) = self
            .library
            .viewer
            .as_ref()
            .and_then(|v| Some((v.path.clone(), v.pick?)))
        else {
            return;
        };
        if !typed.trim().is_empty() {
            self.learn_face_if_needed(&path, rect);
            self.library.tag_face(&path, rect, &typed);
        }
        self.viewer_unpick();
        cx.notify();
    }

    /// Name a face as an existing person — a completion or a "yes" to
    /// the recogniser's suggestion.
    pub(super) fn viewer_name_as(&mut self, rect: FaceRect, person: usize, cx: &mut Context<Self>) {
        let (Some(path), Some(name)) = (
            self.library.viewer.as_ref().map(|v| v.path.clone()),
            self.library.people.get(person).map(|p| p.name.clone()),
        ) else {
            return;
        };
        self.learn_face_if_needed(&path, rect);
        self.library.tag_face(&path, rect, &name);
        self.viewer_unpick();
        cx.notify();
    }

    /// Wave a detected face away.
    pub(super) fn viewer_ignore(&mut self, rect: FaceRect, cx: &mut Context<Self>) {
        let Some(path) = self.library.viewer.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        self.library.ignore_face(&path, rect);
        self.viewer_unpick();
        cx.notify();
    }

    /// Take the name off a face.
    pub(super) fn viewer_untag(&mut self, rect: FaceRect, cx: &mut Context<Self>) {
        let Some(path) = self.library.viewer.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        self.library.untag_face(&path, &rect);
        self.viewer_unpick();
        cx.notify();
    }

    /// A face being named that no detection carries a vector for — one
    /// drawn by hand, or found before the recogniser was installed —
    /// gets one from the viewer's decode, so the person can be
    /// suggested elsewhere. A few dozen milliseconds, once per face.
    fn learn_face_if_needed(&mut self, path: &Path, rect: FaceRect) {
        let carried = self.library.detected_faces(path).is_some_and(|faces| {
            faces
                .iter()
                .any(|f| f.rect.same_face(&rect) && f.embed.is_some())
        });
        if carried {
            return;
        }
        let Some(model) = schist_neural::get("face-embed") else {
            return;
        };
        let Some(image) = self.library.viewer.as_ref().and_then(|v| v.image.as_ref()) else {
            return;
        };
        let (side, _) = model.spec.input.dims();
        let Some(crop) = face_crop_rgb(
            &image.rgba,
            image.width,
            image.height,
            &rect,
            EMBED_GROW,
            side as u32,
        ) else {
            return;
        };
        match schist_neural::embed_face(&model, &crop) {
            Ok(embed) => self.library.learn_face(path, rect, embed),
            Err(err) => log::warn!("face embedding failed for {}: {err:#}", path.display()),
        }
    }

    /// A pointer position as a fraction of the photo, from where the
    /// picture landed last paint.
    fn viewer_fraction(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.library.viewer.as_ref()?.image_bounds;
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        Some((
            ((f32::from(position.x) - f32::from(bounds.origin.x)) / w).clamp(0.0, 1.0),
            ((f32::from(position.y) - f32::from(bounds.origin.y)) / h).clamp(0.0, 1.0),
        ))
    }

    /// A press on the picture: on a face, that face is picked; anywhere
    /// else starts drawing a box for one the detector missed.
    pub(super) fn viewer_mouse_down(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((fx, fy)) = self.viewer_fraction(position) else {
            return;
        };
        let Some(path) = self.library.viewer.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        log::debug!(
            "viewer press at {position:?} -> ({fx:.3}, {fy:.3}) in {:?}",
            self.library.viewer.as_ref().map(|v| v.image_bounds)
        );
        let hit = self
            .library
            .faces_in(&path)
            .into_iter()
            .find(|f| f.rect.contains(fx, fy));
        match hit {
            Some(face) => self.viewer_pick(face.rect),
            None => {
                self.viewer_unpick();
                if let Some(viewer) = &mut self.library.viewer {
                    viewer.drawing = Some(((fx, fy), (fx, fy)));
                }
            }
        }
        cx.notify();
    }

    pub(super) fn viewer_mouse_move(
        &mut self,
        position: Point<Pixels>,
        pressed: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(viewer) = &mut self.library.viewer else {
            return;
        };
        if viewer.drawing.is_none() {
            return;
        }
        if !pressed {
            // The button went up somewhere we never heard about.
            viewer.drawing = None;
            cx.notify();
            return;
        }
        let Some((fx, fy)) = self.viewer_fraction(position) else {
            return;
        };
        if let Some(drawing) = self
            .library
            .viewer
            .as_mut()
            .and_then(|v| v.drawing.as_mut())
        {
            drawing.1 = (fx, fy);
            cx.notify();
        }
    }

    /// The box is done: big enough to be a face, it is picked for
    /// naming; a mere click clears the pick instead.
    pub(super) fn viewer_mouse_up(&mut self, cx: &mut Context<Self>) {
        let Some(viewer) = &mut self.library.viewer else {
            return;
        };
        let Some(((x0, y0), (x1, y1))) = viewer.drawing.take() else {
            return;
        };
        let bounds = viewer.image_bounds;
        let rect = FaceRect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
        .clamped();
        // Under a dozen pixels either way is a click, not a face.
        if rect.w * f32::from(bounds.size.width) < 12.0
            || rect.h * f32::from(bounds.size.height) < 12.0
        {
            cx.notify();
            return;
        }
        self.viewer_pick(rect);
        cx.notify();
    }

    /// The viewer's keys. Returns whether it took the keystroke: with
    /// a face picked the name field has them all; otherwise the arrows
    /// walk the photos and Space closes what Space opened.
    pub(super) fn gallery_viewer_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.library.viewer.is_none() {
            return false;
        }
        if self.focused_field == Some("face-name") {
            match ev.keystroke.key.as_str() {
                "enter" | "tab" => self.viewer_commit_name(cx),
                key => {
                    self.field_key(key, ev.keystroke.key_char.as_deref());
                }
            }
            cx.notify();
            return true;
        }
        match ev.keystroke.key.as_str() {
            "left" | "up" => self.viewer_step(-1, cx),
            "right" | "down" => self.viewer_step(1, cx),
            "space" => self.close_viewer(cx),
            _ => return false,
        }
        true
    }

    /// Escape in the gallery, innermost first: a search, then a face
    /// being named, then the viewer itself. Returns whether anything
    /// was there to leave.
    pub fn gallery_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.gallery_search_clear(cx) {
            return true;
        }
        let Some(viewer) = &self.library.viewer else {
            return false;
        };
        if viewer.pick.is_some() || viewer.drawing.is_some() {
            self.viewer_unpick();
        } else {
            self.library.viewer = None;
        }
        cx.notify();
        true
    }

    /// The sidebar's People rows: show a person's photos (or the
    /// unnamed faces) in the grid, or go back to the folders.
    pub fn show_person(&mut self, filter: Option<PersonFilter>, cx: &mut Context<Self>) {
        self.library.person_filter = filter;
        self.library.map_view = false;
        self.library.folder_filter = None;
        self.library.bucket_filter = None;
        // A person chosen while a photo is up wants the grid of them.
        if self.library.viewer.is_some() {
            self.close_viewer(cx);
        }
        cx.notify();
    }

    /// Offer the two People models, licences first.
    pub fn open_people_models(&mut self, cx: &mut Context<Self>) {
        self.open_modal(Modal::PeopleModels, cx);
    }

    /// The rename dialog, its field already taking typing.
    pub fn rename_person_prompt(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(name) = self.library.people.get(index).map(|p| p.name.clone()) else {
            return;
        };
        self.open_modal(
            Modal::PersonName {
                index,
                name: name.clone(),
            },
            cx,
        );
        self.focus_field("person-name", name);
    }

    /// The avatar for a face: from the cut cache, or queued to be cut
    /// from the photo's thumbnail. `None` until it is ready.
    pub(super) fn face_avatar(
        &mut self,
        path: &Path,
        rect: &FaceRect,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        let key = format!("{}|{}", path.display(), rect.key());
        match self.library.avatar_state(&key) {
            Some(state) => state,
            None => {
                if self.library.request_avatar(key, path.to_path_buf(), *rect) {
                    self.kick_avatar_loader(cx);
                }
                None
            }
        }
    }

    /// Cut the queued avatars on the background executor, a batch at a
    /// time, until the queue is empty. One task at a time.
    fn kick_avatar_loader(&mut self, cx: &mut Context<Self>) {
        if self.library.avatar_ticking(true) {
            return;
        }
        cx.spawn(async move |this, cx| loop {
            let jobs: Vec<(String, PathBuf, u64, FaceRect)> = match this.update(cx, |ws, _| {
                let mut jobs = Vec::new();
                for (key, path, rect) in ws.library.take_avatar_jobs() {
                    match ws.library.entry_of(&path).cloned() {
                        Some(entry) => jobs.push((
                            key,
                            thumb_source(&entry.path, entry.edited),
                            entry.mtime,
                            rect,
                        )),
                        // Asked before the scan knew the photo (the
                        // sidebar draws before the folders are walked):
                        // forget the request, so the next frame asks again.
                        None => ws.library.forget_avatar(&key),
                    }
                }
                jobs
            }) {
                Ok(jobs) => jobs,
                Err(_) => return,
            };
            if jobs.is_empty() {
                this.update(cx, |ws, _| {
                    ws.library.avatar_ticking(false);
                })
                .ok();
                return;
            }
            let cut = cx
                .background_executor()
                .spawn(async move {
                    jobs.into_iter()
                        .map(|(key, source, mtime, rect)| (key, cut_avatar(&source, mtime, &rect)))
                        .collect::<Vec<_>>()
                })
                .await;
            let kept = this.update(cx, |ws, cx| {
                for (key, image) in cut {
                    ws.library.store_avatar(key, image);
                }
                cx.notify();
            });
            if kept.is_err() {
                return;
            }
        })
        .detach();
    }
}

/// Cut a face avatar from a photo's cached thumbnail — rendering the
/// thumbnail when the cache has none yet. Blocking.
fn cut_avatar(source: &Path, mtime: u64, rect: &FaceRect) -> Option<Arc<RenderImage>> {
    let cached = thumb_cache_path(source, mtime)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| image::load_from_memory(&bytes).ok())
        .map(|img| {
            let img = img.into_rgba8();
            (img.width(), img.height(), img.into_raw())
        });
    let (width, height, rgba) = match cached {
        Some(thumb) => thumb,
        None => {
            let preview = schist_preview::render_file(source, THUMB_EDGE).ok()?;
            (preview.width, preview.height, preview.rgba)
        }
    };
    let crop = face_crop_rgba(&rgba, width, height, rect, AVATAR_GROW, AVATAR_PX)?;
    super::library::rgba_to_render_image(AVATAR_PX, AVATAR_PX, crop)
}
