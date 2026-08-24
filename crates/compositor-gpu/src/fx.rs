//! The GPU side of the [`schist_fx`] seam: blurs and mesh warps.
//!
//! These share the compositor's device and its work mutex — error scopes
//! are a per-device stack, so an fx submission running beside a composite
//! batch would pop the other's scope. Each job is one upload, a short
//! chain of dispatches and one readback, except the warp, whose source
//! plane stays resident between calls (a Puppet Warp drag re-warps the
//! same snapshot on every pointer move).

use crate::exec::GpuContext;
use schist_fx::{BlurJob, CarveJob, Carved, FxBackend, LensJob, WarpParams};
use std::sync::Arc;

pub struct GpuFx {
    ctx: Arc<GpuContext>,
    /// The warp source currently on the device, by `WarpParams::src_token`.
    resident: parking_lot::Mutex<Option<Resident>>,
}

struct Resident {
    token: u64,
    buffer: wgpu::Buffer,
}

impl GpuFx {
    pub fn new(ctx: Arc<GpuContext>) -> GpuFx {
        GpuFx {
            ctx,
            resident: parking_lot::Mutex::new(None),
        }
    }

    /// Whether a plane fits in one storage binding. The blurs band
    /// themselves past this; the warp cannot, because an arbitrary
    /// displacement may read anywhere in its source.
    fn plane_ok(&self, floats: usize) -> bool {
        floats
            .checked_mul(4)
            .is_some_and(|bytes| bytes <= self.ctx.binding_limit())
    }
}

impl FxBackend for GpuFx {
    fn name(&self) -> &'static str {
        "gpu"
    }

    fn blur(&self, job: &BlurJob<'_>) -> Option<Vec<f32>> {
        let pixels = job.width * job.height;
        let taps = (job.radius * 2 + 1) * 2 * job.passes;
        if !schist_fx::worth_offloading(pixels, taps) {
            return None;
        }
        self.ctx.run_blur(job)
    }

    fn lens_blur(&self, job: &LensJob<'_>) -> Option<Vec<f32>> {
        let pixels = job.width * job.height;
        let r = job.radius.max(0) as usize;
        // Roughly πr², the disc the kernel actually visits.
        let taps = (r * r * 314) / 100;
        if !schist_fx::worth_offloading(pixels, taps.max(1)) {
            return None;
        }
        self.ctx.run_lens_blur(job)
    }

    fn carve(&self, job: &CarveJob<'_>) -> Option<Carved> {
        let seams = job.width.abs_diff(job.target_width.max(1));
        if seams == 0 {
            return None;
        }
        let pixels = job.width.checked_mul(job.height)?;
        // The cumulative-cost scan is the one stage that cannot be made
        // embarrassingly parallel — it walks the rows in order, a band per
        // dispatch — and that fixed cost is what a small image cannot pay
        // for. On an adapter that *is* the CPU (llvmpipe, WARP) the bar is
        // higher again: there is no memory bandwidth to win, only the
        // parallelism, and the two paths measured here cross around 2 MP.
        let floor = if self.ctx.adapter_info().device_type == wgpu::DeviceType::Cpu {
            2_500_000
        } else {
            500_000
        };
        if pixels < floor {
            return None;
        }
        self.ctx.run_carve(job)
    }

    fn warp_source_resident(&self, token: u64) -> bool {
        token != 0
            && self
                .resident
                .lock()
                .as_ref()
                .is_some_and(|r| r.token == token)
    }

    fn warp(&self, params: &WarpParams<'_>, src: &[f32]) -> Option<Vec<f32>> {
        // Four bilinear taps plus the grid lookup: thin work per pixel, so
        // this only pays when the source stays resident across a drag and
        // a pointer move costs one dispatch and one readback rather than
        // two transfers of the whole layer. A tool that re-renders only
        // what its brush touched declines the deal by passing no token —
        // its jobs are too small to leave the CPU.
        let pixels = params.dst_width * params.dst_height;
        if params.src_token == 0 || !schist_fx::worth_offloading(pixels, 24) {
            return None;
        }
        if !self.plane_ok(pixels * 4) {
            return None;
        }
        let mut resident = self.resident.lock();
        let reuse = resident
            .as_ref()
            .is_some_and(|r| r.token == params.src_token);
        if !reuse {
            // `src` is empty only when we just told the caller we had the
            // plane, so this is a genuine upload.
            if src.is_empty() || !self.plane_ok(src.len()) {
                return None;
            }
            *resident = Some(Resident {
                token: params.src_token,
                buffer: self.ctx.upload_warp_source(src)?,
            });
        }
        let out = self
            .ctx
            .run_warp(params, &resident.as_ref().expect("just installed").buffer);
        if out.is_none() {
            // A failed submission may leave the buffer in a state the next
            // call would inherit; drop it and upload again.
            *resident = None;
        }
        out
    }
}

pub(crate) fn cast_f32s(values: &[f32]) -> &[u8] {
    // f32 → u8 view; alignment only shrinks, so this cannot fail.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr() as *const u8, std::mem::size_of_val(values))
    }
}
