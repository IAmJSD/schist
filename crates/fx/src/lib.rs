//! Whole-buffer image operations, with a seam a GPU backend can take over.
//!
//! These are the filter and warp kernels that sweep an entire selection
//! per keystroke of a dialog — blurs with a large kernel, and the
//! displacement resample Puppet Warp re-runs on every pointer move —
//! plus Content-Aware Scale, which sweeps it once per carved seam and so
//! does hundreds of passes for one command. They are the only pixel work
//! in the editor where a round trip to a second wgpu device can pay for
//! itself; brush-footprint tools do a few thousand pixels per dab and stay
//! on the CPU, where the latency is. Liquify is the one that goes both
//! ways: a dab resamples only the footprint of the brush and stays here,
//! but the warp it runs is the same one.
//!
//! The functions here are the entry points callers use. Each dispatches to
//! the installed [`FxBackend`] and falls back to the `*_cpu` reference —
//! which is the semantic contract, exactly as `schist-compositor`'s CPU
//! compositor is for compositing.

use rayon::prelude::*;
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
    /// resident between them — a Puppet Warp drag re-warps the same
    /// snapshot on every pointer move. Any change to the source pixels must
    /// change the token; 0 means "do not cache", and is also how a caller
    /// says this plane is a one-off it cropped for a single call.
    pub src_token: u64,
}

/// Resize an image's width by seam carving: repeatedly remove (or
/// duplicate) the lowest-energy connected path of pixels from top to
/// bottom, so flat sky gives way before a face does.
///
/// Only the width moves; the caller transposes to do the height, which is
/// the standard way to avoid writing every routine twice.
pub struct CarveJob<'a> {
    /// Straight-alpha RGBA f32, `width * height * 4` floats, row major.
    pub px: &'a [f32],
    /// Extra energy per pixel, keeping seams away from protected areas.
    pub protect: &'a [f32],
    pub width: usize,
    pub height: usize,
    pub target_width: usize,
}

/// What a carve leaves behind. `protect` comes back too: growing marks its
/// inserted columns, so a second pass over the same image needs it.
pub struct Carved {
    pub px: Vec<f32>,
    pub protect: Vec<f32>,
    pub width: usize,
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

    /// Seam-carve to `job.target_width`.
    ///
    /// Unlike the others this is not a single sweep but hundreds of them,
    /// each depending on the last, so a backend is expected to run the
    /// whole loop without coming back — the answer is one image, not one
    /// pass.
    fn carve(&self, job: &CarveJob<'_>) -> Option<Carved> {
        let _ = job;
        None
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

/// One separable box pass over RGBA f32, clamping at the edges and
/// carrying a running sum.
///
/// This re-summed the whole `2r+1` window for every output pixel, making
/// the pass O(pixels * r) rather than O(pixels): a gaussian at radius 50
/// on 2048x2048 took seconds. The window only gains and loses one sample
/// per step, which is the formulation `layer-fx::blur` already used.
///
/// Rows are independent and could be parallel too, but the vertical pass
/// walks columns, which are not contiguous in the buffer, so that needs a
/// transpose rather than a `par_chunks_mut`. The algorithmic fix is the
/// dominant one; threading is a separate change.
///
/// `fx_blur.wgsl` still sums each window itself, so the two differ by
/// accumulation order and the drift grows with row length. The parity
/// tests compare with a tolerance, at long rows as well as short ones.
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
        // Seed the window: the edge sample repeated for the leading half,
        // which is the same clamped edge handling as before.
        let mut acc = [0.0f32; 4];
        for c in 0..4 {
            acc[c] = src[base + c] * r as f32;
        }
        for k in 0..=r {
            let at = base + k.min(inner - 1) * stride;
            for c in 0..4 {
                acc[c] += src[at + c];
            }
        }
        for i in 0..inner {
            let at = base + i * stride;
            for c in 0..4 {
                dst[at + c] = acc[c] / window;
            }
            let add = base + (i + r + 1).min(inner - 1) * stride;
            let sub = base + i.saturating_sub(r) * stride;
            for c in 0..4 {
                acc[c] += src[add + c] - src[sub + c];
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
    // The disc was rediscovered per pixel by testing `dx*dx + dy*dy > r*r`
    // across the whole bounding square, so roughly a quarter of the taps
    // were tested and thrown away every time. Build it once.
    let mut offsets = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                offsets.push((dx, dy));
            }
        }
    }
    // Each row writes a disjoint slice and reads only the immutable `src`,
    // so this is a plain parallel gather. It was single-threaded, which at
    // radius 12 on 2048x2048 measured 5.62 s.
    px.par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            let y = y as i32;
            for x in 0..width as i32 {
                let mut acc = [0.0f32; 4];
                let mut n = 0.0f32;
                for &(dx, dy) in &offsets {
                    let p = at(&src, width, height, x + dx, y + dy);
                    // Weighting bright samples up spreads highlights into
                    // discs instead of smearing them away.
                    let k = 1.0 + luma(&p).powi(3) * boost * 8.0;
                    for c in 0..4 {
                        acc[c] += p[c] * k;
                    }
                    n += k;
                }
                if n > 0.0 {
                    let i = x as usize * 4;
                    for c in 0..4 {
                        row[i + c] = acc[c] / n;
                    }
                }
            }
        });
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
    if job.dst_width == 0 {
        return out;
    }
    // A row of the result depends on nothing but the source and the grid,
    // so they go out across the cores. This is the one kernel here that a
    // gesture waits on directly — a Liquify dab re-renders the footprint of
    // the brush between one pointer move and the next — and the offload
    // threshold keeps a footprint-sized job on this path.
    out.par_chunks_mut(job.dst_width * 4)
        .enumerate()
        .for_each(|(row, dst)| {
            for col in 0..job.dst_width {
                let x = job.dst_origin.0 + col as i32;
                let y = job.dst_origin.1 + row as i32;
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let (dx, dy) = mesh_sample(job, fx, fy);
                let px = fetch(job, src, fx + dx - 0.5, fy + dy - 0.5);
                dst[col * 4..col * 4 + 4].copy_from_slice(&px);
            }
        });
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

// ===== seam carving =====

/// Carve or grow vertical seams until the image is `target_width` wide.
pub fn carve(job: &CarveJob<'_>) -> Carved {
    if let Some(out) = backend().carve(job) {
        return out;
    }
    carve_cpu(job)
}

pub fn carve_cpu(job: &CarveJob<'_>) -> Carved {
    let mut img = Plane {
        width: job.width,
        height: job.height,
        px: job.px.to_vec(),
        protect: job.protect.to_vec(),
        energy: Vec::new(),
    };
    img.energy = img.compute_energy();
    // A one-pixel image has no seam to remove and nothing to interpolate
    // against, so both loops stop there.
    while img.width > job.target_width.max(1) {
        img.carve_one();
    }
    while img.width < job.target_width {
        img.grow_one();
    }
    Carved {
        px: img.px,
        protect: img.protect,
        width: img.width,
    }
}

/// The working image a carve mutates in place.
struct Plane {
    width: usize,
    height: usize,
    px: Vec<f32>,
    protect: Vec<f32>,
    /// The energy field, kept across seams.
    ///
    /// It used to be rebuilt from scratch for every seam -- and there is
    /// one seam per pixel of width change, so shrinking a 2000-pixel
    /// image by a quarter rebuilt a two-megapixel field five hundred
    /// times. Removing a seam only changes the energy within a pixel of
    /// where it ran, so the rest carries over.
    energy: Vec<f32>,
}

impl Plane {
    #[inline]
    fn lum(&self, x: usize, y: usize) -> f32 {
        let i = (y * self.width + x) * 4;
        0.299 * self.px[i] + 0.587 * self.px[i + 1] + 0.114 * self.px[i + 2]
    }

    /// Gradient magnitude plus protection, at one pixel.
    #[inline]
    fn energy_at(&self, x: usize, y: usize) -> f32 {
        let (w, h) = (self.width, self.height);
        let l = self.lum(x.saturating_sub(1), y);
        let r = self.lum((x + 1).min(w - 1), y);
        let u = self.lum(x, y.saturating_sub(1));
        let d = self.lum(x, (y + 1).min(h - 1));
        // Fully transparent pixels are free to remove.
        let alpha = self.px[(y * w + x) * 4 + 3];
        ((r - l).abs() + (d - u).abs()) * alpha + self.protect[y * w + x]
    }

    /// The whole energy field. Only the first seam pays for this.
    fn compute_energy(&self) -> Vec<f32> {
        let (w, h) = (self.width, self.height);
        let mut out = vec![0.0f32; w * h];
        out.par_chunks_mut(w.max(1))
            .enumerate()
            .for_each(|(y, row)| {
                for (x, v) in row.iter_mut().enumerate() {
                    *v = self.energy_at(x, y);
                }
            });
        out
    }

    /// Recompute energy in a band around `x` on rows `y - 1 ..= y + 1`.
    ///
    /// Energy reads one pixel each way, so removing or inserting a column
    /// at `x` invalidates `x - 1 ..= x + 1` on that row, and the rows
    /// either side through the vertical difference.
    fn refresh_energy_near(&mut self, x: usize, y: usize) {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 {
            return;
        }
        let y0 = y.saturating_sub(1);
        let y1 = (y + 1).min(h - 1);
        let x0 = x.saturating_sub(1);
        let x1 = (x + 1).min(w - 1);
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                self.energy[yy * w + xx] = self.energy_at(xx, yy);
            }
        }
    }

    /// The lowest-energy top-to-bottom seam, as one x per row.
    fn seam(&self) -> Vec<usize> {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let energy = &self.energy;
        // Cumulative cost, and which of the three pixels above we came
        // from, so the seam can be walked back.
        let mut cost = energy.clone();
        let mut from = vec![0i8; w * h];
        for y in 1..h {
            for x in 0..w {
                let mut best = cost[(y - 1) * w + x];
                let mut best_d = 0i8;
                if x > 0 && cost[(y - 1) * w + x - 1] < best {
                    best = cost[(y - 1) * w + x - 1];
                    best_d = -1;
                }
                if x + 1 < w && cost[(y - 1) * w + x + 1] < best {
                    best = cost[(y - 1) * w + x + 1];
                    best_d = 1;
                }
                cost[y * w + x] = energy[y * w + x] + best;
                from[y * w + x] = best_d;
            }
        }
        let mut x = (0..w)
            .min_by(|a, b| cost[(h - 1) * w + a].total_cmp(&cost[(h - 1) * w + b]))
            .unwrap_or(0);
        let mut seam = vec![0usize; h];
        for y in (0..h).rev() {
            seam[y] = x;
            let d = from[y * w + x];
            x = (x as isize + d as isize).clamp(0, w as isize - 1) as usize;
        }
        seam
    }

    /// Remove one vertical seam, narrowing the image by a pixel.
    fn carve_one(&mut self) {
        let seam = self.seam();
        if seam.is_empty() || self.width == 0 {
            return;
        }
        let (w, h) = (self.width, self.height);
        let mut px = Vec::with_capacity((w - 1) * h * 4);
        let mut prot = Vec::with_capacity((w - 1) * h);
        for (y, cut) in seam.iter().enumerate() {
            for x in 0..w {
                if x == *cut {
                    continue;
                }
                let i = (y * w + x) * 4;
                px.extend_from_slice(&self.px[i..i + 4]);
                prot.push(self.protect[y * w + x]);
            }
        }
        self.px = px;
        self.protect = prot;
        self.width = w - 1;
        // Carry the field over, dropping the removed column from each
        // row, then repair the band the removal disturbed.
        let mut energy = Vec::with_capacity((w - 1) * h);
        for (y, cut) in seam.iter().enumerate() {
            for x in 0..w {
                if x == *cut {
                    continue;
                }
                energy.push(self.energy[y * w + x]);
            }
        }
        self.energy = energy;
        for (y, cut) in seam.iter().enumerate() {
            self.refresh_energy_near((*cut).min(self.width.saturating_sub(1)), y);
        }
    }

    /// Duplicate one vertical seam, widening the image by a pixel.
    fn grow_one(&mut self) {
        let seam = self.seam();
        if seam.is_empty() {
            return;
        }
        let (w, h) = (self.width, self.height);
        let mut px = Vec::with_capacity((w + 1) * h * 4);
        let mut prot = Vec::with_capacity((w + 1) * h);
        for (y, cut) in seam.iter().enumerate() {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px.extend_from_slice(&self.px[i..i + 4]);
                prot.push(self.protect[y * w + x]);
                if x == *cut {
                    // The inserted pixel is the average of its neighbours,
                    // so the duplicate does not read as a hard repeat.
                    let j = (y * w + (x + 1).min(w - 1)) * 4;
                    for c in 0..4 {
                        px.push((self.px[i + c] + self.px[j + c]) / 2.0);
                    }
                    // Protect the inserted column too, or every later seam
                    // picks the same place and a crease forms.
                    prot.push(self.protect[y * w + x] + 200.0);
                }
            }
        }
        self.px = px;
        self.protect = prot;
        self.width = w + 1;
        let mut energy = Vec::with_capacity((w + 1) * h);
        for (y, cut) in seam.iter().enumerate() {
            for x in 0..w {
                energy.push(self.energy[y * w + x]);
                if x == *cut {
                    // Filled in by the repair pass below.
                    energy.push(0.0);
                }
            }
        }
        self.energy = energy;
        for (y, cut) in seam.iter().enumerate() {
            self.refresh_energy_near((*cut + 1).min(self.width - 1), y);
        }
    }
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
    /// The window re-summed per pixel, as `box_pass` used to be.
    fn naive_box_pass(
        src: &[f32],
        dst: &mut [f32],
        width: usize,
        height: usize,
        r: usize,
        vertical: bool,
    ) {
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

    #[test]
    fn the_running_sum_matches_the_window_it_replaced() {
        // The speed-up must not change the picture. Both edge handling and
        // the sum have to agree with the naive version, on both axes.
        let (w, h) = (64usize, 48usize);
        let src: Vec<f32> = (0..w * h * 4)
            .map(|i| ((i * 7919) % 997) as f32 / 997.0)
            .collect();
        for r in [0usize, 1, 3, 9, 20] {
            for vertical in [false, true] {
                let mut fast = vec![0.0; src.len()];
                let mut slow = vec![0.0; src.len()];
                box_pass(&src, &mut fast, w, h, r, vertical);
                naive_box_pass(&src, &mut slow, w, h, r, vertical);
                let worst = fast
                    .iter()
                    .zip(&slow)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    worst < 1e-4,
                    "r={r} vertical={vertical} diverged by {worst}"
                );
            }
        }
    }

    /// The energy field is carried across seams and repaired only where
    /// the seam ran. It used to be rebuilt in full for every seam, and
    /// there is one seam per pixel of width change — shrinking a
    /// 2000-pixel image by a quarter rebuilt a two-megapixel field five
    /// hundred times.
    ///
    /// A seam moves at most one column per row, so removing it can only
    /// disturb the energy within one column of where it ran; this checks
    /// that reasoning against a full rebuild, pixel for pixel.
    #[test]
    fn incremental_energy_carves_the_same_seams() {
        let (w, h) = (48usize, 32usize);
        // Structure the carve has to make choices about: a bright bar
        // down the middle and a noisy background.
        let mut px = vec![0f32; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let bar = (20..26).contains(&x);
                let n = ((x * 37 + y * 17) % 23) as f32 / 23.0;
                let v = if bar { 0.9 } else { 0.2 + n * 0.3 };
                px[i] = v;
                px[i + 1] = v * 0.8;
                px[i + 2] = 1.0 - v;
                px[i + 3] = 1.0;
            }
        }
        let protect = vec![0f32; w * h];

        for target in [w - 1, w - 5, w - 12, w + 1, w + 6] {
            let job = CarveJob {
                px: &px,
                protect: &protect,
                width: w,
                height: h,
                target_width: target,
            };
            let fast = carve_cpu(&job);
            let slow = reference_carve(&job);
            assert_eq!(fast.width, slow.width, "target {target}");
            assert_eq!(fast.px.len(), slow.px.len(), "target {target}");
            for i in 0..fast.px.len() {
                assert!(
                    (fast.px[i] - slow.px[i]).abs() < 1e-6,
                    "target {target}, sample {i}: {} != {}",
                    fast.px[i],
                    slow.px[i]
                );
            }
        }
    }

    /// `carve_cpu` with the energy field rebuilt from scratch per seam,
    /// which is what it used to do.
    fn reference_carve(job: &CarveJob<'_>) -> Carved {
        let mut img = Plane {
            width: job.width,
            height: job.height,
            px: job.px.to_vec(),
            protect: job.protect.to_vec(),
            energy: Vec::new(),
        };
        while img.width > job.target_width.max(1) {
            img.energy = img.compute_energy();
            img.carve_one();
        }
        while img.width < job.target_width {
            img.energy = img.compute_energy();
            img.grow_one();
        }
        Carved {
            px: img.px,
            protect: img.protect,
            width: img.width,
        }
    }
}
