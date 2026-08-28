//! Tests over real Affinity-authored files in fixtures/affinity.
//!
//! Following the PSD convention, these skip (pass) when the corpus is
//! missing so a partial checkout still builds green.

use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/affinity")
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    // The vendored corpus, plus (when set) SCHIST_AFFINITY_CORPUS —
    // colon-separated directories of private real-world files to sweep
    // in addition.
    let mut dirs = vec![fixture_dir()];
    if let Ok(extra) = std::env::var("SCHIST_AFFINITY_CORPUS") {
        dirs.extend(
            extra
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("skipping: no fixtures at {}", dir.display());
            continue;
        };
        out.extend(
            entries
                .flatten()
                .filter(|e| {
                    // A document open in Affinity leaves a sibling
                    // "name.afphoto~lock~" behind; its extension also
                    // starts with "af", so match real extensions only.
                    e.path().extension().is_some_and(|x| {
                        matches!(
                            x.to_string_lossy().as_ref(),
                            "af" | "afphoto" | "afdesign" | "afpub"
                        )
                    })
                })
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        std::fs::read(e.path()).unwrap(),
                    )
                }),
        );
    }
    out.sort();
    out
}

#[test]
fn archives_parse_and_all_entries_extract() {
    for (name, bytes) in fixtures() {
        let archive =
            schist_codec_affinity::Archive::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let names: Vec<String> = archive.names().map(str::to_owned).collect();
        assert!(names.iter().any(|n| n == "doc.dat"), "{name}: no doc.dat");
        for entry_name in &names {
            if let Some(entry) = archive.head(entry_name) {
                let data = archive
                    .extract(entry)
                    .unwrap_or_else(|e| panic!("{name}/{entry_name}: {e}"));
                assert!(!data.is_empty(), "{name}/{entry_name}: empty");
            }
        }
        println!("{name}: {} entries ok", names.len());
    }
}

#[test]
fn graphs_parse() {
    for (name, bytes) in fixtures() {
        let dump =
            schist_codec_affinity::import::dump(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(dump.contains("DocR"), "{name}: no document root in dump");
        println!("=== {name}: dump {} chars", dump.len());
    }
}

/// raster_test.afdesign holds one pixel layer: a black brush stroke whose
/// alpha tile is bounded by rect [22,32]–[157,157] (visible in the raw
/// block table). Verify the decoded pixels honor that geometry.
#[test]
fn raster_pixels_land_where_the_file_says() {
    let path = fixture_dir().join("raster_test.afdesign");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipping: no {}", path.display());
        return;
    };
    let (doc, report) = schist_codec_affinity::read_affinity(&bytes).unwrap();
    assert!(report.complete());
    assert_eq!(report.raster_layers, 1);

    fn find_raster(layers: &[schist_core::Layer]) -> Option<&schist_core::Layer> {
        layers.iter().find_map(|l| match l.children() {
            Some(children) => find_raster(children),
            None => l.as_raster().is_some().then_some(l),
        })
    }
    let layer = find_raster(&doc.tree.layers).expect("a raster layer");
    let tiles = &layer.as_raster().unwrap().tiles;

    let mut inside = 0u32;
    let mut outside = 0u32;
    for y in 0..180 {
        for x in 0..180 {
            let px = tiles.pixel(x, y).to_u8();
            if px[3] > 0 {
                let in_rect = (22..157).contains(&x) && (32..157).contains(&y);
                if in_rect {
                    inside += 1;
                    // The stroke is black paint: color must be black
                    // wherever it is opaque.
                    if px[3] == 255 {
                        assert_eq!(&px[..3], &[0, 0, 0], "at {x},{y}");
                    }
                } else {
                    outside += 1;
                }
            }
        }
    }
    assert!(inside > 100, "stroke has substance ({inside} px)");
    assert_eq!(outside, 0, "no paint outside the block rect");
}

/// The embedded 512×512 thumbnail — Affinity's own render of the
/// document — decoded as ground truth for geometry tests.
fn thumbnail(bytes: &[u8]) -> image::RgbaImage {
    let archive = schist_codec_affinity::Archive::parse(bytes).unwrap();
    let png = archive.thumbnail().expect("fixture has a thumbnail");
    image::load_from_memory(png).unwrap().to_rgba8()
}

/// Is this thumbnail pixel ink (stroke or fill) rather than background?
/// Fixture thumbnails render on white; fills are light gray or saturated
/// colour, strokes black.
fn thumb_ink(px: &image::Rgba<u8>) -> bool {
    px.0[3] >= 128 && (px.0[0] as u32 + px.0[1] as u32 + px.0[2] as u32) <= 750
}

/// Ink coverage of one imported document layer at canvas resolution.
fn layer_ink(layer: &schist_core::Layer, w: u32, h: u32) -> Vec<bool> {
    let tiles = &layer.as_raster().unwrap().tiles;
    let mut out = vec![false; (w * h) as usize];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            out[(y as u32 * w + x as u32) as usize] = tiles.pixel(x, y).to_u8()[3] >= 128;
        }
    }
    out
}

/// Thumbnail ink resampled to canvas resolution: a canvas pixel is ink
/// when the majority of its thumbnail block is.
fn thumb_ink_grid(thumb: &image::RgbaImage, w: u32, h: u32) -> Vec<bool> {
    let mut out = vec![false; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let (tx0, tx1) = (x * thumb.width() / w, (x + 1) * thumb.width() / w);
            let (ty0, ty1) = (y * thumb.height() / h, (y + 1) * thumb.height() / h);
            let mut ink = 0u32;
            let mut total = 0u32;
            for ty in ty0..ty1.max(ty0 + 1) {
                for tx in tx0..tx1.max(tx0 + 1) {
                    total += 1;
                    ink += thumb_ink(thumb.get_pixel(tx, ty)) as u32;
                }
            }
            out[(y * w + x) as usize] = ink * 2 > total;
        }
    }
    out
}

fn leaf_rasters(layers: &[schist_core::Layer]) -> Vec<&schist_core::Layer> {
    let mut out = Vec::new();
    for l in layers {
        match l.children() {
            Some(children) => out.extend(leaf_rasters(children)),
            None if l.as_raster().is_some() => out.push(l),
            None => {}
        }
    }
    out
}

fn iou(a: &[bool], b: &[bool]) -> f64 {
    let inter = a.iter().zip(b).filter(|(x, y)| **x && **y).count();
    let union = a.iter().zip(b).filter(|(x, y)| **x || **y).count();
    if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    }
}

/// shape_test.afdesign holds one of each Designer 1.x shape whose
/// geometry we rebuild: square star, star, ellipse, cloud, plain
/// rectangle (whose inert corner radii must stay sharp), heart and
/// rounded rectangle. The import must match Affinity's own render of
/// the same file — the embedded thumbnail — shape by shape.
#[test]
fn shapes_match_the_embedded_thumbnail() {
    let path = fixture_dir().join("shape_test.afdesign");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipping: no {}", path.display());
        return;
    };
    let (doc, report) = schist_codec_affinity::read_affinity(&bytes).unwrap();
    assert!(report.complete(), "skipped: {:?}", report.skipped);
    assert_eq!(report.shapes, 7);

    let truth = thumb_ink_grid(&thumbnail(&bytes), doc.width, doc.height);
    let layers = leaf_rasters(&doc.tree.layers);
    assert_eq!(layers.len(), 7);

    // Every shape individually agrees with the thumbnail over its own
    // extent (the shapes tile the canvas without overlap)…
    for layer in &layers {
        let ours = layer_ink(layer, doc.width, doc.height);
        let bounds = layer.as_raster().unwrap().tiles.content_bounds();
        let mut inter = 0u32;
        let mut union = 0u32;
        for y in bounds.top.max(0)..bounds.bottom.min(doc.height as i32) {
            for x in bounds.left.max(0)..bounds.right.min(doc.width as i32) {
                let i = (y as u32 * doc.width + x as u32) as usize;
                inter += (ours[i] && truth[i]) as u32;
                union += (ours[i] || truth[i]) as u32;
            }
        }
        let iou = inter as f64 / union.max(1) as f64;
        println!("{}: IoU {:.3}", layer.name, iou);
        assert!(iou > 0.85, "{}: IoU {:.3} vs thumbnail", layer.name, iou);
    }

    // …and the union does too, so nothing is missing or invented.
    let mut ours = vec![false; truth.len()];
    for layer in &layers {
        for (o, v) in ours.iter_mut().zip(layer_ink(layer, doc.width, doc.height)) {
            *o |= v;
        }
    }
    let total = iou(&ours, &truth);
    println!("union IoU {total:.3}");
    assert!(total > 0.9, "union IoU {total:.3} vs thumbnail");
}

/// test_path.afdesign is handwriting drawn with the pen tool: stroked
/// curves with no fill. The strokes must land on the thumbnail's ink.
#[test]
fn pen_paths_land_on_affinitys_ink() {
    let path = fixture_dir().join("test_path.afdesign");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipping: no {}", path.display());
        return;
    };
    let (doc, report) = schist_codec_affinity::read_affinity(&bytes).unwrap();
    assert!(report.complete(), "skipped: {:?}", report.skipped);
    assert_eq!(report.shapes, 8);

    let thumb = thumbnail(&bytes);
    let scale = thumb.width() as f32 / doc.width as f32;
    let mut ours = vec![false; (doc.width * doc.height) as usize];
    for layer in leaf_rasters(&doc.tree.layers) {
        for (o, v) in ours.iter_mut().zip(layer_ink(layer, doc.width, doc.height)) {
            *o |= v;
        }
    }

    // Thin AA strokes never align pixel-perfect between renderers;
    // require most of our ink to land within a hair of Affinity's, and
    // comparable amounts of it.
    let thumb_count =
        thumb.pixels().filter(|p| thumb_ink(p)).count() as f64 / (scale * scale) as f64;
    let our_count = ours.iter().filter(|v| **v).count() as f64;
    let mut on_ink = 0usize;
    for y in 0..doc.height as i32 {
        for x in 0..doc.width as i32 {
            if !ours[(y as u32 * doc.width + x as u32) as usize] {
                continue;
            }
            let (cx, cy) = ((x as f32 + 0.5) * scale, (y as f32 + 0.5) * scale);
            let mut hit = false;
            for dy in -3i32..=3 {
                for dx in -3i32..=3 {
                    let (tx, ty) = (cx as i32 + dx, cy as i32 + dy);
                    if tx >= 0
                        && ty >= 0
                        && (tx as u32) < thumb.width()
                        && (ty as u32) < thumb.height()
                        && thumb_ink(thumb.get_pixel(tx as u32, ty as u32))
                    {
                        hit = true;
                    }
                }
            }
            on_ink += hit as usize;
        }
    }
    let precision = on_ink as f64 / our_count.max(1.0);
    let ratio = our_count / thumb_count.max(1.0);
    println!("stroke ink: {our_count} px, {precision:.3} on thumbnail ink, {ratio:.2}× its amount");
    assert!(precision > 0.85, "{precision:.3} of stroke ink is astray");
    assert!((0.4..=2.5).contains(&ratio), "ink amount ratio {ratio:.2}");
}

/// color.afdesign fills the four canvas quadrants with red, green, blue
/// and magenta rectangles (RGBA and HSLA fills, scaled by transforms).
#[test]
fn designer_fills_read_correct_colors() {
    let path = fixture_dir().join("color.afdesign");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("skipping: no {}", path.display());
        return;
    };
    let (doc, report) = schist_codec_affinity::read_affinity(&bytes).unwrap();
    assert!(report.complete(), "skipped: {:?}", report.skipped);

    let layers = leaf_rasters(&doc.tree.layers);
    for (x, y, expected) in [
        (45, 45, [255u8, 0, 0]),
        (135, 45, [0, 255, 0]),
        (45, 135, [0, 0, 255]),
        (135, 135, [255, 0, 255]),
    ] {
        let found = layers.iter().find_map(|l| {
            let px = l.as_raster().unwrap().tiles.pixel(x, y).to_u8();
            (px[3] > 200).then_some(px)
        });
        let px = found.unwrap_or_else(|| panic!("no opaque layer at {x},{y}"));
        assert_eq!(&px[..3], &expected, "at {x},{y}");
    }
}

#[test]
fn documents_import() {
    for (name, bytes) in fixtures() {
        let (doc, report) =
            schist_codec_affinity::read_affinity(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(doc.width > 0 && doc.height > 0, "{name}: empty canvas");
        println!(
            "{name}: {}×{}, {} raster, {} groups, {} text, {} shapes, {} adj, {} masks, {} skipped {:?}",
            doc.width,
            doc.height,
            report.raster_layers,
            report.groups,
            report.text_layers,
            report.shapes,
            report.adjustments,
            report.masks,
            report.skipped.len(),
            report.skipped
        );
    }
}

/// The app rebuilds styled rasters when a document opens; the probe
/// comparison has to do the same for layer effects to composite.
fn restyle(layers: &mut [schist_core::Layer]) {
    for l in layers {
        if let schist_core::LayerKind::Group(g) = &mut l.kind {
            restyle(&mut g.children);
        }
        if !l.style.is_empty() {
            l.styled = schist_compositor::render_styled(l).map(std::sync::Arc::new);
        }
    }
}

/// The adjustment fixtures in fixtures/affinity-probe were each drawn in
/// Affinity itself — one document per adjustment type, distinctive
/// slider values — and their embedded thumbnails are Affinity's own
/// renders. Compositing our import must stay close to that ground
/// truth; the bounds below encode how close each importer currently
/// gets (0.5 = exact up to resampling, larger = a documented
/// approximation).
/// The blur probes are a white square on transparency, so the
/// over-white metric the test above uses would score them zero however
/// wrong the blur was. Compare them premultiplied instead: an alpha
/// difference counts, and the undefined colour of a fully transparent
/// fringe pixel does not.
#[test]
fn probed_blur_radii_match_affinitys_alpha() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/affinity-probe");
    // Radi 30, 90 and 180; Affinity's own sigma there is 11.3, 31.9
    // and 67.1 pixels, which is what the 0.373 factor in the importer
    // was measured from.
    for (file, max_rms) in [
        ("blur_r10.af", 1.2),
        ("blur_r30.af", 0.6),
        ("blur_r60.af", 1.3),
    ] {
        let path = dir.join(file);
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipping: no {}", path.display());
            continue;
        };
        let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
        let thumb = image::load_from_memory(archive.thumbnail().expect("thumb"))
            .expect("decode")
            .to_rgba8();
        let (mut doc, _) = schist_codec_affinity::read_affinity(&bytes).expect("import");
        restyle(&mut doc.tree.layers);
        let region = schist_core::IntRect::from_size(doc.width, doc.height);
        let pixels = schist_compositor::composite_region_rgba8(&doc, region);
        let ours = image::RgbaImage::from_raw(doc.width, doc.height, pixels).expect("buffer");
        let (mut sum, mut n) = (0.0f64, 0u64);
        for (a, b) in ours.pixels().zip(thumb.pixels()) {
            let pm = |p: &image::Rgba<u8>, i: usize| {
                if i == 3 {
                    p.0[3] as f64
                } else {
                    p.0[i] as f64 * p.0[3] as f64 / 255.0
                }
            };
            for i in 0..4 {
                let d = pm(a, i) - pm(b, i);
                sum += d * d;
                n += 1;
            }
        }
        let rms = (sum / n as f64).sqrt();
        println!("{file}: premultiplied rms {rms:.2} (bound {max_rms})");
        assert!(rms <= max_rms, "{file}: rms {rms:.2} above bound {max_rms}");
    }
}

#[test]
fn probed_adjustments_match_affinitys_render() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/affinity-probe");
    // (file, max RMS over white, vs. Affinity's embedded render)
    let bounds = [
        ("blackwhite.af", 1.0),
        ("bright_only.af", 2.0),    // sampled from Affinity's own curve
        ("brightcontrast.af", 4.0), // sampled curves, composed
        ("channelmixer.af", 1.0),
        ("colourbalance.af", 3.0),
        ("contrast_neg.af", 4.0),
        ("contrast_pos.af", 1.0),
        ("exposure.af", 1.0),
        ("gradientmap.af", 0.5),
        ("hsl_hue.af", 1.0),
        ("hsl_hue_neg.af", 0.5),
        ("hsl_lum.af", 0.5),
        ("hsl_lum_neg.af", 0.5),
        ("hsl_probe.af", 0.5),
        ("hsl_range_green_lum.af", 0.5),
        ("hsl_range_green_lumneg.af", 0.5),
        ("hsl_range_mix.af", 0.5),
        ("hsl_range_red_hue.af", 0.5),
        ("hsl_range_red_sat.af", 0.5),
        ("hsl_sat.af", 0.5),
        ("hsl_sat_neg.af", 0.5),
        ("invert.af", 0.5),
        ("lensfilter.af", 1.5),
        ("levels.af", 2.0),
        ("posterise.af", 1.0),
        ("recolour.af", 1.0),
        ("selectivecolour.af", 4.0),
        ("splittoning.af", 8.0), // no-op layer keeping its native data
        ("threshold.af", 8.0),   // saturated-colour boundary differs
        ("sat_only50.af", 2.0),  // Lab chroma scale, gamut clipping aside
        ("sat_only_neg50.af", 2.0),
        ("vib_only100.af", 3.0),
        ("vib_only50.af", 2.0),
        ("vibrance.af", 2.0), // both sliders
        ("wb_cool100.af", 0.5),
        ("wb_cool30.af", 0.5),
        ("wb_cool50.af", 0.5),
        ("wb_tint.af", 0.5),
        ("wb_tint_neg.af", 0.5),
        ("wb_warm.af", 0.5),
        ("wb_warm30.af", 0.5),
        // Both sliders at once, now that the measured interaction
        // table corrects the two tables' product.
        ("whitebalance.af", 1.0),
        // The RGB-cube probes: one document per feature, whose
        // thumbnail is a byte-exact render, so these pin the whole
        // transfer function rather than one card's worth of it.
        ("cube_vib100.af", 4.0),    // hue window + chroma curve
        ("cube_vib50.af", 3.0),     // the slider law
        ("cube_vibneg100.af", 1.5), // a flat half-chroma scale
        ("cube_lens100.af", 1.0),
        ("cube_lens100_nolum.af", 0.5),
        ("cube_wb_w50t50.af", 0.5),
        ("cube_wb_wneg50t50.af", 0.5),
        ("cube_wb_w100t100.af", 0.5),
        ("cube_wb_wneg100tneg100.af", 0.5),
        // Layer effects, one probe document per panel setting: the
        // mapping is exact, the residuals below are our own effect
        // renderers' falloffs against Affinity's.
        ("fx_bevel_emboss.af", 10.5),
        ("fx_bevel_inner.af", 4.5),
        ("fx_bevel_outer.af", 2.5),
        ("fx_bevel_pillow.af", 16.0),
        ("fx_glow_outer.af", 0.5),
        ("fx_gradient_overlay.af", 0.5),
        ("fx_inner_glow.af", 4.0),
        ("fx_inner_shadow.af", 1.0),
        ("fx_shadow_spread.af", 2.0),
        ("fx_stroke_centre.af", 1.0),
        ("fx_stroke_inside.af", 0.5),
        ("fx_stroke_outside.af", 1.0),
        // Effects and filters we don't implement at all: the bound is
        // the size of the gap, and drops when we do.
        // Pillow bevel at two radii on a mid-grey square. These are
        // the size of the gap, not a tolerance: Affinity's ramp is a
        // distance field `Radi` wide whose slope does not depend on
        // `Radi`, ours is the gradient of an alpha blurred by it, so
        // the wider bevel comes out nearly flat.
        ("bv_pillow_10.af", 17.0),
        ("bv_pillow_30.af", 28.0),
        // Inner glow on a hard-edged square, which isolates the blur
        // from the card's own content: one radius pair for the size
        // convention, one intensity for the post-blur gain.
        // An outside stroke at two widths: `Radi` is the width in
        // pixels exactly, and the outer corner is the shape dilated by
        // it (a quarter circle), so what these pin is the edge.
        ("st_out_10.af", 1.0),
        ("st_out_30.af", 1.2),
        ("ig_r40_i0.af", 1.0),
        ("ig_r80_i0.af", 1.0),
        ("ig_r40_i80.af", 3.0),
        ("fx_3d.af", 11.0),           // PhgB, the 3D effect
        ("fx_gaussian.af", 2.0),      // Gaus, a layer-style blur
        ("flrn_perspective.af", 0.5), // Live Perspective, resampled through
        // The geometric live filters, each on the RGB cube — whose
        // every pixel is a distinct colour, so the render is the
        // filter's own displacement field and the mapping falls out of
        // one file. Two settings each where a second one pins something
        // the first cannot: the twirl's radius, the pinch's sign, the
        // spherical's two directions and the ripple's slider law.
        ("lf_twirl.af", 0.1),
        ("lf_twirl45r80.af", 0.1),
        ("lf_punch.af", 0.1),
        ("lf_sphericalneg.af", 0.1),
        ("lf_lensdist.af", 0.1),
        ("lf_pixelate.af", 0.1),
        ("lf_pixelate32.af", 0.1),
        ("lf_ripple.af", 0.5),
        ("lf_ripple25.af", 1.0),
        ("lf_ripple50.af", 2.5), // the fitted amplitude law, +/-2%
        // Where the filter magnifies, Affinity resamples each 256x256
        // bitmap tile on its own: the source seam at x = y = 256 comes
        // back unblended, and these two disagree along that cross and
        // nowhere else — 99.4% of both cards is bit-identical.
        ("lf_pinch.af", 1.5),
        ("lf_spherical.af", 1.5),
        ("lf_spherical100.af", 2.0),
        // The live blurs. Maximum and median come back bit-exact, down
        // to the darker band the median leaves where its window hangs
        // off the page; the rest are the fitted size conventions.
        ("lf_maxblur.af", 0.1),
        ("lf_medblur.af", 0.1),
        ("lf_box10.af", 0.5),
        ("lf_motion30.af", 0.6),
        ("lf_radial.af", 1.0),
        ("lf_gauss10.af", 0.5), // three box passes, sigma = Radi/3
        ("lf_gauss30.af", 0.5),
        // Unsharp mask over the same blur. The widest of the three is
        // where Affinity's own blur stops scaling with Radi.
        ("lf_usm_r5.af", 0.3),
        ("lf_usm_r10.af", 0.3),
        ("lf_usm_r20.af", 3.0),
        ("lf_highpass.af", 1.0), // mid grey plus half the same detail
        // Dust & Scratches is the median gated by a tolerance, and both
        // probes come back byte-exact.
        ("lf_dust_r8.af", 0.1),
        ("lf_dust_r8t50.af", 0.1),
        // The vignette: one probe per field, all on the same ellipse.
        // `lf_vig_hard1` is a hard edge, where the whole disagreement is
        // the one ring Affinity antialiases and we do not; away from it
        // that probe is at 0.31.
        ("lf_vig_base.af", 1.5),
        ("lf_vig_b.af", 1.5),      // Hard 0, a ramp from the centre
        ("lf_vig_c.af", 1.5),      // Scal 0.5
        ("lf_vig_shap50.af", 1.5), // Shap 0.5, half as wide
        ("lf_vig_hard1.af", 5.0),
        // Shapes — each drawn once with its tool; the embedded
        // thumbnail is Affinity's own render of it.
        ("shp_arrow.af", 1.0),
        ("shp_callout_ellipse.af", 1.0),
        ("shp_callout_rrect.af", 1.0),
        ("shp_cog.af", 2.0),
        ("shp_corner_concave.af", 1.0),
        ("shp_corner_cutout.af", 0.5),
        ("shp_corner_straight.af", 0.5),
        ("shp_crescent.af", 1.0),
        ("shp_diamond.af", 0.5),
        ("shp_donut.af", 1.0),
        ("shp_doublestar.af", 0.5),
        ("shp_pie.af", 1.0),
        ("shp_polygon.af", 0.5),
        ("shp_segment.af", 1.0),
        ("shp_star_curved.af", 4.5), // fitted bow model
        ("shp_tear.af", 2.5),        // fitted profile
        ("shp_trapezoid.af", 0.5),
        ("shp_triangle.af", 0.5),
        ("text_rotated.af", 2.5), // resampled through the rotation
    ];
    for (file, max_rms) in bounds {
        let path = dir.join(file);
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipping: no {}", path.display());
            continue;
        };
        let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
        let thumb = image::load_from_memory(archive.thumbnail().expect("thumb"))
            .expect("decode")
            .to_rgba8();
        let (mut doc, _) = schist_codec_affinity::read_affinity(&bytes).expect("import");
        // The app rebuilds styled rasters when a document opens; without
        // that the layer-effect fixtures would composite bare.
        restyle(&mut doc.tree.layers);
        let region = schist_core::IntRect::from_size(doc.width, doc.height);
        let pixels = schist_compositor::composite_region_rgba8(&doc, region);
        let ours = image::RgbaImage::from_raw(doc.width, doc.height, pixels).expect("buffer");
        // `resize` is a convolution even at the same size, which
        // smears a probe card of hard colour edges into a blur the
        // comparison then blames on the importer; only resample when
        // the thumbnail really is a different size.
        let ours = if ours.dimensions() == thumb.dimensions() {
            ours
        } else {
            image::imageops::resize(
                &ours,
                thumb.width(),
                thumb.height(),
                image::imageops::Triangle,
            )
        };
        let mut sum = 0.0f64;
        let mut n = 0u64;
        for (a, b) in ours.pixels().zip(thumb.pixels()) {
            let over = |p: &image::Rgba<u8>, i: usize| {
                let alpha = p.0[3] as f64 / 255.0;
                p.0[i] as f64 * alpha + 255.0 * (1.0 - alpha)
            };
            for i in 0..3 {
                let d = over(a, i) - over(b, i);
                sum += d * d;
                n += 1;
            }
        }
        let rms = (sum / n as f64).sqrt();
        println!("{file}: rms {rms:.2} (bound {max_rms})");
        assert!(rms <= max_rms, "{file}: rms {rms:.2} above bound {max_rms}");
    }
}
