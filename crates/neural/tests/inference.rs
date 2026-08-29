//! The built-in model has to load, run, and actually help.

use schist_neural::{self as neural, Input};

/// Serialises the test that repoints the model directory against the two
/// that read it.
///
/// `SCHIST_MODEL_DIR` is process-wide and the test harness runs these on
/// threads, so without this the install test can move the directory out
/// from under a model that another test has already decided is
/// installed -- which fails, rarely, and only under load.
fn model_dir_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The tile a tiled model works in, for a test that needs to line up with
/// it.
fn tiling(model: &neural::Model) -> (usize, usize) {
    match model.spec.input {
        Input::Tiles { size, overlap } => (size, overlap),
        other => panic!("{} is {other:?}, not tiled", model.spec.id),
    }
}

/// A test image with photograph-like statistics: a multi-octave noise
/// field with roughly a 1/f spectrum, plus hard-edged shapes over it.
///
/// The shape of the input matters more than it looks. The network was
/// trained on natural images and it is *supposed* to be specialised to
/// them; fed a pure sinusoid, a fine checkerboard, or white noise it
/// gains nothing, because nothing in a photograph looks like that and
/// none of it survives a downscale anyway. What a photograph is made of
/// is smooth variation at every scale with edges cut through it, and
/// that is what an enlargement destroys, so that is what this builds.
fn photo_like(w: usize, h: usize) -> Vec<f32> {
    // A small deterministic PRNG, so the test image is fixed without
    // pulling in a dependency for it.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f32 / (1u64 << 53) as f32
    };

    // One octave: random values on a lattice, smoothly interpolated.
    let mut octave = |cell: usize, out: &mut Vec<f32>, amp: f32| {
        let (gw, gh) = (w / cell + 2, h / cell + 2);
        let grid: Vec<f32> = (0..gw * gh * 3).map(|_| next()).collect();
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 / cell as f32, y as f32 / cell as f32);
                let (gx, gy) = (fx as usize, fy as usize);
                // Smoothstep, so there are no lattice creases.
                let tx = {
                    let t = fx - gx as f32;
                    t * t * (3.0 - 2.0 * t)
                };
                let ty = {
                    let t = fy - gy as f32;
                    t * t * (3.0 - 2.0 * t)
                };
                for c in 0..3 {
                    let g = |ix: usize, iy: usize| grid[(iy * gw + ix) * 3 + c];
                    let top = g(gx, gy) * (1.0 - tx) + g(gx + 1, gy) * tx;
                    let bot = g(gx, gy + 1) * (1.0 - tx) + g(gx + 1, gy + 1) * tx;
                    out[(y * w + x) * 3 + c] += (top * (1.0 - ty) + bot * ty) * amp;
                }
            }
        }
    };

    let mut px = vec![0.0f32; w * h * 3];
    let mut total = 0.0;
    for cell in [64usize, 32, 16, 8, 4, 2] {
        // Amplitude proportional to scale is what makes it 1/f.
        let amp = cell as f32;
        octave(cell, &mut px, amp);
        total += amp;
    }
    for v in px.iter_mut() {
        *v = 0.25 + (*v / total) * 0.5;
    }

    // Edges cut through it. Tinted rather than flat-filled, so the
    // interiors keep their texture the way a real subject would.
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f32 / (1u64 << 53) as f32
    };
    for _ in 0..40 {
        let tint = [next(), next(), next()];
        let round = next() < 0.5;
        let (cx, cy) = (next() * w as f32, next() * h as f32);
        let (rw, rh) = (4.0 + next() * 40.0, 4.0 + next() * 40.0);
        for y in 0..h {
            for x in 0..w {
                let (dx, dy) = (x as f32 - cx, y as f32 - cy);
                let inside = if round {
                    (dx / rw).hypot(dy / rh) < 1.0
                } else {
                    dx.abs() < rw && dy.abs() < rh
                };
                if inside {
                    for c in 0..3 {
                        let v = &mut px[(y * w + x) * 3 + c];
                        *v = (*v * 0.35 + tint[c] * 0.65).clamp(0.0, 1.0);
                    }
                }
            }
        }
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
    let (t, _) = tiling(&model);
    let out = model.run_tile(&photo_like(t, t)).expect("runs");
    assert_eq!(out.len(), t * t * 3);
    for (i, v) in out.iter().enumerate() {
        assert!(v.is_finite(), "index {i} is {v}");
        assert!((0.0..=1.0).contains(v), "index {i} is {v}, outside 0..=1");
    }
}

/// 128x128 of raw RGB, from a photograph the model has never seen:
/// `kodim23` of the Kodak True Color suite is in the validation split in
/// `tools/train/detail.py`, so it was held out of training. Stored raw
/// rather than as a PNG so the test needs no decoder.
const PHOTO: &[u8] = include_bytes!("fixtures/photo.rgb");

#[test]
fn the_model_recovers_detail_a_downscale_destroyed() {
    // The property the network was trained for, checked end to end
    // through the tiled runner rather than on a single tile -- and on a
    // real photograph, because that is the distribution it was fitted to
    // and synthetic images flatter it or punish it for the wrong reasons.
    let (w, h) = (128usize, 128usize);
    let truth: Vec<f32> = PHOTO.iter().map(|&b| b as f32 / 255.0).collect();
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
    let (tile, overlap) = tiling(&model);
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
    let step = tile - overlap * 2;
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
    let _guard = model_dir_lock();
    let dir = std::env::temp_dir().join(format!("schist-model-test-{}", std::process::id()));
    // SAFETY: no other test reads the model directory while the lock is
    // held, and it is put back before the lock is released.
    unsafe { std::env::set_var("SCHIST_MODEL_DIR", &dir) };
    let mut spec = neural::spec("style-mosaic").expect("catalogued").clone();
    spec.sha256 = Some("0000000000000000000000000000000000000000000000000000000000000000");
    let err = neural::install(&spec, b"whatever").unwrap_err().to_string();
    // SAFETY: as above.
    unsafe { std::env::remove_var("SCHIST_MODEL_DIR") };
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
    spec.input = Input::Tiles {
        size: 8,
        overlap: 0,
    };
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

#[test]
fn every_built_in_model_loads() {
    // The catalogue and the binary have to agree: a spec with no URL is
    // a promise that the bytes are compiled in.
    for spec in neural::CATALOG.iter().filter(|s| s.built_in()) {
        assert!(neural::installed(spec.id), "{} is not installed", spec.id);
        assert!(
            neural::get(spec.id).is_some(),
            "{} did not load from the binary",
            spec.id
        );
    }
}

/// The same 128x128 photograph, through JPEG at quality 20. Made with
/// Pillow -- `Image.fromarray(photo).save(buf, "JPEG", quality=20)` --
/// and stored decoded, so the test needs no JPEG decoder to reproduce
/// what the filter is pointed at.
const JPEG: &[u8] = include_bytes!("fixtures/photo-q20.rgb");

#[test]
fn the_deblocker_undoes_some_of_a_jpeg() {
    let (w, h) = (128usize, 128usize);
    let truth: Vec<f32> = PHOTO.iter().map(|&b| b as f32 / 255.0).collect();
    let squashed: Vec<f32> = JPEG.iter().map(|&b| b as f32 / 255.0).collect();
    let mut cleaned = squashed.clone();
    let model = neural::get("dejpeg").expect("model");
    neural::run_tiled(&model, &mut cleaned, w, h, 1.0);

    let before = psnr(&squashed, &truth);
    let after = psnr(&cleaned, &truth);
    assert!(
        after > before,
        "the model made it worse: {before:.2} dB -> {after:.2} dB"
    );
    println!(
        "jpeg {before:.2} dB -> model {after:.2} dB (+{:.2})",
        after - before
    );
}

/// Luminance, the way the filters and the training script both compute
/// it.
fn grey_of(rgb: &[f32]) -> Vec<f32> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .flat_map(|p| {
            let y = 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
            [y, y, y]
        })
        .collect()
}

#[test]
fn colour_is_put_back_into_a_photograph_that_lost_it() {
    // Take the colour out of a photograph the model has never seen and
    // ask for it back.
    //
    // What is checked is *correlation*, not error. A colouriser that
    // hedges -- predicting a faint brown everywhere, which is what a
    // network trained to minimise error does -- scores better on error
    // than one that commits, because the average photograph is not very
    // colourful and being timid is never very wrong. It also looks
    // obviously broken. So: does the colour it invents go up where the
    // real colour goes up, and does it use about as much of it as the
    // photograph did?
    let (w, h) = (128usize, 128usize);
    let truth: Vec<f32> = PHOTO.iter().map(|&b| b as f32 / 255.0).collect();
    let grey = grey_of(&truth);
    let model = neural::get("colorize").expect("model");
    let predicted = neural::chroma(&model, &grey, w, h).expect("runs");
    assert_eq!(predicted.len(), w * h * 2);
    assert!(predicted.iter().all(|v| v.is_finite()));

    let real = chroma_of(&truth);
    let r = correlation(&predicted, &real);
    let ours: f32 = predicted.iter().map(|v| v.abs()).sum::<f32>() / predicted.len() as f32;
    let theirs: f32 = real.iter().map(|v| v.abs()).sum::<f32>() / real.len() as f32;
    println!(
        "chroma correlation {r:.3}, colourfulness {:.2}x",
        ours / theirs
    );
    assert!(
        r > 0.25,
        "the colour it chose has nothing to do with the photograph: {r:.3}"
    );
    // A wide band on purpose. Over 200 held-out photographs this model
    // averages 1.1x, but this is one 128-pixel crop of a very colourful
    // bird, and being twice as bold as one crop is not a defect -- being
    // grey, or being ten times as bold, would be.
    assert!(
        (0.4..3.0).contains(&(ours / theirs)),
        "colourfulness {:.2} of the real thing",
        ours / theirs
    );

    // And recombining has to leave the luminance exactly where it was,
    // wherever the colour asked for is one the RGB cube can hold at that
    // brightness. Where it cannot, the answer is clipped, which moves the
    // luminance -- that is the one thing allowed to.
    let mut checked = 0;
    for (i, p) in grey.as_chunks::<3>().0.iter().enumerate() {
        let (ca, cb) = (predicted[i * 2], predicted[i * 2 + 1]);
        let before = 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
        let (r, b) = (before + ca, before + cb);
        let g = (before - 0.299 * r - 0.114 * b) / 0.587;
        if [r, g, b].iter().any(|v| !(0.0..=1.0).contains(v)) {
            continue;
        }
        let out = neural::recolour(p, [ca, cb]);
        let after = 0.299 * out[0] + 0.587 * out[1] + 0.114 * out[2];
        assert!(
            (after - before).abs() < 1e-4,
            "pixel {i}: {before} -> {after}"
        );
        checked += 1;
    }
    assert!(checked > w * h / 2, "only {checked} pixels fitted the cube");
}

/// The chroma of an image: `R - Y` then `B - Y`, two floats a pixel.
fn chroma_of(rgb: &[f32]) -> Vec<f32> {
    rgb.as_chunks::<3>()
        .0
        .iter()
        .flat_map(|p| {
            let y = 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
            [p[0] - y, p[2] - y]
        })
        .collect()
}

/// Pearson correlation, over both chroma channels at once.
fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len() as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let (mut cov, mut va, mut vb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        cov += (x - ma) * (y - mb);
        va += (x - ma) * (x - ma);
        vb += (y - mb) * (y - mb);
    }
    cov / (va.sqrt() * vb.sqrt()).max(1e-9)
}

#[test]
fn depth_comes_back_as_a_map_of_the_image() {
    // Downloaded rather than built in, so this is a no-op on a machine
    // that has not fetched it -- including CI.
    let _guard = model_dir_lock();
    let Some(model) = neural::get("depth") else {
        eprintln!("skipping: the depth model is not installed");
        return;
    };
    let (w, h) = (128usize, 128usize);
    let photo: Vec<f32> = PHOTO.iter().map(|&b| b as f32 / 255.0).collect();
    let map = neural::depth_map(&model, &photo, w, h).expect("runs");

    assert_eq!(map.len(), w * h);
    assert!(map.iter().all(|v| (0.0..=1.0).contains(v)), "out of range");
    // Normalised over its own range, so both ends have to be reached.
    let lo = map.iter().cloned().fold(f32::INFINITY, f32::min);
    let hi = map.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    assert!(lo < 0.01 && hi > 0.99, "not normalised: {lo}..{hi}");
    // Depth is a property of the scene, not of the texture on it, so
    // neighbouring pixels are nearly always at nearly the same distance.
    let step: f32 = (0..h)
        .flat_map(|y| (1..w).map(move |x| (y, x)))
        .map(|(y, x)| (map[y * w + x] - map[y * w + x - 1]).abs())
        .sum::<f32>()
        / ((w - 1) * h) as f32;
    assert!(step < 0.05, "the map is not smooth: mean step {step:.4}");
}

#[test]
fn the_face_detector_finds_no_faces_in_a_photograph_with_none() {
    let _guard = model_dir_lock();
    let Some(model) = neural::get("face") else {
        eprintln!("skipping: the face model is not installed");
        return;
    };
    let (w, h) = (128usize, 128usize);
    let photo: Vec<f32> = PHOTO.iter().map(|&b| b as f32 / 255.0).collect();
    // A false positive here is the failure that matters: Skin Smoothing
    // gates on this, and a detector that sees a face in foliage would
    // smooth the foliage.
    let found = neural::faces(&model, &photo, w, h).expect("runs");
    assert!(
        found.is_empty(),
        "invented {} faces: {found:?}",
        found.len()
    );

    // Flat grey is not a photograph at all, and must not crash or find
    // anything either.
    let flat = vec![0.5f32; w * h * 3];
    assert!(neural::faces(&model, &flat, w, h).expect("runs").is_empty());
}
