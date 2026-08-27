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
    blend::BlendMode, BevelStyle, BevelStyle_, BlurStyle, GlowStyle, GradientOverlayStyle,
    GradientShape, IntRect, Layer, LayerStyle, SatinStyle, ShadowStyle, StrokePosition, StrokeStyle,
    StyledRaster, Technique, TileCoord, TileMap,
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

    // Affinity's Gaussian Blur effect softens the layer itself, and it
    // sits at the bottom of the panel's list, so everything below works
    // from the blurred shape rather than the original one.
    if let Some(b) = style.blur.on() {
        blur_content(&mut base, &mut alpha, w, h, b);
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

/// Blur the layer's own pixels in place, and the alpha the rest of the
/// stack derives from with them.
///
/// Colour is blurred premultiplied so transparent neighbours cannot drag
/// their (undefined) colour in. `preserve_alpha` puts the original alpha
/// back afterwards, which smears the contents inside an unchanged shape.
fn blur_content(base: &mut Plane, alpha: &mut [f32], w: usize, h: usize, b: &BlurStyle) {
    if b.radius < 0.5 {
        return;
    }
    let n = w * h;
    let mut chan = vec![0.0f32; n];
    let kept: Vec<f32> = base.px.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    for c in 0..4 {
        for i in 0..n {
            let a = base.px[i * 4 + 3];
            chan[i] = if c == 3 {
                a
            } else {
                base.px[i * 4 + c] * a
            };
        }
        gaussian_alpha(&mut chan, w, h, b.radius);
        for i in 0..n {
            base.px[i * 4 + c] = chan[i];
        }
    }
    for i in 0..n {
        let a = base.px[i * 4 + 3];
        if a > f32::EPSILON {
            let unpremul = 1.0 / a;
            for c in 0..3 {
                base.px[i * 4 + c] *= unpremul;
            }
        }
        if b.preserve_alpha {
            base.px[i * 4 + 3] = kept[i];
        }
    }
    if !b.preserve_alpha {
        gaussian_alpha(alpha, w, h, b.radius);
    }
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
    blur::gaussian_alpha(&mut a, w, h, s.size);
    apply_spread(&mut a, s.spread);
    Plane::from_alpha(rect, &a, s.color)
}

fn inner_shadow_plane(alpha: &[f32], w: usize, h: usize, rect: IntRect, s: &ShadowStyle) -> Plane {
    shadow_plane(alpha, w, h, rect, s, true)
}

fn outer_glow_plane(alpha: &[f32], w: usize, h: usize, rect: IntRect, g: &GlowStyle) -> Plane {
    let mut a = alpha.to_vec();
    match g.technique {
        Technique::Softer => blur::gaussian_alpha(&mut a, w, h, g.size),
        Technique::Precise => precise_grow(&mut a, w, h, g.size),
    }
    apply_spread(&mut a, g.spread);
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
    match g.technique {
        Technique::Softer => blur::gaussian_alpha(&mut a, w, h, g.size),
        Technique::Precise => precise_grow(&mut a, w, h, g.size),
    }
    apply_spread(&mut a, g.spread);
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
/// Harden a blurred matte, which is what Affinity's Intensity slider
/// does: probed at radius 40 with intensity 0, 50, 80 and 100 % on a
/// hard-edged square (`ig_r*_i*.af`), the glow comes back as the plain
/// blurred step scaled by exactly `1 / (1 - intensity)` and clipped —
/// 50 % doubles it to within 1/255, 80 % quintuples it.
///
/// This runs *after* the blur. Photoshop's Spread/Choke is a dilation
/// before it instead, but running this before the blur made it a no-op
/// on any hard-edged layer, which is most of them.
fn apply_spread(a: &mut [f32], spread: f32) {
    if spread <= 0.0 {
        return;
    }
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
/// searched out to `limit`. A brute-force search over a small window is
/// enough: `limit` is an effect size, so tens of pixels at most.
fn signed_distance(alpha: &[f32], w: usize, h: usize, limit: f32) -> Vec<f32> {
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
            // `best` is the distance to the nearest pixel of the other
            // class; the edge itself lies half a pixel before that one,
            // so the geometric distance from this pixel's centre is
            // half a pixel less. Without the correction an outside
            // stroke's own edge lands exactly on a pixel centre and
            // comes back half covered, where Affinity's is solid — a
            // one-pixel seam all the way round the shape, and on the
            // stroke probes the whole of the residual.
            let best = (best - 0.5).max(0.0);
            out[i] = if inside { -best } else { best };
        }
    }
    out
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
