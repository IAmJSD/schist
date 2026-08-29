//! Filter ▸ Stylize: filters built on edges and local contrast.

use crate::util::{at, convolve3, gaussian_rgba, luma, put, value_noise};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Sobel gradient magnitude of the luminance at every pixel, 0..~1.
fn edges(px: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; w * h];
    const GX: [f32; 9] = [-1.0, 0.0, 1.0, -2.0, 0.0, 2.0, -1.0, 0.0, 1.0];
    const GY: [f32; 9] = [-1.0, -2.0, -1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 1.0];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (mut gx, mut gy) = (0.0, 0.0);
            for i in 0..9 {
                let p = at(px, w, h, x + (i % 3) as i32 - 1, y + (i / 3) as i32 - 1);
                let l = luma(&p);
                gx += l * GX[i];
                gy += l * GY[i];
            }
            out[y as usize * w + x as usize] = gx.hypot(gy);
        }
    }
    out
}

simple_filter!(
    FindEdges,
    "filter.find_edges",
    "Find Edges",
    "Stylize",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        let e = edges(px, w, h);
        for y in 0..h {
            for x in 0..w {
                let a = at(px, w, h, x as i32, y as i32)[3];
                // Photoshop draws edges dark on white.
                let v = (1.0 - e[y * w + x]).clamp(0.0, 1.0);
                put(px, w, x, y, [v, v, v, a]);
            }
        }
    }
);

simple_filter!(
    GlowingEdges,
    "filter.glowing_edges",
    "Glowing Edges",
    "Stylize",
    [
        param("width", "Edge Width", 1.0, 14.0, 2.0, ""),
        param("brightness", "Edge Brightness", 0.0, 20.0, 6.0, ""),
        param("smoothness", "Smoothness", 1.0, 15.0, 5.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let width = v.get("width").max(1.0);
        let brightness = v.get("brightness");
        let smoothness = v.get("smoothness");
        // Smoothness settles the picture before the edges are found, so
        // the glow follows the subject's outline rather than every leaf.
        let mut settled = px.to_vec();
        if smoothness > 1.0 {
            gaussian_rgba(&mut settled, w, h, (smoothness - 1.0) * 0.35);
        }
        let e = edges(&settled, w, h);
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let p = at(&src, w, h, x as i32, y as i32);
                let k = (e[y * w + x] * brightness * width * 0.25).clamp(0.0, 1.0);
                // Keep the hue, throw away everything that is not an edge.
                let l = luma(&p).max(1e-4);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (p[0] / l * k).clamp(0.0, 1.0),
                        (p[1] / l * k).clamp(0.0, 1.0),
                        (p[2] / l * k).clamp(0.0, 1.0),
                        p[3],
                    ],
                );
            }
        }
    }
);

simple_filter!(
    Emboss,
    "filter.emboss",
    "Emboss",
    "Stylize",
    [
        param("angle", "Angle", -180.0, 180.0, 135.0, "\u{b0}"),
        param("height", "Height", 1.0, 10.0, 3.0, " px"),
        param("amount", "Amount", 1.0, 500.0, 100.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let angle = v.get("angle").to_radians();
        let amount = v.get("amount") / 100.0 * v.get("height");
        let (s, c) = angle.sin_cos();
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let p = at(&src, w, h, x, y);
                // Difference along the light direction, on grey.
                let step = v.get("height").max(1.0);
                let a = luma(&at(
                    &src,
                    w,
                    h,
                    x - (c * step) as i32,
                    y + (s * step) as i32,
                ));
                let b = luma(&at(
                    &src,
                    w,
                    h,
                    x + (c * step) as i32,
                    y - (s * step) as i32,
                ));
                let g = (0.5 + (b - a) * amount).clamp(0.0, 1.0);
                put(px, w, x as usize, y as usize, [g, g, g, p[3]]);
            }
        }
    }
);

simple_filter!(
    Solarize,
    "filter.solarize",
    "Solarize",
    "Stylize",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        // Invert everything above mid grey, which is what over-exposing a
        // negative to light does.
        let _ = (w, h);
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            for c in p.iter_mut().take(3) {
                if *c > 0.5 {
                    *c = 1.0 - *c;
                }
                *c = (*c * 2.0).clamp(0.0, 1.0);
            }
        }
    }
);

simple_filter!(
    TraceContour,
    "filter.trace_contour",
    "Trace Contour",
    "Stylize",
    [
        param("level", "Level", 0.0, 1.0, 0.5, ""),
        choice("edge", "Edge", &["Lower", "Upper"], 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Mark where each channel crosses the level, per channel, which is
        // what gives Trace Contour its coloured outlines.
        let level = v.get("level");
        // Which side of the crossing gets the ink: Lower outlines the
        // darker side of each contour, Upper the lighter one, so running
        // both and combining gives a contour two pixels wide with the
        // level exactly between them.
        let upper = v.get("edge") >= 0.5;
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let p = at(&src, w, h, x, y);
                let r = at(&src, w, h, x + 1, y);
                let d = at(&src, w, h, x, y + 1);
                let mut out = [1.0, 1.0, 1.0, p[3]];
                for c in 0..3 {
                    let here = p[c] < level;
                    let crosses = here != (r[c] < level) || here != (d[c] < level);
                    if crosses && (here != upper) {
                        out[c] = 0.0;
                    }
                }
                put(px, w, x as usize, y as usize, out);
            }
        }
    }
);

simple_filter!(
    Wind,
    "filter.wind",
    "Wind",
    "Stylize",
    [
        param("strength", "Strength", 1.0, 100.0, 20.0, " px"),
        choice("method", "Method", &["Wind", "Blast", "Stagger"], 0),
        choice(
            "direction",
            "Direction",
            &["From the Left", "From the Right"],
            0
        )
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Streak edge pixels sideways, with random-length tails. Blast is
        // the same streaks pulled much further; Stagger breaks them into
        // steps that jump line to line, which is what makes it look torn
        // rather than blown.
        let method = (v.get("method").round().max(0.0) as usize).min(2);
        let strength = v.get("strength").max(1.0) * if method == 1 { 2.5 } else { 1.0 };
        let right = v.get("direction") >= 0.5;
        let stagger = method == 2;
        let e = edges(px, w, h);
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let mut out = at(&src, w, h, x as i32, y as i32);
                for back in 1..strength as usize {
                    let sx = if right {
                        x as i32 + back as i32
                    } else {
                        x as i32 - back as i32
                    };
                    if sx < 0 || sx >= w as i32 {
                        break;
                    }
                    let idx = y * w + sx as usize;
                    if e[idx] < 0.25 {
                        continue;
                    }
                    // Longer streaks are rarer, so the tail thins out.
                    // Stagger picks a new length every few rows rather
                    // than every row, which is what breaks the streaks
                    // into blocks.
                    let seed_y = if stagger {
                        (y / 3 * 3) as f32
                    } else {
                        y as f32
                    };
                    let len = value_noise(sx as f32, seed_y, 613) * strength;
                    if (back as f32) > len {
                        continue;
                    }
                    let p = at(&src, w, h, sx, y as i32);
                    let k = 1.0 - back as f32 / strength;
                    for c in 0..3 {
                        out[c] = out[c] * (1.0 - k) + p[c] * k;
                    }
                }
                put(px, w, x, y, out);
            }
        }
    }
);

/// What Photoshop leaves in the gaps between the tiles.
const TILE_FILLS: &[&str] = &[
    "Transparent",
    "Black",
    "White",
    "Inverse Image",
    "Unaltered Image",
];

simple_filter!(
    Tiles,
    "filter.tiles",
    "Tiles",
    "Stylize",
    [
        param("count", "Number of Tiles", 2.0, 64.0, 10.0, ""),
        param("offset", "Maximum Offset", 1.0, 99.0, 10.0, "%"),
        choice("fill", "Fill Empty Area With", TILE_FILLS, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Break the image into tiles and shove each one off its place.
        let count = v.get("count").max(2.0) as usize;
        let offset = v.get("offset") / 100.0;
        let fill = (v.get("fill").round().max(0.0) as usize).min(TILE_FILLS.len() - 1);
        let tw = w.div_ceil(count);
        let th = h.div_ceil(count);
        let src = px.to_vec();
        // What shows through the gaps the tiles leave. Photoshop offers
        // the foreground and background colours, which a filter cannot
        // see, so those are black and white here.
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            match fill {
                0 => *p = [0.0, 0.0, 0.0, 0.0],
                1 => *p = [0.0, 0.0, 0.0, 1.0],
                2 => *p = [1.0, 1.0, 1.0, 1.0],
                3 => {
                    let o = &src[i * 4..i * 4 + 4];
                    *p = [1.0 - o[0], 1.0 - o[1], 1.0 - o[2], o[3]];
                }
                _ => {}
            }
        }
        for ty in 0..h.div_ceil(th) {
            for tx in 0..w.div_ceil(tw) {
                // Round, and never round a requested offset away to
                // nothing: with small tiles the shift is under a pixel and
                // truncating would make the filter a no-op.
                let shift = |seed: u32, span: usize| -> i32 {
                    let v = (value_noise(tx as f32, ty as f32, seed) - 0.5)
                        * 2.0
                        * offset
                        * span as f32;
                    if offset > 0.0 && v.abs() < 1.0 {
                        if v < 0.0 {
                            -1
                        } else {
                            1
                        }
                    } else {
                        v.round() as i32
                    }
                };
                let dx = shift(17, tw);
                let dy = shift(71, th);
                for y in 0..th {
                    for x in 0..tw {
                        let (sx, sy) = ((tx * tw + x) as i32, (ty * th + y) as i32);
                        let (ox, oy) = (sx + dx, sy + dy);
                        if ox < 0 || oy < 0 || ox >= w as i32 || oy >= h as i32 {
                            continue;
                        }
                        if sx >= w as i32 || sy >= h as i32 {
                            continue;
                        }
                        put(px, w, ox as usize, oy as usize, at(&src, w, h, sx, sy));
                    }
                }
            }
        }
    }
);

/// Photoshop's four ways of frosting an image.
const DIFFUSE_MODES: &[&str] = &["Normal", "Darken Only", "Lighten Only", "Anisotropic"];

simple_filter!(
    Diffuse,
    "filter.diffuse",
    "Diffuse",
    "Stylize",
    [
        param("amount", "Amount", 1.0, 32.0, 4.0, " px"),
        choice("mode", "Mode", DIFFUSE_MODES, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Swap each pixel with a random neighbour, which frosts the image.
        // Darken and Lighten Only take the swap conditionally, so the
        // frost only ever moves one way; Anisotropic takes the neighbour
        // that agrees with the pixel most, which smears *along* edges
        // rather than across them and comes out looking brushed.
        let r = v.get("amount").max(1.0);
        let mode = (v.get("mode").round().max(0.0) as usize).min(DIFFUSE_MODES.len() - 1);
        let src = px.to_vec();
        for y in 0..h {
            for x in 0..w {
                let here = at(&src, w, h, x as i32, y as i32);
                let pick = if mode == 3 {
                    // Anisotropic: of eight neighbours at this distance,
                    // the closest in colour.
                    let mut best = here;
                    let mut best_d = f32::INFINITY;
                    for k in 0..8 {
                        let a = k as f32 * std::f32::consts::TAU / 8.0;
                        let p = at(
                            &src,
                            w,
                            h,
                            x as i32 + (a.cos() * r) as i32,
                            y as i32 + (a.sin() * r) as i32,
                        );
                        let d = (0..3).map(|c| (p[c] - here[c]).abs()).fold(0.0, f32::max);
                        if d < best_d {
                            best_d = d;
                            best = p;
                        }
                    }
                    best
                } else {
                    let dx = ((value_noise(x as f32, y as f32, 5) - 0.5) * 2.0 * r) as i32;
                    let dy = ((value_noise(x as f32, y as f32, 9) - 0.5) * 2.0 * r) as i32;
                    at(&src, w, h, x as i32 + dx, y as i32 + dy)
                };
                let out = match mode {
                    1 if luma(&pick) > luma(&here) => here,
                    2 if luma(&pick) < luma(&here) => here,
                    _ => pick,
                };
                put(px, w, x, y, out);
            }
        }
    }
);

simple_filter!(
    OilPaint,
    "filter.oil_paint",
    "Oil Paint",
    "Stylize",
    [
        param("radius", "Stylization", 1.0, 12.0, 4.0, " px"),
        param("levels", "Cleanliness", 2.0, 64.0, 20.0, ""),
        param("bristle", "Bristle Detail", 0.0, 10.0, 4.0, ""),
        param("shine", "Shine", 0.0, 10.0, 2.0, ""),
        param("angle", "Lighting Angle", 0.0, 360.0, 45.0, "\u{b0}")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Kuwahara-ish: take the colour of the most common intensity bin
        // in the neighbourhood, which flattens into brush strokes.
        let r = v.get("radius").max(1.0) as i32;
        let levels = v.get("levels").max(2.0) as usize;
        let bristle = v.get("bristle") / 10.0;
        let shine = v.get("shine") / 10.0;
        let light = v.get("angle").to_radians();
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let mut counts = vec![0u32; levels];
                let mut sums = vec![[0.0f32; 3]; levels];
                for dy in -r..=r {
                    for dx in -r..=r {
                        if dx * dx + dy * dy > r * r {
                            continue;
                        }
                        let p = at(&src, w, h, x + dx, y + dy);
                        let bin = ((luma(&p) * levels as f32) as usize).min(levels - 1);
                        counts[bin] += 1;
                        for c in 0..3 {
                            sums[bin][c] += p[c];
                        }
                    }
                }
                let best = counts
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, c)| **c)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                let n = counts[best].max(1) as f32;
                let a = at(&src, w, h, x, y)[3];
                let mut out = [sums[best][0] / n, sums[best][1] / n, sums[best][2] / n, a];
                if bristle > 0.0 {
                    // Bristles: the brush has hairs, and they run across
                    // the stroke. The stroke direction is the local
                    // gradient, so the comb follows the painting.
                    let gx = luma(&at(&src, w, h, x + 1, y)) - luma(&at(&src, w, h, x - 1, y));
                    let gy = luma(&at(&src, w, h, x, y + 1)) - luma(&at(&src, w, h, x, y - 1));
                    let across = x as f32 * -gy + y as f32 * gx;
                    let comb = (across * 2.0).sin() * bristle * 0.05;
                    // ...and paint stands proud, so the ridges catch the
                    // light from wherever it is coming.
                    let facing = gx * light.cos() + gy * light.sin();
                    let lit = 1.0 + comb + facing * shine * 2.0;
                    for c in out.iter_mut().take(3) {
                        *c = (*c * lit).clamp(0.0, 1.0);
                    }
                }
                put(px, w, x as usize, y as usize, out);
            }
        }
    }
);

simple_filter!(
    Extrude,
    "filter.extrude",
    "Extrude",
    "Stylize",
    [
        choice("type", "Type", &["Blocks", "Pyramids"], 0),
        param("size", "Size", 2.0, 64.0, 12.0, " px"),
        param("depth", "Depth", 1.0, 200.0, 30.0, " px"),
        choice("basis", "Depth From", &["Level", "Random"], 0),
        param("solid", "Solid Front Faces", 0.0, 1.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Blocks pushed towards the viewer by their own brightness --
        // or by nothing but chance, which is what Random does, and which
        // Photoshop offers because a photograph's own brightness often
        // makes a very orderly heap.
        let pyramids = v.get("type") >= 0.5;
        let size = v.get("size").max(2.0) as usize;
        let depth = v.get("depth");
        let random = v.get("basis") >= 0.5;
        let solid = v.get("solid") >= 0.5;
        let src = px.to_vec();
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            p[0] = 0.0;
            p[1] = 0.0;
            p[2] = 0.0;
        }
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        for by in 0..h.div_ceil(size) {
            for bx in 0..w.div_ceil(size) {
                let (x0, y0) = (bx * size, by * size);
                let mut colour = at(&src, w, h, (x0 + size / 2) as i32, (y0 + size / 2) as i32);
                // Brighter blocks come further forward, so they grow.
                let level = if random {
                    value_noise(bx as f32, by as f32, 4177)
                } else {
                    luma(&colour)
                };
                let push = level * depth / 100.0;
                let scale = 1.0 + push;
                for y in 0..size {
                    for x in 0..size {
                        let (fx, fy) = ((x0 + x) as f32, (y0 + y) as f32);
                        let ox = cx + (fx - cx) * scale;
                        let oy = cy + (fy - cy) * scale;
                        if ox < 0.0 || oy < 0.0 || ox >= w as f32 || oy >= h as f32 {
                            continue;
                        }
                        if !solid {
                            // The face carries the picture rather than
                            // one flat colour, which is the check box
                            // turned off.
                            colour = at(&src, w, h, (x0 + x) as i32, (y0 + y) as i32);
                        }
                        let shade = if pyramids {
                            // A pyramid is lit by its own slope: the
                            // facets meet at the middle of the tile, so
                            // each side catches the light differently.
                            let (u, vv) =
                                (x as f32 / size as f32 - 0.5, y as f32 / size as f32 - 0.5);
                            1.0 - (u + vv).abs() * 0.9
                        } else {
                            1.0
                        };
                        put(
                            px,
                            w,
                            ox as usize,
                            oy as usize,
                            [
                                (colour[0] * shade).clamp(0.0, 1.0),
                                (colour[1] * shade).clamp(0.0, 1.0),
                                (colour[2] * shade).clamp(0.0, 1.0),
                                colour[3],
                            ],
                        );
                    }
                }
            }
        }
        let _ = (convolve3, gaussian_rgba);
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(FindEdges));
    registry.register_filter(Box::new(GlowingEdges));
    registry.register_filter(Box::new(Emboss));
    registry.register_filter(Box::new(Solarize));
    registry.register_filter(Box::new(TraceContour));
    registry.register_filter(Box::new(Wind));
    registry.register_filter(Box::new(Tiles));
    registry.register_filter(Box::new(Diffuse));
    registry.register_filter(Box::new(OilPaint));
    registry.register_filter(Box::new(Extrude));
}
