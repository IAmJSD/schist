//! GPU/CPU parity: the wgpu compositor must match the CPU reference
//! tile-for-tile. Every test builds a document, checks the plan compiles
//! (so a silent CPU fallback can't fake a pass), and compares both
//! backends within ±1 RGBA8 step.

use schist_adjustments::{Curve, Curves, Levels, LevelsChannel, Params};
use schist_compositor::{composite_region_rgba8_cpu, Compositor, CpuCompositor};
use schist_compositor_gpu::{plan, GpuCompositor};
use schist_core::color::{Depth, Rgba};
use schist_core::{
    AdjustmentData, AdjustmentKind, BlendMode, Document, IntRect, Layer, LayerKind, LayerMask,
    TileCoord, TILE_SIZE,
};
use std::sync::{Arc, OnceLock};

fn gpu() -> Option<&'static GpuCompositor> {
    static GPU: OnceLock<Option<GpuCompositor>> = OnceLock::new();
    GPU.get_or_init(|| match GpuCompositor::new() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipping GPU parity tests: {e}");
            None
        }
    })
    .as_ref()
}

/// Small deterministic RNG so tests exercise varied pixels without a dep.
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
        // Mix in exact edge values: blend-mode formulas branch at 0, 0.5, 1.
        match self.next() % 10 {
            0 => 0.0,
            1 => 1.0,
            2 => 0.5,
            _ => (self.next() % 1000) as f32 / 999.0,
        }
    }
}

fn random_layer(name: &str, rect: IntRect, depth: Depth, seed: u64) -> Layer {
    let mut layer = Layer::new_raster(name);
    let tiles = &mut layer.as_raster_mut().unwrap().tiles;
    let mut rng = Lcg(seed);
    for y in rect.top..rect.bottom {
        for x in rect.left..rect.right {
            let coord = TileCoord::containing(x, y);
            let buf = tiles.get_mut_or_insert(coord, depth);
            let ix = (y.rem_euclid(TILE_SIZE) * TILE_SIZE + x.rem_euclid(TILE_SIZE)) as usize;
            buf.set(
                ix,
                Rgba::new(rng.unit(), rng.unit(), rng.unit(), rng.unit()),
            );
        }
    }
    layer
}

fn random_mask(bounds: IntRect, default_value: u8, seed: u64) -> LayerMask {
    let mut mask = LayerMask::new_revealing();
    mask.default_value = default_value;
    mask.bounds = bounds;
    let mut rng = Lcg(seed);
    for y in bounds.top..bounds.bottom {
        for x in bounds.left..bounds.right {
            let coord = TileCoord::containing(x, y);
            let buf = mask.tiles.get_mut_or_insert(coord);
            let ix = (y.rem_euclid(TILE_SIZE) * TILE_SIZE + x.rem_euclid(TILE_SIZE)) as usize;
            buf[ix] = (rng.next() % 256) as u8;
        }
    }
    mask
}

fn adjustment(kind: AdjustmentKind, params: Params) -> Layer {
    let mut layer = Layer::new_raster("adj");
    layer.kind = LayerKind::Adjustment(AdjustmentData {
        kind,
        raw: Vec::new(),
        params_json: Some(serde_json::to_string(&params).unwrap()),
    });
    layer
}

/// Compare both backends over the whole canvas. `require_gpu` asserts the
/// plan actually compiled (i.e. the GPU path ran, not its CPU fallback).
///
/// The bar is ±1 RGBA8 step, with a small slack for boundary pixels:
/// several blend functions are discontinuous (Color Burn at b=1, Color
/// Dodge at b=0, Hard Mix at b+s=1, the luminance-tie modes…), and a
/// driver's legal fma contraction can shift a computed backdrop by one
/// ULP across such a cliff — a rounding difference the discontinuity
/// then amplifies arbitrarily. That is inherent to comparing two IEEE
/// pipelines, confined to measure-zero boundary pixels (observed: 3
/// channels in 360k on a Metal runner), so a handful of outliers must
/// not fail the suite.
fn assert_parity(doc: &Document, require_gpu: bool, allowed_bad: usize, ctx: &str) {
    let Some(gpu) = gpu() else { return };
    if require_gpu {
        assert!(
            plan::build(doc).is_ok(),
            "{ctx}: expected the GPU plan to compile"
        );
    }
    let coords: Vec<TileCoord> = TileCoord::covering(&doc.canvas_rect()).collect();
    let gpu_tiles = gpu.tiles_rgba8(doc, &coords);
    let cpu_tiles = CpuCompositor.tiles_rgba8(doc, &coords);
    let total: usize = gpu_tiles.iter().map(|t| t.len()).sum();
    let mut bad = 0usize;
    let mut worst = 0i32;
    for (g, c) in gpu_tiles.iter().zip(&cpu_tiles) {
        for (a, b) in g.iter().zip(c.iter()) {
            let d = (*a as i32 - *b as i32).abs();
            worst = worst.max(d);
            if d > 1 {
                bad += 1;
            }
        }
    }
    // 0.01% for discontinuity boundary hits, on top of any per-test
    // allowance (Dissolve's threshold flips need more).
    let allowed = allowed_bad + (total / 10_000).max(8);
    assert!(
        bad <= allowed,
        "{ctx}: {bad} channels off by >1 (worst {worst}, allowed {allowed})"
    );
}

fn base_doc(depth: Depth) -> Document {
    let mut doc = Document::new("t", 300, 300, Depth::Eight);
    doc.push_layer(random_layer(
        "bg",
        IntRect::from_xywh(0, 0, 300, 300),
        depth,
        7,
    ));
    doc
}

#[test]
fn every_blend_mode_matches() {
    for &mode in BlendMode::layer_modes() {
        let mut doc = base_doc(Depth::Eight);
        let mut top = random_layer(
            "top",
            IntRect::from_xywh(20, 10, 250, 260),
            Depth::Eight,
            99,
        );
        top.blend = mode;
        top.opacity = 0.8;
        doc.push_layer(top);
        // Dissolve compares alpha against a hash per pixel, so a one-ULP
        // alpha difference flips whole pixels rather than boundary cases.
        let allowed = if mode == BlendMode::Dissolve { 600 } else { 0 };
        assert_parity(&doc, true, allowed, mode.display_name());
    }
}

#[test]
fn opacity_fill_opacity_and_partial_coverage() {
    let mut doc = base_doc(Depth::Eight);
    let mut top = random_layer("a", IntRect::from_xywh(-30, -30, 100, 400), Depth::Eight, 3);
    top.opacity = 0.35;
    top.fill_opacity = 0.7;
    top.blend = BlendMode::Multiply;
    doc.push_layer(top);
    // A layer sparse in some visible tiles.
    doc.push_layer(random_layer(
        "b",
        IntRect::from_xywh(260, 260, 39, 39),
        Depth::Eight,
        4,
    ));
    assert_parity(&doc, true, 0, "opacity/fill/partial");
}

#[test]
fn masks_bounded_and_defaults() {
    for default_value in [0u8, 255] {
        let mut doc = base_doc(Depth::Eight);
        let mut top = random_layer("m", IntRect::from_xywh(0, 0, 300, 300), Depth::Eight, 11);
        top.mask = Some(random_mask(
            IntRect::from_xywh(40, 60, 150, 120),
            default_value,
            12,
        ));
        top.blend = BlendMode::Screen;
        doc.push_layer(top);
        assert_parity(&doc, true, 0, &format!("mask default {default_value}"));
    }
}

#[test]
fn groups_isolated_pass_through_masked_nested() {
    let mut doc = base_doc(Depth::Eight);

    let mut inner = Layer::new_group("inner");
    inner.blend = BlendMode::Overlay;
    inner.opacity = 0.6;
    if let LayerKind::Group(g) = &mut inner.kind {
        g.children.push(random_layer(
            "i1",
            IntRect::from_xywh(10, 10, 200, 200),
            Depth::Eight,
            21,
        ));
        let mut clipped = random_layer("i2", IntRect::from_xywh(0, 0, 300, 300), Depth::Eight, 22);
        clipped.clipping = true;
        clipped.blend = BlendMode::ColorDodge;
        g.children.push(clipped);
    }

    let mut outer = Layer::new_group("outer");
    outer.blend = BlendMode::PassThrough; // with a mask → isolates anyway
    outer.mask = Some(random_mask(IntRect::from_xywh(0, 0, 300, 160), 255, 23));
    if let LayerKind::Group(g) = &mut outer.kind {
        g.children.push(random_layer(
            "o1",
            IntRect::from_xywh(50, 50, 220, 220),
            Depth::Eight,
            24,
        ));
        g.children.push(inner);
    }
    doc.push_layer(outer);

    let mut pass = Layer::new_group("pass");
    pass.blend = BlendMode::PassThrough;
    if let LayerKind::Group(g) = &mut pass.kind {
        let mut child = random_layer("p1", IntRect::from_xywh(100, 0, 150, 300), Depth::Eight, 25);
        child.blend = BlendMode::Difference;
        child.opacity = 0.5;
        g.children.push(child);
    }
    doc.push_layer(pass);

    assert_parity(&doc, true, 0, "groups");
}

#[test]
fn clip_runs_with_adjustments_and_group_base() {
    let mut doc = base_doc(Depth::Eight);

    // Raster base with clipped raster + clipped adjustment.
    doc.push_layer(random_layer(
        "base",
        IntRect::from_xywh(30, 30, 140, 140),
        Depth::Eight,
        31,
    ));
    let mut c1 = random_layer("c1", IntRect::from_xywh(0, 0, 300, 300), Depth::Eight, 32);
    c1.clipping = true;
    c1.blend = BlendMode::HardLight;
    c1.opacity = 0.7;
    doc.push_layer(c1);
    let mut c2 = adjustment(AdjustmentKind::Invert, Params::Invert);
    c2.clipping = true;
    c2.opacity = 0.5;
    doc.push_layer(c2);

    // Group base (mask double-applies there, like the CPU) with a clipped
    // group on top.
    let mut gbase = Layer::new_group("gbase");
    gbase.blend = BlendMode::Normal;
    gbase.mask = Some(random_mask(IntRect::from_xywh(150, 150, 140, 140), 0, 33));
    if let LayerKind::Group(g) = &mut gbase.kind {
        g.children.push(random_layer(
            "gb1",
            IntRect::from_xywh(140, 140, 160, 160),
            Depth::Eight,
            34,
        ));
    }
    doc.push_layer(gbase);
    let mut cgroup = Layer::new_group("cgroup");
    cgroup.clipping = true;
    cgroup.blend = BlendMode::Screen;
    if let LayerKind::Group(g) = &mut cgroup.kind {
        g.children.push(random_layer(
            "cg1",
            IntRect::from_xywh(0, 0, 300, 300),
            Depth::Eight,
            35,
        ));
    }
    doc.push_layer(cgroup);

    assert_parity(&doc, true, 0, "clip runs");
}

#[test]
fn lut_adjustments_and_fills() {
    let mut doc = base_doc(Depth::Eight);
    doc.push_layer(adjustment(
        AdjustmentKind::BrightnessContrast,
        Params::BrightnessContrast {
            brightness: 30.0,
            contrast: -20.0,
        },
    ));
    let mut levels = adjustment(
        AdjustmentKind::Levels,
        Params::Levels(Levels {
            rgb: LevelsChannel {
                input_black: 0.1,
                input_white: 0.9,
                gamma: 1.3,
                ..Default::default()
            },
            ..Default::default()
        }),
    );
    levels.mask = Some(random_mask(IntRect::from_xywh(0, 100, 300, 100), 0, 41));
    levels.opacity = 0.8;
    doc.push_layer(levels);
    doc.push_layer(adjustment(
        AdjustmentKind::Curves,
        Params::Curves(Curves {
            rgb: Curve {
                points: vec![(0.0, 0.1), (0.4, 0.6), (1.0, 0.95)],
            },
            ..Default::default()
        }),
    ));
    let mut fill = adjustment(
        AdjustmentKind::SolidColor,
        Params::SolidColor {
            rgba: [0.9, 0.3, 0.1, 1.0],
        },
    );
    fill.blend = BlendMode::Color;
    fill.opacity = 0.6;
    fill.mask = Some(random_mask(IntRect::from_xywh(50, 0, 120, 300), 255, 42));
    doc.push_layer(fill);
    assert_parity(&doc, true, 0, "lut adjustments");
}

#[test]
fn sixteen_and_thirty_two_bit_documents() {
    for depth in [Depth::Sixteen, Depth::ThirtyTwo] {
        let mut doc = base_doc(depth);
        let mut top = random_layer("top", IntRect::from_xywh(10, 10, 280, 280), depth, 51);
        top.blend = BlendMode::SoftLight;
        top.opacity = 0.9;
        doc.push_layer(top);
        doc.push_layer(adjustment(AdjustmentKind::Invert, Params::Invert));
        assert_parity(&doc, true, 0, &format!("{depth:?}"));
    }
}

/// The four full-colour adjustments, each alone over a random backdrop so
/// nothing else can mask a mismatch. Threshold and posterize are step
/// functions, and the test data deliberately includes exact 0/0.5/1
/// channels: a pixel sitting precisely on a step flips whole under any
/// legal fma contraction, so those get a wider allowance (observed on
/// lavapipe: zero) than the continuous kinds.
#[test]
fn direct_adjustments_match() {
    let cases: Vec<(&str, AdjustmentKind, Params, usize)> = vec![
        (
            "hue/sat",
            AdjustmentKind::HueSaturation,
            Params::HueSaturation {
                hue: 47.0,
                saturation: 35.0,
                lightness: -20.0,
                colorize: false,
                lightness_desaturates: false,
                reciprocal_saturation: false,
                ranges: Vec::new(),
            },
            0,
        ),
        (
            "hue/sat colorize",
            AdjustmentKind::HueSaturation,
            Params::HueSaturation {
                hue: 210.0,
                saturation: 60.0,
                lightness: 15.0,
                colorize: true,
                lightness_desaturates: false,
                reciprocal_saturation: false,
                ranges: Vec::new(),
            },
            0,
        ),
        (
            "hue/sat wrap",
            AdjustmentKind::HueSaturation,
            Params::HueSaturation {
                hue: -175.0,
                saturation: -100.0,
                lightness: 0.0,
                colorize: false,
                lightness_desaturates: false,
                reciprocal_saturation: false,
                ranges: Vec::new(),
            },
            0,
        ),
        // The Affinity slider conventions: a lightness lift that also
        // flattens colour, and a reciprocal saturation boost.
        (
            "hue/sat affinity",
            AdjustmentKind::HueSaturation,
            Params::HueSaturation {
                hue: 12.0,
                saturation: 40.0,
                lightness: 30.0,
                colorize: false,
                lightness_desaturates: true,
                reciprocal_saturation: true,
                ranges: Vec::new(),
            },
            0,
        ),
        // Negative saturation takes the ordinary 1 + A path even with
        // the reciprocal flag set, and lightness desaturates on |l|.
        (
            "hue/sat affinity negative",
            AdjustmentKind::HueSaturation,
            Params::HueSaturation {
                hue: -60.0,
                saturation: -35.0,
                lightness: -45.0,
                colorize: false,
                lightness_desaturates: true,
                reciprocal_saturation: true,
                ranges: Vec::new(),
            },
            0,
        ),
        (
            "black & white",
            AdjustmentKind::BlackWhite,
            Params::BlackWhite {
                reds: 40.0,
                yellows: 60.0,
                greens: 40.0,
                cyans: 60.0,
                blues: 20.0,
                magentas: 80.0,
            },
            0,
        ),
        (
            "threshold",
            AdjustmentKind::Threshold,
            Params::Threshold { level: 0.5 },
            600,
        ),
        (
            "posterize",
            AdjustmentKind::Posterize,
            Params::Posterize { levels: 4 },
            600,
        ),
        (
            "posterize max",
            AdjustmentKind::Posterize,
            Params::Posterize { levels: 255 },
            600,
        ),
    ];
    for (name, kind, params, allowed) in cases {
        let mut doc = base_doc(Depth::Eight);
        doc.push_layer(adjustment(kind, params));
        assert_parity(&doc, true, allowed, name);
    }
}

/// Masked, partially opaque, and clipped to the layer below: the three
/// weightings `apply_adjustment` folds in before the result is mixed.
#[test]
fn direct_adjustments_with_masks_opacity_and_clipping() {
    let mut doc = base_doc(Depth::Eight);
    let mut hue = adjustment(
        AdjustmentKind::HueSaturation,
        Params::HueSaturation {
            hue: 90.0,
            saturation: 25.0,
            lightness: 10.0,
            colorize: false,
            lightness_desaturates: false,
            reciprocal_saturation: false,
            ranges: Vec::new(),
        },
    );
    hue.opacity = 0.65;
    hue.mask = Some(random_mask(IntRect::from_xywh(20, 40, 200, 180), 0, 71));
    doc.push_layer(hue);

    doc.push_layer(random_layer(
        "clip base",
        IntRect::from_xywh(40, 40, 200, 200),
        Depth::Eight,
        72,
    ));
    let mut bw = adjustment(
        AdjustmentKind::BlackWhite,
        Params::BlackWhite {
            reds: 90.0,
            yellows: 10.0,
            greens: 55.0,
            cyans: 5.0,
            blues: 70.0,
            magentas: 30.0,
        },
    );
    bw.clipping = true;
    bw.opacity = 0.8;
    doc.push_layer(bw);
    assert_parity(&doc, true, 0, "direct adjustments weighted");
}

/// An adjustment kind the plan has no branch for still has to be caught
/// and handed to the CPU rather than silently rendered wrong.
#[test]
fn an_unknown_direct_adjustment_falls_back() {
    use schist_adjustments::Prepared;
    // Guard the assumption the plan relies on: only these four kinds
    // prepare to `Direct`, and each has a GPU branch.
    for params in [
        Params::HueSaturation {
            hue: 1.0,
            saturation: 1.0,
            lightness: 1.0,
            colorize: false,
            lightness_desaturates: false,
            reciprocal_saturation: false,
            ranges: Vec::new(),
        },
        Params::BlackWhite {
            reds: 1.0,
            yellows: 1.0,
            greens: 1.0,
            cyans: 1.0,
            blues: 1.0,
            magentas: 1.0,
        },
        Params::Threshold { level: 0.5 },
        Params::Posterize { levels: 4 },
    ] {
        assert!(matches!(params.prepare(), Prepared::Direct(_)));
    }
    let mut doc = base_doc(Depth::Eight);
    let mut layer = random_layer(
        "dragged",
        IntRect::from_xywh(0, 0, 300, 300),
        Depth::Eight,
        8,
    );
    layer.render_offset = (3, 4);
    doc.push_layer(layer);
    assert!(
        plan::build(&doc).is_err(),
        "a mid-drag layer must stay on the CPU"
    );
    assert_parity(&doc, false, 0, "render offset fallback");
}

#[test]
fn regions_match_the_cpu_reference() {
    let Some(gpu) = gpu() else { return };
    let mut doc = base_doc(Depth::Eight);
    let mut top = random_layer("top", IntRect::from_xywh(5, 15, 260, 250), Depth::Eight, 61);
    top.blend = BlendMode::LinearLight;
    doc.push_layer(top);
    let region = IntRect::from_xywh(37, 41, 201, 173);
    let g = gpu.region_rgba8(&doc, region);
    let c = composite_region_rgba8_cpu(&doc, region);
    assert_eq!(g.len(), c.len());
    let bad = g
        .iter()
        .zip(&c)
        .filter(|(a, b)| (**a as i32 - **b as i32).abs() > 1)
        .count();
    assert_eq!(bad, 0, "region rgba8");

    let gf = gpu.region_f32(&doc, region);
    let cf = schist_compositor::composite_region_f32_cpu(&doc, region);
    let worst = gf
        .iter()
        .zip(&cf)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 5e-4, "region f32 worst diff {worst}");
}

#[test]
fn viewport_matches_cpu_for_all_sampling_modes() {
    use schist_compositor::viewport::{render_viewport_cpu, ViewportParams};
    let Some(gpu) = gpu() else { return };

    let mut rng = Lcg(77);
    let (cols, rows) = (3usize, 3usize);
    let grid: Vec<Option<Arc<Vec<u8>>>> = (0..cols * rows)
        .map(|i| {
            if i == 4 {
                return None; // a missing tile
            }
            let mut tile = vec![0u8; 256 * 256 * 4];
            for b in tile.iter_mut() {
                *b = (rng.next() % 256) as u8;
            }
            Some(Arc::new(tile))
        })
        .collect();

    // (zoom, scale factor, rotation) hitting crisp, bilinear and box.
    let cases = [
        (2.0f32, 1.0f32, 0.0f32, "crisp"),
        (1.5, 1.0, 0.0, "bilinear"),
        (0.3, 1.0, 0.0, "box"),
        (0.8, 2.0, 0.35, "rotated"),
        (0.15, 2.0, -0.6, "rotated minified"),
    ];
    for (zoom, sf, rotation, name) in cases {
        let params = ViewportParams {
            width: 320,
            height: 240,
            origin: (-40.0, -25.0),
            zoom,
            scale_factor: sf,
            rotation,
            canvas: IntRect::from_xywh(0, 0, 700, 700),
            grid_origin: (0, 0),
            grid_cols: cols,
            grid_rows: rows,
            surround: 0x3C,
        };
        let cpu = render_viewport_cpu(&params, &grid);
        let gpu_img = gpu
            .viewport(&params, &grid)
            .expect("gpu viewport should render");
        assert_eq!(cpu.len(), gpu_img.len());
        // Sampling sits on float boundaries; allow a whisker of drift.
        let bad = cpu
            .iter()
            .zip(&gpu_img)
            .filter(|(a, b)| (**a as i32 - **b as i32).abs() > 1)
            .count();
        let allowed = cpu.len() / 500; // 0.2%
        assert!(bad <= allowed, "{name}: {bad} channels off by >1");
    }
}
