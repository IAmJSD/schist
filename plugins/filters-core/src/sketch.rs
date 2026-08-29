//! Filter Gallery ▸ Sketch.
//!
//! Fourteen effects that all end in two colours. In Photoshop those are
//! the foreground and background from the toolbox; a filter here is
//! handed pixels and numbers and never sees the toolbox, so these draw in
//! black on white -- which is what the swatches are set to by default and
//! what every one of these effects is pictured with.
//!
//! Nearly all of them are a *tone curve with a texture in it*: decide
//! which pixels are ink, decide what the ink looks like where it lands,
//! and let the paper take the rest. The ones that are not -- Bas Relief,
//! Chrome, Note Paper, Plaster -- light the image as a surface instead,
//! which is why they look raised rather than drawn.

use crate::util::{
    blur_plane, edges, fbm, from_luma, gradient, luma_map, streak, surface, value_noise,
};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Photoshop's eight light positions, as a direction to light from.
pub const LIGHTS: &[&str] = &[
    "Top",
    "Top Right",
    "Right",
    "Bottom Right",
    "Bottom",
    "Bottom Left",
    "Left",
    "Top Left",
];

pub fn light_of(pick: f32) -> (f32, f32) {
    let i = (pick.round().max(0.0) as usize).min(7);
    let a = i as f32 * std::f32::consts::FRAC_PI_4 - std::f32::consts::FRAC_PI_2;
    (a.cos(), a.sin())
}

/// Relief: light a plane as though it were a surface, mid grey being flat.
///
/// The shared core of Bas Relief, Note Paper, Plaster and Chrome. What
/// separates those four is what they hand in as the height field.
fn relief(plane: &[f32], w: usize, h: usize, light: (f32, f32), strength: f32) -> Vec<f32> {
    gradient(plane, w, h)
        .iter()
        .map(|(gx, gy)| (0.5 + (gx * light.0 + gy * light.1) * strength).clamp(0.0, 1.0))
        .collect()
}

simple_filter!(
    BasRelief,
    "filter.bas_relief",
    "Bas Relief",
    "Sketch",
    [
        param("detail", "Detail", 1.0, 15.0, 13.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 3.0, ""),
        choice("light", "Light", LIGHTS, 3)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Carved shallowly into stone: the picture as a height field, lit
        // from one side, with everything flat going mid grey.
        let detail = v.get("detail");
        let smoothness = v.get("smoothness");
        let light = light_of(v.get("light"));
        let mut plane = luma_map(px, w, h);
        blur_plane(&mut plane, w, h, smoothness * 0.25);
        let out = relief(&plane, w, h, light, detail * 0.9);
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    ChalkAndCharcoal,
    "filter.chalk_charcoal",
    "Chalk & Charcoal",
    "Sketch",
    [
        param("charcoal", "Charcoal Area", 0.0, 20.0, 6.0, ""),
        param("chalk", "Chalk Area", 0.0, 20.0, 6.0, ""),
        param("pressure", "Stroke Pressure", 0.0, 5.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Charcoal in the shadows, chalk in the highlights, mid grey
        // paper in between -- and the two drawn at opposite diagonals,
        // which is what stops the result reading as a posterisation.
        let charcoal = v.get("charcoal") / 20.0;
        let chalk = v.get("chalk") / 20.0;
        let pressure = 0.5 + v.get("pressure") / 5.0;
        let plane = luma_map(px, w, h);
        let dark = streak(&plane, w, h, 4.0, |_, _| (0.707, 0.707));
        let light = streak(&plane, w, h, 4.0, |_, _| (0.707, -0.707));
        let mut out = vec![0.5f32; w * h];
        for i in 0..w * h {
            let l = plane[i];
            if l < 0.5 {
                // How far into the charcoal end this pixel is. The
                // multiplier is what makes it a drawing rather than a
                // grey wash: charcoal covers fast once it touches.
                let t = ((0.5 - l) / 0.5 * (0.6 + charcoal * 2.0)).min(1.0) * pressure;
                out[i] = 0.5 - t * (0.5 + (0.5 - dark[i]));
            } else {
                let t = ((l - 0.5) / 0.5 * (0.6 + chalk * 2.0)).min(1.0) * pressure;
                out[i] = 0.5 + t * (0.5 + (light[i] - 0.5));
            }
            out[i] = out[i].clamp(0.0, 1.0);
        }
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    Charcoal,
    "filter.charcoal",
    "Charcoal",
    "Sketch",
    [
        param("thickness", "Charcoal Thickness", 1.0, 7.0, 1.0, ""),
        param("detail", "Detail", 0.0, 5.0, 5.0, ""),
        param("balance", "Light/Dark Balance", 0.0, 100.0, 50.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Smudged charcoal on paper: the edges are where the stick
        // presses hardest, the tone is dragged along the stroke, and the
        // balance slides the whole thing between a sketch and a
        // silhouette.
        let thickness = v.get("thickness");
        let detail = v.get("detail") / 5.0;
        let balance = v.get("balance") / 100.0;
        let plane = luma_map(px, w, h);
        let edge = edges(&plane, w, h);
        let smudged = streak(&plane, w, h, thickness * 2.0, |_, _| (0.707, 0.707));
        let mut out = vec![0.0f32; w * h];
        for i in 0..w * h {
            // Paper tone, darkened by the edges and by how dark the
            // subject was.
            let ink = (edge[i] * (2.0 + detail * 6.0)).min(1.0) * (0.5 + detail * 0.5)
                + (1.0 - smudged[i]) * balance;
            out[i] = (1.0 - ink).clamp(0.0, 1.0);
        }
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    Chrome,
    "filter.chrome",
    "Chrome",
    "Sketch",
    [
        param("detail", "Detail", 0.0, 10.0, 4.0, ""),
        param("smoothness", "Smoothness", 0.0, 10.0, 7.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Polished metal: the picture as a surface, lit, and then its
        // tones folded back on themselves so that every slope crosses
        // black and white several times. The folding is the whole trick
        // -- it is what turns shading into reflections.
        let detail = v.get("detail");
        let smoothness = v.get("smoothness");
        let mut plane = luma_map(px, w, h);
        blur_plane(&mut plane, w, h, 0.5 + smoothness * 0.5);
        let lit = relief(&plane, w, h, (0.707, -0.707), 6.0 + detail * 3.0);
        let mut out = vec![0.0f32; w * h];
        for i in 0..w * h {
            let folded = ((lit[i] - 0.5) * (2.0 + detail) * std::f32::consts::PI).sin();
            out[i] = (0.5 + folded * 0.5).clamp(0.0, 1.0);
        }
        blur_plane(&mut out, w, h, 0.4);
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    ConteCrayon,
    "filter.conte_crayon",
    "Cont\u{e9} Crayon",
    "Sketch",
    [
        param("foreground", "Foreground Level", 1.0, 15.0, 8.0, ""),
        param("background", "Background Level", 1.0, 15.0, 8.0, ""),
        choice("texture", "Texture", crate::artistic::SURFACES, 0),
        param("scaling", "Scaling", 50.0, 200.0, 100.0, "%"),
        param("relief", "Relief", 0.0, 50.0, 20.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A dense, soft crayon that only touches the high points of the
        // paper. Foreground Level is how far up the tones the dark
        // crayon reaches; Background Level is how far down the light one
        // comes to meet it.
        let fg = v.get("foreground") / 15.0;
        let bg = v.get("background") / 15.0;
        let kind = v.get("texture").round().max(0.0) as u32;
        let scaling = (v.get("scaling") / 100.0 * 6.0).max(1.0);
        let relief_amount = v.get("relief") / 50.0;
        let plane = luma_map(px, w, h);
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let tex = surface(kind, x as f32, y as f32, scaling, 53);
                // The crayon lands where the paper is high. Clamped
                // before the curve below, which raises it to a power and
                // would otherwise be asked for the root of a negative.
                let grip = 1.0 + (tex - 0.5) * relief_amount * 1.5;
                let l = (plane[i] * grip).clamp(0.0, 1.0);
                out[i] = if l < 0.5 {
                    (l / 0.5).powf(1.0 + fg * 2.0) * 0.5
                } else {
                    1.0 - ((1.0 - l) / 0.5).powf(1.0 + bg * 2.0) * 0.5
                }
                .clamp(0.0, 1.0);
            }
        }
        from_luma(px, &out, 0.15);
    }
);

simple_filter!(
    GraphicPen,
    "filter.graphic_pen",
    "Graphic Pen",
    "Sketch",
    [
        param("length", "Stroke Length", 1.0, 15.0, 15.0, ""),
        param("balance", "Light/Dark Balance", 0.0, 100.0, 50.0, ""),
        choice("direction", "Stroke Direction", crate::brush::DIRECTIONS, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // One pen, one direction, no grey: the tone is carried entirely
        // by how much of the paper the hatching covers. Every stroke is
        // the same weight, which is what makes it a pen drawing rather
        // than a wash.
        let length = v.get("length");
        let balance = v.get("balance") / 100.0;
        let (dx, dy) = crate::brush::direction_of(v.get("direction"));
        let mut plane = luma_map(px, w, h);
        plane = streak(&plane, w, h, length * 0.5, |_, _| (dx, dy));
        let mut out = vec![1.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                // The hatch runs across the stroke direction, so a
                // horizontal stroke draws horizontal lines.
                let phase = (x as f32 * dy - y as f32 * dx) * 0.5;
                let comb = (phase * std::f32::consts::PI).sin().abs();
                let ink = (1.0 - plane[i]) * (0.4 + balance * 1.2);
                out[i] = if comb < ink { 0.0 } else { 1.0 };
            }
        }
        from_luma(px, &out, 0.0);
    }
);

/// The three patterns Photoshop's halftone offers.
const HALFTONE_PATTERNS: &[&str] = &["Circle", "Dot", "Line"];

simple_filter!(
    HalftonePattern,
    "filter.halftone_pattern",
    "Halftone Pattern",
    "Sketch",
    [
        param("size", "Size", 1.0, 12.0, 1.0, ""),
        param("contrast", "Contrast", 0.0, 50.0, 5.0, ""),
        choice("pattern", "Pattern Type", HALFTONE_PATTERNS, 1)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A screen, as a newspaper would print it -- but drawn *over* the
        // tone rather than replacing it, which is the difference between
        // this and Pixelate ▸ Color Halftone. Circles ripple out from the
        // middle of the image; dots and lines are a grid.
        let size = v.get("size").max(1.0) * 4.0;
        let contrast = v.get("contrast") / 50.0;
        let pattern = (v.get("pattern").round().max(0.0) as usize).min(2);
        let plane = luma_map(px, w, h);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let screen = match pattern {
                    0 => {
                        let d = (x as f32 - cx).hypot(y as f32 - cy);
                        (d / size * std::f32::consts::TAU).sin() * 0.5 + 0.5
                    }
                    1 => {
                        let u = (x as f32 / size * std::f32::consts::TAU).sin();
                        let vv = (y as f32 / size * std::f32::consts::TAU).sin();
                        (u * vv) * 0.5 + 0.5
                    }
                    _ => (y as f32 / size * std::f32::consts::TAU).sin() * 0.5 + 0.5,
                };
                // Contrast pushes the tone towards the two extremes
                // before the screen decides.
                let tone = ((plane[i] - 0.5) * (1.0 + contrast * 4.0) + 0.5).clamp(0.0, 1.0);
                out[i] = if screen < tone { 1.0 } else { 0.0 };
            }
        }
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    NotePaper,
    "filter.note_paper",
    "Note Paper",
    "Sketch",
    [
        param("balance", "Image Balance", 0.0, 50.0, 25.0, ""),
        param("graininess", "Graininess", 0.0, 20.0, 10.0, ""),
        param("relief", "Relief", 0.0, 25.0, 11.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Handmade paper with the image pressed into it: the picture is
        // reduced to two levels, and the boundary between them is
        // embossed as though the light paper had been pushed through the
        // dark. The grain is in the paper, not in the image.
        let balance = v.get("balance") / 50.0;
        let grain = v.get("graininess") / 20.0;
        let relief_amount = v.get("relief") / 25.0;
        let mut plane = luma_map(px, w, h);
        blur_plane(&mut plane, w, h, 1.5);
        let embossed = relief(&plane, w, h, (0.707, -0.707), 6.0 * relief_amount);
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let sheet = if plane[i] > 1.0 - balance { 0.85 } else { 0.35 };
                let g = (value_noise(x as f32 * 1.7, y as f32 * 1.7, 811) - 0.5) * grain * 0.5;
                out[i] = (sheet + (embossed[i] - 0.5) * 0.9 + g).clamp(0.0, 1.0);
            }
        }
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    Photocopy,
    "filter.photocopy",
    "Photocopy",
    "Sketch",
    [
        param("detail", "Detail", 1.0, 24.0, 7.0, ""),
        param("darkness", "Darkness", 1.0, 50.0, 8.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A bad photocopy: the machine holds the edges and the deepest
        // shadows and gives up on everything in between, which is why a
        // photocopied photograph comes back as an outline.
        let detail = v.get("detail");
        let darkness = v.get("darkness") / 50.0;
        let plane = luma_map(px, w, h);
        let mut local = plane.clone();
        blur_plane(&mut local, w, h, 1.0 + detail * 0.5);
        let mut out = vec![0.0f32; w * h];
        for i in 0..w * h {
            // Where the pixel is darker than its surroundings, ink; where
            // it is not, paper. Plus the shadows the toner floods.
            let below = (local[i] - plane[i]) * (4.0 + darkness * 20.0);
            let flooded = ((0.25 - plane[i]) * 6.0 * darkness).max(0.0);
            out[i] = (1.0 - below.max(0.0) - flooded).clamp(0.0, 1.0);
        }
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    Plaster,
    "filter.plaster",
    "Plaster",
    "Sketch",
    [
        param("balance", "Image Balance", 0.0, 50.0, 25.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 2.0, ""),
        choice("light", "Light", LIGHTS, 5)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Poured and set: the dark half of the picture rises out of the
        // light half as a smooth blob, lit from one side. Everything
        // inside a region is flat, so it reads as moulded rather than
        // drawn.
        let balance = v.get("balance") / 50.0;
        let smoothness = v.get("smoothness");
        let light = light_of(v.get("light"));
        let mut plane = luma_map(px, w, h);
        blur_plane(&mut plane, w, h, smoothness * 0.8);
        // A soft step: the height field is the picture pushed to two
        // levels, then rounded off.
        let mut height: Vec<f32> = plane
            .iter()
            .map(|l| if *l > 1.0 - balance { 1.0 } else { 0.0 })
            .collect();
        blur_plane(&mut height, w, h, 2.0 + smoothness);
        let lit = relief(&height, w, h, light, 14.0);
        let out: Vec<f32> = lit
            .iter()
            .zip(&height)
            .map(|(l, hgt)| (l * 0.65 + hgt * 0.35).clamp(0.0, 1.0))
            .collect();
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    Reticulation,
    "filter.reticulation",
    "Reticulation",
    "Sketch",
    [
        param("density", "Density", 0.0, 50.0, 12.0, ""),
        param("black", "Black Level", 0.0, 50.0, 40.0, ""),
        param("white", "White Level", 0.0, 50.0, 5.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Film emulsion that has cracked and clumped -- what happens when
        // a negative is developed at the wrong temperature. The clumps
        // gather in the shadows and the highlights break into speckle.
        let density = 1.0 + v.get("density") / 50.0 * 8.0;
        let black = v.get("black") / 50.0;
        let white = v.get("white") / 50.0;
        let plane = luma_map(px, w, h);
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let clump = fbm(x as f32 / density, y as f32 / density, 4409, 3);
                let l = plane[i];
                // Dark pixels take the clumping hardest, light ones
                // sparkle.
                let grain = (clump - 0.5) * (black * (1.0 - l) + white * l) * 2.0;
                out[i] = (l + grain).clamp(0.0, 1.0);
            }
        }
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    Stamp,
    "filter.stamp",
    "Stamp",
    "Sketch",
    [
        param("balance", "Light/Dark Balance", 0.0, 50.0, 25.0, ""),
        param("smoothness", "Smoothness", 1.0, 50.0, 5.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A rubber stamp: one threshold, and enough smoothing first that
        // the boundary could have been cut out of rubber.
        let balance = v.get("balance") / 50.0;
        let smoothness = v.get("smoothness");
        let mut plane = luma_map(px, w, h);
        blur_plane(&mut plane, w, h, smoothness * 0.4);
        let out: Vec<f32> = plane
            .iter()
            .map(|l| if *l > 1.0 - balance { 1.0 } else { 0.0 })
            .collect();
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    TornEdges,
    "filter.torn_edges",
    "Torn Edges",
    "Sketch",
    [
        param("balance", "Image Balance", 0.0, 50.0, 25.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 9.0, ""),
        param("contrast", "Contrast", 1.0, 25.0, 12.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Stamp, but the threshold wanders: the boundary is pushed about
        // by a noise field, so the shapes come out ragged the way torn
        // paper is ragged rather than cut.
        let balance = v.get("balance") / 50.0;
        let smoothness = v.get("smoothness");
        let contrast = v.get("contrast") / 25.0;
        let mut plane = luma_map(px, w, h);
        blur_plane(&mut plane, w, h, smoothness * 0.3);
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                // Low frequency on purpose: a tear wanders across the
                // sheet, it does not speckle. High-frequency noise here
                // reads as dirt on the scanner.
                let tear = (fbm(x as f32 / 24.0, y as f32 / 24.0, 1723, 3) - 0.5) * 0.5;
                let edge = 1.0 - balance + tear;
                // Contrast decides how much fibre is left in the tear:
                // low keeps a soft fringe, high snaps to the two tones.
                let t = ((plane[i] - edge) * (2.0 + contrast * 30.0) + 0.5).clamp(0.0, 1.0);
                out[i] = t;
            }
        }
        from_luma(px, &out, 0.0);
    }
);

simple_filter!(
    WaterPaper,
    "filter.water_paper",
    "Water Paper",
    "Sketch",
    [
        param("fiber", "Fiber Length", 3.0, 50.0, 15.0, ""),
        param("brightness", "Brightness", 0.0, 100.0, 60.0, ""),
        param("contrast", "Contrast", 0.0, 100.0, 80.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Painted onto fibrous, wet paper: the colour runs along the
        // fibres, which lie in whatever direction the paper was made in.
        // The one effect in this group that keeps its colour, because
        // that is what it is for.
        let fiber = v.get("fiber");
        let brightness = v.get("brightness") / 100.0;
        let contrast = v.get("contrast") / 100.0;
        let plane = luma_map(px, w, h);
        // Fibres wander: the direction is a slowly varying noise field
        // rather than a constant, which is what makes the runs look wet.
        let run = streak(&plane, w, h, fiber * 0.3, |x, y| {
            let a = fbm(x as f32 / 40.0, y as f32 / 40.0, 907, 2) * std::f32::consts::TAU;
            (a.cos(), a.sin())
        });
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let tone = ((run[i] - 0.5) * (0.5 + contrast * 1.5) + 0.5) * (0.6 + brightness * 0.8);
            let scale = tone.clamp(0.0, 1.0) / plane[i].max(1e-4);
            for v in p.iter_mut().take(3) {
                *v = (*v * scale).clamp(0.0, 1.0);
            }
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(BasRelief));
    registry.register_filter(Box::new(ChalkAndCharcoal));
    registry.register_filter(Box::new(Charcoal));
    registry.register_filter(Box::new(Chrome));
    registry.register_filter(Box::new(ConteCrayon));
    registry.register_filter(Box::new(GraphicPen));
    registry.register_filter(Box::new(HalftonePattern));
    registry.register_filter(Box::new(NotePaper));
    registry.register_filter(Box::new(Photocopy));
    registry.register_filter(Box::new(Plaster));
    registry.register_filter(Box::new(Reticulation));
    registry.register_filter(Box::new(Stamp));
    registry.register_filter(Box::new(TornEdges));
    registry.register_filter(Box::new(WaterPaper));
}
