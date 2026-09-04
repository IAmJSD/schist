//! Filter ▸ Blur Gallery, and the two Blur entries that were still
//! missing.
//!
//! The Blur Gallery is Photoshop's answer to "blur *some* of it": five
//! filters that all apply an ordinary blur through a mask they compute
//! themselves. Field Blur ramps it across the frame, Iris Blur keeps an
//! ellipse sharp, Tilt-Shift keeps a band sharp, and Spin and Path blur
//! along a direction rather than in a circle.
//!
//! In Photoshop you place their pins on the canvas. A filter here is
//! handed pixels and numbers, so the pin is a pair of position sliders --
//! which is also how these filters worked in every program that had them
//! before the canvas UI arrived.
//!
//! The graded blur underneath them is three fixed levels, blended. A
//! true per-pixel radius costs the largest radius everywhere and these
//! are lens effects, not measurements: what matters is that the falloff
//! is smooth and that the sharp part is genuinely untouched.

use crate::util::{at, gaussian_rgba, luma, premultiply, put, sample, unpremultiply};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Blur by a per-pixel amount in 0..=1, where 1 is `radius`.
///
/// Three levels rather than one: blending a sharp copy against a single
/// heavily blurred one leaves the middle of a falloff looking like a
/// double exposure rather than like something slightly out of focus.
fn graded_blur(px: &mut [f32], w: usize, h: usize, radius: f32, amount: impl Fn(usize) -> f32) {
    if radius <= 0.0 || w == 0 || h == 0 {
        return;
    }
    let sharp = px.to_vec();
    let levels: Vec<Vec<f32>> = [0.34f32, 0.67, 1.0]
        .iter()
        .map(|k| {
            let mut buf = sharp.clone();
            gaussian_rgba(&mut buf, w, h, radius * k);
            buf
        })
        .collect();
    for i in 0..w * h {
        let t = amount(i).clamp(0.0, 1.0) * levels.len() as f32;
        // Which two levels this pixel sits between; the sharp original is
        // level -1.
        let lower = t.floor() as isize - 1;
        let frac = t - t.floor();
        let pick = |level: isize, c: usize| -> f32 {
            if level < 0 {
                sharp[i * 4 + c]
            } else {
                levels[(level as usize).min(levels.len() - 1)][i * 4 + c]
            }
        };
        for c in 0..4 {
            px[i * 4 + c] = pick(lower, c) * (1.0 - frac) + pick(lower + 1, c) * frac;
        }
    }
}

/// A smooth 0..=1 ramp, for the feathered edges every one of these has.
fn feathered(distance: f32, edge: f32, feather: f32) -> f32 {
    if feather <= 1e-4 {
        return if distance > edge { 1.0 } else { 0.0 };
    }
    ((distance - edge) / feather).clamp(0.0, 1.0)
}

simple_filter!(
    FieldBlur,
    "filter.field_blur",
    "Field Blur",
    "Blur Gallery",
    [
        param("blur", "Blur", 0.0, 200.0, 15.0, " px"),
        param("angle", "Direction", 0.0, 360.0, 90.0, "\u{b0}"),
        param("position", "Sharp At", 0.0, 100.0, 50.0, "%"),
        param("spread", "Transition", 1.0, 100.0, 50.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // One pin's worth of Field Blur: sharp along a line across the
        // frame and increasingly blurred away from it. With several pins
        // Photoshop interpolates between them; with one it does this.
        let blur = v.get("blur");
        let angle = v.get("angle").to_radians();
        let position = v.get("position") / 100.0;
        let spread = (v.get("spread") / 100.0).max(0.01);
        let (dx, dy) = (angle.cos(), angle.sin());
        // How far the frame runs along the blur's own axis.
        let extent = (w as f32 * dx).abs() + (h as f32 * dy).abs();
        let centre = extent * position;
        graded_blur(px, w, h, blur, |i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let along = x * dx + y * dy;
            (along - centre).abs() / (extent * spread).max(1.0)
        });
    }
);

simple_filter!(
    IrisBlur,
    "filter.iris_blur",
    "Iris Blur",
    "Blur Gallery",
    [
        param("blur", "Blur", 0.0, 200.0, 20.0, " px"),
        param("x", "Centre X", 0.0, 100.0, 50.0, "%"),
        param("y", "Centre Y", 0.0, 100.0, 50.0, "%"),
        param("radius", "Radius", 1.0, 100.0, 35.0, "%"),
        param("roundness", "Roundness", 0.0, 100.0, 100.0, "%"),
        param("feather", "Feather", 0.0, 100.0, 50.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // An ellipse of sharpness with everything outside it going soft,
        // which is the portrait effect: the subject inside the iris, the
        // room outside it.
        let blur = v.get("blur");
        let (cx, cy) = (v.get("x") / 100.0 * w as f32, v.get("y") / 100.0 * h as f32);
        let reach = (w.min(h) as f32) * (v.get("radius") / 100.0);
        // Roundness stretches the ellipse towards the frame's own shape.
        let round = v.get("roundness") / 100.0;
        let (rx, ry) = (
            reach * (1.0 + (1.0 - round) * (w as f32 / h.max(1) as f32 - 1.0).max(0.0)),
            reach * (1.0 + (1.0 - round) * (h as f32 / w.max(1) as f32 - 1.0).max(0.0)),
        );
        let feather = v.get("feather") / 100.0;
        graded_blur(px, w, h, blur, |i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let d = ((x - cx) / rx.max(1.0)).hypot((y - cy) / ry.max(1.0));
            feathered(d, 1.0 - feather, feather.max(1e-3))
        });
    }
);

simple_filter!(
    TiltShift,
    "filter.tilt_shift",
    "Tilt-Shift",
    "Blur Gallery",
    [
        param("blur", "Blur", 0.0, 200.0, 24.0, " px"),
        param("position", "Centre", 0.0, 100.0, 50.0, "%"),
        param("band", "Sharp Band", 1.0, 100.0, 20.0, "%"),
        param("feather", "Transition", 0.0, 100.0, 30.0, "%"),
        param("angle", "Angle", 0.0, 360.0, 0.0, "\u{b0}")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A band of the picture in focus and everything beyond it soft,
        // which reads as a very shallow depth of field and therefore as a
        // photograph of a model railway. That is the entire appeal.
        let blur = v.get("blur");
        let angle = v.get("angle").to_radians();
        let (dx, dy) = (-angle.sin(), angle.cos());
        let extent = (w as f32 * dx).abs() + (h as f32 * dy).abs();
        let centre = extent * (v.get("position") / 100.0);
        let band = extent * (v.get("band") / 100.0) / 2.0;
        let feather = extent * (v.get("feather") / 100.0);
        graded_blur(px, w, h, blur, |i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let along = (x * dx + y * dy - centre).abs();
            feathered(along, band, feather)
        });
    }
);

simple_filter!(
    SpinBlur,
    "filter.spin_blur",
    "Spin Blur",
    "Blur Gallery",
    [
        param("angle", "Blur Angle", 0.0, 60.0, 12.0, "\u{b0}"),
        param("x", "Centre X", 0.0, 100.0, 50.0, "%"),
        param("y", "Centre Y", 0.0, 100.0, 50.0, "%"),
        param("radius", "Radius", 1.0, 100.0, 50.0, "%"),
        param("feather", "Feather", 0.0, 100.0, 30.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // A wheel turning: the smear follows the arc through each pixel,
        // and it happens inside a feathered circle rather than over the
        // whole frame, which is what separates this from Radial Blur's
        // spin. The angle is in degrees of rotation, as Photoshop's is.
        let sweep = v.get("angle").to_radians();
        let (cx, cy) = (v.get("x") / 100.0 * w as f32, v.get("y") / 100.0 * h as f32);
        let reach = (w.max(h) as f32) * (v.get("radius") / 100.0);
        let feather = (v.get("feather") / 100.0).max(1e-3);
        if sweep <= 0.0 || reach <= 0.0 {
            return;
        }
        premultiply(px);
        let src = px.to_vec();
        const STEPS: usize = 24;
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let r = fx.hypot(fy);
                // Inside the circle it spins; the feather takes it back
                // to still by the rim.
                let inside = 1.0 - feathered(r / reach, 1.0 - feather, feather);
                if inside <= 0.001 {
                    continue;
                }
                let theta = fy.atan2(fx);
                let mut acc = [0.0f32; 4];
                for s in 0..STEPS {
                    let t = s as f32 / (STEPS - 1) as f32 - 0.5;
                    let a = theta + t * sweep * inside;
                    let p = sample(&src, w, h, cx + r * a.cos() - 0.5, cy + r * a.sin() - 0.5);
                    for c in 0..4 {
                        acc[c] += p[c] / STEPS as f32;
                    }
                }
                put(px, w, x, y, acc);
            }
        }
        unpremultiply(px);
    }
);

simple_filter!(
    PathBlur,
    "filter.path_blur",
    "Path Blur",
    "Blur Gallery",
    [
        param("speed", "Speed", 0.0, 100.0, 50.0, "%"),
        param("angle", "Direction", 0.0, 360.0, 0.0, "\u{b0}"),
        param("curve", "Curvature", -100.0, 100.0, 0.0, "%"),
        param("taper", "Taper", 0.0, 100.0, 0.0, "%")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Motion along a path rather than along a straight line: the
        // smear starts in the chosen direction and bends as it goes, so a
        // car blurs along the road it is on instead of along the frame.
        // Photoshop draws the path; here it is a direction and a
        // curvature, which covers the case the filter is used for.
        let speed = v.get("speed") / 100.0 * 60.0;
        let angle = v.get("angle").to_radians();
        let curve = v.get("curve") / 100.0;
        let taper = v.get("taper") / 100.0;
        if speed <= 0.0 {
            return;
        }
        premultiply(px);
        let src = px.to_vec();
        const STEPS: usize = 24;
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f32; 4];
                let mut total = 0.0f32;
                for s in 0..STEPS {
                    let t = s as f32 / (STEPS - 1) as f32;
                    // Taper thins the trail towards its end, the way a
                    // stroke lifts off.
                    let weight = 1.0 - taper * t;
                    // The path: a straight line that turns as it runs.
                    let a = angle + curve * t * std::f32::consts::FRAC_PI_2;
                    let d = t * speed;
                    let p = sample(&src, w, h, x as f32 + a.cos() * d, y as f32 + a.sin() * d);
                    for c in 0..4 {
                        acc[c] += p[c] * weight;
                    }
                    total += weight;
                }
                let total = total.max(1e-6);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        acc[0] / total,
                        acc[1] / total,
                        acc[2] / total,
                        acc[3] / total,
                    ],
                );
            }
        }
        unpremultiply(px);
    }
);

/// The kernel shapes Shape Blur offers.
///
/// Photoshop loads these from the shape presets, which is a file of
/// vector art; these are the same silhouettes, generated.
const SHAPES: &[&str] = &["Square", "Diamond", "Hexagon", "Cross", "Ring", "Star"];

/// Whether a point inside the kernel's unit square belongs to the shape.
fn in_shape(kind: usize, u: f32, v: f32) -> bool {
    let (au, av) = (u.abs(), v.abs());
    match kind {
        0 => au <= 1.0 && av <= 1.0,
        1 => au + av <= 1.0,
        // A hexagon is a square with two corners cut off at 60 degrees.
        2 => au <= 0.9 && av <= 1.0 && au * 0.5 + av * 0.866 <= 0.95,
        3 => au <= 0.3 || av <= 0.3,
        4 => {
            let r = u.hypot(v);
            (0.6..=1.0).contains(&r)
        }
        _ => {
            // Five-pointed: the radius the point would need at its own
            // angle, which oscillates five times around.
            let r = u.hypot(v);
            let a = v.atan2(u);
            let spike = 0.55 + 0.45 * (a * 5.0).cos().abs();
            r <= spike
        }
    }
}

simple_filter!(
    ShapeBlur,
    "filter.shape_blur",
    "Shape Blur",
    "Blur",
    [
        param("radius", "Radius", 1.0, 60.0, 10.0, " px"),
        choice("shape", "Shape", SHAPES, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // An average over a kernel shaped like something other than a
        // disc, which is what a lens with a shaped aperture does to its
        // out-of-focus highlights. The shape is what the filter is for:
        // at a large radius every specular highlight becomes one.
        let radius = v.get("radius").round().max(1.0) as i32;
        let kind = (v.get("shape").round().max(0.0) as usize).min(SHAPES.len() - 1);
        let mut offsets: Vec<(i32, i32)> = Vec::new();
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let (u, vv) = (dx as f32 / radius as f32, dy as f32 / radius as f32);
                if in_shape(kind, u, vv) {
                    offsets.push((dx, dy));
                }
            }
        }
        if offsets.is_empty() {
            return;
        }
        premultiply(px);
        let src = px.to_vec();
        let n = offsets.len() as f32;
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let mut acc = [0.0f32; 4];
                for (dx, dy) in offsets.iter() {
                    let p = at(&src, w, h, x + dx, y + dy);
                    for c in 0..4 {
                        acc[c] += p[c] / n;
                    }
                }
                put(px, w, x as usize, y as usize, acc);
            }
        }
        unpremultiply(px);
    }
);

/// What Smart Blur does with the edges it finds.
const SMART_MODES: &[&str] = &["Normal", "Edge Only", "Overlay Edge"];

simple_filter!(
    SmartBlur,
    "filter.smart_blur",
    "Smart Blur",
    "Blur",
    [
        param("radius", "Radius", 0.1, 100.0, 5.0, " px"),
        param("threshold", "Threshold", 0.1, 100.0, 25.0, ""),
        choice("mode", "Mode", SMART_MODES, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Blur everything that is *nearly* the same as its surroundings
        // and nothing that is not, which flattens skin and skies while
        // leaving every edge exactly where it was. Photoshop's two other
        // modes throw the picture away and keep the edges it found, which
        // is the same computation read the other way round.
        let radius = v.get("radius").round().max(1.0) as i32;
        let threshold = v.get("threshold") / 100.0 * 0.6;
        let mode = (v.get("mode").round().max(0.0) as usize).min(2);
        let src = px.to_vec();
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let here = at(&src, w, h, x, y);
                let mut acc = [0.0f32; 3];
                let mut n = 0.0f32;
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        if dx * dx + dy * dy > radius * radius {
                            continue;
                        }
                        let p = at(&src, w, h, x + dx, y + dy);
                        // Only neighbours within the threshold count, so
                        // an edge never averages across itself.
                        if (0..3).map(|c| (p[c] - here[c]).abs()).fold(0.0, f32::max) > threshold {
                            continue;
                        }
                        for c in 0..3 {
                            acc[c] += p[c];
                        }
                        n += 1.0;
                    }
                }
                let n = n.max(1.0);
                let smooth = [acc[0] / n, acc[1] / n, acc[2] / n];
                // How much of the neighbourhood was rejected is exactly
                // how much of an edge this pixel is on.
                let area = ((2 * radius + 1) * (2 * radius + 1)) as f32;
                let edge = (1.0 - n / area).clamp(0.0, 1.0);
                let out = match mode {
                    0 => [smooth[0], smooth[1], smooth[2], here[3]],
                    1 => {
                        let e = (edge * 2.5).min(1.0);
                        [e, e, e, here[3]]
                    }
                    _ => {
                        let e = (edge * 2.5).min(1.0);
                        [
                            (smooth[0] + e).min(1.0),
                            (smooth[1] + e).min(1.0),
                            (smooth[2] + e).min(1.0),
                            here[3],
                        ]
                    }
                };
                put(px, w, x as usize, y as usize, out);
            }
        }
    }
);

simple_filter!(
    Deinterlace,
    "filter.deinterlace",
    "De-Interlace",
    "Video",
    [
        choice("field", "Eliminate", &["Odd Fields", "Even Fields"], 0),
        choice(
            "fill",
            "Create New Fields By",
            &["Interpolation", "Duplication"],
            0
        )
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Interlaced video carries half its lines from one instant and
        // half from the next, so anything that moved comes out combed.
        // Throwing one field away and rebuilding it is the fix, and has
        // been since Photoshop 2.5.
        let drop_odd = v.get("field") < 0.5;
        let interpolate = v.get("fill") < 0.5;
        let src = px.to_vec();
        for y in 0..h as i32 {
            if (y % 2 == 1) != drop_odd {
                continue;
            }
            for x in 0..w as i32 {
                let out = if interpolate {
                    let above = at(&src, w, h, x, y - 1);
                    let below = at(&src, w, h, x, y + 1);
                    [
                        (above[0] + below[0]) / 2.0,
                        (above[1] + below[1]) / 2.0,
                        (above[2] + below[2]) / 2.0,
                        (above[3] + below[3]) / 2.0,
                    ]
                } else {
                    at(&src, w, h, x, y - 1)
                };
                put(px, w, x as usize, y as usize, out);
            }
        }
    }
);

simple_filter!(
    NtscColors,
    "filter.ntsc_colors",
    "NTSC Colors",
    "Video",
    [],
    |px: &mut [f32], w: usize, h: usize, _v: &FilterValues| {
        // Television could not carry the corners of the RGB cube: too
        // bright and the signal clipped, too saturated and the colour
        // subcarrier bled into the luminance. This is the clamp broadcast
        // engineers used to insist on, and it is why reds on old
        // television look orange.
        let _ = (w, h);
        for p in px.as_chunks_mut::<4>().0.iter_mut() {
            // The legal range, 16..235 of 255, and a ceiling on how far
            // chroma may sit from luma.
            for v in p.iter_mut().take(3) {
                *v = 0.0627 + *v * (0.9216 - 0.0627);
            }
            let l = luma(p);
            const MAX_CHROMA: f32 = 0.32;
            for v in p.iter_mut().take(3) {
                let c = *v - l;
                if c.abs() > MAX_CHROMA {
                    *v = l + c.signum() * MAX_CHROMA;
                }
            }
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(FieldBlur));
    registry.register_filter(Box::new(IrisBlur));
    registry.register_filter(Box::new(TiltShift));
    registry.register_filter(Box::new(SpinBlur));
    registry.register_filter(Box::new(PathBlur));
    registry.register_filter(Box::new(ShapeBlur));
    registry.register_filter(Box::new(SmartBlur));
    registry.register_filter(Box::new(Deinterlace));
    registry.register_filter(Box::new(NtscColors));
}
