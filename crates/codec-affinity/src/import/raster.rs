//! Raster layers: where a bitmap lands on the canvas, and getting
//! its pixels there.

use super::*;

impl Walker<'_> {
    /// Build a raster layer from a node holding a `Bitm` bitmap.
    pub(super) fn raster_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let bitm = self.graph.child(node, b"Bitm")?;
        if &bitm.type_tag().to_be_bytes() != b"DyBm" {
            self.report
                .skipped
                .push((name.to_string(), tag_name(bitm.type_tag())));
            return None;
        }
        let rgba = match self.decode_bitmap(bitm) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("affinity: bitmap of {name:?}: {e}");
                self.report
                    .skipped
                    .push((name.to_string(), format!("Rstr: {e}")));
                return None;
            }
        };

        let (rect, rgba) = self.place_raster(node, rgba)?;
        let mut layer = Layer::new_raster(name);
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            rect,
            &rgba.pixels,
        );
        self.report.raster_layers += 1;
        Some(layer)
    }

    /// The map from a bitmap's pixel space onto the canvas: the layer
    /// transform chain, exactly as for vectors. `BitR` (the content's
    /// bounding rect, usually in bitmap space) plays no part in
    /// placement — treating it as a destination squashes any layer
    /// whose transform isn't the identity, which real Photo documents
    /// (scaled brush strokes, rotated placed images with pre-rotated
    /// pixel caches) proved wrong against their own thumbnails.
    pub(super) fn raster_map(&self, node: &Node) -> Mat {
        self.node_ctm(node)
    }

    /// Place a decoded bitmap on the canvas. Axis-aligned placements
    /// scale bilinearly (identity is free); rotated or sheared ones go
    /// through a full affine resample.
    pub(super) fn place_raster(&self, node: &Node, img: RgbaImage) -> Option<(IntRect, RgbaImage)> {
        let mut map = self.raster_map(node);
        // A live warp filter reshapes the layer's own pixels before the
        // layer transform places them, so resample here and slide the
        // transform by wherever the destination quad ended up.
        let mut img = img;
        // The geometric filters and the blurs rework the same pixels
        // in place, so they run before the quad warp that can move them
        // somewhere else.
        for f in self.live_filters(node) {
            img.pixels = match f {
                LiveFilter::Geometry(d) => {
                    crate::distort::apply(img.width, img.height, &img.pixels, &d)
                }
                LiveFilter::Blur(b) => {
                    crate::liveblur::apply(img.width, img.height, &img.pixels, &b)
                }
                LiveFilter::Vignette(v) => {
                    crate::vignette::apply(img.width, img.height, &img.pixels, &v)
                }
            };
        }
        if let Some(h) = self.live_warp(node) {
            // A degenerate map is not worth losing the layer over: draw
            // it where it would have been instead.
            if let Some((ox, oy, warped)) = perspective_resample(&img, &h) {
                map = map.then(&Mat::translation(ox as f64, oy as f64));
                img = warped;
            } else {
                log::warn!("affinity: live warp is degenerate; placing the layer unwarped");
            }
        }
        let img = img;
        if !map.axis_aligned() {
            return affine_resample(&img, &map);
        }
        let (ax, ay) = map.apply(0.0, 0.0);
        let (bx, by) = map.apply(img.width as f64, img.height as f64);
        let sane = |v: f64| v.is_finite() && v.abs() < (1 << 24) as f64;
        let rect = if sane(ax) && sane(ay) && sane(bx) && sane(by) {
            IntRect::new(
                ax.min(bx).round() as i32,
                ay.min(by).round() as i32,
                ax.max(bx).round() as i32,
                ay.max(by).round() as i32,
            )
        } else {
            IntRect::EMPTY
        };
        let rect = if rect.is_empty() {
            IntRect::from_size(img.width, img.height)
        } else {
            rect
        };
        let mut img = resample_to(img, rect.width() as u32, rect.height() as u32);
        // A mirror is axis-aligned too — zero shear, negative scale — so
        // it lands here rather than in the resampler. The rect above is
        // normalised, which puts the box in the right place but leaves
        // the pixels facing the wrong way; turn them over.
        mirror(&mut img, map.0[0] < 0.0, map.0[4] < 0.0);
        (img.pixels.len() == rect.width() as usize * rect.height() as usize * 4)
            .then_some((rect, img))
    }

    pub(super) fn decode_bitmap(&self, bitm: &Node) -> Result<RgbaImage, AffinityError> {
        decode_bitmap(self.archive, self.graph, bitm)
    }
}
