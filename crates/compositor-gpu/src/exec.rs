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
    device: wgpu::Device,
    queue: wgpu::Queue,
    composite: wgpu::ComputePipeline,
    pack: wgpu::ComputePipeline,
    viewport: wgpu::ComputePipeline,
    info: wgpu::AdapterInfo,
}

pub enum BatchOut {
    F32(Vec<Vec<f32>>),
    Rgba8(Vec<Vec<u8>>),
}

impl GpuContext {
    pub fn new() -> Result<GpuContext, String> {
        let instance = wgpu::Instance::default();
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

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composite.wgsl").into()),
        });
        let viewport_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("viewport.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("viewport.wgsl").into()),
        });
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
        Ok(GpuContext {
            composite: make(&module, "composite"),
            pack: make(&module, "pack_rgba8"),
            viewport: make(&viewport_module, "viewport"),
            info: adapter.get_info(),
            device,
            queue,
        })
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
        Some(out)
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
                0,
                0,
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
                data.chunks_exact(TILE_PIXELS * 4)
                    .map(|c| c.to_vec())
                    .collect(),
            )
        } else {
            BatchOut::F32(
                data.chunks_exact(TILE_PIXELS * 16)
                    .map(|c| {
                        c.chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            .collect()
                    })
                    .collect(),
            )
        };
        drop(data);
        staging.unmap();
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
            out.extend(d.chunks_exact(4).map(|p| {
                p[0] as u32 | (p[1] as u32) << 8 | (p[2] as u32) << 16 | (p[3] as u32) << 24
            }));
        }
        (TileBuf::U8(d), 1) => {
            // v/255 == (v*257)/65535, exactly.
            out.extend(
                d.chunks_exact(2)
                    .map(|p| (p[0] as u32 * 257) | (p[1] as u32 * 257) << 16),
            );
        }
        (TileBuf::U8(d), _) => {
            out.extend(d.iter().map(|&v| (v as f32 / 255.0).to_bits()));
        }
        (TileBuf::U16(d), 1) => {
            out.extend(d.chunks_exact(2).map(|p| p[0] as u32 | (p[1] as u32) << 16));
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
        buf.chunks_exact(4)
            .map(|p| p[0] as u32 | (p[1] as u32) << 8 | (p[2] as u32) << 16 | (p[3] as u32) << 24),
    );
}
