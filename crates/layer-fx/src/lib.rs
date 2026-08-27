//! Rasterizing layer styles.
//!
//! Photoshop's effects all derive from one thing: the layer's alpha
//! channel. Shadows and glows are that alpha blurred, offset and coloured;
//! strokes are a band along its edge; bevels shade it by the gradient of a
//! blurred copy. So the whole module works on a single f32 alpha buffer
//! covering the layer's content grown by the effects' reach, and composites
//! the results in Photoshop's own stacking order.
//!
//! The output is one styled raster that the compositor blends exactly as it
//! would blend the layer's own pixels, which is what makes layer opacity
//! apply to the effects while fill opacity applies only to the content.

use schist_color::Rgba;
use schist_core::{
    blend::BlendMode, BevelStyle, BevelStyle_, GlowStyle, GradientOverlayStyle, GradientShape,
    IntRect, Layer, LayerStyle, SatinStyle, ShadowStyle, StrokePosition, StrokeStyle, StyledRaster,
    Technique, TileCoord, TileMap,
};

mod blur;
pub use blur::gaussian_alpha;

/// A straight-alpha RGBA plane in document coordinates.
struct Plane {
    rect: IntRect,
    /// `rect.width() * rect.height() * 4` floats.
    px: Vec<f32>,
}

impl Plane {
    fn blank(rect: IntRect) -> Plane {
        let n = (rect.width().max(0) as usize) * (rect.height().max(0) as usize);
        Plane {
            rect,
            px: vec![0.0; n * 4],
        }
    }

    #[inline]
    fn w(&self) -> usize {
        self.rect.width().max(0) as usize
    }

    #[inline]
    fn h(&self) -> usize {
        self.rect.height().max(0) as usize
    }

    /// Blend `src` over this plane with `mode`, scaled by `opacity` and
    /// optionally masked to `mask` (same rect).
    fn blend_over(&mut self, src: &Plane, mode: BlendMode, opacity: f32, mask: Option<&[f32]>) {
        let w = self.w();
        debug_assert_eq!(self.rect, src.rect);
        for i in 0..self.px.len() / 4 {
            let mut a = src.px[i * 4 + 3] * opacity;
            if let Some(mask) = mask {
                a *= mask[i];
            }
            if a <= 0.0 {
                continue;
            }
            let top = Rgba::new(src.px[i * 4], src.px[i * 4 + 1], src.px[i * 4 + 2], a);
            let bottom = Rgba::new(
                self.px[i * 4],
                self.px[i * 4 + 1],
                self.px[i * 4 + 2],
                self.px[i * 4 + 3],
            );
            // Dissolve is position-dependent, so it needs the pixel's
            // document coordinates rather than its index.
            let (x, y) = (
                self.rect.left + (i % w.max(1)) as i32,
                self.rect.top + (i / w.max(1)) as i32,
            );
            let out = schist_pixel_ops::blend_pixel(mode, top, bottom, x, y);
            self.px[i * 4] = out.r;
            self.px[i * 4 + 1] = out.g;
            self.px[i * 4 + 2] = out.b;
            self.px[i * 4 + 3] = out.a;
        }
    }

    /// Flat colour everywhere, alpha from `alpha`.
    fn from_alpha(rect: IntRect, alpha: &[f32], color: Rgba) -> Plane {
        let mut p = Plane::blank(rect);
        for (i, &a) in alpha.iter().enumerate() {
            p.px[i * 4] = color.r;
            p.px[i * 4 + 1] = color.g;
            p.px[i * 4 + 2] = color.b;
            p.px[i * 4 + 3] = a * color.a;
        }
        p
    }
}

/// Rasterize `layer`'s style around its pixels.
///
/// Returns `None` when the layer has no effects switched on, or is not a
/// raster layer, in which case the compositor uses its pixels directly.
pub fn render(layer: &Layer) -> Option<StyledRaster> {
    if layer.style.is_empty() {
        return None;
    }
    let raster = layer.as_raster()?;
    let content = raster.tiles.content_bounds();
    render_content(
        content,
        |x, y| raster.tiles.pixel(x, y),
        &layer.style,
        layer.fill_opacity,
    )
}

/// Rasterize a style around arbitrary content pixels.
///
/// This is the path group layers take: this crate cannot composite, so
/// the compositor flattens a styled group's children and hands the
/// result here. `pixel` is sampled over `content` grown by the style's
/// reach and must return straight-alpha values (transparent outside the
/// content).
pub fn render_content(
    content: IntRect,
    pixel: impl Fn(i32, i32) -> Rgba,
    style: &LayerStyle,
    fill_opacity: f32,
) -> Option<StyledRaster> {
    if style.is_empty() || content.is_empty() {
        return None;
    }
    let pad = style.outset();
    let rect = IntRect::new(
        content.left - pad,
        content.top - pad,
        content.right + pad,
        content.bottom + pad,
    );

    // The layer's own pixels, with fill opacity applied: Photoshop scales
    // the content but not the effects.
    let mut base = Plane::blank(rect);
    let (w, h) = (base.w(), base.h());
    let mut alpha = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let px = pixel(rect.left + x as i32, rect.top + y as i32);
            let i = y * w + x;
            alpha[i] = px.a;
            base.px[i * 4] = px.r;
            base.px[i * 4 + 1] = px.g;
            base.px[i * 4 + 2] = px.b;
            base.px[i * 4 + 3] = px.a * fill_opacity;
        }
    }

    let mut out = Plane::blank(rect);

    // Bottom of the stack: effects that sit behind the layer.
    if let Some(s) = style.drop_shadow.on() {
        let shadow = shadow_plane(&alpha, w, h, rect, s, false);
        let knock = s.knockout.then_some(alpha.as_slice());
        composite_behind(&mut out, &shadow, s.opacity, knock);
    }
    if let Some(g) = style.outer_glow.on() {
        let glow = outer_glow_plane(&alpha, w, h, rect, g);
        composite_behind(&mut out, &glow, g.opacity, Some(&alpha));
    }

    // The layer itself, then everything that shades its interior.
    out.blend_over(&base, BlendMode::Normal, 1.0, None);

    if let Some(o) = style.gradient_overlay.on() {
        let grad = gradient_plane(rect, o, content);
        out.blend_over(&grad, o.blend, o.opacity, Some(&alpha));
    }
    if let Some(o) = style.color_overlay.on() {
        let flat = Plane::from_alpha(rect, &vec![1.0; w * h], o.color);
        out.blend_over(&flat, o.blend, o.opacity, Some(&alpha));
    }
    if let Some(s) = style.satin.on() {
        let satin = satin_plane(&alpha, w, h, rect, s);
        out.blend_over(&satin, s.blend, s.opacity, Some(&alpha));
    }
    if let Some(g) = style.inner_glow.on() {
        let glow = inner_glow_plane(&alpha, w, h, rect, g);
        out.blend_over(&glow, g.blend, g.opacity, Some(&alpha));
    }
    if let Some(s) = style.inner_shadow.on() {
        let shadow = inner_shadow_plane(&alpha, w, h, rect, s);
        out.blend_over(&shadow, s.blend, s.opacity, Some(&alpha));
    }
    if let Some(b) = style.bevel.on() {
        bevel(&mut out, &alpha, w, h, rect, b);
    }
    if let Some(s) = style.stroke.on() {
        let stroke = stroke_plane(&alpha, w, h, rect, s);
        // An outside stroke draws beyond the layer, so it is not masked.
        out.blend_over(&stroke, s.blend, s.opacity, None);
    }

    Some(StyledRaster {
        tiles: plane_to_tiles(&out),
        bounds: rect,
        key: 0,
    })
}

/// Put `src` behind whatever is already in `dst`, optionally hiding it
/// where `knockout` is opaque (a drop shadow does not show through its own
/// layer, which matters once the layer is partly transparent).
fn composite_behind(dst: &mut Plane, src: &Plane, opacity: f32, knockout: Option<&[f32]>) {
    let mut layer_on_top = Plane::blank(dst.rect);
    layer_on_top.px.copy_from_slice(&src.px);
    for i in 0..layer_on_top.px.len() / 4 {
        let mut a = layer_on_top.px[i * 4 + 3] * opacity;
        if let Some(k) = knockout {
            a *= 1.0 - k[i];
        }
        layer_on_top.px[i * 4 + 3] = a;
    }
    layer_on_top.blend_over(dst, BlendMode::Normal, 1.0, None);
    dst.px = layer_on_top.px;
}

/// Offset, spread and blur the alpha into a shadow.
fn shadow_plane(
    alpha: &[f32],
    w: usize,
    h: usize,
    rect: IntRect,
    s: &ShadowStyle,
    inner: bool,
) -> Plane {
    let (dx, dy) = polar(s.angle, s.distance);
    let mut a = offset_alpha(alpha, w, h, dx, dy);
    if inner {
        for v in a.iter_mut() {
            *v = 1.0 - *v;
        }
    }
    apply_spread(&mut a, s.spread);
    blur::gaussian_alpha(&mut a, w, h, s.size);
    Plane::from_alpha(rect, &a, s.color)
}

fn inner_shadow_plane(alpha: &[f32], w: usize, h: usize, rect: IntRect, s: &ShadowStyle) -> Plane {
    shadow_plane(alpha, w, h, rect, s, true)
}

fn outer_glow_plane(alpha: &[f32], w: usize, h: usize, rect: IntRect, g: &GlowStyle) -> Plane {
    let mut a = alpha.to_vec();
    apply_spread(&mut a, g.spread);
    match g.technique {
        Technique::Softer => blur::gaussian_alpha(&mut a, w, h, g.size),
        Technique::Precise => precise_grow(&mut a, w, h, g.size),
    }
    Plane::from_alpha(rect, &a, g.color)
}

fn inner_glow_plane(alpha: &[f32], w: usize, h: usize, rect: IntRect, g: &GlowStyle) -> Plane {
    // Glow inwards from the edge: invert, blur, then keep what lands
    // inside the shape.
    let mut a: Vec<f32> = if g.from_edge {
        alpha.iter().map(|v| 1.0 - v).collect()
    } else {
        // "Center": a glow that fades outwards from the middle.
        let mut inv: Vec<f32> = alpha.iter().map(|v| 1.0 - v).collect();
        blur::gaussian_alpha(&mut inv, w, h, g.size);
        inv.iter().map(|v| 1.0 - v).collect()
    };
    apply_spread(&mut a, g.spread);
    match g.technique {
        Technique::Softer => blur::gaussian_alpha(&mut a, w, h, g.size),
        Technique::Precise => precise_grow(&mut a, w, h, g.size),
    }
    Plane::from_alpha(rect, &a, g.color)
}

/// The layer's shape differenced against two offset, blurred copies of
/// itself: Photoshop's satin.
fn satin_plane(alpha: &[f32], w: usize, h: usize, rect: IntRect, s: &SatinStyle) -> Plane {
    let (dx, dy) = polar(s.angle, s.distance);
    let mut a = offset_alpha(alpha, w, h, dx, dy);
    let mut b = offset_alpha(alpha, w, h, -dx, -dy);
    blur::gaussian_alpha(&mut a, w, h, s.size);
    blur::gaussian_alpha(&mut b, w, h, s.size);
    let mut out = vec![0.0f32; w * h];
    for i in 0..out.len() {
        let d = (a[i] - b[i]).abs();
        out[i] = if s.invert { 1.0 - d } else { d };
    }
    Plane::from_alpha(rect, &out, s.color)
}

/// A band along the layer's edge.
fn stroke_plane(alpha: &[f32], w: usize, h: usize, rect: IntRect, s: &StrokeStyle) -> Plane {
    let (outer, inner) = match s.position {
        StrokePosition::Outside => (s.size, 0.0),
        StrokePosition::Inside => (0.0, s.size),
        StrokePosition::Center => (s.size / 2.0, s.size / 2.0),
    };
    // Distance from the shape's edge, negative inside.
    let dist = signed_distance(alpha, w, h, outer.max(inner) + 2.0);
    let mut band = vec![0.0f32; w * h];
    for i in 0..band.len() {
        let d = dist[i];
        // Antialias by half a pixel either side of the band's edges.
        let cov = smooth_band(d, -inner, outer);
        band[i] = cov;
    }
    Plane::from_alpha(rect, &band, s.color)
}

/// Coverage of the interval [lo, hi], softened by half a pixel.
fn smooth_band(d: f32, lo: f32, hi: f32) -> f32 {
    if hi <= lo {
        return 0.0;
    }
    let up = ((d - lo) / 0.5 + 0.5).clamp(0.0, 1.0);
    let down = ((hi - d) / 0.5 + 0.5).clamp(0.0, 1.0);
    (up * down).clamp(0.0, 1.0)
}

fn gradient_plane(rect: IntRect, o: &GradientOverlayStyle, content: IntRect) -> Plane {
    let w = rect.width().max(0) as usize;
    let h = rect.height().max(0) as usize;
    let mut p = Plane::blank(rect);
    let (cx, cy) = (
        (content.left + content.right) as f32 / 2.0,
        (content.top + content.bottom) as f32 / 2.0,
    );
    let half = (content.width().max(content.height()) as f32 / 2.0).max(1.0) * o.scale.max(0.1);
    let rad = o.angle.to_radians();
    let (ux, uy) = (rad.cos(), -rad.sin());
    for y in 0..h {
        for x in 0..w {
            let px = rect.left as f32 + x as f32 - cx;
            let py = rect.top as f32 + y as f32 - cy;
            let mut t = match o.shape {
                GradientShape::Linear => (px * ux + py * uy) / (2.0 * half) + 0.5,
                GradientShape::Radial => px.hypot(py) / half,
            }
            .clamp(0.0, 1.0);
            if o.reverse {
                t = 1.0 - t;
            }
            let i = (y * w + x) * 4;
            p.px[i] = o.from.r + (o.to.r - o.from.r) * t;
            p.px[i + 1] = o.from.g + (o.to.g - o.from.g) * t;
            p.px[i + 2] = o.from.b + (o.to.b - o.from.b) * t;
            p.px[i + 3] = o.from.a + (o.to.a - o.from.a) * t;
        }
    }
    p
}

/// Shade the layer by the slope of a blurred copy of its alpha, lit from
/// `b.angle` / `b.altitude`.
fn bevel(out: &mut Plane, alpha: &[f32], w: usize, h: usize, rect: IntRect, b: &BevelStyle) {
    let mut height = alpha.to_vec();
    blur::gaussian_alpha(&mut height, w, h, b.size.max(0.5));
    if b.soften > 0.0 {
        blur::gaussian_alpha(&mut height, w, h, b.soften);
    }
    let rad = b.angle.to_radians();
    let alt = b.altitude.to_radians();
    // Light direction, with y down to match image coordinates.
    let (lx, ly, lz) = (
        rad.cos() * alt.cos(),
        -rad.sin() * alt.cos(),
        alt.sin().max(1e-3),
    );
    let invert = matches!(b.style, BevelStyle_::PillowEmboss);
    let sign = if invert { -1.0 } else { 1.0 };

    let mut hi = vec![0.0f32; w * h];
    let mut lo = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let l = height[i.saturating_sub(if x > 0 { 1 } else { 0 })];
            let r = height[if x + 1 < w { i + 1 } else { i }];
            let u = height[if y > 0 { i - w } else { i }];
            let d = height[if y + 1 < h { i + w } else { i }];
            // Surface normal of the height field, depth-scaled.
            let (nx, ny) = ((l - r) * b.depth * sign, (u - d) * b.depth * sign);
            let nz = 1.0;
            let len = (nx * nx + ny * ny + nz * nz).sqrt();
            let dot = (nx * lx + ny * ly + nz * lz) / len;
            // Flat surface -> dot == lz, which must read as unlit.
            let shade = (dot - lz) / (1.0 - lz).max(1e-3);
            // Outside-only styles must not shade the layer's interior.
            let gate = match b.style {
                BevelStyle_::OuterBevel => 1.0 - alpha[i],
                BevelStyle_::InnerBevel => alpha[i],
                BevelStyle_::Emboss | BevelStyle_::PillowEmboss => 1.0,
            };
            if shade > 0.0 {
                hi[i] = shade.min(1.0) * gate;
            } else {
                lo[i] = (-shade).min(1.0) * gate;
            }
        }
    }
    let mask: Vec<f32> = match b.style {
        // An outer bevel paints beyond the layer, so it is not clipped.
        BevelStyle_::OuterBevel => vec![1.0; w * h],
        _ => alpha.to_vec(),
    };
    let hp = Plane::from_alpha(rect, &hi, b.highlight);
    out.blend_over(&hp, b.highlight_blend, b.highlight_opacity, Some(&mask));
    let sp = Plane::from_alpha(rect, &lo, b.shadow);
    out.blend_over(&sp, b.shadow_blend, b.shadow_opacity, Some(&mask));
}

/// Push alpha towards fully on or fully off, which is what Photoshop's
/// Spread/Choke sliders do before the blur.
fn apply_spread(a: &mut [f32], spread: f32) {
    if spread <= 0.0 {
        return;
    }
    // At spread 1.0 the ramp collapses to a hard edge.
    let k = (1.0 - spread.clamp(0.0, 0.99)).max(0.01);
    for v in a.iter_mut() {
        *v = (*v / k).min(1.0);
    }
}

/// Grow the shape by `size` with a hard shoulder, for the Precise
/// technique. Uses the same distance field as strokes.
fn precise_grow(a: &mut [f32], w: usize, h: usize, size: f32) {
    let dist = signed_distance(a, w, h, size + 2.0);
    for i in 0..a.len() {
        a[i] = smooth_band(dist[i], -f32::INFINITY, size);
    }
}

/// Signed distance to the shape's edge in pixels, negative inside,
/// clamped at `limit`.
///
/// This used to scan a `(2r+1)^2` window per pixel looking for a sample
/// on the other side of the 0.5-alpha threshold. The early break only
/// fires within one pixel of an edge, so every interior and far-exterior
/// pixel paid the full window: on a 1000x1000 layer an outside stroke
/// measured 205 ms at size 4, 937 ms at size 12 and 5.53 s at size 30,
/// growing as r^2 -- and Photoshop's stroke and glow sizes go to 250.
///
/// An exact Euclidean distance transform gives the same answer in O(w*h),
/// independent of the radius. Two of them: one seeded on the inside
/// pixels and one on the outside, so each pixel reads the distance to the
/// nearest sample of the opposite class -- exactly what the window search
/// was looking for. Clamping at `limit` afterwards matches the old
/// bound, since any true nearest within `limit` also lay inside the
/// square window.
fn signed_distance(alpha: &[f32], w: usize, h: usize, limit: f32) -> Vec<f32> {
    let inside: Vec<bool> = alpha.iter().map(|&a| a >= 0.5).collect();
    let to_inside = squared_edt(&inside, w, h, false);
    let to_outside = squared_edt(&inside, w, h, true);
    (0..w * h)
        .map(|i| {
            if inside[i] {
                -to_outside[i].sqrt().min(limit)
            } else {
                to_inside[i].sqrt().min(limit)
            }
        })
        .collect()
}

/// Squared Euclidean distance to the nearest seed pixel.
///
/// Felzenszwalb and Huttenlocher's lower-envelope transform: one 1-D pass
/// down the columns, one across the rows. `invert` seeds on the *false*
/// entries instead of the true ones.
fn squared_edt(seed: &[bool], w: usize, h: usize, invert: bool) -> Vec<f32> {
    // Large but finite: an actual infinity turns the parabola
    // intersections below into NaN.
    const FAR: f32 = 1e20;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut grid: Vec<f32> = seed
        .iter()
        .map(|&s| if s != invert { 0.0 } else { FAR })
        .collect();

    let n = w.max(h);
    let mut f = vec![0.0f32; n];
    let mut d = vec![0.0f32; n];
    let mut v = vec![0usize; n];
    let mut z = vec![0.0f32; n + 1];

    for x in 0..w {
        for (y, slot) in f[..h].iter_mut().enumerate() {
            *slot = grid[y * w + x];
        }
        lower_envelope(&f[..h], &mut d[..h], &mut v[..h], &mut z[..h + 1]);
        for y in 0..h {
            grid[y * w + x] = d[y];
        }
    }
    for y in 0..h {
        f[..w].copy_from_slice(&grid[y * w..y * w + w]);
        lower_envelope(&f[..w], &mut d[..w], &mut v[..w], &mut z[..w + 1]);
        grid[y * w..y * w + w].copy_from_slice(&d[..w]);
    }
    grid
}

/// The 1-D squared distance transform: the lower envelope of the
/// parabolas `(q - i)^2 + f[i]`.
fn lower_envelope(f: &[f32], d: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let mut k: isize = 0;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    let sq = |i: usize| (i * i) as f32;
    for q in 1..n {
        let mut s;
        loop {
            let p = v[k as usize];
            s = ((f[q] + sq(q)) - (f[p] + sq(p))) / (2.0 * q as f32 - 2.0 * p as f32);
            // `z[0]` is -inf, so this never walks off the front.
            if s > z[k as usize] {
                break;
            }
            k -= 1;
        }
        k += 1;
        v[k as usize] = q;
        z[k as usize] = s;
        z[k as usize + 1] = f32::INFINITY;
    }
    let mut k: usize = 0;
    for (q, out) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let dq = q as f32 - v[k] as f32;
        *out = dq * dq + f[v[k]];
    }
}

/// Shift an alpha buffer by a fractional offset, sampling bilinearly.
fn offset_alpha(alpha: &[f32], w: usize, h: usize, dx: f32, dy: f32) -> Vec<f32> {
    if dx == 0.0 && dy == 0.0 {
        return alpha.to_vec();
    }
    let mut out = vec![0.0f32; w * h];
    let sample = |x: i32, y: i32| -> f32 {
        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
            0.0
        } else {
            alpha[y as usize * w + x as usize]
        }
    };
    for y in 0..h {
        for x in 0..w {
            let sx = x as f32 - dx;
            let sy = y as f32 - dy;
            let (x0, y0) = (sx.floor() as i32, sy.floor() as i32);
            let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
            let v = sample(x0, y0) * (1.0 - fx) * (1.0 - fy)
                + sample(x0 + 1, y0) * fx * (1.0 - fy)
                + sample(x0, y0 + 1) * (1.0 - fx) * fy
                + sample(x0 + 1, y0 + 1) * fx * fy;
            out[y * w + x] = v;
        }
    }
    out
}

/// The offset a shadow is thrown by, in image coordinates.
///
/// Photoshop's angle is where the light comes *from*, measured
/// counter-clockwise with 0 pointing right, so the shadow falls the
/// opposite way; image y grows downwards, which flips the sine.
fn polar(angle_deg: f32, distance: f32) -> (f32, f32) {
    let rad = angle_deg.to_radians();
    (-rad.cos() * distance, rad.sin() * distance)
}

fn plane_to_tiles(p: &Plane) -> TileMap {
    let mut tiles = TileMap::new();
    let w = p.w();
    for coord in TileCoord::covering(&p.rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&p.rect);
        if clip.is_empty() {
            continue;
        }
        let mut any = false;
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let i = ((y - p.rect.top) as usize * w + (x - p.rect.left) as usize) * 4;
                if p.px[i + 3] > 0.0 {
                    any = true;
                    break;
                }
            }
            if any {
                break;
            }
        }
        if !any {
            continue;
        }
        let buf = tiles.get_mut_or_insert(coord, schist_color::Depth::ThirtyTwo);
        for y in clip.top..clip.bottom {
            for x in clip.left..clip.right {
                let i = ((y - p.rect.top) as usize * w + (x - p.rect.left) as usize) * 4;
                let ix = ((y - trect.top) * schist_core::TILE_SIZE + (x - trect.left)) as usize;
                buf.set(
                    ix,
                    Rgba::new(p.px[i], p.px[i + 1], p.px[i + 2], p.px[i + 3]),
                );
            }
        }
    }
    tiles
}

/// Convenience for callers holding a whole style rather than a layer.
pub fn outset(style: &LayerStyle) -> i32 {
    style.outset()
}

/// Re-exported so callers can name the settings types without also
/// depending on core's module layout.
pub use schist_core::style;

#[cfg(test)]
mod distance_tests {
    use super::signed_distance;

    /// The distance transform must return exactly what the window search
    /// it replaced returned, for every pixel and every limit.
    #[test]
    fn the_distance_transform_matches_the_window_search() {
        let (w, h) = (37usize, 29usize);
        // A blob with a hole, plus a detached speck, so both signs and
        // both near and far pixels are exercised.
        let alpha: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let d = ((x - 12.0).powi(2) + (y - 12.0).powi(2)).sqrt();
                let speck = (30..33).contains(&(x as usize)) && (4..7).contains(&(y as usize));
                if (3.0..8.0).contains(&d) || speck {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();

        for limit in [1.0f32, 2.0, 4.5, 12.0, 100.0] {
            let got = signed_distance(&alpha, w, h, limit);
            let want = brute_force_signed_distance(&alpha, w, h, limit);
            for i in 0..w * h {
                assert!(
                    (got[i] - want[i]).abs() < 1e-4,
                    "limit {limit}, pixel ({}, {}): {} != {}",
                    i % w,
                    i / w,
                    got[i],
                    want[i]
                );
            }
        }
    }

    /// A plane with no edge in it sits entirely at the clamp.
    #[test]
    fn a_uniform_plane_is_entirely_at_the_limit() {
        let (w, h) = (8usize, 8usize);
        let solid = vec![1.0f32; w * h];
        let empty = vec![0.0f32; w * h];
        assert!(signed_distance(&solid, w, h, 5.0)
            .iter()
            .all(|&d| (d + 5.0).abs() < 1e-4));
        assert!(signed_distance(&empty, w, h, 5.0)
            .iter()
            .all(|&d| (d - 5.0).abs() < 1e-4));
    }

    /// `signed_distance` as it was written before the transform.
    fn brute_force_signed_distance(alpha: &[f32], w: usize, h: usize, limit: f32) -> Vec<f32> {
        let r = limit.ceil().max(1.0) as i32;
        let mut out = vec![0.0f32; w * h];
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let i = y as usize * w + x as usize;
                let inside = alpha[i] >= 0.5;
                let mut best = limit;
                'search: for dy in -r..=r {
                    let sy = y + dy;
                    if sy < 0 || sy >= h as i32 {
                        continue;
                    }
                    for dx in -r..=r {
                        let sx = x + dx;
                        if sx < 0 || sx >= w as i32 {
                            continue;
                        }
                        let other = alpha[sy as usize * w + sx as usize] >= 0.5;
                        if other == inside {
                            continue;
                        }
                        let d = ((dx * dx + dy * dy) as f32).sqrt();
                        if d < best {
                            best = d;
                            if best <= 1.0 {
                                break 'search;
                            }
                        }
                    }
                }
                out[i] = if inside { -best } else { best };
            }
        }
        out
    }
    /// The transform is O(w*h) regardless of the radius, which the
    /// window search was not: it grew as r^2, from 205 ms at size 4 to
    /// 5.53 s at size 30 on a 1000x1000 layer.
    ///
    /// Asserted on the *result* rather than on wall clock, which would be
    /// a CI flake and would mostly time the new code against itself: a
    /// large limit and a small one have to agree everywhere the small one
    /// did not clamp.
    #[test]
    fn a_larger_limit_only_changes_what_was_clamped() {
        let (w, h) = (64usize, 48usize);
        let alpha: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                if ((x - 32.0).powi(2) + (y - 24.0).powi(2)).sqrt() < 10.0 {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        let near = signed_distance(&alpha, w, h, 4.0);
        let far = signed_distance(&alpha, w, h, 250.0);
        for i in 0..w * h {
            if near[i].abs() < 4.0 {
                assert!(
                    (near[i] - far[i]).abs() < 1e-4,
                    "unclamped sample {i} disagrees: {} vs {}",
                    near[i],
                    far[i]
                );
            } else {
                assert!(far[i].abs() >= 4.0 - 1e-4, "sample {i} should not shrink");
            }
        }
    }
}
