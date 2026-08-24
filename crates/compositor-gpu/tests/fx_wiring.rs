//! The seam as the editor actually uses it: install the GPU backend
//! globally, run a real filter and a real warp through their normal entry
//! points, and check the pixels are the ones the CPU would have produced.
//!
//! The parity suite proves the kernels agree; this proves the callers are
//! wired to them, at sizes past the "worth shipping" threshold so the GPU
//! path genuinely runs.

use schist_color::Depth;
use schist_core::{blit_rgba8, IntRect, TileMap};
use schist_fx::{BlurJob, CarveJob, Carved, FxBackend, LensJob, WarpParams};
use schist_plugin_api::{FilterPlugin, FilterValues};
use schist_tools_warp::mesh::{warp_tiles, Mesh};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The GPU backend, counting the jobs it actually took. Without this a
/// test could pass because every job was declined and quietly ran on the
/// CPU — which is exactly the failure "is it wired up?" is asking about.
#[derive(Default)]
struct Counting {
    inner: Option<schist_compositor_gpu::GpuFx>,
    accepted: AtomicUsize,
}

impl Counting {
    fn took(&self) -> usize {
        self.accepted.load(Ordering::Relaxed)
    }

    fn count<T>(&self, out: Option<T>) -> Option<T> {
        if out.is_some() {
            self.accepted.fetch_add(1, Ordering::Relaxed);
        }
        out
    }
}

impl FxBackend for Counting {
    fn name(&self) -> &'static str {
        "gpu"
    }

    fn blur(&self, job: &BlurJob<'_>) -> Option<Vec<f32>> {
        self.count(self.inner.as_ref()?.blur(job))
    }

    fn lens_blur(&self, job: &LensJob<'_>) -> Option<Vec<f32>> {
        self.count(self.inner.as_ref()?.lens_blur(job))
    }

    fn warp(&self, params: &WarpParams<'_>, src: &[f32]) -> Option<Vec<f32>> {
        self.count(self.inner.as_ref()?.warp(params, src))
    }

    fn carve(&self, job: &CarveJob<'_>) -> Option<Carved> {
        self.count(self.inner.as_ref()?.carve(job))
    }

    fn warp_source_resident(&self, token: u64) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|fx| fx.warp_source_resident(token))
    }
}

/// `set_backend` is global, and both tests swap it to compare against the
/// CPU, so they cannot overlap.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn install_gpu() -> Option<Arc<Counting>> {
    match schist_compositor_gpu::GpuCompositor::new() {
        Ok(gpu) => {
            let counting = Arc::new(Counting {
                inner: Some(gpu.fx()),
                accepted: AtomicUsize::new(0),
            });
            schist_fx::set_backend(counting.clone());
            Some(counting)
        }
        Err(e) => {
            eprintln!("skipping fx wiring tests: {e}");
            None
        }
    }
}

fn noise_f32(w: usize, h: usize, seed: u64) -> Vec<f32> {
    let mut state = seed | 1;
    (0..w * h * 4)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / 16777216.0
        })
        .collect()
}

fn values(pairs: &[(&'static str, f32)]) -> FilterValues {
    let mut v = FilterValues::default();
    for (k, n) in pairs {
        v.set(k, *n);
    }
    v
}

fn run_filter(
    filter: &dyn FilterPlugin,
    px: &[f32],
    w: usize,
    h: usize,
    v: &FilterValues,
) -> Vec<f32> {
    let mut buf = px.to_vec();
    filter.apply(&mut buf, w, h, v);
    buf
}

fn assert_close(gpu: &[f32], cpu: &[f32], what: &str) {
    let worst = gpu
        .iter()
        .zip(cpu)
        .map(|(g, c)| (g - c).abs())
        .fold(0.0f32, f32::max);
    assert!(worst <= 1e-4, "{what}: worst channel difference {worst}");
}

#[test]
fn the_blur_filters_run_through_the_installed_backend() {
    let _guard = exclusive();
    let Some(backend) = install_gpu() else { return };
    // Big enough that the backend accepts the job rather than declining.
    let (w, h) = (512usize, 384usize);
    let px = noise_f32(w, h, 0x5EED);
    let cases: Vec<(Box<dyn FilterPlugin>, FilterValues)> = vec![
        (
            Box::new(schist_filters_core::GaussianBlur),
            values(&[("radius", 8.0)]),
        ),
        (
            Box::new(schist_filters_core::BoxBlur),
            values(&[("radius", 20.0)]),
        ),
        (
            Box::new(schist_filters_core::other::LensBlur),
            values(&[("radius", 10.0), ("brightness", 40.0)]),
        ),
    ];
    for (filter, v) in cases {
        let before = backend.took();
        let with_gpu = run_filter(filter.as_ref(), &px, w, h, &v);
        assert!(
            backend.took() > before,
            "{} never reached the GPU backend",
            filter.id()
        );
        schist_fx::set_backend(Arc::new(schist_fx::CpuFx));
        let with_cpu = run_filter(filter.as_ref(), &px, w, h, &v);
        schist_fx::set_backend(backend.clone());
        assert_close(&with_gpu, &with_cpu, filter.id());
        assert_ne!(with_gpu, px, "{} did nothing at all", filter.id());
    }
}

#[test]
fn a_tokened_warp_runs_through_the_installed_backend() {
    let _guard = exclusive();
    let Some(backend) = install_gpu() else { return };
    let (w, h) = (700usize, 620usize);
    let rect = IntRect::from_xywh(0, 0, w as u32, h as u32);
    let bytes: Vec<u8> = noise_f32(w, h, 0xB0A7)
        .iter()
        .map(|v| (v * 255.0) as u8)
        .collect();
    let mut src = TileMap::new();
    blit_rgba8(&mut src, Depth::Eight, rect, &bytes);

    let mut mesh = Mesh::new(rect);
    for (i, off) in mesh.offsets.iter_mut().enumerate() {
        *off = (
            ((i % 17) as f32 - 8.0) * 0.9,
            ((i % 23) as f32 - 11.0) * 0.7,
        );
    }
    let token = schist_tools_warp::mesh::next_source_token();
    let warped = warp_tiles(&src, &mesh, Depth::Eight, rect, token);
    // Second call: the source is resident now, so this is the path a drag
    // spends all its time on.
    let again = warp_tiles(&src, &mesh, Depth::Eight, rect, token);
    assert_eq!(backend.took(), 2, "both warps should have run on the GPU");

    schist_fx::set_backend(Arc::new(schist_fx::CpuFx));
    let on_cpu = warp_tiles(&src, &mesh, Depth::Eight, rect, token);

    for (label, tiles) in [("first", &warped), ("resident", &again)] {
        for y in (0..h as i32).step_by(7) {
            for x in (0..w as i32).step_by(5) {
                let g = tiles.pixel(x, y);
                let c = on_cpu.pixel(x, y);
                assert!(
                    (g.r - c.r).abs() <= 1.0 / 255.0
                        && (g.g - c.g).abs() <= 1.0 / 255.0
                        && (g.b - c.b).abs() <= 1.0 / 255.0
                        && (g.a - c.a).abs() <= 1.0 / 255.0,
                    "{label} warp at ({x}, {y}): {g:?} vs {c:?}"
                );
            }
        }
    }
    assert_ne!(
        warped.pixel(300, 300),
        src.pixel(300, 300),
        "the mesh did not move anything"
    );
}

/// Content-Aware Scale through the tool's own entry point. Big enough that
/// the backend accepts it, but only a few seams: each one is a handful of
/// full-image passes and the point here is the wiring, not the throughput.
#[test]
fn a_content_aware_scale_runs_through_the_installed_backend() {
    let _guard = exclusive();
    let Some(backend) = install_gpu() else { return };
    let (w, h) = (2000usize, 1500usize);
    let noise = noise_f32(w, h, 0xCA5E);
    let mut px = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            if x > w / 3 && x < 2 * w / 3 && y > h / 4 && y < 3 * h / 4 {
                px.extend_from_slice(&noise[i..i + 4]);
            } else {
                let t = y as f32 / h as f32;
                px.extend_from_slice(&[0.4 + t * 0.2, 0.5 + t * 0.2, 0.9, 1.0]);
            }
        }
    }
    let make = || schist_tools_warp::scale::Image {
        width: w,
        height: h,
        px: px.clone(),
        protect: vec![0.0; w * h],
    };

    let mut on_gpu = make();
    on_gpu.content_aware_resize(w - 4, h);
    assert!(
        backend.took() > 0,
        "content-aware scale never reached the GPU backend"
    );

    schist_fx::set_backend(Arc::new(schist_fx::CpuFx));
    let mut on_cpu = make();
    on_cpu.content_aware_resize(w - 4, h);
    schist_fx::set_backend(backend.clone());

    assert_eq!((on_gpu.width, on_gpu.height), (w - 4, h));
    assert_close(&on_gpu.px, &on_cpu.px, "content-aware scale");
    assert_ne!(on_gpu.px.len(), px.len(), "nothing was carved");
}
