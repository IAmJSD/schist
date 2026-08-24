//! GPU/CPU parity for the filter and warp kernels behind the
//! `schist_fx` seam. Same contract as the compositor's parity suite: the
//! CPU reference is the semantics, the GPU has to match it, and a machine
//! with no adapter skips.

use schist_compositor_gpu::{GpuCompositor, GpuContext};
use schist_fx::{BlurJob, FxBackend, LensJob, WarpParams};
use std::sync::{Arc, OnceLock};

fn gpu() -> Option<&'static Arc<GpuContext>> {
    static GPU: OnceLock<Option<GpuCompositor>> = OnceLock::new();
    GPU.get_or_init(|| match GpuCompositor::new() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipping GPU fx parity tests: {e}");
            None
        }
    })
    .as_ref()
    .map(|g| g.context())
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn unit(&mut self) -> f32 {
        // Exact 0 and 1 included: the unpremultiply guard branches at
        // alpha 0, and clamping branches at 1.
        match self.next() % 8 {
            0 => 0.0,
            1 => 1.0,
            _ => (self.next() % 1000) as f32 / 999.0,
        }
    }
}

fn noise(w: usize, h: usize, seed: u64) -> Vec<f32> {
    let mut rng = Lcg(seed);
    (0..w * h * 4).map(|_| rng.unit()).collect()
}

/// Straight-alpha RGBA compared channel by channel. The kernels do the
/// same adds in the same order as the reference — bit-exact on lavapipe —
/// so the slack here is only for a driver's legal fma contraction, which
/// the trailing unpremultiply can divide up by a small alpha.
fn assert_close(gpu: &[f32], cpu: &[f32], what: &str) {
    assert_eq!(gpu.len(), cpu.len(), "{what}: length");
    let mut worst = 0.0f32;
    let mut worst_at = 0;
    for (i, (g, c)) in gpu.iter().zip(cpu).enumerate() {
        let d = (g - c).abs();
        if d > worst {
            worst = d;
            worst_at = i;
        }
    }
    assert!(
        worst <= 1e-4,
        "{what}: worst channel difference {worst} at {worst_at} \
         (gpu {}, cpu {})",
        gpu[worst_at],
        cpu[worst_at]
    );
}

#[test]
fn blur_matches_the_cpu_reference() {
    let Some(ctx) = gpu() else { return };
    // Odd sizes so the 16×16 workgroups have a ragged edge, and a radius
    // wider than the buffer so the window clamp gets exercised.
    for (w, h, radius, passes) in [
        (64usize, 48usize, 1usize, 1usize),
        (61, 47, 5, 3),
        (129, 3, 9, 3),
        (7, 7, 20, 3),
        (256, 200, 12, 3),
    ] {
        let px = noise(w, h, 100 + w as u64);
        let out = ctx
            .run_blur(&BlurJob {
                px: &px,
                width: w,
                height: h,
                radius,
                passes,
            })
            .expect("blur dispatch");
        let mut cpu = px.clone();
        schist_fx::blur_rgba_cpu(&mut cpu, w, h, radius, passes);
        assert_close(&out, &cpu, &format!("blur {w}x{h} r{radius} p{passes}"));
    }
}

#[test]
fn lens_blur_matches_the_cpu_reference() {
    let Some(ctx) = gpu() else { return };
    for (w, h, radius, boost) in [
        (48usize, 40usize, 1i32, 0.0f32),
        (37, 29, 5, 0.6),
        (64, 64, 12, 1.0),
        (5, 90, 8, 0.25),
    ] {
        let px = noise(w, h, 200 + w as u64);
        let out = ctx
            .run_lens_blur(&LensJob {
                px: &px,
                width: w,
                height: h,
                radius,
                boost,
            })
            .expect("lens dispatch");
        let mut cpu = px.clone();
        schist_fx::lens_blur_rgba_cpu(&mut cpu, w, h, radius, boost);
        assert_close(&out, &cpu, &format!("lens {w}x{h} r{radius} b{boost}"));
    }
}

fn warp_job<'a>(
    mesh: &'a [f32],
    w: usize,
    h: usize,
    cols: usize,
    rows: usize,
    token: u64,
) -> WarpParams<'a> {
    WarpParams {
        src_width: w,
        src_height: h,
        // Non-zero origins on both planes: the shader works in document
        // coordinates and a dropped offset would still look plausible.
        src_origin: (-20, 13),
        dst_origin: (-14, 20),
        dst_width: w - 6,
        dst_height: h - 5,
        mesh,
        mesh_cols: cols,
        mesh_rows: rows,
        cell: 4.0,
        mesh_origin: (-16, 16),
        src_token: token,
    }
}

#[test]
fn warp_matches_the_cpu_reference() {
    let Some(ctx) = gpu() else { return };
    let (w, h) = (70usize, 54usize);
    let (cols, rows) = (20usize, 16usize);
    let src = noise(w, h, 303);
    let mut rng = Lcg(404);
    // Displacements from a few pixels to well past the source plane, so
    // both the interpolated and the fully-transparent cases run.
    let mesh: Vec<f32> = (0..cols * rows * 2)
        .map(|i| {
            let v = (rng.next() % 2000) as f32 / 100.0 - 10.0;
            if i % 37 == 0 {
                v * 30.0
            } else {
                v
            }
        })
        .collect();
    let job = warp_job(&mesh, w, h, cols, rows, 1);
    let source = ctx.upload_warp_source(&src).expect("upload");
    let out = ctx.run_warp(&job, &source).expect("warp dispatch");
    assert_close(&out, &schist_fx::warp_cpu(&job, &src), "warp");
}

#[test]
fn a_degenerate_mesh_warps_to_the_identity() {
    let Some(ctx) = gpu() else { return };
    let (w, h) = (40usize, 36usize);
    let src = noise(w, h, 505);
    // Fewer than two vertices per axis: no gradient to interpolate, so
    // both paths must fall through to a zero displacement.
    let mesh = vec![7.0f32, -3.0];
    let job = warp_job(&mesh, w, h, 1, 1, 2);
    let source = ctx.upload_warp_source(&src).expect("upload");
    let out = ctx.run_warp(&job, &source).expect("warp dispatch");
    assert_close(&out, &schist_fx::warp_cpu(&job, &src), "degenerate mesh");
}

#[test]
fn the_backend_declines_work_too_small_to_ship() {
    let Some(ctx) = gpu() else { return };
    let fx = schist_compositor_gpu::GpuFx::new(ctx.clone());
    let px = noise(16, 16, 606);
    assert!(
        fx.blur(&BlurJob {
            px: &px,
            width: 16,
            height: 16,
            radius: 2,
            passes: 3,
        })
        .is_none(),
        "a 16×16 blur is not worth a round trip"
    );
    assert!(
        fx.warp(&warp_job(&[0.0; 8], 16, 16, 2, 2, 0), &px)
            .is_none(),
        "an untokened warp source cannot be kept resident"
    );
    assert!(
        !fx.warp_source_resident(0),
        "token 0 must never look resident"
    );
}

/// A Liquify drag re-warps one snapshot on every pointer move; the source
/// is uploaded once and only the result comes back. The second call has
/// to produce the same answer as the first, not a stale one.
#[test]
fn a_resident_warp_source_serves_repeated_drags() {
    let Some(ctx) = gpu() else { return };
    let fx = schist_compositor_gpu::GpuFx::new(ctx.clone());
    let (w, h) = (700usize, 620usize);
    let (cols, rows) = (180usize, 160usize);
    let src = noise(w, h, 707);
    let other = noise(w, h, 808);
    for (source, mesh_scale, token) in [
        (&src, 1.0f32, 1u64),
        (&src, -2.0, 1),
        (&other, 1.5, 2),
        (&src, 0.5, 1),
    ] {
        let mesh: Vec<f32> = (0..cols * rows * 2)
            .map(|i| ((i % 23) as f32 - 11.0) * mesh_scale)
            .collect();
        let job = warp_job(&mesh, w, h, cols, rows, token);
        // Hand over the pixels only when the backend says it needs them,
        // exactly as the dispatcher does — so a stale resident plane shows
        // up here as a wrong answer rather than being papered over.
        let out = if fx.warp_source_resident(token) {
            fx.warp(&job, &[]).expect("resident warp accepted")
        } else {
            fx.warp(&job, source).expect("warp accepted")
        };
        assert_close(&out, &schist_fx::warp_cpu(&job, source), "resident warp");
    }
}

/// A plane too big for one storage binding is blurred in horizontal bands
/// with an overlap. The rows each band keeps have to be the ones the
/// whole-image pass would have produced — so this forces a limit small
/// enough to band even a tiny image and checks nothing seams.
#[test]
fn banded_blurs_match_the_whole_image_result() {
    // Its own device: the limit override is per-context, and the other
    // tests share one.
    let Ok(own) = GpuCompositor::new() else {
        return;
    };
    let ctx = own.context();
    let (w, h) = (48usize, 200usize);
    let px = noise(w, h, 909);
    for (radius, passes, rows_per_band) in [
        (1usize, 1usize, 8usize),
        (3, 3, 20),
        (5, 3, 31),
        (2, 1, 3),
        (7, 2, 200),
    ] {
        // A binding that holds `rows_per_band + 2 * halo` rows, which is
        // what `band_plan` divides down to `rows_per_band` kept rows.
        let halo = passes * radius;
        ctx.set_binding_limit((rows_per_band + halo * 2) * w * 16);
        let banded = ctx
            .run_blur(&BlurJob {
                px: &px,
                width: w,
                height: h,
                radius,
                passes,
            })
            .expect("banded blur");
        let mut cpu = px.clone();
        schist_fx::blur_rgba_cpu(&mut cpu, w, h, radius, passes);
        assert_close(&banded, &cpu, &format!("banded blur r{radius} p{passes}"));

        ctx.set_binding_limit((rows_per_band + radius * 2) * w * 16);
        let banded = ctx
            .run_lens_blur(&LensJob {
                px: &px,
                width: w,
                height: h,
                radius: radius as i32,
                boost: 0.4,
            })
            .expect("banded lens blur");
        let mut cpu = px.clone();
        schist_fx::lens_blur_rgba_cpu(&mut cpu, w, h, radius as i32, 0.4);
        assert_close(&banded, &cpu, &format!("banded lens r{radius}"));
    }
    // Narrower than one band's overlap: nothing useful fits, so decline.
    ctx.set_binding_limit(w * 16);
    assert!(
        ctx.run_blur(&BlurJob {
            px: &px,
            width: w,
            height: h,
            radius: 4,
            passes: 3,
        })
        .is_none(),
        "a binding under one band's overlap must decline, not seam"
    );
}

// ===== seam carving =====

fn carve_image(w: usize, h: usize, seed: u64) -> (Vec<f32>, Vec<f32>) {
    let mut rng = Lcg(seed);
    let mut px = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            // A textured subject in the middle, smooth gradient either
            // side: the carve has an obvious place to eat, so a wrong
            // seam shows up as a wrong image rather than noise.
            let inside = x > w / 3 && x < 2 * w / 3 && y > h / 4 && y < 3 * h / 4;
            if inside {
                let n = rng.unit();
                px.extend_from_slice(&[n, 1.0 - n, (n * 3.0) % 1.0, 1.0]);
            } else {
                let t = y as f32 / h as f32;
                px.extend_from_slice(&[0.4 + t * 0.2, 0.5 + t * 0.2, 0.9, 1.0]);
            }
        }
    }
    (px, vec![0.0; w * h])
}

fn assert_carve_matches(w: usize, h: usize, target: usize, protect: Option<Vec<f32>>, what: &str) {
    let Some(ctx) = gpu() else { return };
    let (px, default_protect) = carve_image(w, h, 1000 + w as u64);
    let protect = protect.unwrap_or(default_protect);
    let job = schist_fx::CarveJob {
        px: &px,
        protect: &protect,
        width: w,
        height: h,
        target_width: target,
    };
    let gpu_out = ctx.run_carve(&job).expect("carve dispatch");
    let cpu_out = schist_fx::carve_cpu(&job);
    assert_eq!(gpu_out.width, cpu_out.width, "{what}: width");
    assert_eq!(gpu_out.px.len(), cpu_out.px.len(), "{what}: pixel count");
    assert_close(&gpu_out.px, &cpu_out.px, what);
    assert_close(
        &gpu_out.protect,
        &cpu_out.protect,
        &format!("{what} protect"),
    );
}

#[test]
fn carving_matches_the_cpu_reference() {
    assert_carve_matches(64, 48, 50, None, "carve 64->50");
    assert_carve_matches(97, 33, 60, None, "carve 97->60 (ragged)");
    assert_carve_matches(40, 1, 30, None, "carve single row");
    assert_carve_matches(300, 200, 290, None, "carve wide");
}

#[test]
fn growing_matches_the_cpu_reference() {
    assert_carve_matches(48, 40, 60, None, "grow 48->60");
    assert_carve_matches(70, 55, 74, None, "grow 70->74");
}

#[test]
fn a_protect_mask_steers_the_seam_the_same_way() {
    let (w, h) = (80usize, 60usize);
    let mut protect = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..25 {
            protect[y * w + x] = 1000.0;
        }
    }
    assert_carve_matches(w, h, 60, Some(protect), "protected carve");
}

/// Sizes that fall off the ends of the loops: a single row (so the scan is
/// the seed pass alone), a single column (nothing left to carve), and a
/// target the reference clamps rather than honouring.
#[test]
fn degenerate_carves_agree_with_the_reference() {
    let Some(ctx) = gpu() else { return };
    for (w, h, target) in [
        (30usize, 1usize, 1usize),
        (1, 30, 1),
        (12, 9, 0),
        (2, 2, 3),
        (33, 65, 32),
    ] {
        let (px, protect) = carve_image(w, h, 4000 + w as u64);
        let job = schist_fx::CarveJob {
            px: &px,
            protect: &protect,
            width: w,
            height: h,
            target_width: target,
        };
        let cpu = schist_fx::carve_cpu(&job);
        match ctx.run_carve(&job) {
            // Nothing to do is allowed to decline; anything it does run
            // has to match.
            None => assert_eq!(w, cpu.width, "declined a carve it had started"),
            Some(out) => {
                assert_eq!(out.width, cpu.width, "{w}x{h} -> {target}: width");
                assert_close(&out.px, &cpu.px, &format!("{w}x{h} -> {target}"));
            }
        }
    }
}
