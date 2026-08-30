//! Live filters: which ones we can reproduce, and the warp or
//! distortion each applies to the pixels beneath it.

use super::*;

impl Walker<'_> {
    /// True when a live filter node's warp maps every source quad onto
    /// itself — it changes nothing on screen.
    ///
    /// Only Live Perspective carries quads. Every other filter keeps
    /// plain parameter fields, and a node with no quads at all is not
    /// inert but unexamined, so it has to say so rather than claim to
    /// be an identity.
    pub(super) fn filter_is_identity(&self, node: &Node) -> bool {
        let Some(filt) = self.graph.child(node, b"Filt") else {
            return false;
        };
        let mut saw_quad = false;
        for (src, dst) in [(b"DSrA", b"DDsA"), (b"DSrB", b"DDsB"), (b"Src ", b"Dst ")] {
            match (self.quad(filt, src), self.quad(filt, dst)) {
                (Some(a), Some(b)) => {
                    saw_quad = true;
                    if a.iter()
                        .zip(&b)
                        .any(|(p, q)| (p.0 - q.0).abs() > 1e-6 || (p.1 - q.1).abs() > 1e-6)
                    {
                        return false;
                    }
                }
                (None, None) => {}
                _ => return false,
            }
        }
        saw_quad
    }

    /// One of a filter's corner quads. `Quad` stores its corners as
    /// eight `F64` fields in file order — the four xs, then the four ys
    /// — going top-left, bottom-left, bottom-right, top-right.
    pub(super) fn quad(&self, filt: &Node, name: &[u8; 4]) -> Option<[(f64, f64); 4]> {
        let q = self.graph.child(filt, name)?;
        let v: Vec<f64> = q
            .fields
            .iter()
            .filter_map(|(_, v)| match v {
                Value::F64(f) => Some(*f),
                _ => None,
            })
            .collect();
        if v.len() < 8 || !v[..8].iter().all(|f| f.is_finite()) {
            return None;
        }
        Some([(v[0], v[4]), (v[1], v[5]), (v[2], v[6]), (v[3], v[7])])
    }

    /// The projective map one live filter applies to the pixels it sits
    /// on, in their own bitmap space. `None` means we cannot reproduce
    /// this filter — either it is not a warp at all, or it is Live
    /// Perspective in its two-plane mode (`DMod`), which folds two
    /// homographies over halves of the layer.
    pub(super) fn warp_of(&self, flrn: &Node) -> Option<Homography> {
        let filt = self.graph.child(flrn, b"Filt")?;
        if bool_of(filt, b"DMod") == Some(true) {
            return None;
        }
        let (src, dst) = (self.quad(filt, b"Src ")?, self.quad(filt, b"Dst ")?);
        let h = Homography::from_quads(&src, &dst)?;
        (!h.is_identity()).then_some(h)
    }

    /// The geometric filter one `FlRN` applies, if we know how to run
    /// it. Its centre and radii are already in the layer's own pixel
    /// space, which is where the fields are stored.
    ///
    /// Every mapping is measured, not guessed; the derivations are in
    /// [`crate::distort`] and in the spec.
    pub(super) fn distort_of(&self, flrn: &Node) -> Option<LiveFilter> {
        let filt = self.graph.child(flrn, b"Filt")?;
        let centre = |tag: &[u8; 4]| -> (f64, f64) {
            match f64s(filt, tag) {
                Some([x, y, ..]) => (*x, *y),
                _ => (0.0, 0.0),
            }
        };
        let num = |tag: &[u8; 4]| f64_of(filt, tag).unwrap_or(0.0);
        let d = match &filt.type_tag().to_be_bytes() {
            b"RTwC" => {
                let (cx, cy) = centre(b"Orin");
                Distort::Twirl {
                    cx,
                    cy,
                    radius: num(b"Radi"),
                    angle_deg: num(b"Angl"),
                }
            }
            b"RPPC" => {
                let (cx, cy) = centre(b"Orig");
                Distort::Pinch {
                    cx,
                    cy,
                    radius: num(b"Radi"),
                    amount: num(b"Inte") / 100.0,
                }
            }
            b"RSpC" => {
                let (cx, cy) = centre(b"Orig");
                Distort::Spherical {
                    cx,
                    cy,
                    radius: num(b"Radi"),
                    amount: num(b"Inte") / 100.0,
                }
            }
            b"RRiC" => {
                let (cx, cy) = centre(b"Orig");
                Distort::Ripple {
                    cx,
                    cy,
                    intensity: num(b"Inte"),
                }
            }
            b"RLdC" => {
                let (cx, cy) = centre(b"Orig");
                Distort::Lens {
                    cx,
                    cy,
                    rad_x: num(b"RadX"),
                    rad_y: num(b"RadY"),
                    amount: num(b"Inte"),
                }
            }
            b"RPxC" => Distort::Pixelate { size: num(b"Quan") },
            b"RGBC" => {
                return Some(LiveFilter::Blur(LiveBlur::Gaussian {
                    radius: num(b"Radi"),
                }))
            }
            b"RBBC" => {
                return Some(LiveFilter::Blur(LiveBlur::Box {
                    radius: num(b"Radi"),
                }))
            }
            b"RMoB" => {
                return Some(LiveFilter::Blur(LiveBlur::Motion {
                    radius: num(b"Radi"),
                    angle_rad: num(b"Angl"),
                }))
            }
            b"RRaB" => {
                let (cx, cy) = centre(b"Cent");
                return Some(LiveFilter::Blur(LiveBlur::Radial {
                    cx,
                    cy,
                    angle_deg: num(b"Angl"),
                }));
            }
            b"RMBC" => {
                return Some(LiveFilter::Blur(LiveBlur::Maximum {
                    radius: num(b"Radi"),
                    circular: bool_of(filt, b"Circ").unwrap_or(false),
                }))
            }
            b"RMeB" => {
                return Some(LiveFilter::Blur(LiveBlur::Median {
                    radius: num(b"Radi"),
                }))
            }
            b"RVgC" => {
                return Some(LiveFilter::Vignette(Vignette {
                    exposure: num(b"Expo"),
                    hardness: num(b"Hard"),
                    scale: num(b"Scal"),
                    shape: num(b"Shap"),
                }))
            }
            b"D&SC" => {
                return Some(LiveFilter::Blur(LiveBlur::DustAndScratches {
                    radius: num(b"Radi"),
                    tolerance: num(b"Tole"),
                    per_channel: bool_of(filt, b"Chan").unwrap_or(false),
                }))
            }
            b"RHPC" => {
                return Some(LiveFilter::Blur(LiveBlur::HighPass {
                    radius: num(b"Radi"),
                    mono: bool_of(filt, b"Mono").unwrap_or(false),
                }))
            }
            b"RUSC" => {
                return Some(LiveFilter::Blur(LiveBlur::Unsharp {
                    radius: num(b"Radi"),
                    factor: num(b"Fact"),
                    threshold: num(b"Thrs"),
                }))
            }
            _ => return None,
        };
        Some(LiveFilter::Geometry(d))
    }

    /// Every live filter we can run on a node, in `AdCh` order — a later
    /// one works on what the earlier one produced.
    pub(super) fn live_filters(&self, node: &Node) -> Vec<LiveFilter> {
        self.graph
            .children(node, b"AdCh")
            .into_iter()
            .filter(|f| f.types.iter().any(|(t, _)| *t == graph::tag(b"FlRN")))
            .filter(|f| bool_of(f, b"Visi") != Some(false))
            .filter_map(|f| self.distort_of(f))
            .collect()
    }

    /// Every warp on a node, composed into one map. Filters apply in
    /// `AdCh` order, so a later one maps the earlier one's output.
    pub(super) fn live_warp(&self, node: &Node) -> Option<Homography> {
        let mut out: Option<Homography> = None;
        for f in self.graph.children(node, b"AdCh") {
            if !f.types.iter().any(|(t, _)| *t == graph::tag(b"FlRN")) {
                continue;
            }
            if bool_of(f, b"Visi") == Some(false) {
                continue;
            }
            let Some(h) = self.warp_of(f) else { continue };
            out = Some(match out {
                None => h,
                Some(prev) => h.compose(&prev),
            });
        }
        out
    }
}
