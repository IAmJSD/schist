//! Layer masks, and reporting the live filters we cannot run.

use super::*;

impl Walker<'_> {
    /// Attach the layer's masks: "MRst" (mask raster) nodes in the
    /// `AdCh` list, each a full layer node with its own transform and a
    /// single-channel bitmap where white reveals. Several masks
    /// multiply, exactly as Affinity stacks them; the product becomes
    /// one real, editable [`LayerMask`].
    /// Record live filter nodes that actually change their content.
    ///
    /// A `FlRN` hangs off a layer's `AdCh` list beside its masks. Its
    /// `Filt` class names the filter — "Pers" for Live Perspective —
    /// and, for the warping ones, `Src` and `Dst` are the projective
    /// map's quads in the layer's own pixel space, corners in the order
    /// top-left, bottom-left, bottom-right, top-right (probed with
    /// fixtures/affinity-probe/flrn_perspective.af, which pulls one
    /// corner in; `DSrA`/`DDsA`/`DSrB`/`DDsB` are the second pair of
    /// quads "Two planes" mode uses, and `DMod` says whether it's on).
    /// The geometric ones are resampled through in `place_raster`; the
    /// rest we cannot run yet, so say what was dropped rather than
    /// drawing the layer unfiltered in silence.
    pub(super) fn report_live_filters(&mut self, node: &Node, layer: &Layer) {
        let filters: Vec<&Node> = self
            .graph
            .children(node, b"AdCh")
            .into_iter()
            .filter(|n| n.types.iter().any(|(t, _)| *t == graph::tag(b"FlRN")))
            .filter(|n| !self.filter_is_identity(n))
            .collect();
        for f in filters {
            if self.warp_of(f).is_some() || self.distort_of(f).is_some() {
                continue; // resampled through in place_raster
            }
            let name = self
                .graph
                .child(f, b"Filt")
                .map(|c| tag_name(c.type_tag()))
                .unwrap_or_else(|| "?".into());
            log::warn!(
                "affinity: live filter {name} on {:?} warps its content; not applied",
                layer.name
            );
            self.report
                .skipped
                .push((format!("{} (live filter)", layer.name), name));
        }
    }

    pub(super) fn apply_mask(&mut self, node: &Node, layer: &mut Layer) {
        let masks: Vec<&Node> = self
            .graph
            .children(node, b"AdCh")
            .into_iter()
            .filter(|n| n.types.iter().any(|(t, _)| *t == graph::tag(b"MRst")))
            .collect();
        if masks.is_empty() {
            return;
        }
        // A lone mask imports whole, keeping its enabled toggle; of
        // several, only the visible ones join the product.
        let visible: Vec<&Node> = masks
            .iter()
            .copied()
            .filter(|m| bool_of(m, b"Visi").unwrap_or(true))
            .collect();
        let solo = masks.len() == 1;
        let used: Vec<&Node> = if solo || visible.is_empty() {
            vec![masks[0]]
        } else {
            visible
        };

        // A mask hangs off the layer and is stored in the layer's own
        // space, so it has to be placed through the layer's transform
        // as well as its own — exactly like a clipped child. Placing it
        // with only its own transform leaves it wherever the untransformed
        // mask happened to sit, which for a rotated or scaled layer is
        // nowhere near the pixels it is supposed to be cutting.
        let saved = self.ctm;
        self.ctm = self.node_ctm(node);
        let mut placed: Vec<(IntRect, RgbaImage)> = Vec::new();
        for mask_node in &used {
            let Some(bitm) = self.graph.child(mask_node, b"Bitm") else {
                continue;
            };
            let gray = match self.decode_bitmap(bitm) {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("affinity: mask of {:?}: {e}", layer.name);
                    self.report
                        .skipped
                        .push((format!("{} (mask)", layer.name), "MRst".to_string()));
                    continue;
                }
            };
            if let Some((rect, gray)) = self.place_raster(mask_node, gray) {
                placed.push((rect, gray));
            }
        }
        self.ctm = saved;
        if placed.is_empty() {
            return;
        }

        // Union extent; outside its own rect a mask contributes the
        // revealing default (255), so only stored pixels multiply.
        let mut bounds = IntRect::EMPTY;
        for (r, _) in &placed {
            bounds = bounds.union(r);
        }
        let (w, h) = (bounds.width() as usize, bounds.height() as usize);
        if bounds.is_empty() || w * h > (1 << 28) {
            return;
        }
        let mut buf = vec![255u8; w * h];
        for (r, img) in &placed {
            let rw = r.width() as usize;
            for y in r.top..r.bottom {
                for x in r.left..r.right {
                    let v =
                        img.pixels[((y - r.top) as usize * rw + (x - r.left) as usize) * 4] as u16;
                    let px = &mut buf[(y - bounds.top) as usize * w + (x - bounds.left) as usize];
                    *px = (*px as u16 * v / 255) as u8;
                }
            }
        }

        let mut mask = schist_core::LayerMask::new_revealing();
        mask.bounds = bounds;
        mask.enabled = if solo {
            bool_of(used[0], b"Visi").unwrap_or(true)
        } else {
            true
        };
        blit_mask(&mut mask.tiles, bounds, &buf);
        layer.mask = Some(mask);
        self.report.masks += used.len();
    }
}
