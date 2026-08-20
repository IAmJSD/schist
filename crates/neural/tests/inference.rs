//! The built-in model has to load, run, and actually help.

use photoslop_neural as neural;

/// A test image with photograph-like statistics: hard-edged shapes at a
/// range of sizes, plus fine texture.
///
/// The shape of the input matters more than it looks. The network was
/// trained on natural images, and it is *supposed* to be specialised to
/// them -- fed a pure high-frequency sinusoid or a fine checkerboard it
/// overshoots badly, because nothing in a photograph looks like that.
/// Testing it on such an image would measure the wrong thing. Edges plus
/// texture is what a photograph is made of and what an enlargement
/// destroys.
fn photo_like(w: usize, h: usize) -> Vec<f32> {
    // A small deterministic PRNG, so the test image is fixed without
    // pulling in a dependency for it.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f32 / (1u64 << 53) as f32
    };
    let mut px = vec![0.5f32; w * h * 3];
    for _ in 0..60 {
        let colour = [next(), next(), next()];
        if next() < 0.5 {
            let x0 = (next() * w as f32) as usize;
            let y0 = (next() * h as f32) as usize;
            let rw = 4 + (next() * 56.0) as usize;
            let rh = 4 + (next() * 56.0) as usize;
            for y in y0..(y0 + rh).min(h) {
                for x in x0..(x0 + rw).min(w) {
                    px[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(&colour);
                }
            }
        } else {
            let cx = next() * w as f32;
            let cy = next() * h as f32;
            let r = 4.0 + next() * 36.0;
            for y in 0..h {
                for x in 0..w {
                    if (x as f32 - cx).hypot(y as f32 - cy) < r {
                        px[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(&colour);
                    }
                }
            }
        }
    }
    // Fine texture on top, which is the first thing a downscale loses.
    for v in px.iter_mut() {
        *v = (*v + (next() - 0.5) * 0.06).clamp(0.0, 1.0);
    }
    px
}

/// Halve and restore, which is what an enlargement costs an image.
fn degrade(src: &[f32], w: usize, h: usize) -> Vec<f32> {
    let (hw, hh) = (w / 2, h / 2);
    let mut small = vec![0.0f32; hw * hh * 3];
    for y in 0..hh {
        for x in 0..hw {
            for c in 0..3 {
                let mut acc = 0.0;
                for dy in 0..2 {
                    for dx in 0..2 {
                        acc += src[((y * 2 + dy) * w + x * 2 + dx) * 3 + c];
                    }
                }
                small[(y * hw + x) * 3 + c] = acc / 4.0;
            }
        }
    }
    let mut back = vec![0.0f32; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            // Bilinear back up.
            let (fx, fy) = (x as f32 / 2.0 - 0.25, y as f32 / 2.0 - 0.25);
            let (x0, y0) = (fx.floor().max(0.0) as usize, fy.floor().max(0.0) as usize);
            let (x1, y1) = ((x0 + 1).min(hw - 1), (y0 + 1).min(hh - 1));
            let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
            for c in 0..3 {
                let at = |px: usize, py: usize| small[(py * hw + px) * 3 + c];
                let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
                let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
                back[(y * w + x) * 3 + c] = (top * (1.0 - ty) + bot * ty).clamp(0.0, 1.0);
            }
        }
    }
    back
}

fn psnr(a: &[f32], b: &[f32]) -> f32 {
    let mse: f32 = a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>() / a.len() as f32;
    if mse <= 1e-12 {
        99.0
    } else {
        10.0 * (1.0 / mse).log10()
    }
}

#[test]
fn the_built_in_model_is_always_available() {
    assert!(neural::installed("detail"), "the shipped model is missing");
    let spec = neural::spec("detail").expect("catalogued");
    assert!(spec.built_in());
    assert!(
        neural::get("detail").is_some(),
        "the shipped model did not load"
    );
}

#[test]
fn a_tile_runs_and_stays_in_range() {
    let model = neural::get("detail").expect("model");
    let t = model.spec.tile;
    let out = model.run_tile(&photo_like(t, t)).expect("runs");
    assert_eq!(out.len(), t * t * 3);
    for (i, v) in out.iter().enumerate() {
        assert!(v.is_finite(), "index {i} is {v}");
        assert!((0.0..=1.0).contains(v), "index {i} is {v}, outside 0..=1");
    }
}

#[test]
fn the_model_recovers_detail_a_downscale_destroyed() {
    // The property the network was trained for, checked end to end
    // through the tiled runner rather than on a single tile.
    let (w, h) = (256usize, 192usize);
    let truth = photo_like(w, h);
    let soft = degrade(&truth, w, h);
    let mut restored = soft.clone();
    let model = neural::get("detail").expect("model");
    neural::run_tiled(&model, &mut restored, w, h, 1.0);

    let before = psnr(&soft, &truth);
    let after = psnr(&restored, &truth);
    assert!(
        after > before,
        "the model made it worse: {before:.2} dB -> {after:.2} dB"
    );
    println!(
        "bicubic {before:.2} dB -> model {after:.2} dB (+{:.2})",
        after - before
    );
}

#[test]
fn tiling_leaves_no_seams() {
    // A grid of seams is what happens when tile overlap is mishandled, and
    // it shows up as a spike in the horizontal difference at tile
    // boundaries. Compare against the difference elsewhere.
    let (w, h) = (400usize, 120usize);
    let flat: Vec<f32> = (0..w * h * 3)
        .map(|i| 0.35 + 0.3 * ((i / 3) % 7) as f32 / 7.0)
        .collect();
    let mut out = flat.clone();
    let model = neural::get("detail").expect("model");
    let tile = model.spec.tile;
    neural::run_tiled(&model, &mut out, w, h, 1.0);

    let column_step = |x: usize| -> f32 {
        (0..h)
            .map(|y| {
                let a = out[(y * w + x) * 3];
                let b = out[(y * w + x - 1) * 3];
                (a - b).abs()
            })
            .sum::<f32>()
            / h as f32
    };
    let step = tile - model.spec.overlap * 2;
    let mut worst_seam = 0.0f32;
    let mut x = step;
    while x < w {
        worst_seam = worst_seam.max(column_step(x));
        x += step;
    }
    let typical: f32 = (2..w).map(column_step).sum::<f32>() / (w - 2) as f32;
    assert!(
        worst_seam < typical + 0.02,
        "tile seam visible: {worst_seam:.4} against a typical {typical:.4}"
    );
}

#[test]
fn odd_sizes_and_tiny_images_are_handled() {
    let model = neural::get("detail").expect("model");
    for (w, h) in [(1usize, 1usize), (3, 257), (257, 3), (5, 5)] {
        let mut px = photo_like(w, h);
        neural::run_tiled(&model, &mut px, w, h, 1.0);
        assert_eq!(px.len(), w * h * 3);
        assert!(px.iter().all(|v| v.is_finite()), "{w}x{h} produced NaN");
    }
}

#[test]
fn blend_zero_leaves_the_image_alone() {
    let model = neural::get("detail").expect("model");
    let (w, h) = (128usize, 128usize);
    let before = photo_like(w, h);
    let mut after = before.clone();
    neural::run_tiled(&model, &mut after, w, h, 0.0);
    assert_eq!(before, after);
}

#[test]
fn a_missing_model_is_none_rather_than_a_panic() {
    assert!(neural::get("no-such-model").is_none());
    assert!(!neural::installed("no-such-model"));
    assert!(neural::spec("no-such-model").is_none());
}

#[test]
fn a_corrupt_model_is_rejected() {
    let spec = neural::spec("detail").expect("catalogued");
    assert!(neural::Model::from_bytes(spec, b"not an onnx file").is_err());
    assert!(neural::Model::from_bytes(spec, &[]).is_err());
}

#[test]
fn install_checks_the_hash() {
    let dir = std::env::temp_dir().join(format!("photoslop-model-test-{}", std::process::id()));
    // SAFETY: single-threaded test setup, before any model is loaded.
    unsafe { std::env::set_var("PHOTOSLOP_MODEL_DIR", &dir) };
    let mut spec = neural::spec("style-mosaic").expect("catalogued").clone();
    spec.sha256 = Some("0000000000000000000000000000000000000000000000000000000000000000");
    let err = neural::install(&spec, b"whatever").unwrap_err().to_string();
    assert!(err.contains("checksum"), "unexpected error: {err}");
    assert!(!dir.join(spec.file).exists(), "a bad download was kept");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hand-built opset-9 graph exercising both operators [`compat`] rewrites:
/// `Upsample` doubles an 8x8 input to 16x16 with nearest sampling, then an
/// attribute-style `Slice` crops the top-left 8x8 back out. So the output is
/// the input's top-left quadrant, pixel-doubled -- something we can predict
/// exactly, which a graph that merely *loaded* would not reproduce.
///
/// Regenerate with `onnx.helper` if it ever needs to change; it is 221 bytes.
const OPSET9: &[u8] = include_bytes!("fixtures/opset9.onnx");

#[test]
fn a_pre_opset_10_graph_is_rewritten_and_runs() {
    let mut spec = neural::spec("detail").expect("catalogued").clone();
    spec.tile = 8;
    spec.range = neural::Range::Unit;
    let spec: &'static neural::ModelSpec = Box::leak(Box::new(spec));

    let model = neural::Model::from_bytes(spec, OPSET9)
        .expect("the opset-9 rewrite should make this loadable");

    // A gradient, so every pixel is distinguishable from its neighbours.
    let input: Vec<f32> = (0..8 * 8 * 3)
        .map(|i| ((i * 37) % 199) as f32 / 199.0)
        .collect();
    let out = model.run_tile(&input).unwrap();

    let at = |px: &[f32], x: usize, y: usize, c: usize| px[(y * 8 + x) * 3 + c];
    for y in 0..8 {
        for x in 0..8 {
            for c in 0..3 {
                let want = at(&input, x / 2, y / 2, c);
                let got = at(&out, x, y, c);
                assert!(
                    (want - got).abs() < 1e-5,
                    "({x},{y},{c}): expected {want}, got {got}"
                );
            }
        }
    }
}
