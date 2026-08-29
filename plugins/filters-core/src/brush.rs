//! Filter Gallery ▸ Brush Strokes.
//!
//! Eight ways of putting a mark on the paper. Where the Artistic group is
//! about *paint* -- flat areas, thick pigment, texture -- this one is
//! about *strokes*, and a stroke is one operation: smear the image along
//! a line rather than in a circle. The whole group is
//! [`crate::util::streak`] with a different line each time, plus what
//! each effect does with the ink afterwards.

use crate::util::{blur_plane, edges, from_luma, luma_map, put, sample, streak, value_noise};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// The four stroke directions Photoshop offers, as unit vectors.
pub const DIRECTIONS: &[&str] = &["Right Diagonal", "Horizontal", "Left Diagonal", "Vertical"];

pub fn direction_of(pick: f32) -> (f32, f32) {
    match (pick.round().max(0.0) as usize).min(3) {
        0 => (0.707, -0.707),
        1 => (1.0, 0.0),
        2 => (0.707, 0.707),
        _ => (0.0, 1.0),
    }
}

simple_filter!(
    AccentedEdges,
    "filter.accented_edges",
    "Accented Edges",
    "Brush Strokes",
    [
        param("width", "Edge Width", 1.0, 14.0, 2.0, ""),
        param("brightness", "Edge Brightness", 0.0, 50.0, 38.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 5.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // The edges are picked out in ink: bright ink above the halfway
        // mark on the brightness slider, black below it, which is the
        // control Photoshop gives and the reason the filter can look
        // either chalky or charred.
        let width = v.get("width");
        let brightness = v.get("brightness") / 50.0;
        let smoothness = v.get("smoothness");
        // Smoothness settles the picture *before* the edges are found,
        // which is what keeps the accents on the subject's outline
        // rather than on every leaf and ripple.
        let mut plane = luma_map(px, w, h);
        if smoothness > 1.0 {
            blur_plane(&mut plane, w, h, smoothness * 0.2);
        }
        let mut edge = edges(&plane, w, h);
        blur_plane(&mut edge, w, h, width * 0.25);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let ink = (edge[i] * (2.0 + width) * 2.5).min(1.0);
            let target = if brightness >= 0.5 { 1.0 } else { 0.0 };
            let amount = ((brightness - 0.5).abs() * 2.0).max(0.15);
            for v in p.iter_mut().take(3) {
                *v = (*v + (target - *v) * ink * amount).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    AngledStrokes,
    "filter.angled_strokes",
    "Angled Strokes",
    "Brush Strokes",
    [
        param("balance", "Direction Balance", 0.0, 100.0, 50.0, ""),
        param("length", "Stroke Length", 3.0, 50.0, 15.0, ""),
        param("sharpness", "Sharpness", 0.0, 10.0, 3.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Light areas are painted one way and dark areas the other, so
        // the two sets of strokes meet along the tonal boundaries. The
        // balance decides where the handover happens.
        let balance = v.get("balance") / 100.0;
        let length = v.get("length");
        let sharpness = v.get("sharpness") / 10.0;
        let plane = luma_map(px, w, h);
        let up = streak(&plane, w, h, length * 0.3, |_, _| (0.707, -0.707));
        let down = streak(&plane, w, h, length * 0.3, |_, _| (0.707, 0.707));
        let mut out = vec![0.0f32; w * h];
        for i in 0..w * h {
            let pick = if plane[i] > balance { &up } else { &down };
            // Sharpness returns some of the original tone, which is what
            // keeps the strokes from dissolving the subject.
            out[i] = pick[i] + (plane[i] - pick[i]) * sharpness;
        }
        from_luma(px, &out, 1.0);
    }
);

simple_filter!(
    Crosshatch,
    "filter.crosshatch",
    "Crosshatch",
    "Brush Strokes",
    [
        param("length", "Stroke Length", 3.0, 50.0, 9.0, ""),
        param("sharpness", "Sharpness", 0.0, 20.0, 6.0, ""),
        param("strength", "Strength", 1.0, 3.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Pencil hatching in both diagonals at once. Taking the darker of
        // the two passes is what makes them read as crossing rather than
        // as an average of two blurs.
        let length = v.get("length");
        let sharpness = v.get("sharpness") / 20.0;
        let strength = v.get("strength");
        let plane = luma_map(px, w, h);
        let mut out = plane.clone();
        for pass in 0..strength.round().max(1.0) as usize {
            let a = streak(&out, w, h, length * 0.3, |_, _| (0.707, -0.707));
            let b = streak(&out, w, h, length * 0.3, |_, _| (0.707, 0.707));
            for i in 0..w * h {
                let hatched = a[i].min(b[i]);
                // Every pass bites a little deeper, as a second layer of
                // hatching would.
                let bite = 0.6 + 0.2 * pass as f32;
                out[i] = out[i] + (hatched - out[i]) * bite;
            }
        }
        for i in 0..w * h {
            out[i] += (plane[i] - out[i]) * sharpness;
        }
        from_luma(px, &out, 1.0);
    }
);

simple_filter!(
    DarkStrokes,
    "filter.dark_strokes",
    "Dark Strokes",
    "Brush Strokes",
    [
        param("balance", "Balance", 0.0, 10.0, 5.0, ""),
        param("black", "Black Intensity", 0.0, 10.0, 6.0, ""),
        param("white", "White Intensity", 0.0, 10.0, 2.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Short strokes, black in the shadows and white in the
        // highlights, with the balance deciding which tone counts as
        // which. Photoshop's darkens hard; so does this.
        let balance = v.get("balance") / 10.0;
        let black = v.get("black") / 10.0;
        let white = v.get("white") / 10.0;
        let plane = luma_map(px, w, h);
        let strokes = streak(&plane, w, h, 4.0, |_, _| (0.707, 0.707));
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let l = strokes[i];
            let (target, amount) = if l < balance {
                (0.0, (balance - l) / balance.max(1e-3) * black)
            } else {
                (1.0, (l - balance) / (1.0 - balance).max(1e-3) * white)
            };
            for v in p.iter_mut().take(3) {
                let smeared = *v + (l - plane[i]);
                *v = (smeared + (target - smeared) * amount.clamp(0.0, 1.0)).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    InkOutlines,
    "filter.ink_outlines",
    "Ink Outlines",
    "Brush Strokes",
    [
        param("length", "Stroke Length", 1.0, 50.0, 4.0, ""),
        param("dark", "Dark Intensity", 0.0, 50.0, 20.0, ""),
        param("light", "Light Intensity", 0.0, 50.0, 10.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A pen drawing over the photograph: fine ink lines where the
        // edges are, dragged along the stroke so they have a nib's
        // thick-and-thin rather than a uniform outline.
        let length = v.get("length");
        let dark = v.get("dark") / 50.0;
        let light = v.get("light") / 50.0;
        let plane = luma_map(px, w, h);
        let edge = edges(&plane, w, h);
        let inked = streak(&edge, w, h, length * 0.25, |_, _| (0.707, 0.707));
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let ink = (inked[i] * 3.0).min(1.0);
            for v in p.iter_mut().take(3) {
                // Dark ink on the lines, and the highlights bleached to
                // make room for it.
                let darkened = *v * (1.0 - ink * dark * 1.6);
                *v = (darkened + (1.0 - darkened) * (1.0 - ink) * light * plane[i] * 0.4)
                    .clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Spatter,
    "filter.spatter",
    "Spatter",
    "Brush Strokes",
    [
        param("radius", "Spray Radius", 0.0, 25.0, 10.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 5.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // An airbrush spitting: every pixel is fetched from somewhere
        // nearby at random, so edges break up into flecks. The randomness
        // is hashed from the coordinate, so the same image always
        // spatters the same way.
        let radius = v.get("radius");
        let smoothness = v.get("smoothness");
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let jx = value_noise(x as f32 * 1.7, y as f32 * 1.3, 6151) - 0.5;
                let jy = value_noise(x as f32 * 1.1, y as f32 * 1.9, 7919) - 0.5;
                // Smoothness pulls the flecks back towards where they
                // came from.
                let reach = radius * (16.0 - smoothness) / 15.0;
                let p = sample(
                    &src,
                    w,
                    h,
                    x as f32 + jx * reach * 2.0,
                    y as f32 + jy * reach * 2.0,
                );
                put(px, w, x, y, p);
            }
        }
    }
);

simple_filter!(
    SprayedStrokes,
    "filter.sprayed_strokes",
    "Sprayed Strokes",
    "Brush Strokes",
    [
        param("length", "Stroke Length", 0.0, 20.0, 12.0, ""),
        param("radius", "Spray Radius", 0.0, 25.0, 7.0, ""),
        choice("direction", "Stroke Direction", DIRECTIONS, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Spatter with a grain to it: the jitter runs along the chosen
        // stroke direction rather than in every direction, so the flecks
        // line up into strokes.
        let length = v.get("length");
        let radius = v.get("radius");
        let (dx, dy) = direction_of(v.get("direction"));
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let along = (value_noise(x as f32 * 0.9, y as f32 * 0.9, 3571) - 0.5) * length;
                let across = (value_noise(x as f32 * 1.6, y as f32 * 1.2, 2963) - 0.5) * radius;
                let (px_, py_) = (
                    x as f32 + dx * along - dy * across * 0.4,
                    y as f32 + dy * along + dx * across * 0.4,
                );
                put(px, w, x, y, sample(&src, w, h, px_, py_));
            }
        }
    }
);

simple_filter!(
    SumiE,
    "filter.sumi_e",
    "Sumi-e",
    "Brush Strokes",
    [
        param("width", "Stroke Width", 3.0, 15.0, 10.0, ""),
        param("pressure", "Stroke Pressure", 0.0, 15.0, 2.0, ""),
        param("contrast", "Contrast", 0.0, 40.0, 16.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Ink on wet rice paper: a loaded brush, very few tones, and the
        // dark drowning everything it touches. The width is a smear
        // across the stroke, which is what gives the soft edge; the
        // pressure decides how much ink is in the brush.
        let width = v.get("width");
        let pressure = v.get("pressure") / 15.0;
        let contrast = v.get("contrast") / 40.0;
        let mut plane = luma_map(px, w, h);
        blur_plane(&mut plane, w, h, width * 0.15);
        let ink = streak(&plane, w, h, width * 0.5, |_, _| (0.707, 0.707));
        let mut out = vec![0.0f32; w * h];
        for i in 0..w * h {
            // Contrast around the midpoint, then the ink pooled into the
            // darks by the brush pressure.
            let hard = ((ink[i] - 0.5) * (1.0 + contrast * 1.5) + 0.5).clamp(0.0, 1.0);
            out[i] = hard * (1.0 - (1.0 - hard) * pressure);
        }
        // Enough colour left to tell what the subject was, which is how
        // sumi-e over a photograph reads rather than a threshold.
        from_luma(px, &out, 0.5);
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(AccentedEdges));
    registry.register_filter(Box::new(AngledStrokes));
    registry.register_filter(Box::new(Crosshatch));
    registry.register_filter(Box::new(DarkStrokes));
    registry.register_filter(Box::new(InkOutlines));
    registry.register_filter(Box::new(Spatter));
    registry.register_filter(Box::new(SprayedStrokes));
    registry.register_filter(Box::new(SumiE));
}
