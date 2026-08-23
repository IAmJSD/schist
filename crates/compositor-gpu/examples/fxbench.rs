//! CPU vs GPU for the filter and warp kernels behind the `schist_fx`
//! seam, at the sizes a dialog preview actually sweeps. Run with
//! `cargo run --release -p schist-compositor-gpu --example fxbench`.
//!
//! Note what the numbers mean on a software rasteriser (lavapipe): both
//! columns are then the same silicon and the GPU one is *not*
//! representative. Run it on a real adapter.

use schist_compositor_gpu::GpuCompositor;
use schist_fx::{BlurJob, FxBackend, LensJob, WarpParams};
use std::time::Instant;

fn noise(w: usize, h: usize) -> Vec<f32> {
    let mut state = 0x2545F4914F6CDD1Du64;
    (0..w * h * 4)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / 16777216.0
        })
        .collect()
}

fn time(label: &str, cpu: impl FnOnce(), gpu: impl FnOnce() -> bool) {
    let start = Instant::now();
    cpu();
    let cpu_ms = start.elapsed().as_secs_f64() * 1000.0;
    let start = Instant::now();
    let ran = gpu();
    let gpu_ms = start.elapsed().as_secs_f64() * 1000.0;
    if ran {
        println!(
            "{label:<34} cpu {cpu_ms:8.1} ms   gpu {gpu_ms:8.1} ms   {:.1}x",
            cpu_ms / gpu_ms
        );
    } else {
        println!("{label:<34} cpu {cpu_ms:8.1} ms   gpu declined");
    }
}

fn main() {
    let gpu = match GpuCompositor::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("no GPU adapter: {e}");
            return;
        }
    };
    println!("adapter: {}\n", gpu.describe());
    let fx = gpu.fx();

    // A 12 MP layer, which is where a full-canvas filter preview hurts.
    let (w, h) = (4000usize, 3000usize);
    let px = noise(w, h);

    for radius in [4.0f32, 20.0, 60.0] {
        let r = ((radius / 3.0f32.sqrt()).round() as usize).max(1);
        time(
            &format!("gaussian blur {w}x{h} r={radius:.0}"),
            || {
                let mut buf = px.clone();
                schist_fx::blur_rgba_cpu(&mut buf, w, h, r, 3);
            },
            || {
                fx.blur(&BlurJob {
                    px: &px,
                    width: w,
                    height: h,
                    radius: r,
                    passes: 3,
                })
                .is_some()
            },
        );
    }

    // Lens blur is O(r²) per pixel, so a smaller plane still takes seconds
    // on the CPU at the slider's top end.
    let (lw, lh) = (1200usize, 900usize);
    let lpx = noise(lw, lh);
    for radius in [8i32, 30, 60] {
        time(
            &format!("lens blur {lw}x{lh} r={radius}"),
            || {
                let mut buf = lpx.clone();
                schist_fx::lens_blur_rgba_cpu(&mut buf, lw, lh, radius, 0.5);
            },
            || {
                fx.lens_blur(&LensJob {
                    px: &lpx,
                    width: lw,
                    height: lh,
                    radius,
                    boost: 0.5,
                })
                .is_some()
            },
        );
    }

    // Liquify: the same snapshot re-warped as the pointer moves, which is
    // what the resident source plane is for. The whole source has to fit
    // one storage binding — an arbitrary displacement may read anywhere in
    // it — so this is the one kernel that declines on a big layer rather
    // than banding, and both sizes are worth seeing.
    println!(
        "\nbinding limit: {} MB",
        gpu.context().binding_limit() >> 20
    );
    for (ww, wh) in [(2000usize, 1500usize), (w, h)] {
        let wpx = noise(ww, wh);
        let (cols, rows) = (ww / 4 + 1, wh / 4 + 1);
        let mesh: Vec<f32> = (0..cols * rows * 2)
            .map(|i| ((i % 41) as f32 - 20.0) * 0.4)
            .collect();
        let params = WarpParams {
            src_width: ww,
            src_height: wh,
            src_origin: (0, 0),
            dst_origin: (0, 0),
            dst_width: ww,
            dst_height: wh,
            mesh: &mesh,
            mesh_cols: cols,
            mesh_rows: rows,
            cell: 4.0,
            mesh_origin: (0, 0),
            src_token: ww as u64,
        };
        // Prime the resident plane, as the first move of a drag would.
        let _ = fx.warp(&params, &wpx);
        let resident = fx.warp_source_resident(params.src_token);
        time(
            &format!("mesh warp {ww}x{wh} (resident: {resident})"),
            || {
                schist_fx::warp_cpu(&params, &wpx);
            },
            || {
                let src: &[f32] = if resident { &[] } else { &wpx };
                fx.warp(&params, src).is_some()
            },
        );
    }
}
