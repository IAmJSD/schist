//! `RVgC`, the live Vignette — the one live filter that darkens rather
//! than moving or mixing pixels.
//!
//! Four fields, all measured against Affinity's own renders of the
//! RGB-cube card (`fixtures/affinity-probe/lf_vig_*.af`):
//!
//! - `Scal` and `Shap` are the ellipse. `Scal` scales it against half
//!   the layer, and `Shap` squeezes the horizontal semi-axis alone: at
//!   `Shap` 1 the iso-curves are circles, at 0.5 the ellipse is half as
//!   wide and exactly as tall (its profile down the y axis matches
//!   `Scal` 1's and across the x axis matches `Scal` 0.5's, which is
//!   what identifies it), and at 0 it collapses and the whole layer
//!   takes the full exposure.
//! - `Hard` is where the ramp starts, as a fraction of the ellipse: the
//!   weight is a smoothstep from `Hard` to just short of the edge, so
//!   `Hard` 1 is a clean step at the ellipse itself and `Hard` 0 ramps
//!   from the centre out. Probed at 0, 0.75 and 1, this lands the
//!   quarter, half and three-quarter points within about 4 pixels.
//! - `Expo` is the darkening, and it is *not* an exposure in linear
//!   light. It multiplies the encoded value by a constant: −1, −2 and
//!   −4 stops come back as 0.726, 0.529 and 0.280 of the input across
//!   the whole ramp, to a spread of 0.004. That is 2^(`Expo`/2.2) —
//!   an exposure taken in a plain 2.2-gamma space — to within 0.4%.

/// The vignette's four fields, its ellipse already in layer pixels.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Vignette {
    pub exposure: f64,
    pub hardness: f64,
    pub scale: f64,
    pub shape: f64,
}

/// The gamma the exposure is taken in — see `Expo` above.
const VIGNETTE_GAMMA: f64 = 2.2;

/// How far short of the ellipse the ramp finishes, as a fraction of the
/// way from `Hard` to the edge. Fitted over the three hardness probes.
const RAMP_INSET: f64 = 0.2;

pub(crate) fn apply(width: u32, height: u32, pixels: &[u8], v: &Vignette) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || v.exposure == 0.0 {
        return pixels.to_vec();
    }
    let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
    let (a, b) = (v.scale * cx * v.shape, v.scale * cy);
    let inner = v.hardness.clamp(0.0, 1.0);
    let outer = 1.0 - RAMP_INSET * (1.0 - inner);
    // Everything the ramp needs is per pixel bar the exposure, which is
    // one table lookup wide: 256 encoded values by whatever weight the
    // pixel lands on.
    let mut out = pixels.to_vec();
    for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (x, y) = ((i % w) as f64 + 0.5, (i / w) as f64 + 0.5);
        let weight = if a <= 0.0 || b <= 0.0 {
            1.0
        } else {
            let rho = ((x - cx) / a).hypot((y - cy) / b);
            let t = ((rho - inner) / (outer - inner).max(1e-6)).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        };
        if weight <= 0.0 {
            continue;
        }
        let gain = ((v.exposure * weight) / VIGNETTE_GAMMA).exp2() as f32;
        for v in &mut px[..3] {
            *v = (*v as f32 * gain + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    out
}
