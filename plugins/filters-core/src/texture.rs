//! Filter Gallery ▸ Texture.
//!
//! Six effects that put the image on, or into, a surface. Five of them
//! are the same idea with a different surface -- cracked plaster, film
//! grain, tiles, patches, canvas -- and the sixth, Stained Glass, builds
//! its surface out of the picture instead of over it.

use crate::util::{at, luma, put, surface, value_noise};
use crate::{choice, param, simple_filter};
use schist_plugin_api::{FilterParam, FilterPlugin, FilterValues};

/// Where a jittered grid puts the cell nearest a point, and how far away
/// its second-nearest neighbour is.
///
/// The gap between the two distances is what draws the border: it goes to
/// zero exactly along the line where two cells meet, so thresholding it
/// gives the lead in stained glass and the grout between tiles.
fn cell_of(x: f32, y: f32, size: f32, seed: u32) -> ([f32; 2], f32, f32) {
    let (gx, gy) = ((x / size).floor(), (y / size).floor());
    let mut best = (f32::INFINITY, [0.0f32; 2]);
    let mut second = f32::INFINITY;
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (cx, cy) = (gx + dx as f32, gy + dy as f32);
            // The centre of this cell, jittered inside its square.
            let jx = value_noise(cx * 13.0, cy * 7.0, seed);
            let jy = value_noise(cx * 7.0, cy * 13.0, seed ^ 0x9e37_79b9);
            let px = (cx + jx) * size;
            let py = (cy + jy) * size;
            let d = (px - x).hypot(py - y);
            if d < best.0 {
                second = best.0;
                best = (d, [px, py]);
            } else if d < second {
                second = d;
            }
        }
    }
    (best.1, best.0, second)
}

simple_filter!(
    Craquelure,
    "filter.craquelure",
    "Craquelure",
    "Texture",
    [
        param("spacing", "Crack Spacing", 2.0, 100.0, 15.0, ""),
        param("depth", "Crack Depth", 0.0, 10.0, 6.0, ""),
        param("brightness", "Crack Brightness", 0.0, 10.0, 9.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // An old painting whose surface has cracked. The cracks are the
        // seams of a cell network -- the same construction as stained
        // glass, drawn thin -- and they are shaded on both sides so the
        // plaster looks lifted rather than drawn on.
        let spacing = v.get("spacing").max(2.0);
        let depth = v.get("depth") / 10.0;
        let brightness = v.get("brightness") / 10.0;
        for y in 0..h {
            for x in 0..w {
                let (_, d1, d2) = cell_of(x as f32, y as f32, spacing, 3323);
                // How close this pixel is to the seam.
                let seam = ((d2 - d1) / (spacing * 0.35)).min(1.0);
                let crack = (1.0 - seam).powf(3.0);
                // One side of the crack catches the light, the other is
                // in shadow: sample the seam a pixel over to find out
                // which side this is.
                let (_, e1, e2) = cell_of(x as f32 + 1.0, y as f32 + 1.0, spacing, 3323);
                let lean = ((e2 - e1) - (d2 - d1)).signum();
                let p = at(px, w, h, x as i32, y as i32);
                let shade = 1.0 - crack * depth + crack * lean * brightness * 0.35;
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (p[0] * shade).clamp(0.0, 1.0),
                        (p[1] * shade).clamp(0.0, 1.0),
                        (p[2] * shade).clamp(0.0, 1.0),
                        p[3],
                    ],
                );
            }
        }
    }
);

/// Photoshop's grain types, all ten of them.
const GRAIN_TYPES: &[&str] = &[
    "Regular",
    "Soft",
    "Sprinkles",
    "Clumped",
    "Contrasty",
    "Enlarged",
    "Stippled",
    "Horizontal",
    "Vertical",
    "Speckle",
];

simple_filter!(
    Grain,
    "filter.grain",
    "Grain",
    "Texture",
    [
        param("intensity", "Intensity", 0.0, 100.0, 40.0, ""),
        param("contrast", "Contrast", 0.0, 100.0, 50.0, ""),
        choice("kind", "Grain Type", GRAIN_TYPES, 0)
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Ten grains, which are ten different noise fields rather than
        // ten strengths of one: soft is blurred, clumped is low
        // frequency, sprinkles and speckle are sparse and only land
        // sometimes, horizontal and vertical are stretched.
        let intensity = v.get("intensity") / 100.0;
        let contrast = v.get("contrast") / 100.0;
        let kind = (v.get("kind").round().max(0.0) as usize).min(9);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32, y as f32);
                let n = match kind {
                    // Soft: a coarser field, so the grain has size.
                    1 => value_noise(fx / 2.0, fy / 2.0, 101) - 0.5,
                    // Sprinkles: sparse white specks only.
                    2 => (value_noise(fx, fy, 211) - 0.85).max(0.0) * 4.0,
                    // Clumped: low frequency, in patches.
                    3 => crate::util::fbm(fx / 6.0, fy / 6.0, 307, 3) - 0.5,
                    // Contrasty: pushed to its extremes.
                    4 => ((value_noise(fx, fy, 401) - 0.5) * 3.0).clamp(-0.5, 0.5),
                    // Enlarged: big soft blobs.
                    5 => value_noise(fx / 4.0, fy / 4.0, 503) - 0.5,
                    // Stippled: sparse dark specks, the opposite of
                    // sprinkles.
                    6 => -(value_noise(fx, fy, 601) - 0.85).max(0.0) * 4.0,
                    7 => value_noise(fx / 6.0, fy, 701) - 0.5,
                    8 => value_noise(fx, fy / 6.0, 809) - 0.5,
                    // Speckle: fine and sparse in both directions.
                    9 => (value_noise(fx * 1.7, fy * 1.7, 907) - 0.5).powi(3) * 8.0,
                    _ => value_noise(fx, fy, 13) - 0.5,
                };
                let p = at(px, w, h, x as i32, y as i32);
                let mut out = p;
                for c in 0..3 {
                    // Contrast in Photoshop's Grain works on the image,
                    // not the noise: it pushes the picture apart and lets
                    // the grain sit in what is left.
                    let base = ((p[c] - 0.5) * (1.0 + contrast) + 0.5).clamp(0.0, 1.0);
                    out[c] = (base + n * intensity).clamp(0.0, 1.0);
                }
                put(px, w, x, y, out);
            }
        }
    }
);

simple_filter!(
    MosaicTiles,
    "filter.mosaic_tiles",
    "Mosaic Tiles",
    "Texture",
    [
        param("size", "Tile Size", 2.0, 100.0, 22.0, ""),
        param("grout", "Grout Width", 1.0, 15.0, 3.0, ""),
        param("lighten", "Lighten Grout", 0.0, 10.0, 9.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Irregular tiles with grout between them. Not to be confused
        // with Pixelate ▸ Mosaic, which squares the image off: here the
        // picture keeps its detail and gets *grouted*.
        let size = v.get("size").max(2.0);
        let grout = v.get("grout");
        let lighten = v.get("lighten") / 10.0;
        for y in 0..h {
            for x in 0..w {
                let (_, d1, d2) = cell_of(x as f32, y as f32, size, 5051);
                let seam = (d2 - d1) / (grout * 0.6).max(0.5);
                let p = at(px, w, h, x as i32, y as i32);
                let mut out = p;
                if seam < 1.0 {
                    // In the grout: towards white or towards black
                    // depending on Lighten Grout, and hardest at the
                    // centre line.
                    let amount = (1.0 - seam) * 0.9;
                    let target = if lighten >= 0.5 { 1.0 } else { 0.0 };
                    let pull = amount * (lighten - 0.5).abs() * 2.0;
                    for c in 0..3 {
                        out[c] = (p[c] + (target - p[c]) * pull).clamp(0.0, 1.0);
                    }
                }
                put(px, w, x, y, out);
            }
        }
    }
);

simple_filter!(
    Patchwork,
    "filter.patchwork",
    "Patchwork",
    "Texture",
    [
        param("size", "Square Size", 0.0, 10.0, 4.0, ""),
        param("relief", "Relief", 0.0, 25.0, 8.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Needlepoint: the picture is squared off into stitches, and each
        // stitch is raised or sunk by how dark it is, so the surface
        // ripples the way canvaswork does.
        let size = (2.0 + v.get("size") * 3.0).round().max(2.0) as usize;
        let relief = v.get("relief") / 25.0;
        let src = px.to_vec();
        for by in (0..h).step_by(size) {
            for bx in (0..w).step_by(size) {
                // The average colour of the square is the thread colour.
                let (mut acc, mut n) = ([0.0f32; 4], 0.0f32);
                for y in by..(by + size).min(h) {
                    for x in bx..(bx + size).min(w) {
                        let p = at(&src, w, h, x as i32, y as i32);
                        for c in 0..4 {
                            acc[c] += p[c];
                        }
                        n += 1.0;
                    }
                }
                for a in acc.iter_mut() {
                    *a /= n.max(1.0);
                }
                let height = luma(&acc);
                for y in by..(by + size).min(h) {
                    for x in bx..(bx + size).min(w) {
                        // Shade within the square so it reads as a raised
                        // stitch: light at the top left, dark at the
                        // bottom right, scaled by the square's height.
                        let u = (x - bx) as f32 / size as f32 - 0.5;
                        let vv = (y - by) as f32 / size as f32 - 0.5;
                        let shade = 1.0 + (-(u + vv)) * relief * (0.4 + height);
                        let mut out = acc;
                        for c in 0..3 {
                            out[c] = (acc[c] * shade).clamp(0.0, 1.0);
                        }
                        put(px, w, x, y, out);
                    }
                }
            }
        }
    }
);

simple_filter!(
    StainedGlass,
    "filter.stained_glass",
    "Stained Glass",
    "Texture",
    [
        param("size", "Cell Size", 2.0, 50.0, 12.0, ""),
        param("border", "Border Thickness", 1.0, 20.0, 4.0, ""),
        param("light", "Light Intensity", 0.0, 10.0, 3.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // Every cell takes one colour, the seams between them go to lead,
        // and a light behind the window falls off towards the edges of
        // the image -- which is the part that sells it, because a real
        // window is lit from behind and unevenly.
        let size = v.get("size").max(2.0);
        let border = v.get("border");
        let light = v.get("light") / 10.0;
        let src = px.to_vec();
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let reach = cx.hypot(cy).max(1.0);
        for y in 0..h {
            for x in 0..w {
                let (centre, d1, d2) = cell_of(x as f32, y as f32, size, 8191);
                let p = at(&src, w, h, centre[0] as i32, centre[1] as i32);
                let seam = (d2 - d1) / (border * 0.35).max(0.3);
                let mut out = p;
                if seam < 1.0 {
                    // Lead: dark, and not perfectly even.
                    let lead = 1.0 - (1.0 - seam) * 0.95;
                    for c in 0..3 {
                        out[c] = p[c] * lead;
                    }
                } else if light > 0.0 {
                    let d = (x as f32 - cx).hypot(y as f32 - cy) / reach;
                    let lamp = 1.0 + (1.0 - d) * light;
                    for c in 0..3 {
                        out[c] = (p[c] * lamp).clamp(0.0, 1.0);
                    }
                }
                out[3] = at(&src, w, h, x as i32, y as i32)[3];
                put(px, w, x, y, out);
            }
        }
    }
);

simple_filter!(
    Texturizer,
    "filter.texturizer",
    "Texturizer",
    "Texture",
    [
        choice("texture", "Texture", crate::artistic::SURFACES, 0),
        param("scaling", "Scaling", 50.0, 200.0, 100.0, "%"),
        param("relief", "Relief", 0.0, 50.0, 4.0, ""),
        choice("light", "Light", crate::sketch::LIGHTS, 7),
        param("invert", "Invert", 0.0, 1.0, 0.0, "")
    ],
    |px: &mut [f32], w: usize, h: usize, v: &FilterValues| {
        // The plain one: the image, printed on a surface. The surface is
        // generated rather than loaded from a file, so there is nothing
        // to ship and nothing to go missing -- see `util::surface`.
        let kind = v.get("texture").round().max(0.0) as u32;
        let scaling = (v.get("scaling") / 100.0 * 8.0).max(1.0);
        let relief = v.get("relief") / 50.0;
        let (lx, ly) = crate::sketch::light_of(v.get("light"));
        let invert = v.get("invert") >= 0.5;
        for y in 0..h {
            for x in 0..w {
                let sample_at = |dx: f32, dy: f32| {
                    let t = surface(kind, x as f32 + dx, y as f32 + dy, scaling, 61);
                    if invert {
                        1.0 - t
                    } else {
                        t
                    }
                };
                // Light the surface by its own slope, which is what makes
                // canvas look woven instead of merely mottled.
                let gx = sample_at(1.0, 0.0) - sample_at(-1.0, 0.0);
                let gy = sample_at(0.0, 1.0) - sample_at(0.0, -1.0);
                let shade = 1.0 + (gx * lx + gy * ly) * relief * 6.0;
                let p = at(px, w, h, x as i32, y as i32);
                put(
                    px,
                    w,
                    x,
                    y,
                    [
                        (p[0] * shade).clamp(0.0, 1.0),
                        (p[1] * shade).clamp(0.0, 1.0),
                        (p[2] * shade).clamp(0.0, 1.0),
                        p[3],
                    ],
                );
            }
        }
    }
);

pub fn register(registry: &mut schist_plugin_api::PluginRegistry) {
    registry.register_filter(Box::new(Craquelure));
    registry.register_filter(Box::new(Grain));
    registry.register_filter(Box::new(MosaicTiles));
    registry.register_filter(Box::new(Patchwork));
    registry.register_filter(Box::new(StainedGlass));
    registry.register_filter(Box::new(Texturizer));
}
