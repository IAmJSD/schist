//! Whole-buffer image operations, with a seam a GPU backend can take over.
//!
//! These are the filter and warp kernels that sweep an entire selection
//! per keystroke of a dialog — blurs with a large kernel, and the
//! displacement resample Liquify and Puppet Warp re-run on every pointer
//! move. They are the only pixel work in the editor where a round trip to
//! a second wgpu device can pay for itself; brush-footprint tools do a few
//! thousand pixels per dab and stay on the CPU, where the latency is.
//!
//! The functions here are the entry points callers use. Each dispatches to
//! the installed [`FxBackend`] and falls back to the `*_cpu` reference —
//! which is the semantic contract, exactly as `schist-compositor`'s CPU
//! compositor is for compositing.

use std::sync::{Arc, OnceLock, RwLock};

/// A separable box blur: `passes` rounds of one horizontal and one
/// vertical pass over premultiplied alpha.
pub struct BlurJob<'a> {
    /// Straight-alpha RGBA f32, `width * height * 4` floats, row major.
    pub px: &'a [f32],
    pub width: usize,
    pub height: usize,
    /// Half-width of the window; the window is `2 * radius + 1` wide.
    pub radius: usize,
    pub passes: usize,
}

/// A disc-kernel blur, with bright samples weighted up so out-of-focus
/// highlights come out as bokeh circles.
pub struct LensJob<'a> {
    pub px: &'a [f32],
    pub width: usize,
    pub height: usize,
    pub radius: i32,
    /// 0..1; how much a sample's cubed luma adds to its weight.
    pub boost: f32,
}

/// Resample a source plane through a coarse displacement grid.
///
/// Both planes are straight-alpha RGBA f32 in document coordinates; the
/// sampling is bilinear on premultiplied alpha so a soft edge does not
/// fringe. Reads outside the source plane are transparent.
///
/// The source pixels are *not* part of this: see [`warp`], which only
/// materialises them when whoever runs the job actually needs them.
pub struct WarpParams<'a> {
    pub src_width: usize,
    pub src_height: usize,
    /// Document position of the source plane's top-left pixel.
    pub src_origin: (i32, i32),
    /// Document position of the destination plane's top-left pixel.
    pub dst_origin: (i32, i32),
    pub dst_width: usize,
    pub dst_height: usize,
    /// `(dx, dy)` per grid vertex, interleaved, row major: where a point's
    /// colour is fetched from, relative to itself.
    pub mesh: &'a [f32],
    pub mesh_cols: usize,
    pub mesh_rows: usize,
    /// Grid spacing in pixels.
    pub cell: f32,
    /// Document position of mesh vertex (0, 0).
    pub mesh_origin: (i32, i32),
    /// Identifies the source plane across calls so a backend can keep it
    /// resident between them — a Liquify drag re-warps the same snapshot
    /// on every pointer move. Any change to the source pixels must change
    /// the token; 0 means "do not cache".
    pub src_token: u64,
}

/// The accelerated-effects seam.
///
/// Every method may decline by returning `None` — too small to be worth a
/// round trip, over a buffer limit, a readback that failed — and the
/// caller then runs the CPU reference. A backend must never return
/// something *different*; the parity tests in `schist-compositor-gpu` hold
/// the wgpu one to that.
pub trait FxBackend: Send + Sync {
    /// Short name for logs ("cpu", "gpu").
    fn name(&self) -> &'static str;

    fn blur(&self, job: &BlurJob<'_>) -> Option<Vec<f32>> {
        let _ = job;
        None
    }

    fn lens_blur(&self, job: &LensJob<'_>) -> Option<Vec<f32>> {
        let _ = job;
        None
    }

    /// `src` is the source plane, or empty when
    /// [`warp_source_resident`](Self::warp_source_resident) has just said
    /// this backend already holds `params.src_token`.
    fn warp(&self, params: &WarpParams<'_>, src: &[f32]) -> Option<Vec<f32>> {
        let _ = (params, src);
        None
    }

    /// Whether `token`'s pixels are already on the device. Flattening a
    /// tile map into a plane costs a pass over the layer, so a caller that
    /// hears "yes" can skip it entirely.
    fn warp_source_resident(&self, token: u64) -> bool {
        let _ = token;
        false
    }
}

/// The reference: everything on the CPU, nothing declined.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuFx;

impl FxBackend for CpuFx {
    fn name(&self) -> &'static str {
        "cpu"
    }
}

static BACKEND: OnceLock<RwLock<Arc<dyn FxBackend>>> = OnceLock::new();

fn backend_cell() -> &'static RwLock<Arc<dyn FxBackend>> {
    BACKEND.get_or_init(|| RwLock::new(Arc::new(CpuFx)))
}

/// Install the backend the dispatchers below use.
pub fn set_backend(backend: Arc<dyn FxBackend>) {
    *backend_cell().write().unwrap() = backend;
}

/// The currently active backend.
pub fn backend() -> Arc<dyn FxBackend> {
    backend_cell().read().unwrap().clone()
}

/// Whether a job is big enough to be worth uploading.
///
/// The round trip costs bytes in and bytes out regardless of what happens
/// in between, so what decides it is arithmetic intensity: `taps` is how
/// many source samples each output pixel reads. The threshold is
/// deliberately conservative — being wrong here costs a slower frame, and
/// the CPU path is already interactive at small sizes.
pub fn worth_offloading(pixels: usize, taps: usize) -> bool {
    pixels.saturating_mul(taps) >= 8_000_000
}

// ===== blur =====

/// Gaussian blur by three box passes — close enough that the difference is
/// invisible, at a fraction of the cost.
pub fn gaussian_rgba(px: &mut [f32], width: usize, height: usize, radius: f32) {
    if radius < 0.5 || width == 0 || height == 0 {
        return;
    }
    let r = ((radius / 3.0f32.sqrt()).round() as usize).max(1);
    blur_rgba(px, width, height, r, 3);
}

/// One box pass in each direction.
pub fn box_blur_rgba(px: &mut [f32], width: usize, height: usize, radius: usize) {
    if radius == 0 || width == 0 || height == 0 {
        return;
    }
    blur_rgba(px, width, height, radius, 1);
}

/// `passes` rounds of horizontal-then-vertical box blur, premultiplying
/// first so transparent pixels do not bleed their colour in.
pub fn blur_rgba(px: &mut [f32], width: usize, height: usize, radius: usize, passes: usize) {
    if radius == 0 || passes == 0 || width == 0 || height == 0 {
        return;
    }
    let job = BlurJob {
        px,
        width,
        height,
        radius,
        passes,
    };
    if let Some(out) = backend().blur(&job) {
        px.copy_from_slice(&out);
        return;
    }
    blur_rgba_cpu(px, width, height, radius, passes);
}

pub fn blur_rgba_cpu(px: &mut [f32], width: usize, height: usize, radius: usize, passes: usize) {
    if radius == 0 || passes == 0 || width == 0 || height == 0 {
        return;
    }
    premultiply(px);
    let mut tmp = vec![0.0f32; px.len()];
    for _ in 0..passes {
        box_pass(px, &mut tmp, width, height, radius, false);
        box_pass(&tmp, px, width, height, radius, true);
    }
    unpremultiply(px);
}

/// One separable box pass, clamping at the edges.
fn box_pass(src: &[f32], dst: &mut [f32], width: usize, height: usize, r: usize, vertical: bool) {
    let (outer, inner) = if vertical {
        (width, height)
    } else {
        (height, width)
    };
    let stride = if vertical { width * 4 } else { 4 };
    let step = if vertical { 4 } else { width * 4 };
    let window = (r * 2 + 1) as f32;
    for o in 0..outer {
        let base = o * step;
        for i in 0..inner {
            let mut acc = [0.0f32; 4];
            for k in 0..=(r * 2) {
                let s = (i + k).saturating_sub(r).min(inner - 1);
                let at = base + s * stride;
                for c in 0..4 {
                    acc[c] += src[at + c];
                }
            }
            let at = base + i * stride;
            for c in 0..4 {
                dst[at + c] = acc[c] / window;
            }
        }
    }
}

// ===== lens blur =====

/// Disc-kernel blur. `radius` in pixels, `boost` 0..1.
pub fn lens_blur_rgba(px: &mut [f32], width: usize, height: usize, radius: i32, boost: f32) {
    if radius < 1 || width == 0 || height == 0 {
        return;
    }
    let job = LensJob {
        px,
        width,
        height,
        radius,
        boost,
    };
    if let Some(out) = backend().lens_blur(&job) {
        px.copy_from_slice(&out);
        return;
    }
    lens_blur_rgba_cpu(px, width, height, radius, boost);
}

pub fn lens_blur_rgba_cpu(px: &mut [f32], width: usize, height: usize, radius: i32, boost: f32) {
    if radius < 1 || width == 0 || height == 0 {
        return;
    }
    let r = radius;
    premultiply(px);
    let src = px.to_vec();
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let mut acc = [0.0f32; 4];
            let mut n = 0.0f32;
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                    let p = at(&src, width, height, x + dx, y + dy);
                    // Weighting bright samples up spreads highlights into
                    // discs instead of smearing them away.
                    let k = 1.0 + luma(&p).powi(3) * boost * 8.0;
                    for c in 0..4 {
                        acc[c] += p[c] * k;
                    }
                    n += k;
                }
            }
            if n > 0.0 {
                for a in acc.iter_mut() {
                    *a /= n;
                }
                let i = (y as usize * width + x as usize) * 4;
                px[i..i + 4].copy_from_slice(&acc);
            }
        }
    }
    unpremultiply(px);
}

// ===== warp =====

/// Resample through the displacement grid, returning a fresh destination
/// plane.
///
/// `src` is a thunk because a backend holding `params.src_token` already
/// has the pixels: a Liquify drag then costs one dispatch and one
/// readback per pointer move, with no pass over the tile map at all. It is
/// still called if the backend declines, so it must produce the same
/// plane every time for a given token.
pub fn warp(params: &WarpParams<'_>, src: impl FnOnce() -> Vec<f32>) -> Vec<f32> {
    let backend = backend();
    if params.src_token != 0 && backend.warp_source_resident(params.src_token) {
        if let Some(out) = backend.warp(params, &[]) {
            return out;
        }
    }
    let src = src();
    if let Some(out) = backend.warp(params, &src) {
        return out;
    }
    warp_cpu(params, &src)
}

pub fn warp_cpu(job: &WarpParams<'_>, src: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0f32; job.dst_width * job.dst_height * 4];
    for row in 0..job.dst_height {
        for col in 0..job.dst_width {
            let x = job.dst_origin.0 + col as i32;
            let y = job.dst_origin.1 + row as i32;
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (dx, dy) = mesh_sample(job, fx, fy);
            let px = fetch(job, src, fx + dx - 0.5, fy + dy - 0.5);
            let i = (row * job.dst_width + col) * 4;
            out[i..i + 4].copy_from_slice(&px);
        }
    }
    out
}

/// Bilinear displacement at a document position.
fn mesh_sample(job: &WarpParams<'_>, x: f32, y: f32) -> (f32, f32) {
    if job.mesh_cols < 2 || job.mesh_rows < 2 {
        return (0.0, 0.0);
    }
    let fx = ((x - job.mesh_origin.0 as f32) / job.cell).clamp(0.0, (job.mesh_cols - 1) as f32);
    let fy = ((y - job.mesh_origin.1 as f32) / job.cell).clamp(0.0, (job.mesh_rows - 1) as f32);
    let (c0, r0) = (fx.floor() as usize, fy.floor() as usize);
    let (c1, r1) = (
        (c0 + 1).min(job.mesh_cols - 1),
        (r0 + 1).min(job.mesh_rows - 1),
    );
    let (tx, ty) = (fx - c0 as f32, fy - r0 as f32);
    let at = |c: usize, r: usize| {
        let i = (r * job.mesh_cols + c) * 2;
        (job.mesh[i], job.mesh[i + 1])
    };
    let (a, b, cc, d) = (at(c0, r0), at(c1, r0), at(c0, r1), at(c1, r1));
    let top = (a.0 + (b.0 - a.0) * tx, a.1 + (b.1 - a.1) * tx);
    let bottom = (cc.0 + (d.0 - cc.0) * tx, cc.1 + (d.1 - cc.1) * tx);
    (
        top.0 + (bottom.0 - top.0) * ty,
        top.1 + (bottom.1 - top.1) * ty,
    )
}

/// Bilinear fetch on premultiplied alpha, returning straight alpha.
fn fetch(job: &WarpParams<'_>, src: &[f32], fx: f32, fy: f32) -> [f32; 4] {
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let (x0, y0) = (x0 as i32, y0 as i32);
    let mut acc = [0.0f32; 4];
    for (dx, dy, w) in [
        (0, 0, (1.0 - tx) * (1.0 - ty)),
        (1, 0, tx * (1.0 - ty)),
        (0, 1, (1.0 - tx) * ty),
        (1, 1, tx * ty),
    ] {
        if w <= 0.0 {
            continue;
        }
        let p = src_pixel(job, src, x0 + dx, y0 + dy);
        acc[0] += p[0] * p[3] * w;
        acc[1] += p[1] * p[3] * w;
        acc[2] += p[2] * p[3] * w;
        acc[3] += p[3] * w;
    }
    if acc[3] <= 1e-6 {
        return [0.0; 4];
    }
    [acc[0] / acc[3], acc[1] / acc[3], acc[2] / acc[3], acc[3]]
}

/// Source pixel in document coordinates; transparent outside the plane.
fn src_pixel(job: &WarpParams<'_>, src: &[f32], x: i32, y: i32) -> [f32; 4] {
    let lx = x - job.src_origin.0;
    let ly = y - job.src_origin.1;
    if lx < 0 || ly < 0 || lx >= job.src_width as i32 || ly >= job.src_height as i32 {
        return [0.0; 4];
    }
    let i = (ly as usize * job.src_width + lx as usize) * 4;
    [src[i], src[i + 1], src[i + 2], src[i + 3]]
}

// ===== shared pixel helpers =====

pub fn premultiply(px: &mut [f32]) {
    for p in px.as_chunks_mut::<4>().0.iter_mut() {
        p[0] *= p[3];
        p[1] *= p[3];
        p[2] *= p[3];
    }
}

pub fn unpremultiply(px: &mut [f32]) {
    for p in px.as_chunks_mut::<4>().0.iter_mut() {
        if p[3] > 1e-6 {
            p[0] /= p[3];
            p[1] /= p[3];
            p[2] /= p[3];
        } else {
            p[0] = 0.0;
            p[1] = 0.0;
            p[2] = 0.0;
        }
    }
}

#[inline]
fn luma(p: &[f32; 4]) -> f32 {
    0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]
}

/// Read a pixel, clamping to the edge.
#[inline]
fn at(px: &[f32], w: usize, h: usize, x: i32, y: i32) -> [f32; 4] {
    let x = x.clamp(0, w as i32 - 1) as usize;
    let y = y.clamp(0, h as i32 - 1) as usize;
    let i = (y * w + x) * 4;
    [px[i], px[i + 1], px[i + 2], px[i + 3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(w: usize, h: usize) -> Vec<f32> {
        let mut px = vec![0.0f32; w * h * 4];
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let t = i as f32 / (w * h) as f32;
            *p = [t, 1.0 - t, (t * 7.0) % 1.0, ((t * 3.0) % 1.0).max(0.05)];
        }
        px
    }

    #[test]
    fn a_blur_spreads_an_impulse_and_conserves_energy() {
        let (w, h) = (33, 33);
        let mut px = vec![0.0f32; w * h * 4];
        let mid = ((h / 2) * w + w / 2) * 4;
        px[mid..mid + 4].copy_from_slice(&[1.0, 1.0, 1.0, 1.0]);
        let before: f32 = px.as_chunks::<4>().0.iter().map(|p| p[3]).sum();
        gaussian_rgba(&mut px, w, h, 4.0);
        let after: f32 = px.as_chunks::<4>().0.iter().map(|p| p[3]).sum();
        assert!((before - after).abs() < 0.05, "{before} -> {after}");
        assert!(px[mid + 3] < 0.2, "the impulse did not spread");
    }

    #[test]
    fn the_dispatcher_matches_the_reference() {
        // With no backend installed the two must be the same code path;
        // this is the guard that keeps them from drifting apart.
        let (w, h) = (24, 18);
        let mut a = ramp(w, h);
        let mut b = a.clone();
        gaussian_rgba(&mut a, w, h, 3.0);
        blur_rgba_cpu(&mut b, w, h, 2, 3);
        assert_eq!(a, b);

        let mut a = ramp(w, h);
        let mut b = a.clone();
        lens_blur_rgba(&mut a, w, h, 3, 0.5);
        lens_blur_rgba_cpu(&mut b, w, h, 3, 0.5);
        assert_eq!(a, b);
    }

    #[test]
    fn a_zero_mesh_warp_is_the_identity() {
        let (w, h) = (16, 12);
        let src = ramp(w, h);
        let job = WarpParams {
            src_width: w,
            src_height: h,
            src_origin: (0, 0),
            dst_origin: (0, 0),
            dst_width: w,
            dst_height: h,
            mesh: &[0.0; 5 * 4 * 2],
            mesh_cols: 5,
            mesh_rows: 4,
            cell: 4.0,
            mesh_origin: (0, 0),
            src_token: 0,
        };
        let out = warp(&job, || src.clone());
        for (o, s) in out.as_chunks::<4>().0.iter().zip(src.as_chunks::<4>().0) {
            for c in 0..4 {
                assert!((o[c] - s[c]).abs() < 1e-5, "{o:?} != {s:?}");
            }
        }
    }

    #[test]
    fn a_warp_outside_the_source_plane_is_transparent() {
        let (w, h) = (8, 8);
        let src = ramp(w, h);
        let mesh = vec![100.0f32; 3 * 3 * 2];
        let job = WarpParams {
            src_width: w,
            src_height: h,
            src_origin: (0, 0),
            dst_origin: (0, 0),
            dst_width: w,
            dst_height: h,
            mesh: &mesh,
            mesh_cols: 3,
            mesh_rows: 3,
            cell: 4.0,
            mesh_origin: (0, 0),
            src_token: 0,
        };
        assert!(warp(&job, || src.clone()).iter().all(|v| *v == 0.0));
    }
}
