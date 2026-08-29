//! Filter Gallery ▸ Artistic.
//!
//! Fifteen ways to make a photograph look like it was made by hand. They
//! are the oldest effects in Photoshop -- they came in with Gallery
//! Effects in 1992 and have not changed since -- and they all work the
//! same way underneath: throw away the detail that a brush could not have
//! painted, keep the tone, and put something back where the detail was.
//! What differs is *what* goes back: dabs, hatching, flat paper, grain,
//! a wet edge.
//!
//! None of these are the original implementations, which nobody outside
//! Adobe has seen. They are each written from what the effect does to an
//! image, which for this family is unusually legible: run one at full
//! strength on a gradient and a photograph and the recipe falls out.

use crate::util::{
    at, blur_plane, edges, flatten_colour, from_luma, gaussian_rgba, luma, luma_map, put, sample,
    streak, surface, value_noise,
};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Flatten an image into regions a brush could have painted.
///
/// A blur that stops at edges: the shared first step of Cutout, Dry
/// Brush, Fresco, Palette Knife and Watercolor, which differ mostly in
/// what they do afterwards. Edge-aware rather than plain, because the
/// point is to lose the texture inside a shape while keeping the shape.
fn flatten(px: &mut [f32], w: usize, h: usize, radius: f32, tolerance: f32) {
    if radius < 0.5 {
        return;
    }
    let src = px.to_vec();
    let r = radius.round().max(1.0) as i32;
    let tol = tolerance.max(0.01);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let here = at(&src, w, h, x, y);
            let mut acc = [0.0f32; 3];
            let mut total = 0.0f32;
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                    let p = at(&src, w, h, x + dx, y + dy);
                    // Only pixels close enough in colour to be the same
                    // surface get a vote.
                    let d = (0..3).map(|c| (p[c] - here[c]).abs()).fold(0.0, f32::max);
                    if d > tol {
                        continue;
                    }
                    let weight = 1.0 - d / tol;
                    for c in 0..3 {
                        acc[c] += p[c] * weight;
                    }
                    total += weight;
                }
            }
            let total = total.max(1e-6);
            put(
                px,
                w,
                x as usize,
                y as usize,
                [acc[0] / total, acc[1] / total, acc[2] / total, here[3]],
            );
        }
    }
}

simple_filter!(
    ColoredPencil,
    "filter.colored_pencil",
    "Colored Pencil",
    "Artistic",
    [
        param("width", "Pencil Width", 1.0, 24.0, 4.0, ""),
        param("pressure", "Stroke Pressure", 0.0, 15.0, 8.0, ""),
        param("paper", "Paper Brightness", 0.0, 50.0, 25.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Crosshatched pencil: the edges become strokes, the paper shows
        // through everywhere else, and the hatching runs at a fixed
        // diagonal the way a right-handed hand draws it.
        let width = v.get("width").max(1.0);
        let pressure = v.get("pressure") / 15.0;
        let paper = v.get("paper") / 50.0;
        let plane = luma_map(px, w, h);
        let edge = edges(&plane, w, h);
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                // The hatch: a diagonal comb whose density follows how
                // dark the pixel is.
                let phase = (x as f32 + y as f32) / width;
                let hatch = (phase * std::f32::consts::PI).sin().abs();
                let tone = plane[i];
                let ink = ((1.0 - tone) * 1.4 + edge[i] * 2.0) * pressure;
                let drawn = if hatch < ink { 1.0 - ink.min(1.0) } else { 1.0 };
                // Paper Brightness lifts everything the pencil missed.
                out[i] = (drawn * (0.75 + paper * 0.25)).clamp(0.0, 1.0);
            }
        }
        // A pencil keeps a little of the subject's colour, not all of it.
        from_luma(px, &out, 0.35);
    }
);

simple_filter!(
    Cutout,
    "filter.cutout",
    "Cutout",
    "Artistic",
    [
        param("levels", "Number of Levels", 2.0, 8.0, 4.0, ""),
        param("simplicity", "Edge Simplicity", 0.0, 10.0, 4.0, ""),
        param("fidelity", "Edge Fidelity", 1.0, 3.0, 2.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Coloured paper cut out and layered: a few flat tones, and the
        // boundaries between them simplified until they could have been
        // cut with scissors.
        let levels = v.get("levels").max(2.0);
        let simplicity = v.get("simplicity");
        let fidelity = v.get("fidelity").clamp(1.0, 3.0);
        flatten(px, w, h, 1.0 + simplicity, 0.08 * fidelity);
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            flatten_colour(p, levels, 0.09);
        }
    }
);

simple_filter!(
    DryBrush,
    "filter.dry_brush",
    "Dry Brush",
    "Artistic",
    [
        param("size", "Brush Size", 0.0, 10.0, 2.0, ""),
        param("detail", "Brush Detail", 0.0, 10.0, 8.0, ""),
        param("texture", "Texture", 1.0, 3.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Somewhere between oil and watercolour: flattened into painted
        // regions, banded into a few tones per region, and dragged just
        // enough to show the bristles.
        let size = v.get("size");
        let detail = v.get("detail");
        let texture = v.get("texture");
        flatten(px, w, h, 1.0 + size, 0.05 + detail * 0.02);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let bristle = value_noise(x as f32 * 0.6, y as f32 * 0.6, 91) - 0.5;
            flatten_colour(p, 12.0 - detail * 0.6, 0.05);
            for v in p.iter_mut().take(3) {
                *v = (*v + bristle * 0.06 * texture).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    FilmGrain,
    "filter.film_grain",
    "Film Grain",
    "Artistic",
    [
        param("grain", "Grain", 0.0, 20.0, 4.0, ""),
        param("highlight", "Highlight Area", 0.0, 20.0, 0.0, ""),
        param("intensity", "Intensity", 0.0, 10.0, 10.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let _ = h;
        // Even grain through the shadows and midtones, with the
        // highlights held back and, above the Highlight Area threshold,
        // pushed towards white -- which is what film does and why the
        // effect reads as film rather than as noise.
        let grain = v.get("grain") / 20.0;
        let threshold = 1.0 - v.get("highlight") / 20.0;
        let intensity = v.get("intensity") / 10.0;
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let n = value_noise(x as f32, y as f32, 4127) - 0.5;
            let l = luma(p);
            // Grain is strongest where the emulsion is working hardest.
            let weight = (1.0 - (l - 0.35).abs()).clamp(0.0, 1.0);
            for v in p.iter_mut().take(3) {
                let mut val = *v + n * grain * weight * 0.9;
                if l > threshold {
                    val += (1.0 - val) * (l - threshold) * intensity;
                }
                *v = val.clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Fresco,
    "filter.fresco",
    "Fresco",
    "Artistic",
    [
        param("size", "Brush Size", 0.0, 10.0, 2.0, ""),
        param("detail", "Brush Detail", 0.0, 10.0, 8.0, ""),
        param("texture", "Texture", 1.0, 3.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Painted onto wet plaster in short, dark, hurried dabs: coarse
        // and much higher in contrast than Dry Brush, with the edges
        // stained rather than outlined.
        let size = v.get("size");
        let detail = v.get("detail");
        let texture = v.get("texture");
        let plane = luma_map(px, w, h);
        let edge = edges(&plane, w, h);
        flatten(px, w, h, 1.5 + size, 0.06 + detail * 0.015);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let dab = value_noise(x as f32 * 0.35, y as f32 * 0.35, 613) - 0.5;
            let stain = (edge[i] * 2.0).min(1.0);
            for v in p.iter_mut().take(3) {
                // Contrast around the midpoint, then the stain, then the
                // plaster's own mottling.
                let contrasted = ((*v - 0.5) * 1.35 + 0.5) * (1.0 - stain * 0.7);
                *v = (contrasted + dab * 0.08 * texture).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    NeonGlow,
    "filter.neon_glow",
    "Neon Glow",
    "Artistic",
    [
        param("size", "Glow Size", 1.0, 24.0, 5.0, ""),
        param("brightness", "Glow Brightness", 0.0, 50.0, 15.0, ""),
        param("hue", "Glow Colour", 0.0, 360.0, 200.0, "\u{b0}")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // The image collapses to a dark ghost of itself and its edges
        // light up in one colour. Photoshop takes the colour from the
        // colour picker; there is no picker in a filter dialog, so it is
        // a hue here.
        let size = v.get("size");
        let brightness = v.get("brightness") / 50.0;
        let hue = v.get("hue").to_radians();
        let glow_rgb = [
            (hue.cos() * 0.5 + 0.5).clamp(0.0, 1.0),
            ((hue - 2.094).cos() * 0.5 + 0.5).clamp(0.0, 1.0),
            ((hue + 2.094).cos() * 0.5 + 0.5).clamp(0.0, 1.0),
        ];
        let plane = luma_map(px, w, h);
        let mut glow = edges(&plane, w, h);
        blur_plane(&mut glow, w, h, size);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let base = plane[i] * 0.25;
            let g = (glow[i] * (1.0 + brightness * 6.0)).min(1.0);
            for c in 0..3 {
                p[c] = (base + glow_rgb[c] * g).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    PaintDaubs,
    "filter.paint_daubs",
    "Paint Daubs",
    "Artistic",
    [
        param("size", "Brush Size", 1.0, 50.0, 8.0, ""),
        param("sharpness", "Sharpness", 0.0, 40.0, 7.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Dabs laid *along* what they are painting: the smear follows the
        // edge direction, so a cheek gets dabs that curve with it rather
        // than a grid of blobs.
        let size = v.get("size").max(1.0);
        let sharpness = v.get("sharpness") / 40.0;
        let plane = luma_map(px, w, h);
        let grad = crate::util::gradient(&plane, w, h);
        let src = px.to_vec();
        let steps = size.round().max(1.0) as i32;
        for y in 0..h {
            for x in 0..w {
                let (gx, gy) = grad[y * w + x];
                // Along the edge is across the gradient.
                let (dx, dy) = (-gy, gx);
                let n = dx.hypot(dy);
                let (dx, dy) = if n < 1e-4 {
                    (1.0, 0.0)
                } else {
                    (dx / n, dy / n)
                };
                let mut acc = [0.0f32; 4];
                let mut count = 0.0;
                for t in -steps..=steps {
                    let p = sample(
                        &src,
                        w,
                        h,
                        x as f32 + dx * t as f32,
                        y as f32 + dy * t as f32,
                    );
                    for c in 0..4 {
                        acc[c] += p[c];
                    }
                    count += 1.0;
                }
                let here = at(&src, w, h, x as i32, y as i32);
                let mut out = [0.0f32; 4];
                for c in 0..3 {
                    let dabbed = acc[c] / count;
                    // Sharpness puts some of the original edge back.
                    out[c] = (dabbed + (here[c] - dabbed) * sharpness).clamp(0.0, 1.0);
                }
                out[3] = here[3];
                put(px, w, x, y, out);
            }
        }
    }
);

simple_filter!(
    PaletteKnife,
    "filter.palette_knife",
    "Palette Knife",
    "Artistic",
    [
        param("size", "Stroke Size", 1.0, 50.0, 25.0, ""),
        param("detail", "Stroke Detail", 1.0, 3.0, 3.0, ""),
        param("softness", "Softness", 0.0, 10.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Thin paint spread flat with a blade: large facets, hard edges
        // between them, and no texture inside one.
        let size = v.get("size");
        let detail = v.get("detail");
        let softness = v.get("softness");
        flatten(px, w, h, 1.0 + size / 6.0, 0.1 / detail.max(1.0));
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            flatten_colour(p, 6.0, 0.08);
        }
        if softness > 0.0 {
            gaussian_rgba(px, w, h, softness * 0.25);
        }
    }
);

simple_filter!(
    PlasticWrap,
    "filter.plastic_wrap",
    "Plastic Wrap",
    "Artistic",
    [
        param("strength", "Highlight Strength", 0.0, 20.0, 15.0, ""),
        param("detail", "Detail", 1.0, 15.0, 9.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 7.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Cling film over the subject. The film's shape is the image's
        // own luminance as a height field, and what you see is the
        // specular highlight sliding off it -- which is why the effect
        // hugs the contours instead of sitting on top of them.
        let strength = v.get("strength") / 20.0;
        let detail = v.get("detail") / 15.0;
        let smoothness = v.get("smoothness");
        let mut height = luma_map(px, w, h);
        blur_plane(&mut height, w, h, smoothness * 0.4);
        let grad = crate::util::gradient(&height, w, h);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (gx, gy) = grad[i];
            // A light from the upper left, glancing off the wrap.
            let slope = (-gx - gy) * (6.0 + detail * 24.0);
            let shine = slope.clamp(0.0, 1.0).powf(2.0) * strength;
            let shadow = (-slope).clamp(0.0, 1.0) * strength * 0.4;
            for v in p.iter_mut().take(3) {
                *v = (*v * (1.0 - shadow) + shine).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    PosterEdges,
    "filter.poster_edges",
    "Poster Edges",
    "Artistic",
    [
        param("thickness", "Edge Thickness", 0.0, 10.0, 2.0, ""),
        param("intensity", "Edge Intensity", 0.0, 10.0, 1.0, ""),
        param("levels", "Posterization", 0.0, 6.0, 2.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Posterised colour with the boundaries inked in, which is the
        // look of a screen-printed poster and the reason this one is
        // still used for actual posters.
        let thickness = v.get("thickness");
        let intensity = v.get("intensity") / 10.0;
        let levels = 2.0 + v.get("levels");
        let plane = luma_map(px, w, h);
        let mut edge = edges(&plane, w, h);
        if thickness > 0.0 {
            blur_plane(&mut edge, w, h, thickness * 0.4);
        }
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let ink = (edge[i] * (1.0 + intensity * 8.0)).min(1.0);
            flatten_colour(p, levels, 0.1);
            for v in p.iter_mut().take(3) {
                *v = (*v * (1.0 - ink)).clamp(0.0, 1.0);
            }
        }
    }
);

/// The textures the Artistic and Texture groups draw onto.
pub const SURFACES: &[&str] = &["Canvas", "Sandstone", "Burlap", "Brick"];

simple_filter!(
    RoughPastels,
    "filter.rough_pastels",
    "Rough Pastels",
    "Artistic",
    [
        param("length", "Stroke Length", 0.0, 40.0, 6.0, ""),
        param("detail", "Stroke Detail", 1.0, 20.0, 4.0, ""),
        choice("texture", "Texture", SURFACES, 0),
        param("scaling", "Scaling", 50.0, 200.0, 100.0, "%"),
        param("relief", "Relief", 0.0, 50.0, 20.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Chalk dragged across a rough surface: the stroke smears the
        // colour, and the surface decides where the chalk actually lands.
        let length = v.get("length");
        let detail = v.get("detail");
        let kind = v.get("texture").round().max(0.0) as u32;
        let scaling = v.get("scaling") / 100.0 * 6.0;
        let relief = v.get("relief") / 50.0;
        let mut plane = luma_map(px, w, h);
        if length > 0.5 {
            // Strokes run at a constant angle, as a hand's do.
            plane = streak(&plane, w, h, length * 0.4, |_, _| (0.94, 0.34));
        }
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let tex = surface(kind, x, y, scaling.max(1.0), 17);
            // Where the surface is high the chalk sticks; where it is low
            // the paper shows.
            let grip = 1.0 + (tex - 0.5) * relief;
            let tone = (plane[i] * grip).clamp(0.0, 1.0);
            let l = luma(p).max(1e-4);
            for v in p.iter_mut().take(3) {
                let detailed = *v / l * tone;
                *v = (tone + (detailed - tone) * (0.4 + detail / 40.0)).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    SmudgeStick,
    "filter.smudge_stick",
    "Smudge Stick",
    "Artistic",
    [
        param("length", "Stroke Length", 0.0, 10.0, 2.0, ""),
        param("highlight", "Highlight Area", 0.0, 20.0, 0.0, ""),
        param("intensity", "Intensity", 0.0, 10.0, 10.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Softens by smearing the dark areas along a diagonal while
        // leaving the highlights alone, which is what a smudge stick does
        // to charcoal: the shadows run, the paper stays.
        let length = v.get("length");
        let threshold = 1.0 - v.get("highlight") / 20.0;
        let intensity = v.get("intensity") / 10.0;
        let plane = luma_map(px, w, h);
        let smeared = streak(&plane, w, h, 1.0 + length * 1.5, |_, _| (0.87, 0.5));
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let l = plane[i];
            // Dark pixels smudge, bright ones do not.
            let mix = ((1.0 - l) * intensity).clamp(0.0, 1.0);
            let tone = l + (smeared[i] - l) * mix;
            let tone = if l > threshold {
                (tone + (l - threshold)).min(1.0)
            } else {
                tone
            };
            let scale = tone / l.max(1e-4);
            for v in p.iter_mut().take(3) {
                *v = (*v * scale).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Sponge,
    "filter.sponge",
    "Sponge",
    "Artistic",
    [
        param("size", "Brush Size", 0.0, 10.0, 5.0, ""),
        param("definition", "Definition", 0.0, 25.0, 12.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 5.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Dabbed on with a sponge: blotches of colour with holes in them,
        // the holes coming from a noise field rather than from the image.
        let size = 1.0 + v.get("size");
        let definition = v.get("definition") / 25.0;
        let smoothness = v.get("smoothness");
        flatten(px, w, h, smoothness * 0.4, 0.12);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let blotch = crate::util::fbm(x / size / 3.0, y / size / 3.0, 733, 3);
            // Sharpen the blotch field into holes and pigment.
            let hole = ((blotch - 0.5) * (2.0 + definition * 6.0) + 0.5).clamp(0.0, 1.0);
            for v in p.iter_mut().take(3) {
                let dark = *v * (0.45 + 0.55 * hole);
                *v = (*v + (dark - *v) * (0.4 + definition * 0.6)).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Underpainting,
    "filter.underpainting",
    "Underpainting",
    "Artistic",
    [
        param("size", "Brush Size", 0.0, 40.0, 6.0, ""),
        param("coverage", "Texture Coverage", 0.0, 40.0, 16.0, ""),
        choice("texture", "Texture", SURFACES, 0),
        param("scaling", "Scaling", 50.0, 200.0, 100.0, "%"),
        param("relief", "Relief", 0.0, 50.0, 20.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // The blocked-in first layer of a painting: the subject reduced
        // to masses of colour on a textured ground, with the detail
        // painted back on top only where it survives the coverage.
        let size = v.get("size");
        let coverage = v.get("coverage") / 40.0;
        let kind = v.get("texture").round().max(0.0) as u32;
        let scaling = v.get("scaling") / 100.0 * 6.0;
        let relief = v.get("relief") / 50.0;
        let detail = px.to_vec();
        flatten(px, w, h, 2.0 + size / 3.0, 0.15);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let tex = surface(kind, x, y, scaling.max(1.0), 29);
            let ground = 1.0 + (tex - 0.5) * relief;
            for c in 0..3 {
                let base = (p[c] * ground).clamp(0.0, 1.0);
                let over = detail[i * 4 + c];
                p[c] = (base + (over - base) * (1.0 - coverage) * 0.6).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    Watercolor,
    "filter.watercolor",
    "Watercolor",
    "Artistic",
    [
        param("detail", "Brush Detail", 1.0, 14.0, 9.0, ""),
        param("shadow", "Shadow Intensity", 0.0, 10.0, 1.0, ""),
        param("texture", "Texture", 1.0, 3.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Flat washes that pool darker where they meet an edge, which is
        // what watercolour does when the pigment dries at the boundary of
        // a stroke. Photoshop's is famously heavy-handed in the shadows
        // and this keeps that.
        let detail = v.get("detail");
        let shadow = v.get("shadow") / 10.0;
        let texture = v.get("texture");
        let plane = luma_map(px, w, h);
        let edge = edges(&plane, w, h);
        flatten(px, w, h, 2.0 + (14.0 - detail) * 0.4, 0.12);
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let paper = value_noise(x * 0.9, y * 0.9, 271) - 0.5;
            // The pooled edge, and the shadows dragged down with it.
            let pool = (edge[i] * 3.0).min(1.0) * (0.35 + shadow);
            for v in p.iter_mut().take(3) {
                let washed = *v * (1.0 - pool);
                let deepened = washed * (1.0 - (1.0 - washed) * shadow * 0.5);
                *v = (deepened + paper * 0.05 * texture).clamp(0.0, 1.0);
            }
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(ColoredPencil));
    registry.register_filter(Box::new(Cutout));
    registry.register_filter(Box::new(DryBrush));
    registry.register_filter(Box::new(FilmGrain));
    registry.register_filter(Box::new(Fresco));
    registry.register_filter(Box::new(NeonGlow));
    registry.register_filter(Box::new(PaintDaubs));
    registry.register_filter(Box::new(PaletteKnife));
    registry.register_filter(Box::new(PlasticWrap));
    registry.register_filter(Box::new(PosterEdges));
    registry.register_filter(Box::new(RoughPastels));
    registry.register_filter(Box::new(SmudgeStick));
    registry.register_filter(Box::new(Sponge));
    registry.register_filter(Box::new(Underpainting));
    registry.register_filter(Box::new(Watercolor));
}
