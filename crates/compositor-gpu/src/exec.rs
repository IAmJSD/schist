//! wgpu execution: pack a [`Plan`](crate::plan::Plan) and a batch of tile
//! coords into storage buffers, run the interpreter dispatch, read the
//! result back.
//!
//! This is a second wgpu instance beside GPUI's renderer (GPUI does not
//! expose its device), so every batch pays one upload and one readback.
//! Batching is what amortizes that: the executor splits arbitrarily large
//! coord lists into chunks that respect buffer-binding limits and runs
//! each chunk as a single dispatch.

use crate::plan::{Plan, PlanSource};
use schist_core::{TileBuf, TileCoord, TILE_PIXELS};
use wgpu::util::DeviceExt;

/// Per-chunk ceiling on any one storage buffer, and the tile count that
/// keeps the f32 output under it (256 KiB × 4 channels × 4 bytes = 1 MiB
/// per tile).
const BUDGET_BYTES: usize = 256 << 20;
const MAX_CHUNK_TILES: usize = 128;

pub struct GpuContext {
    /// One batch at a time: error scopes are a per-device stack, so
    /// concurrent submissions would pop each other's scopes and attribute
    /// failures to the wrong caller.
    work: parking_lot::Mutex<()>,
    /// The largest storage buffer this device will bind, and what fx jobs
    /// band themselves to fit. Overridable so tests can force the banded
    /// path on hardware roomy enough to skip it.
    binding_limit: std::sync::atomic::AtomicUsize,
    device: wgpu::Device,
    queue: wgpu::Queue,
    composite: wgpu::ComputePipeline,
    pack: wgpu::ComputePipeline,
    viewport: wgpu::ComputePipeline,
    fx_blur: wgpu::ComputePipeline,
    fx_lens: wgpu::ComputePipeline,
    fx_warp: wgpu::ComputePipeline,
    /// Seam carving: six stages over one shared bind group layout, so the
    /// whole run is a single set of buffers and no layout can drift
    /// between entry points.
    carve: CarvePipelines,
    info: wgpu::AdapterInfo,
}

struct CarvePipelines {
    energy: wgpu::ComputePipeline,
    dp_seed: wgpu::ComputePipeline,
    dp_tile: wgpu::ComputePipeline,
    pick: wgpu::ComputePipeline,
    resample: wgpu::ComputePipeline,
    advance_seam: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

/// Mirrors fx_carve.wgsl: rows per scan tile, and the columns a workgroup
/// owns once the ±1 dependency has eaten one column an end per row.
const CARVE_TILE_ROWS: usize = 64;
const CARVE_WG: usize = 256;
const CARVE_PER_THREAD: usize = 1;
const CARVE_SPAN: usize = CARVE_WG * CARVE_PER_THREAD;
const CARVE_TILE_COLS: usize = CARVE_SPAN - 2 * (CARVE_TILE_ROWS - 1);
/// Uniform bindings are addressed at this granularity, which is what
/// `Limits::default` guarantees.
const UNIFORM_ALIGN: usize = 256;
/// Seams per submission. The run never reads back mid-way, so this only
/// bounds how large one command buffer gets.
const CARVE_SEAMS_PER_SUBMIT: usize = 16;

pub enum BatchOut {
    F32(Vec<Vec<f32>>),
    Rgba8(Vec<Vec<u8>>),
}

impl GpuContext {
    pub fn new() -> Result<GpuContext, String> {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut instance_desc = wgpu::InstanceDescriptor::from_env_or_default();
        // FXC, the default DX12 shader compiler, miscompiles the
        // switch-heavy op interpreter (Color Burn came back as noise on
        // WARP); the statically linked DXC does not. Respect an explicit
        // WGPU_DX12_COMPILER override.
        #[cfg(windows)]
        if matches!(
            instance_desc.backend_options.dx12.shader_compiler,
            wgpu::Dx12Compiler::Fxc
        ) {
            instance_desc.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::StaticDxc;
        }
        let instance = wgpu::Instance::new(&instance_desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|e| format!("no wgpu adapter: {e}"))?;
        let adapter_limits = adapter.limits();
        let mut limits = wgpu::Limits::default();
        limits.max_storage_buffer_binding_size = adapter_limits
            .max_storage_buffer_binding_size
            .min(1 << 30)
            .max(limits.max_storage_buffer_binding_size);
        limits.max_buffer_size = adapter_limits
            .max_buffer_size
            .min(1 << 30)
            .max(limits.max_buffer_size);
        // What fx jobs band themselves to: the smaller of what one
        // binding takes and what one buffer may be.
        let binding_limit = (limits.max_storage_buffer_binding_size as u64)
            .min(limits.max_buffer_size)
            .min(usize::MAX as u64) as usize;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("schist-compositor-gpu"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::default(),
        }))
        .map_err(|e| format!("wgpu device: {e}"))?;
        // Validation failures would otherwise panic in a callback thread;
        // log them and let the CPU fallback produce the frame.
        device.on_uncaptured_error(std::sync::Arc::new(|e| {
            log::error!("gpu compositor error: {e}");
        }));

        // Shader translation differs per backend (SPIR-V, MSL, HLSL); a
        // module that validates on one can still fail another's pipeline
        // creation. Catch that here and report "no GPU" so the caller
        // stays on the CPU, instead of dispatching a dead pipeline and
        // reading back zeroes.
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let make_module = |name: &str, source: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(name),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            })
        };
        let composite_module = make_module("composite.wgsl", include_str!("composite.wgsl"));
        let pack_module = make_module("pack.wgsl", include_str!("pack.wgsl"));
        let viewport_module = make_module("viewport.wgsl", include_str!("viewport.wgsl"));
        // One module per kernel: naga's HLSL backend rejects entry points
        // that share a bind group with different layouts, and on DX12 that
        // shows up as a dispatch that silently does nothing.
        let fx_blur_module = make_module("fx_blur.wgsl", include_str!("fx_blur.wgsl"));
        let fx_lens_module = make_module("fx_lens.wgsl", include_str!("fx_lens.wgsl"));
        let fx_warp_module = make_module("fx_warp.wgsl", include_str!("fx_warp.wgsl"));
        let carve_module = make_module("fx_carve.wgsl", include_str!("fx_carve.wgsl"));
        // Explicit, so every carve entry point provably shares one layout:
        // naga's HLSL backend rejects entry points that disagree, and on
        // DX12 that surfaces as a dispatch that silently does nothing.
        let carve_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("carve"),
            entries: &(0..9)
                .map(|binding| wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: if binding == 8 {
                            wgpu::BufferBindingType::Uniform
                        } else {
                            wgpu::BufferBindingType::Storage {
                                // Only the two input planes are read-only.
                                read_only: binding == 1 || binding == 3,
                            }
                        },
                        // The scan's band index rides on a dynamic offset:
                        // one dispatch per band, no counter to bump.
                        has_dynamic_offset: binding == 8,
                        min_binding_size: wgpu::BufferSize::new(16).filter(|_| binding == 8),
                    },
                    count: None,
                })
                .collect::<Vec<_>>(),
        });
        let carve_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("carve"),
                bind_group_layouts: &[&carve_layout],
                push_constant_ranges: &[],
            });
        let carve_stage = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&carve_pipeline_layout),
                module: &carve_module,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let make = |module: &wgpu::ShaderModule, entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: None,
                module,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let ctx = GpuContext {
            work: parking_lot::Mutex::new(()),
            binding_limit: std::sync::atomic::AtomicUsize::new(binding_limit),
            composite: make(&composite_module, "composite"),
            pack: make(&pack_module, "pack_rgba8"),
            viewport: make(&viewport_module, "viewport"),
            fx_blur: make(&fx_blur_module, "box_pass"),
            fx_lens: make(&fx_lens_module, "lens_blur"),
            fx_warp: make(&fx_warp_module, "mesh_warp"),
            carve: CarvePipelines {
                energy: carve_stage("energy_pass"),
                dp_seed: carve_stage("dp_seed"),
                dp_tile: carve_stage("dp_tile"),
                pick: carve_stage("pick"),
                resample: carve_stage("resample"),
                advance_seam: carve_stage("advance_seam"),
                layout: carve_layout,
            },
            info: adapter.get_info(),
            device,
            queue,
        };
        if let Some(err) = pollster::block_on(ctx.device.pop_error_scope()) {
            return Err(format!("pipeline creation: {err}"));
        }
        Ok(ctx)
    }

    /// GPU version of `schist_compositor::viewport::render_viewport_cpu`.
    /// `None` when the grid exceeds buffer budgets (the CPU path takes
    /// over) or a readback fails.
    pub fn render_viewport(
        &self,
        p: &schist_compositor::viewport::ViewportParams,
        grid: &[Option<std::sync::Arc<Vec<u8>>>],
    ) -> Option<Vec<u8>> {
        if !schist_compositor::viewport::grid_len_ok(p, grid) {
            return None;
        }
        let out_bytes = p.width * p.height * 4;
        let present = grid.iter().flatten().count();
        if out_bytes > BUDGET_BYTES || present * TILE_PIXELS * 4 > BUDGET_BYTES {
            return None;
        }
        let _work = self.work.lock();
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut tile_bytes: Vec<u8> = Vec::with_capacity(present * TILE_PIXELS * 4);
        let mut index = Vec::with_capacity(grid.len());
        for slot in grid {
            match slot {
                Some(tile) => {
                    index.push((tile_bytes.len() / (TILE_PIXELS * 4)) as i32);
                    tile_bytes.extend_from_slice(tile);
                }
                None => index.push(-1),
            }
        }
        // sin/cos computed here once so CPU and GPU share exact constants.
        let (rs, rc) = (-p.rotation).sin_cos();
        let uniform: Vec<u32> = vec![
            p.width as u32,
            p.height as u32,
            p.origin.0.to_bits(),
            p.origin.1.to_bits(),
            (p.width as f32 / 2.0).to_bits(),
            (p.height as f32 / 2.0).to_bits(),
            rs.to_bits(),
            rc.to_bits(),
            (1.0 / p.zoom).to_bits(),
            p.scale_factor.to_bits(),
            (1.0 / (p.zoom * p.scale_factor)).to_bits(),
            0,
            p.canvas.left as u32,
            p.canvas.top as u32,
            p.canvas.right as u32,
            p.canvas.bottom as u32,
            p.grid_origin.0 as u32,
            p.grid_origin.1 as u32,
            p.grid_cols as u32,
            p.grid_rows as u32,
            p.surround,
            p.crisp() as u32,
            p.box_taps() as u32,
            0,
        ];
        let uniform_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("viewport-params"),
                contents: cast_u32s(&uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let tiles_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("viewport-tiles"),
                contents: if tile_bytes.is_empty() {
                    &[0; 4]
                } else {
                    &tile_bytes
                },
                usage: wgpu::BufferUsages::STORAGE,
            });
        let index_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("viewport-index"),
                contents: cast_u32s(cast_i32s(&index)),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let out_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewport-out"),
            size: out_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewport-staging"),
            size: out_bytes as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport"),
            layout: &self.viewport.get_bind_group_layout(0),
            entries: &[
                bind_entry(0, &uniform_buf),
                bind_entry(1, &tiles_buf),
                bind_entry(2, &index_buf),
                bind_entry(3, &out_buf),
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("viewport"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.viewport);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(
                (p.width as u32).div_ceil(16),
                (p.height as u32).div_ceil(16),
                1,
            );
        }
        encoder.copy_buffer_to_buffer(&out_buf, 0, &staging, 0, out_bytes as u64);
        self.queue.submit([encoder.finish()]);

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;
        let data = slice.get_mapped_range();
        let out = data.to_vec();
        drop(data);
        staging.unmap();
        if let Some(err) = pollster::block_on(self.device.pop_error_scope()) {
            log::warn!("gpu viewport failed, falling back to the CPU: {err}");
            return None;
        }
        Some(out)
    }

    /// Run a whole content-aware resize without coming back: every stage,
    /// every seam, one readback at the end.
    ///
    /// `None` when a plane is too big for one storage binding — unlike the
    /// blurs there is nothing to band, since each seam depends on the last
    /// over the whole image.
    pub fn run_carve(&self, job: &schist_fx::CarveJob<'_>) -> Option<schist_fx::Carved> {
        let (w0, h) = (job.width, job.height);
        let target = job.target_width.max(1);
        if w0 == 0 || h == 0 || target == w0 {
            return None;
        }
        // Buffers are strided to the widest the image ever gets, so
        // growing has somewhere to put the extra columns.
        let max_w = w0.max(target);
        let plane = max_w.checked_mul(h)?;
        let limit = self.binding_limit();
        if plane.checked_mul(16)? > limit {
            return None;
        }

        let _work = self.work.lock();
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let storage = |label: &str, bytes: u64| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let upload = |label: &str, data: &[f32]| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some(label),
                    contents: crate::fx::cast_f32s(data),
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                })
        };
        let px = [
            upload(
                "carve-px-0",
                &pad_rows(job.px, w0 * 4, max_w * 4, h, plane * 4),
            ),
            storage("carve-px-1", (plane * 16) as u64),
        ];
        let prot = [
            upload("carve-prot-0", &pad_rows(job.protect, w0, max_w, h, plane)),
            storage("carve-prot-1", (plane * 4) as u64),
        ];
        let energy = storage("carve-energy", (plane * 4) as u64);
        let cost = storage("carve-cost", (plane * 4) as u64);
        let from_dir = storage("carve-from", (plane * 4) as u64);
        // The scan's band index, one 256-byte-aligned slot per band.
        let tiles = (h - 1).div_ceil(CARVE_TILE_ROWS);
        let mut bands = vec![0u32; tiles.max(1) * (UNIFORM_ALIGN / 4)];
        for band in 0..tiles {
            bands[band * (UNIFORM_ALIGN / 4)] = (band * CARVE_TILE_ROWS) as u32;
        }
        let band_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("carve-bands"),
                contents: cast_u32s(&bands),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        // Slot 0..8 are the run's counters (see fx_carve.wgsl); the seam's
        // one column per row follows.
        let mut init = vec![0i32; 8 + h];
        init[0] = w0 as i32;
        init[1] = h as i32;
        init[2] = max_w as i32;
        init[3] = i32::from(target > w0);
        let state = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("carve-state"),
                contents: cast_u32s(cast_i32s(&init)),
                usage: wgpu::BufferUsages::STORAGE,
            });

        // Two bind groups, one per ping-pong direction.
        let binds: Vec<wgpu::BindGroup> = (0..2)
            .map(|i| {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("carve"),
                    layout: &self.carve.layout,
                    entries: &[
                        bind_entry(0, &state),
                        bind_entry(1, &px[i]),
                        bind_entry(2, &px[1 - i]),
                        bind_entry(3, &prot[i]),
                        bind_entry(4, &prot[1 - i]),
                        bind_entry(5, &energy),
                        bind_entry(6, &cost),
                        bind_entry(7, &from_dir),
                        wgpu::BindGroupEntry {
                            binding: 8,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &band_buf,
                                offset: 0,
                                size: wgpu::BufferSize::new(16),
                            }),
                        },
                    ],
                })
            })
            .collect();

        let seams = w0.abs_diff(target);
        let grid = (
            (max_w as u32).div_ceil(16),
            (h as u32).div_ceil(16),
            (max_w as u32).div_ceil(CARVE_WG as u32),
            (max_w as u32).div_ceil(CARVE_TILE_COLS as u32),
        );
        for chunk in 0..seams.div_ceil(CARVE_SEAMS_PER_SUBMIT) {
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("carve"),
                    timestamp_writes: None,
                });
                let first = chunk * CARVE_SEAMS_PER_SUBMIT;
                for seam in first..(first + CARVE_SEAMS_PER_SUBMIT).min(seams) {
                    let bind = &binds[seam % 2];
                    pass.set_bind_group(0, bind, &[0]);
                    pass.set_pipeline(&self.carve.energy);
                    pass.dispatch_workgroups(grid.0, grid.1, 1);
                    pass.set_pipeline(&self.carve.dp_seed);
                    pass.dispatch_workgroups(grid.2, 1, 1);
                    // The scan seeds row 0 outright, so the bands cover
                    // what is left of the image.
                    pass.set_pipeline(&self.carve.dp_tile);
                    for band in 0..tiles {
                        pass.set_bind_group(0, bind, &[(band * UNIFORM_ALIGN) as u32]);
                        pass.dispatch_workgroups(grid.3, 1, 1);
                    }
                    pass.set_bind_group(0, bind, &[0]);
                    pass.set_pipeline(&self.carve.pick);
                    pass.dispatch_workgroups(1, 1, 1);
                    pass.set_pipeline(&self.carve.resample);
                    pass.dispatch_workgroups(grid.0, grid.1, 1);
                    pass.set_pipeline(&self.carve.advance_seam);
                    pass.dispatch_workgroups(1, 1, 1);
                }
            }
            self.queue.submit([encoder.finish()]);
        }

        // Each seam writes to the other plane, so an odd count finishes in
        // the second one.
        let done = seams % 2;
        let px_out = self.read_back(&px[done], (plane * 16) as u64)?;
        let prot_out = self.read_back(&prot[done], (plane * 4) as u64)?;
        if let Some(err) = pollster::block_on(self.device.pop_error_scope()) {
            log::warn!("gpu carve failed, falling back to the CPU: {err}");
            return None;
        }
        Some(schist_fx::Carved {
            px: unpad_rows(&px_out, max_w * 4, target * 4, h),
            protect: unpad_rows(&prot_out, max_w, target, h),
            width: target,
        })
    }

    /// Copy a storage buffer out through a staging buffer.
    fn read_back(&self, buffer: &wgpu::Buffer, bytes: u64) -> Option<Vec<f32>> {
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("carve-staging"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, bytes);
        self.queue.submit([encoder.finish()]);
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;
        let data = slice.get_mapped_range();
        let out: Vec<f32> = data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        drop(data);
        staging.unmap();
        Some(out)
    }

    /// The largest storage buffer this device will bind.
    pub fn binding_limit(&self) -> usize {
        self.binding_limit
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Pretend the device binds no more than `bytes` at once, so a test or
    /// a bench can exercise the banded path anywhere.
    pub fn set_binding_limit(&self, bytes: usize) {
        self.binding_limit
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many rows of `width` fit in one binding, and the overlap a band
    /// needs for its kept rows to come out identical to the whole-image
    /// result.
    ///
    /// A vertical pass spreads information by `radius` rows, so after
    /// `passes` of them a row is correct as long as `passes * radius` real
    /// rows sit either side of it. Horizontal passes move nothing
    /// vertically, and a band that reaches the top or bottom of the image
    /// clamps against the real edge, which is what the reference does too.
    fn band_plan(&self, width: usize, height: usize, halo: usize) -> Option<(usize, usize)> {
        let row_bytes = width.checked_mul(16)?;
        if row_bytes == 0 {
            return None;
        }
        let rows = self.binding_limit() / row_bytes;
        let needed = halo.checked_mul(2)?.checked_add(1)?;
        if rows < needed {
            return None; // not even one useful band fits
        }
        Some(((rows - halo * 2).min(height).max(1), halo))
    }

    /// Run `schist_fx`'s separable box blur: one upload, `2 × passes`
    /// dispatches ping-ponging between two buffers, one readback. The
    /// premultiply and unpremultiply the reference does as separate sweeps
    /// fold into the first read and the last write.
    ///
    /// A plane too big for one binding is split into horizontal bands with
    /// an overlap wide enough that the rows each band keeps are the ones
    /// the whole-image pass would have produced.
    pub fn run_blur(&self, job: &schist_fx::BlurJob<'_>) -> Option<Vec<f32>> {
        let halo = job.passes.checked_mul(job.radius)?;
        let (band_rows, halo) = self.band_plan(job.width, job.height, halo)?;
        if band_rows >= job.height {
            return self.blur_plane(job.px, job.width, job.height, job.radius, job.passes);
        }
        let mut out = vec![0.0f32; job.px.len()];
        let mut top = 0usize;
        while top < job.height {
            let bottom = (top + band_rows).min(job.height);
            let a = top.saturating_sub(halo);
            let b = (bottom + halo).min(job.height);
            let banded = self.blur_plane(
                &job.px[a * job.width * 4..b * job.width * 4],
                job.width,
                b - a,
                job.radius,
                job.passes,
            )?;
            let keep = (top - a) * job.width * 4..(bottom - a) * job.width * 4;
            out[top * job.width * 4..bottom * job.width * 4].copy_from_slice(&banded[keep]);
            top = bottom;
        }
        Some(out)
    }

    fn blur_plane(
        &self,
        px: &[f32],
        width: usize,
        height: usize,
        radius: usize,
        passes: usize,
    ) -> Option<Vec<f32>> {
        let _work = self.work.lock();
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let bytes = std::mem::size_of_val(px) as u64;
        let front = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("fx-blur-a"),
                contents: crate::fx::cast_f32s(px),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            });
        let back = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fx-blur-b"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        // One uniform per dispatch: the axis flips each step, and the
        // premultiply/unpremultiply flags only fire on the first and last.
        let steps = passes * 2;
        let params: Vec<wgpu::Buffer> = (0..steps)
            .map(|step| {
                let vertical = step % 2 == 1;
                let mut flags = 0u32;
                if step == 0 {
                    flags |= 1; // premultiply on read
                }
                if step == steps - 1 {
                    flags |= 2; // unpremultiply on write
                }
                self.device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("fx-blur-params"),
                        contents: cast_u32s(&[
                            width as u32,
                            height as u32,
                            radius as u32,
                            flags,
                            vertical as u32,
                            0,
                            0,
                            0,
                        ]),
                        usage: wgpu::BufferUsages::UNIFORM,
                    })
            })
            .collect();
        let binds: Vec<wgpu::BindGroup> = params
            .iter()
            .enumerate()
            .map(|(step, params)| {
                let (src, dst) = if step % 2 == 1 {
                    (&back, &front)
                } else {
                    (&front, &back)
                };
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("fx-blur"),
                    layout: &self.fx_blur.get_bind_group_layout(0),
                    entries: &[
                        bind_entry(0, params),
                        bind_entry(1, src),
                        bind_entry(2, dst),
                    ],
                })
            })
            .collect();
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("fx-blur"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.fx_blur);
            for bind in &binds {
                pass.set_bind_group(0, bind, &[]);
                pass.dispatch_workgroups(
                    (width as u32).div_ceil(16),
                    (height as u32).div_ceil(16),
                    1,
                );
            }
        }
        self.finish_fx(encoder, &front, bytes, "blur")
    }

    /// Lens blur, banded on the same rule as [`run_blur`](Self::run_blur):
    /// one dispatch reaches `radius` rows, so that is the overlap.
    pub fn run_lens_blur(&self, job: &schist_fx::LensJob<'_>) -> Option<Vec<f32>> {
        let halo = job.radius.max(0) as usize;
        let (band_rows, halo) = self.band_plan(job.width, job.height, halo)?;
        if band_rows >= job.height {
            return self.lens_plane(job.px, job.width, job.height, job.radius, job.boost);
        }
        let mut out = vec![0.0f32; job.px.len()];
        let mut top = 0usize;
        while top < job.height {
            let bottom = (top + band_rows).min(job.height);
            let a = top.saturating_sub(halo);
            let b = (bottom + halo).min(job.height);
            let banded = self.lens_plane(
                &job.px[a * job.width * 4..b * job.width * 4],
                job.width,
                b - a,
                job.radius,
                job.boost,
            )?;
            let keep = (top - a) * job.width * 4..(bottom - a) * job.width * 4;
            out[top * job.width * 4..bottom * job.width * 4].copy_from_slice(&banded[keep]);
            top = bottom;
        }
        Some(out)
    }

    fn lens_plane(
        &self,
        px: &[f32],
        width: usize,
        height: usize,
        radius: i32,
        boost: f32,
    ) -> Option<Vec<f32>> {
        let _work = self.work.lock();
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let bytes = std::mem::size_of_val(px) as u64;
        let src = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("fx-lens-src"),
                contents: crate::fx::cast_f32s(px),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let dst = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fx-lens-dst"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("fx-lens-params"),
                contents: cast_u32s(&[width as u32, height as u32, radius as u32, boost.to_bits()]),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fx-lens"),
            layout: &self.fx_lens.get_bind_group_layout(0),
            entries: &[
                bind_entry(0, &params),
                bind_entry(1, &src),
                bind_entry(2, &dst),
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("fx-lens"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.fx_lens);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups((width as u32).div_ceil(16), (height as u32).div_ceil(16), 1);
        }
        self.finish_fx(encoder, &dst, bytes, "lens blur")
    }

    /// Upload a warp source plane. Callers hold the buffer for as long as
    /// the pixels behind it are unchanged — a whole Liquify drag — so the
    /// per-move cost is one dispatch and one readback.
    pub fn upload_warp_source(&self, src: &[f32]) -> Option<wgpu::Buffer> {
        let contents = crate::fx::cast_f32s(src);
        if contents.is_empty() {
            return None;
        }
        Some(
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("fx-warp-source"),
                    contents,
                    usage: wgpu::BufferUsages::STORAGE,
                }),
        )
    }

    /// Warp through `src`, which the caller keeps resident across a drag.
    pub fn run_warp(
        &self,
        job: &schist_fx::WarpParams<'_>,
        src: &wgpu::Buffer,
    ) -> Option<Vec<f32>> {
        let _work = self.work.lock();
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let bytes = (job.dst_width * job.dst_height * 16) as u64;
        let dst = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fx-warp-dst"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("fx-warp-params"),
                contents: cast_u32s(&[
                    job.src_width as u32,
                    job.src_height as u32,
                    job.src_origin.0 as u32,
                    job.src_origin.1 as u32,
                    job.dst_width as u32,
                    job.dst_height as u32,
                    job.dst_origin.0 as u32,
                    job.dst_origin.1 as u32,
                    job.mesh_cols as u32,
                    job.mesh_rows as u32,
                    job.mesh_origin.0 as u32,
                    job.mesh_origin.1 as u32,
                    job.cell.to_bits(),
                    0,
                    0,
                    0,
                ]),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let mesh = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("fx-warp-mesh"),
                contents: if job.mesh.is_empty() {
                    &[0; 8]
                } else {
                    crate::fx::cast_f32s(job.mesh)
                },
                usage: wgpu::BufferUsages::STORAGE,
            });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fx-warp"),
            layout: &self.fx_warp.get_bind_group_layout(0),
            entries: &[
                bind_entry(0, &params),
                bind_entry(1, src),
                bind_entry(2, &dst),
                bind_entry(3, &mesh),
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("fx-warp"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.fx_warp);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(
                (job.dst_width as u32).div_ceil(16),
                (job.dst_height as u32).div_ceil(16),
                1,
            );
        }
        self.finish_fx(encoder, &dst, bytes, "warp")
    }

    /// Submit, read `bytes` back out of `out`, and turn any validation
    /// failure into `None` so the caller runs the CPU reference.
    fn finish_fx(
        &self,
        mut encoder: wgpu::CommandEncoder,
        out: &wgpu::Buffer,
        bytes: u64,
        what: &str,
    ) -> Option<Vec<f32>> {
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fx-staging"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_buffer_to_buffer(out, 0, &staging, 0, bytes);
        self.queue.submit([encoder.finish()]);
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;
        let data = slice.get_mapped_range();
        let floats: Vec<f32> = data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        drop(data);
        staging.unmap();
        if let Some(err) = pollster::block_on(self.device.pop_error_scope()) {
            log::warn!("gpu {what} failed, falling back to the CPU: {err}");
            return None;
        }
        Some(floats)
    }

    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.info
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Run `plan` over `coords`, splitting into budget-sized chunks.
    /// `None` means the GPU could not run this batch (a single tile's
    /// sources exceed the buffer budget, or a readback failed); the caller
    /// falls back to the CPU reference.
    pub fn composite_batch(
        &self,
        plan: &Plan<'_>,
        coords: &[TileCoord],
        rgba8: bool,
    ) -> Option<BatchOut> {
        let mut f32_out: Vec<Vec<f32>> = Vec::new();
        let mut u8_out: Vec<Vec<u8>> = Vec::new();
        let mut start = 0;
        while start < coords.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < coords.len() && end - start < MAX_CHUNK_TILES {
                let cost = self.tile_cost(plan, coords[end]);
                if bytes + cost > BUDGET_BYTES && end > start {
                    break;
                }
                if cost > BUDGET_BYTES {
                    return None; // one tile alone blows the budget
                }
                bytes += cost;
                end += 1;
            }
            match self.run_chunk(plan, &coords[start..end], rgba8)? {
                BatchOut::F32(mut v) => f32_out.append(&mut v),
                BatchOut::Rgba8(mut v) => u8_out.append(&mut v),
            }
            start = end;
        }
        Some(if rgba8 {
            BatchOut::Rgba8(u8_out)
        } else {
            BatchOut::F32(f32_out)
        })
    }

    /// Upper-bound upload bytes one tile contributes (worst-case f32).
    fn tile_cost(&self, plan: &Plan<'_>, coord: TileCoord) -> usize {
        let mut bytes = 0;
        for src in &plan.sources {
            match src {
                PlanSource::Pixels(map) => {
                    if map.get(coord).is_some() {
                        bytes += TILE_PIXELS * 16;
                    }
                }
                PlanSource::Mask(map) => {
                    if map.get(coord).is_some() {
                        bytes += TILE_PIXELS;
                    }
                }
            }
        }
        bytes
    }

    fn run_chunk(&self, plan: &Plan<'_>, coords: &[TileCoord], rgba8: bool) -> Option<BatchOut> {
        let _work = self.work.lock();
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let n_tiles = coords.len();
        let n_rows = plan.sources.len();
        let mut slots = vec![-1i32; n_rows.max(1) * n_tiles];
        let mut fmts = vec![0u32; n_rows];
        let mut src_words: Vec<u32> = Vec::new();
        let mut mask_words: Vec<u32> = Vec::new();

        // Each pixel row uploads at one format: the widest depth present
        // in this chunk (narrower tiles convert losslessly on the way in).
        for (r, src) in plan.sources.iter().enumerate() {
            if let PlanSource::Pixels(map) = src {
                let mut fmt = 0u32;
                for c in coords {
                    if let Some(buf) = map.get(*c) {
                        fmt = fmt.max(match buf.as_ref() {
                            TileBuf::U8(_) => 0,
                            TileBuf::U16(_) => 1,
                            TileBuf::F32(_) => 2,
                        });
                    }
                }
                fmts[r] = fmt;
            }
        }
        for (r, src) in plan.sources.iter().enumerate() {
            match src {
                PlanSource::Pixels(map) => {
                    for (t, c) in coords.iter().enumerate() {
                        if let Some(buf) = map.get(*c) {
                            slots[r * n_tiles + t] = src_words.len() as i32;
                            pack_pixels(&mut src_words, buf, fmts[r]);
                        }
                    }
                }
                PlanSource::Mask(map) => {
                    for (t, c) in coords.iter().enumerate() {
                        if let Some(buf) = map.get(*c) {
                            slots[r * n_tiles + t] = mask_words.len() as i32;
                            pack_mask(&mut mask_words, buf.as_ref());
                        }
                    }
                }
            }
        }

        // Serialize ops: 20 words each, matching the WGSL struct layout.
        let mut op_words: Vec<u32> = Vec::with_capacity(plan.ops.len() * 20);
        for op in &plan.ops {
            op_words.extend_from_slice(&[
                op.kind,
                op.mode,
                op.opacity.to_bits(),
                op.flags,
                op.src_ref as u32,
                if op.src_ref >= 0 {
                    fmts[op.src_ref as usize]
                } else {
                    0
                },
                op.mask.row as u32,
                op.lut as u32,
                op.mask.bounds[0] as u32,
                op.mask.bounds[1] as u32,
                op.mask.bounds[2] as u32,
                op.mask.bounds[3] as u32,
                op.fill[0].to_bits(),
                op.fill[1].to_bits(),
                op.fill[2].to_bits(),
                op.fill[3].to_bits(),
                op.mask.default_value.to_bits(),
                op.direct,
                op.dparams as u32,
                0,
            ]);
        }
        let mut origins: Vec<i32> = Vec::with_capacity(n_tiles * 2);
        for c in coords {
            let r = c.rect();
            origins.push(r.left);
            origins.push(r.top);
        }

        let storage = |label: &str, words: &[u32]| {
            // Empty bindings are invalid; a dummy word keeps layouts happy.
            let data: &[u32] = if words.is_empty() { &[0] } else { words };
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some(label),
                    contents: cast_u32s(data),
                    usage: wgpu::BufferUsages::STORAGE,
                })
        };
        let globals = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("globals"),
                contents: cast_u32s(&[plan.ops.len() as u32, n_tiles as u32, 0, 0]),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let ops_buf = storage("ops", &op_words);
        let origin_buf = storage("tile-origins", cast_i32s(&origins));
        let src_buf = storage("sources", &src_words);
        let mask_buf = storage("masks", &mask_words);
        let slots_buf = storage("slots", cast_i32s(&slots));
        let luts_buf = storage(
            "luts",
            &plan.luts.iter().map(|f| f.to_bits()).collect::<Vec<u32>>(),
        );
        let directs_buf = storage(
            "direct-params",
            &plan
                .directs
                .iter()
                .map(|f| f.to_bits())
                .collect::<Vec<u32>>(),
        );
        let out_f32 = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("out-f32"),
            size: (n_tiles * TILE_PIXELS * 16) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite"),
            layout: &self.composite.get_bind_group_layout(0),
            entries: &[
                bind_entry(0, &globals),
                bind_entry(1, &ops_buf),
                bind_entry(2, &origin_buf),
                bind_entry(3, &src_buf),
                bind_entry(4, &mask_buf),
                bind_entry(5, &slots_buf),
                bind_entry(6, &luts_buf),
                bind_entry(7, &out_f32),
                bind_entry(8, &directs_buf),
            ],
        });

        let out_bytes = if rgba8 {
            n_tiles * TILE_PIXELS * 4
        } else {
            n_tiles * TILE_PIXELS * 16
        };
        let packed = if rgba8 {
            Some((
                self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("out-rgba8"),
                    size: out_bytes as u64,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                self.device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("pack-globals"),
                        contents: cast_u32s(&[(n_tiles * TILE_PIXELS) as u32, 0, 0, 0]),
                        usage: wgpu::BufferUsages::UNIFORM,
                    }),
            ))
            .map(|(out_u8, pack_globals)| {
                let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("pack"),
                    layout: &self.pack.get_bind_group_layout(0),
                    entries: &[
                        bind_entry(0, &pack_globals),
                        bind_entry(1, &out_f32),
                        bind_entry(2, &out_u8),
                    ],
                });
                (out_u8, pack_globals, bind)
            })
        } else {
            None
        };
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging"),
            size: out_bytes as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("composite"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.composite);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(16, 16, n_tiles as u32);
            if let Some((_, _, pack_bind)) = &packed {
                pass.set_pipeline(&self.pack);
                pass.set_bind_group(0, pack_bind, &[]);
                pass.dispatch_workgroups((n_tiles * TILE_PIXELS / 256) as u32, 1, 1);
            }
        }
        let copy_src = packed.as_ref().map(|(b, _, _)| b).unwrap_or(&out_f32);
        encoder.copy_buffer_to_buffer(copy_src, 0, &staging, 0, out_bytes as u64);
        self.queue.submit([encoder.finish()]);

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;
        let data = slice.get_mapped_range();
        let out = if rgba8 {
            BatchOut::Rgba8(
                (0..n_tiles)
                    .map(|t| data[t * TILE_PIXELS * 4..(t + 1) * TILE_PIXELS * 4].to_vec())
                    .collect(),
            )
        } else {
            BatchOut::F32(
                (0..n_tiles)
                    .map(|t| {
                        data[t * TILE_PIXELS * 16..(t + 1) * TILE_PIXELS * 16]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|b| f32::from_le_bytes(*b))
                            .collect()
                    })
                    .collect(),
            )
        };
        drop(data);
        staging.unmap();
        if let Some(err) = pollster::block_on(self.device.pop_error_scope()) {
            log::warn!("gpu composite failed, falling back to the CPU: {err}");
            return None;
        }
        Some(out)
    }
}

fn bind_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn cast_u32s(words: &[u32]) -> &[u8] {
    // u32 → u8 view; alignment only shrinks, so this cannot fail.
    unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4) }
}

fn cast_i32s(words: &[i32]) -> &[u32] {
    unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u32, words.len()) }
}

/// Append one tile's pixels at the row format (lossless widening only).
fn pack_pixels(out: &mut Vec<u32>, buf: &TileBuf, fmt: u32) {
    match (buf, fmt) {
        (TileBuf::U8(d), 0) => {
            out.extend(d.as_chunks::<4>().0.iter().map(|p| {
                p[0] as u32 | (p[1] as u32) << 8 | (p[2] as u32) << 16 | (p[3] as u32) << 24
            }));
        }
        (TileBuf::U8(d), 1) => {
            // v/255 == (v*257)/65535, exactly.
            out.extend(
                d.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| (p[0] as u32 * 257) | (p[1] as u32 * 257) << 16),
            );
        }
        (TileBuf::U8(d), _) => {
            out.extend(d.iter().map(|&v| (v as f32 / 255.0).to_bits()));
        }
        (TileBuf::U16(d), 1) => {
            out.extend(
                d.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| p[0] as u32 | (p[1] as u32) << 16),
            );
        }
        (TileBuf::U16(d), _) => {
            out.extend(d.iter().map(|&v| (v as f32 / 65535.0).to_bits()));
        }
        (TileBuf::F32(d), _) => {
            out.extend(d.iter().map(|v| v.to_bits()));
        }
    }
}

fn pack_mask(out: &mut Vec<u32>, buf: &[u8; TILE_PIXELS]) {
    out.extend(
        buf.as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[0] as u32 | (p[1] as u32) << 8 | (p[2] as u32) << 16 | (p[3] as u32) << 24),
    );
}

/// Copy `rows` rows of `src_row` values into a `dst_row`-strided plane of
/// `total` values. A carve never widens, so this is a plain clone there;
/// growing needs the room on the right.
fn pad_rows(src: &[f32], src_row: usize, dst_row: usize, rows: usize, total: usize) -> Vec<f32> {
    if src_row == dst_row {
        return src.to_vec();
    }
    let mut out = vec![0.0f32; total];
    for y in 0..rows {
        out[y * dst_row..y * dst_row + src_row]
            .copy_from_slice(&src[y * src_row..(y + 1) * src_row]);
    }
    out
}

/// The inverse: take the left `dst_row` values of each strided row.
fn unpad_rows(src: &[f32], src_row: usize, dst_row: usize, rows: usize) -> Vec<f32> {
    if src_row == dst_row {
        return src.to_vec();
    }
    let mut out = Vec::with_capacity(dst_row * rows);
    for y in 0..rows {
        out.extend_from_slice(&src[y * src_row..y * src_row + dst_row]);
    }
    out
}
