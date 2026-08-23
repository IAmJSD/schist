//! GPU compositor: the wgpu implementation of the `Compositor` seam.
//!
//! GPUI does not expose its render device, so this is a second wgpu
//! instance doing compute only — layer tiles go up as storage buffers,
//! the layer tree (flattened to an op program by [`plan`]) runs as one
//! dispatch per batch, and the composited tiles come back over the same
//! bus. Batched workloads — zoom-outs, exports, multi-tile damage on
//! big documents — are where that trade wins; the semantics are the CPU
//! compositor's, enforced by parity tests, and anything the shader can't
//! express (a handful of adjustment kinds, mid-drag offsets) falls back
//! to the CPU reference per call.
//!
//! Install with `schist_compositor::set_backend(Arc::new(GpuCompositor::new()?))`.

mod exec;
pub mod plan;

pub use exec::GpuContext;

use exec::BatchOut;
use schist_compositor::viewport::ViewportParams;
use schist_compositor::{
    composite_region_f32_cpu, composite_region_rgba8_cpu, composite_tile_cpu, Compositor,
    CpuCompositor,
};
use schist_core::{Document, IntRect, TileCoord, TILE_SIZE};
use std::sync::Arc;

pub struct GpuCompositor {
    ctx: GpuContext,
}

impl GpuCompositor {
    /// Set up the GPU backend. Fails cleanly (with a reason for the log)
    /// when no adapter exists — headless CI, missing drivers.
    pub fn new() -> Result<GpuCompositor, String> {
        let ctx = GpuContext::new()?;
        Ok(GpuCompositor { ctx })
    }

    /// "vulkan · NVIDIA RTX 4070" — for logs and the About dialog.
    pub fn describe(&self) -> String {
        let info = self.ctx.adapter_info();
        format!("{:?} · {}", info.backend, info.name).to_lowercase()
    }

    pub fn context(&self) -> &GpuContext {
        &self.ctx
    }

    /// Composite a batch on the GPU; `None` falls back to the CPU.
    fn batch(&self, doc: &Document, coords: &[TileCoord], rgba8: bool) -> Option<BatchOut> {
        let plan = match plan::build(doc) {
            Ok(plan) => plan,
            Err(why) => {
                log::debug!("gpu compositor fallback: {why:?}");
                return None;
            }
        };
        self.ctx.composite_batch(&plan, coords, rgba8)
    }
}

impl Compositor for GpuCompositor {
    fn name(&self) -> &'static str {
        "gpu"
    }

    fn tile(&self, doc: &Document, coord: TileCoord) -> Vec<f32> {
        match self.batch(doc, &[coord], false) {
            Some(BatchOut::F32(mut tiles)) => tiles.pop().unwrap(),
            _ => composite_tile_cpu(doc, coord),
        }
    }

    fn tiles_rgba8(&self, doc: &Document, coords: &[TileCoord]) -> Vec<Vec<u8>> {
        match self.batch(doc, coords, true) {
            Some(BatchOut::Rgba8(tiles)) => tiles,
            _ => CpuCompositor.tiles_rgba8(doc, coords),
        }
    }

    fn region_f32(&self, doc: &Document, region: IntRect) -> Vec<f32> {
        let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();
        match self.batch(doc, &coords, false) {
            Some(BatchOut::F32(tiles)) => {
                let mut out = vec![0.0f32; region.width() as usize * region.height() as usize * 4];
                for (coord, tile) in coords.iter().zip(&tiles) {
                    crop_into(&region, *coord, &mut out, tile);
                }
                out
            }
            _ => composite_region_f32_cpu(doc, region),
        }
    }

    fn region_rgba8(&self, doc: &Document, region: IntRect) -> Vec<u8> {
        let coords: Vec<TileCoord> = TileCoord::covering(&region).collect();
        match self.batch(doc, &coords, true) {
            Some(BatchOut::Rgba8(tiles)) => {
                let mut out = vec![0u8; region.width() as usize * region.height() as usize * 4];
                for (coord, tile) in coords.iter().zip(&tiles) {
                    crop_into(&region, *coord, &mut out, tile);
                }
                out
            }
            _ => composite_region_rgba8_cpu(doc, region),
        }
    }

    fn viewport(
        &self,
        params: &ViewportParams,
        grid: &[Option<Arc<Vec<u8>>>],
    ) -> Option<Vec<u8>> {
        self.ctx.render_viewport(params, grid)
    }
}

/// Copy the intersection of a composited tile into a tightly packed
/// region buffer (works for any 4-element pixel type).
fn crop_into<T: Copy>(region: &IntRect, coord: TileCoord, out: &mut [T], tile: &[T]) {
    let w = region.width() as usize;
    let trect = coord.rect();
    let clip = trect.intersect(region);
    for y in clip.top..clip.bottom {
        let ly = (y - trect.top) as usize;
        let oy = (y - region.top) as usize;
        for x in clip.left..clip.right {
            let lx = (x - trect.left) as usize;
            let ox = (x - region.left) as usize;
            let s = (ly * TILE_SIZE as usize + lx) * 4;
            let d = (oy * w + ox) * 4;
            out[d..d + 4].copy_from_slice(&tile[s..s + 4]);
        }
    }
}
