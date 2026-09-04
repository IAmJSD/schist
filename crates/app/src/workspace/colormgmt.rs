//! Colour management: display transforms, profile assign/convert, and
//! soft proofing.

use super::*;

impl Workspace {
    // ----- colour management -----

    /// Rebuild the display (and proofing) transforms after the document or
    /// the colour settings change, and drop cached pixels drawn with the
    /// old ones.
    pub fn rebuild_color_transforms(&mut self) {
        let icc = self.doc.as_ref().and_then(|d| d.icc_profile.clone());
        let transform = self.color.transform_for(icc.as_deref());
        self.display_transform = (!transform.is_identity()).then(|| Arc::new(transform));
        self.proof_transform = self.color.proof_transform(icc.as_deref()).map(Arc::new);
        self.cache.invalidate_all();
        self.display_tiles.clear();
        self.invalidate_viewport_image();
        if let Some(old) = self.preview.image.take() {
            self.retired_images.push(old);
        }
        self.preview = Preview::default();
        for (_, (_, old)) in self.thumbs.drain() {
            self.retired_images.push(old);
        }
        self.color_epoch += 1;
    }

    /// Run composited pixels through the proof and display transforms.
    pub(super) fn to_display(&self, pixels: &mut [f32]) {
        if let Some(proof) = &self.proof_transform {
            proof.apply(pixels);
        }
        if let Some(display) = &self.display_transform {
            display.apply(pixels);
        }
    }

    pub(super) fn color_managed(&self) -> bool {
        self.display_transform.is_some() || self.proof_transform.is_some()
    }

    /// Which built-in the document's profile matches, for the Assign /
    /// Convert dialogs to open on.
    ///
    /// Both used to open on index 0 -- sRGB -- whatever the document was
    /// actually in, so the dialog described a conversion the user had not
    /// asked for and OK applied it.
    pub fn current_profile_index(&self) -> usize {
        let Some(name) = self
            .doc
            .as_ref()
            .and_then(|d| d.icc_profile.as_ref())
            .and_then(|bytes| schist_colormgmt::Profile::from_bytes(bytes).ok())
            .map(|p| p.name().to_string())
        else {
            // No embedded profile means the working space, which is what
            // the document is being shown in.
            let working = self.color.working.name().to_string();
            return builtin_index(&working);
        };
        builtin_index(&name)
    }

    /// Assign a profile: same numbers, new interpretation.
    pub fn assign_profile(&mut self, profile: schist_colormgmt::Profile, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc.as_mut() {
            let mut edit = doc.begin_edit(format!("Assign {}", profile.name()));
            edit.set_icc_profile(profile.icc_bytes().map(|b| b.to_vec()));
            edit.commit();
            doc.damage_all();
        }
        self.status = format!("Assigned {}", profile.name()).into();
        self.rebuild_color_transforms();
        self.after_change(cx);
    }

    /// Convert to a profile: rewrite pixels so the appearance is preserved.
    pub fn convert_to_profile(
        &mut self,
        profile: schist_colormgmt::Profile,
        cx: &mut Context<Self>,
    ) {
        let intent = self.color.intent;
        let Some(doc) = self.doc.as_mut() else { return };
        let source = match &doc.icc_profile {
            Some(bytes) => schist_colormgmt::Profile::from_bytes(bytes)
                .unwrap_or_else(|_| self.color.working.clone()),
            None => self.color.working.clone(),
        };
        let transform = match schist_colormgmt::ColorTransform::new(&source, &profile, intent) {
            Ok(t) => t,
            Err(err) => {
                self.status = format!("Convert failed: {err}").into();
                return;
            }
        };

        let mut edit = doc.begin_edit(format!("Convert to {}", profile.name()));
        for id in edit.raster_layer_ids() {
            let Some(raster) = edit.doc().tree.find(id).and_then(|l| l.as_raster()) else {
                continue;
            };
            let coords: Vec<schist_core::TileCoord> = raster.tiles.coords().collect();
            for coord in coords {
                let Some(tile) = edit.writable_tile(id, coord) else {
                    break;
                };
                let mut buf = vec![0.0f32; schist_core::TILE_PIXELS * 4];
                tile.decode_f32(&mut buf);
                transform.apply(&mut buf);
                tile.encode_f32(&buf);
            }
        }
        edit.set_icc_profile(profile.icc_bytes().map(|b| b.to_vec()));
        edit.commit();
        self.status = format!("Converted to {}", profile.name()).into();
        self.rebuild_color_transforms();
        self.after_change(cx);
    }

    /// Toggle soft proofing against a device profile.
    pub fn toggle_proof(&mut self, profile: schist_colormgmt::Profile, cx: &mut Context<Self>) {
        // Picking a different proof profile while one is active switches to
        // it; picking the active profile turns proofing off. Names are not
        // identities: loaded device profiles use a generic display name.
        let already_proofing_this = self.color.proof.as_ref().is_some_and(|current| {
            current.icc_bytes() == profile.icc_bytes() && current.name() == profile.name()
        });
        self.color.proof = (!already_proofing_this).then_some(profile);
        self.status = if self.color.proof.is_some() {
            "Proof colors on".into()
        } else {
            "Proof colors off".into()
        };
        self.rebuild_color_transforms();
        self.after_change(cx);
    }
}
