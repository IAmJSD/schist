//! Filter ▸ Render: filters that generate rather than transform.

use crate::util::{at, blur_plane, fbm, luma_map, put, value_noise};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

simple_filter!(
    Clouds,
    "filter.clouds",
    "Clouds",
    "Render",
    [
        param("scale", "Scale", 4.0, 512.0, 96.0, " px"),
        param("detail", "Detail", 1.0, 8.0, 5.0, ""),
        param("seed", "Seed", 0.0, 999.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Photoshop draws clouds in the foreground and background colours;
        // filters do not see those, so this renders greyscale, which is
        // what Clouds is used as a source for anyway.
        let scale = v.get("scale").max(4.0);
        let octaves = v.get("detail").max(1.0) as u32;
        let seed = v.get("seed") as u32;
        for y in 0..h {
            for x in 0..w {
                let n = fbm(x as f32 / scale, y as f32 / scale, seed, octaves);
                let a = at(px, w, h, x as i32, y as i32)[3];
                put(px, w, x, y, [n, n, n, a.max(1.0)]);
            }
        }
    }
);

simple_filter!(
    DifferenceClouds,
    "filter.difference_clouds",
    "Difference Clouds",
    "Render",
    [
        param("scale", "Scale", 4.0, 512.0, 96.0, " px"),
        param("detail", "Detail", 1.0, 8.0, 5.0, ""),
        param("seed", "Seed", 0.0, 999.0, 1.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Same field, differenced against what is already there, which is
        // what gives the veined look when applied repeatedly.
        let scale = v.get("scale").max(4.0);
        let octaves = v.get("detail").max(1.0) as u32;
        let seed = v.get("seed") as u32;
        for y in 0..h {
            for x in 0..w {
                let n = fbm(x as f32 / scale, y as f32 / scale, seed, octaves);
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [(p[0] - n).abs(), (p[1] - n).abs(), (p[2] - n).abs(), p[3]],
                );
            }
        }
    }
);

simple_filter!(
    Fibers,
    "filter.fibers",
    "Fibers",
    "Render",
    [
        param("variance", "Variance", 1.0, 64.0, 16.0, ""),
        param("strength", "Strength", 1.0, 64.0, 4.0, ""),
        param("seed", "Randomize", 0.0, 999.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Vertical streaks: noise that varies fast across and slowly down.
        // Photoshop's Randomize is a button that reseeds; a filter has to
        // be a function of its settings, so it is a number here.
        let variance = v.get("variance").max(1.0);
        let strength = v.get("strength").max(1.0);
        let seed = 977 + v.get("seed") as u32;
        for y in 0..h {
            for x in 0..w {
                let n = fbm(x as f32 * variance / 16.0, y as f32 / strength, seed, 4);
                let a = at(px, w, h, x as i32, y as i32)[3];
                put(px, w, x, y, [n, n, n, a.max(1.0)]);
            }
        }
    }
);

/// The lenses Photoshop offers, which differ in how their ghosts fall.
const LENS_TYPES: &[&str] = &["50-300mm Zoom", "35mm Prime", "105mm Prime", "Movie Prime"];

simple_filter!(
    LensFlare,
    "filter.lens_flare",
    "Lens Flare",
    "Render",
    [
        param("x", "Centre X", 0.0, 100.0, 50.0, "%"),
        param("y", "Centre Y", 0.0, 100.0, 50.0, "%"),
        param("brightness", "Brightness", 10.0, 300.0, 100.0, "%"),
        choice("lens", "Lens Type", LENS_TYPES, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let cx = v.get("x") / 100.0 * w as f32;
        let cy = v.get("y") / 100.0 * h as f32;
        let strength = v.get("brightness") / 100.0;
        let lens = (v.get("lens").round().max(0.0) as usize).min(LENS_TYPES.len() - 1);
        let span = (w.max(h) as f32).max(1.0);
        // The main glow, plus ghosts spaced along the line through the
        // frame's centre -- which is where real lens ghosts appear.
        //
        // The lens type is the ghost pattern: a zoom has many elements
        // and throws a long chain of small ghosts, a prime has few and
        // throws a handful of large ones, and the movie lens is the
        // anamorphic one everybody knows from the blue streak.
        let (mx, my) = (w as f32 / 2.0, h as f32 / 2.0);
        let ghosts: &[(f32, f32, f32)] = match lens {
            // 50-300mm zoom.
            0 => &[
                (0.35, 0.10, 0.6),
                (0.70, 0.06, 0.9),
                (1.30, 0.05, 0.8),
                (1.70, 0.08, 0.5),
                (2.10, 0.04, 1.1),
            ],
            // 35mm prime: fewer, bigger, warmer.
            1 => &[(0.55, 0.16, 0.8), (1.45, 0.20, 0.6)],
            // 105mm prime: tight and clean.
            2 => &[(0.80, 0.07, 1.0), (1.25, 0.05, 0.7)],
            // Movie prime: one big flare and a long tail.
            _ => &[
                (0.45, 0.22, 0.5),
                (0.95, 0.10, 0.9),
                (1.55, 0.14, 0.4),
                (2.30, 0.06, 0.7),
            ],
        };
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = (fx - cx).hypot(fy - cy) / span;
                // Core glare falls off sharply, with a wide soft halo.
                let mut add =
                    (0.35 / (1.0 + d * d * 900.0) + 0.12 / (1.0 + d * d * 40.0)) * strength;
                for &(t, size, tint) in ghosts {
                    let gx = cx + (mx - cx) * 2.0 * t;
                    let gy = cy + (my - cy) * 2.0 * t;
                    let gd = (fx - gx).hypot(fy - gy) / span;
                    add += (0.06 * tint / (1.0 + gd * gd / (size * size))) * strength;
                }
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (p[0] + add).clamp(0.0, 1.0),
                        (p[1] + add * 0.95).clamp(0.0, 1.0),
                        (p[2] + add * 0.85).clamp(0.0, 1.0),
                        p[3],
                    ],
                );
            }
        }
    }
);

/// The lights this build offers, which are Photoshop's three.
const LIGHT_TYPES: &[&str] = &["Spot", "Point", "Infinite"];

// Filter ▸ Render ▸ Lighting Effects.
//
// Photoshop's version is a room full of controls with lights you drag
// around on the canvas. What is underneath is simpler than the interface
// suggests: a light is a direction and a falloff, the image doubles as a
// bump map, and the result is Phong shading -- ambient plus diffuse plus
// a specular highlight -- multiplied back over the colour.
//
// The bump map is the interesting part and the reason this filter looks
// like nothing else: shading a photograph by *its own luminance* treated
// as a height field is what makes a flat picture look embossed and lit
// rather than merely brightened.
simple_filter!(
    LightingEffects,
    "filter.lighting_effects",
    "Lighting Effects",
    "Render",
    [
        choice("type", "Light Type", LIGHT_TYPES, 0),
        param("x", "Light X", 0.0, 100.0, 30.0, "%"),
        param("y", "Light Y", 0.0, 100.0, 25.0, "%"),
        param("angle", "Direction", 0.0, 360.0, 45.0, "\u{b0}"),
        param("intensity", "Intensity", 0.0, 300.0, 120.0, "%"),
        param("spread", "Spread", 5.0, 200.0, 60.0, "%"),
        param("ambience", "Ambience", 0.0, 100.0, 35.0, "%"),
        param("gloss", "Gloss", 0.0, 100.0, 30.0, "%"),
        param("height", "Texture Height", 0.0, 200.0, 60.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let kind = (v.get("type").round().max(0.0) as usize).min(2);
        let intensity = v.get("intensity") / 100.0;
        let ambience = v.get("ambience") / 100.0;
        let gloss = v.get("gloss") / 100.0;
        let bump = v.get("height") / 100.0;
        let spread = (v.get("spread") / 100.0).max(0.05);
        let angle = v.get("angle").to_radians();
        let (lx, ly) = (v.get("x") / 100.0 * w as f32, v.get("y") / 100.0 * h as f32);
        // The height field, softened: shading straight off the pixels
        // turns every speck of noise into a boulder.
        let mut height: Vec<f32> = luma_map(px, w, h);
        blur_plane(&mut height, w, h, 1.2);
        let diagonal = (w * w + h * h) as f32;
        let reach = (diagonal.sqrt() * spread).max(1.0);

        for y in 0..h {
            for x in 0..w {
                // Surface normal from the height field. The scale is
                // arbitrary and is what Texture Height sets.
                let (gx, gy) = {
                    let at = |xx: i32, yy: i32| -> f32 {
                        let xx = xx.clamp(0, w as i32 - 1) as usize;
                        let yy = yy.clamp(0, h as i32 - 1) as usize;
                        height[yy * w + xx]
                    };
                    (
                        at(x as i32 + 1, y as i32) - at(x as i32 - 1, y as i32),
                        at(x as i32, y as i32 + 1) - at(x as i32, y as i32 - 1),
                    )
                };
                let scale = 8.0 * bump;
                let n = [-gx * scale, -gy * scale, 1.0];
                let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt().max(1e-6);
                let n = [n[0] / nl, n[1] / nl, n[2] / nl];

                // Direction to the light, and how much of it arrives.
                let (to_light, falloff) = match kind {
                    // Infinite: a sun. Parallel rays, no falloff.
                    2 => ([angle.cos(), angle.sin(), 0.8], 1.0),
                    _ => {
                        let (dx, dy) = (lx - x as f32, ly - y as f32);
                        let d = dx.hypot(dy);
                        // Spot: a cone that fades from the middle out.
                        // Point: the same falloff without the height, so
                        // it grazes rather than shines down.
                        let f = (1.0 - (d / reach)).clamp(0.0, 1.0);
                        let f = if kind == 0 { f * f } else { f };
                        (
                            [
                                dx / d.max(1e-6),
                                dy / d.max(1e-6),
                                if kind == 0 { 1.2 } else { 0.5 },
                            ],
                            f,
                        )
                    }
                };
                let ll = (to_light[0] * to_light[0]
                    + to_light[1] * to_light[1]
                    + to_light[2] * to_light[2])
                    .sqrt()
                    .max(1e-6);
                let l = [to_light[0] / ll, to_light[1] / ll, to_light[2] / ll];

                let diffuse = (n[0] * l[0] + n[1] * l[1] + n[2] * l[2]).max(0.0);
                // Specular against the eye, which is straight above.
                let half = [l[0], l[1], l[2] + 1.0];
                let hl = (half[0] * half[0] + half[1] * half[1] + half[2] * half[2])
                    .sqrt()
                    .max(1e-6);
                let spec = ((n[0] * half[0] + n[1] * half[1] + n[2] * half[2]) / hl)
                    .max(0.0)
                    .powf(4.0 + gloss * 60.0)
                    * gloss;

                let lit = ambience + intensity * falloff * (diffuse + spec);
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (p[0] * lit).clamp(0.0, 1.0),
                        (p[1] * lit).clamp(0.0, 1.0),
                        (p[2] * lit).clamp(0.0, 1.0),
                        p[3],
                    ],
                );
            }
        }
    }
);

/// The frame styles this build draws.
///
/// Photoshop's Picture Frame is a script with about forty presets built
/// out of vector art. These five are generated, which means they scale to
/// any size and there is no art file to ship.
const FRAME_STYLES: &[&str] = &["Plain", "Beveled", "Matted", "Rounded", "Ornate"];

// Filter ▸ Render ▸ Picture Frame.
//
// Draws inside the edges of the selection rather than around them,
// because a filter cannot make its buffer bigger. That is also how it
// behaves in Photoshop when you run it on a layer rather than on a new
// document.
simple_filter!(
    PictureFrame,
    "filter.picture_frame",
    "Picture Frame",
    "Render",
    [
        choice("style", "Style", FRAME_STYLES, 1),
        param("width", "Frame Width", 1.0, 40.0, 8.0, "%"),
        param("tone", "Tone", 0.0, 100.0, 25.0, ""),
        param("relief", "Relief", 0.0, 100.0, 60.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let style = (v.get("style").round().max(0.0) as usize).min(FRAME_STYLES.len() - 1);
        let width = (v.get("width") / 100.0 * w.min(h) as f32).max(1.0);
        let tone = v.get("tone") / 100.0;
        let relief = v.get("relief") / 100.0;
        for y in 0..h {
            for x in 0..w {
                // How far inside the frame this pixel is, 0 at the outer
                // edge and 1 at the inner one. Everything past 1 is the
                // picture and is left alone.
                let edge = (x as f32)
                    .min(y as f32)
                    .min((w - 1 - x) as f32)
                    .min((h - 1 - y) as f32);
                let t = edge / width;
                if t >= 1.0 {
                    continue;
                }
                // Which way this pixel's bit of moulding faces, so the
                // light can catch one side of it.
                let facing = {
                    let (l, r) = (x as f32, (w - 1 - x) as f32);
                    let u = y as f32;
                    let m = edge;
                    if m == l {
                        (-1.0, 0.0)
                    } else if m == r {
                        (1.0, 0.0)
                    } else if m == u {
                        (0.0, -1.0)
                    } else {
                        (0.0, 1.0)
                    }
                };
                // The moulding's profile across its width: each style is
                // a different height field, lit from the upper left.
                let (height, slope) = match style {
                    // Plain: flat.
                    0 => (1.0, 0.0),
                    // Beveled: a ramp up and a step down at the picture.
                    1 => (t, 1.0),
                    // Matted: a wide flat mat with a lip at the opening.
                    2 => {
                        if t > 0.75 {
                            ((t - 0.75) * 4.0, 1.0)
                        } else {
                            (0.15, 0.0)
                        }
                    }
                    // Rounded: a half-round moulding.
                    3 => {
                        let a = (t * std::f32::consts::PI).sin();
                        (a, (t * std::f32::consts::PI).cos())
                    }
                    // Ornate: rounded with beading cut into it.
                    _ => {
                        let a = (t * std::f32::consts::PI).sin();
                        let bead = (t * 18.0).sin() * 0.12;
                        (a + bead, (t * std::f32::consts::PI).cos() + bead * 3.0)
                    }
                };
                // Light from the upper left: a face pointing that way is
                // lit, one pointing away is in shadow.
                let lit = 1.0 + slope * (facing.0 + facing.1) * -0.5 * relief;
                let shade = (tone + 0.35) * lit + height * 0.12 * relief;
                let a = at(px, w, h, x as i32, y as i32)[3];
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        shade.clamp(0.0, 1.0),
                        (shade * 0.94).clamp(0.0, 1.0),
                        (shade * 0.86).clamp(0.0, 1.0),
                        a.max(1.0),
                    ],
                );
            }
        }
    }
);

/// One branch waiting to be drawn: where it starts, which way it goes,
/// how long and how thick it is, and how many splits are left in it.
struct Branch {
    x: f32,
    y: f32,
    angle: f32,
    length: f32,
    thickness: f32,
    depth: u32,
}

/// Draw a line of a given thickness into the buffer, darkening as it
/// goes: bark is not flat, and a tree drawn in one tone looks like a
/// diagram.
#[allow(clippy::too_many_arguments)]
fn limb(px: &mut [f32], w: usize, h: usize, b: &Branch, light: f32, colour: [f32; 3]) {
    let (dx, dy) = (b.angle.cos(), b.angle.sin());
    let steps = b.length.max(1.0) as i32;
    let half = b.thickness.max(1.0) / 2.0;
    for s in 0..=steps {
        let t = s as f32 / steps as f32;
        let (cx, cy) = (b.x + dx * b.length * t, b.y + dy * b.length * t);
        let r = half * (1.0 - t * 0.35);
        let ri = r.ceil() as i32;
        for oy in -ri..=ri {
            for ox in -ri..=ri {
                let d = (ox as f32).hypot(oy as f32);
                if d > r {
                    continue;
                }
                let (ix, iy) = (cx as i32 + ox, cy as i32 + oy);
                if ix < 0 || iy < 0 || ix >= w as i32 || iy >= h as i32 {
                    continue;
                }
                // Round the limb: the side away from the light is dark.
                let across = (ox as f32 * dy - oy as f32 * dx) / r.max(1e-3);
                let shade = 1.0 - (across * light).clamp(-0.8, 0.8) * 0.45;
                let a = at(px, w, h, ix, iy)[3];
                put(
                    px,
                    w,
                    ix as usize,
                    iy as usize,
                    [
                        (colour[0] * shade).clamp(0.0, 1.0),
                        (colour[1] * shade).clamp(0.0, 1.0),
                        (colour[2] * shade).clamp(0.0, 1.0),
                        a.max(1.0),
                    ],
                );
            }
        }
    }
}

// Filter ▸ Render ▸ Tree.
//
// Photoshop's is a script with a species list; this is the thing under
// every one of them, which is a recursive branching rule: a trunk splits
// into two, each of those splits again, shorter and thinner each time,
// and leaves go on whatever is left at the end.
//
// Everything about how it looks is in four numbers -- how far it leans,
// how much it splits, how fast it thins, and how much randomness is in
// each -- which is why the species presets are presets rather than
// different code.
simple_filter!(
    Tree,
    "filter.tree",
    "Tree",
    "Render",
    [
        param("height", "Branches Height", 20.0, 100.0, 70.0, "%"),
        param("thickness", "Branches Thickness", 1.0, 100.0, 30.0, ""),
        param("spread", "Branches Spread", 5.0, 90.0, 32.0, "\u{b0}"),
        param("leaves", "Leaves Amount", 0.0, 100.0, 70.0, ""),
        param("size", "Leaves Size", 1.0, 100.0, 40.0, ""),
        param("light", "Light Direction", -100.0, 100.0, -50.0, ""),
        param("seed", "Randomness", 0.0, 999.0, 7.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let trunk = v.get("height") / 100.0 * h as f32 * 0.42;
        let thickness = v.get("thickness") / 100.0 * (w.min(h) as f32) * 0.06;
        let spread = v.get("spread").to_radians();
        let leaves = v.get("leaves") / 100.0;
        let leaf_size = 1.0 + v.get("size") / 100.0 * (w.min(h) as f32) * 0.03;
        let light = v.get("light") / 100.0;
        let seed = v.get("seed") as u32;

        // A hash rather than a generator: a filter has to give the same
        // tree twice, and the preview runs it on every keystroke.
        let mut n = 0u32;
        let mut rand = |lo: f32, hi: f32| {
            n = n.wrapping_add(1);
            lo + value_noise(n as f32 * 7.3, (n % 13) as f32 * 3.1, seed) * (hi - lo)
        };

        let bark = [0.32f32, 0.24, 0.17];
        let leaf = [0.24f32, 0.45, 0.16];
        let mut queue = vec![Branch {
            x: w as f32 / 2.0,
            y: h as f32,
            angle: -std::f32::consts::FRAC_PI_2,
            length: trunk,
            thickness: thickness.max(1.5),
            depth: 7,
        }];
        let mut tips: Vec<(f32, f32, f32)> = Vec::new();
        while let Some(b) = queue.pop() {
            limb(px, w, h, &b, light, bark);
            let (ex, ey) = (
                b.x + b.angle.cos() * b.length,
                b.y + b.angle.sin() * b.length,
            );
            if b.depth == 0 || b.length < 3.0 {
                tips.push((ex, ey, b.thickness));
                continue;
            }
            for side in [-1.0f32, 1.0] {
                let wobble = rand(-0.35, 0.35);
                queue.push(Branch {
                    x: ex,
                    y: ey,
                    angle: b.angle + side * spread + wobble * spread,
                    // Each generation is shorter and thinner, which is
                    // the whole of why it reads as a tree.
                    length: b.length * rand(0.62, 0.78),
                    thickness: b.thickness * 0.7,
                    depth: b.depth - 1,
                });
            }
        }

        // Leaves cluster at the tips, thinning outwards.
        if leaves > 0.0 {
            for (tx, ty, _) in tips.iter() {
                let count = (leaves * 14.0) as i32;
                for _ in 0..count {
                    let a = rand(0.0, std::f32::consts::TAU);
                    let d = rand(0.0, leaf_size * 2.2);
                    let (lx, ly) = (tx + a.cos() * d, ty + a.sin() * d);
                    let r = leaf_size * rand(0.35, 0.8);
                    let ri = r.ceil() as i32;
                    let tint = rand(0.75, 1.25);
                    for oy in -ri..=ri {
                        for ox in -ri..=ri {
                            if (ox as f32).hypot(oy as f32) > r {
                                continue;
                            }
                            let (ix, iy) = (lx as i32 + ox, ly as i32 + oy);
                            if ix < 0 || iy < 0 || ix >= w as i32 || iy >= h as i32 {
                                continue;
                            }
                            let a = at(px, w, h, ix, iy)[3];
                            put(
                                px,
                                w,
                                ix as usize,
                                iy as usize,
                                [
                                    (leaf[0] * tint).clamp(0.0, 1.0),
                                    (leaf[1] * tint).clamp(0.0, 1.0),
                                    (leaf[2] * tint).clamp(0.0, 1.0),
                                    a.max(1.0),
                                ],
                            );
                        }
                    }
                }
            }
        }
    }
);

// Filter ▸ Render ▸ Flame.
//
// Photoshop's runs along a path you drew. A filter never sees the
// document's paths, so these rise from the bottom of the selection --
// which is where a fire is -- and the sliders decide how many, how tall
// and how wild.
//
// A flame is drawn the way one behaves: a column of hot gas that rises,
// narrows, wanders, and cools from white through yellow to red as it
// goes. Sampling that as a field and colouring by temperature gets much
// closer than drawing tongues would.
simple_filter!(
    Flame,
    "filter.flame",
    "Flame",
    "Render",
    [
        param("count", "Flames", 1.0, 24.0, 5.0, ""),
        param("height", "Length", 10.0, 100.0, 55.0, "%"),
        param("width", "Width", 5.0, 100.0, 30.0, ""),
        param("angle", "Angle", -60.0, 60.0, 0.0, "\u{b0}"),
        param("turbulence", "Turbulent", 0.0, 100.0, 45.0, ""),
        param("opacity", "Opacity", 0.0, 100.0, 100.0, ""),
        param("seed", "Randomness", 0.0, 999.0, 3.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        let count = v.get("count").round().max(1.0) as usize;
        let height = v.get("height") / 100.0 * h as f32;
        let width = v.get("width") / 100.0 * (w as f32 / count as f32) * 0.9;
        let lean = v.get("angle").to_radians().tan();
        let turbulence = v.get("turbulence") / 100.0;
        let opacity = v.get("opacity") / 100.0;
        let seed = v.get("seed") as u32;
        if height <= 0.0 || width <= 0.0 || opacity <= 0.0 {
            return;
        }
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32, y as f32);
                // How far up this pixel is inside the flame's own space.
                let rise = (h as f32 - fy) / height;
                if !(0.0..=1.0).contains(&rise) {
                    continue;
                }
                let mut heat = 0.0f32;
                for f in 0..count {
                    // Where this flame's column is at this height: it
                    // leans, and it wanders more the higher it goes.
                    let base = (f as f32 + 0.5) * w as f32 / count as f32;
                    let wander = (fbm(
                        fy / (18.0 + 30.0 * (1.0 - turbulence)),
                        f as f32 * 9.0,
                        seed,
                        3,
                    ) - 0.5)
                        * turbulence
                        * width
                        * 3.0
                        * rise;
                    let cx = base + lean * (h as f32 - fy) + wander;
                    // The column narrows as it rises and dies out at the
                    // top, which is what gives a flame its shape.
                    let taper = (1.0 - rise).powf(0.65) * (1.0 - (rise - 0.05).max(0.0) * 0.35);
                    let reach = (width * taper).max(0.5);
                    let d = ((fx - cx) / reach).abs();
                    if d < 1.0 {
                        // Licks: the flame is not solid, it is torn into
                        // tongues by the same noise field.
                        let tongue = fbm(fx / 9.0, (fy - rise * 60.0) / 7.0, seed ^ 0x51ed, 3);
                        let body = (1.0 - d * d) * (1.0 - rise * 0.85);
                        heat = heat.max((body * (0.55 + tongue * 0.9)).max(0.0));
                    }
                }
                if heat <= 0.01 {
                    continue;
                }
                // Colour by temperature: white at the heart, then yellow,
                // then orange, then a red that fades out.
                let t = heat.min(1.0);
                let fire = [
                    (t * 3.0).min(1.0),
                    (t * 2.0 - 0.35).clamp(0.0, 1.0),
                    (t * 3.2 - 2.1).clamp(0.0, 1.0),
                ];
                let cover = (t * 1.6).min(1.0) * opacity;
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        // Screened over what is there: fire adds light.
                        (p[0] + fire[0] * cover).min(1.0),
                        (p[1] + fire[1] * cover).min(1.0),
                        (p[2] + fire[2] * cover).min(1.0),
                        p[3].max(cover),
                    ],
                );
            }
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(Clouds));
    registry.register_filter(Box::new(DifferenceClouds));
    registry.register_filter(Box::new(Fibers));
    registry.register_filter(Box::new(LensFlare));
    registry.register_filter(Box::new(LightingEffects));
    registry.register_filter(Box::new(PictureFrame));
    registry.register_filter(Box::new(Tree));
    registry.register_filter(Box::new(Flame));
}
