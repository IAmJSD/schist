//! Text layers, re-set from the stored story, runs and frame box.

use super::*;

impl Walker<'_> {
    /// Rebuild a text layer by re-setting its type.
    ///
    /// The graph stores the string (`StSt` story → `Blok` → `Glyp`
    /// `Utf8`, with U+2029 paragraph breaks), the paragraph alignment
    /// (block `PAtt` → `Ints[0]`: 0 left · 1 centre · 2 right), the
    /// first run's font size (`Doub[0]`) and resolved font
    /// (`RFnt`/`DFnt` PostScript + family names, `Wegt` weight, `Ital`),
    /// the fill colour (run `Objs` → `FDsc.FDeF` → `Colr`), and the
    /// frame box `FrmB` whose transformed bottom edge is the first
    /// baseline. The rendered layer carries the same `PsTx` extras
    /// block the type tool writes, so imported text stays editable.
    pub(super) fn text_layer(&mut self, node: &Node, name: &str) -> Option<Layer> {
        let graph = self.graph;
        let frame_text = &node.type_tag().to_be_bytes() == b"TxtF";
        let story = graph.child(node, b"StSt")?;
        let blocks = graph.children(story, b"Blok");
        let text = blocks
            .iter()
            .filter_map(|b| graph.child(b, b"Glyp"))
            .filter_map(|g| str_of(g, b"Utf8"))
            .map(|s| s.trim_end_matches('\0'))
            .collect::<Vec<_>>()
            .join("\n")
            // Affinity breaks lines with the Unicode paragraph and line
            // separators (and a vertical tab for soft returns).
            .replace(['\u{2028}', '\u{2029}', '\u{000B}'], "\n")
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        if text.trim().is_empty() {
            return None;
        }

        // First block's paragraph attributes give the alignment. Like
        // the character attributes, they sit behind a run list:
        // `PAtt` → `Runs` → `Item` → `Ints[0]`.
        let align = blocks
            .iter()
            .find_map(|b| {
                let runs = graph.children(graph.child(b, b"PAtt")?, b"Runs");
                let item = graph.child(*runs.first()?, b"Item")?;
                match item.field(b"Ints") {
                    Some(Value::Array(v)) => match v.first() {
                        Some(Value::I32(a)) => Some(*a),
                        _ => None,
                    },
                    _ => None,
                }
            })
            .unwrap_or(0);
        let align = match align {
            1 => schist_text_engine::Align::Center,
            2 => schist_text_engine::Align::Right,
            _ => schist_text_engine::Align::Left,
        };

        // First run's character attributes speak for the whole layer.
        let run_item = blocks.iter().find_map(|b| {
            let runs = graph.children(graph.child(b, b"GAtt")?, b"Runs");
            graph.child(*runs.first()?, b"Item")
        })?;
        let size = match run_item.field(b"Doub") {
            Some(Value::Array(d)) => match d.first() {
                Some(Value::F64(s)) => *s,
                _ => return None,
            },
            _ => return None,
        };
        // Two descriptors name the font: `DFnt` is what the document was
        // designed with, `RFnt` what the *writing* machine resolved it
        // to — stale advice on any other machine (real corpus files
        // carry RFnt = Helvetica for a document set in Geist). Affinity
        // re-resolves on open, preferring the document font whenever it
        // — or its family stripped of a trailing parenthetical, since
        // "Geist (Beta)" installs as "Geist" — is present; only a
        // document font this machine lacks falls back to RFnt's advice.
        let installed_name = |f: &&Node| -> Option<String> {
            let fam = str_of(f, b"Famy").filter(|s| !s.is_empty())?;
            if schist_text_engine::has_family(fam) {
                return Some(fam.to_string());
            }
            let head = fam.rsplit_once(" (").map(|(head, _)| head.trim())?;
            (!head.is_empty() && schist_text_engine::has_family(head)).then(|| head.to_string())
        };
        let dfnt = graph.child(run_item, b"DFnt");
        let rfnt = graph.child(run_item, b"RFnt");
        let (font, family, family_installed) = match [dfnt, rfnt]
            .into_iter()
            .flatten()
            .find_map(|f| installed_name(&f).map(|fam| (f, fam)))
        {
            Some((f, fam)) => (Some(f), fam, true),
            // Neither is installed: keep the writing app's resolution if
            // it names anything, and let the engine substitute from there.
            None => {
                let f = [rfnt, dfnt]
                    .into_iter()
                    .flatten()
                    .find(|f| str_of(f, b"Famy").is_some_and(|s| !s.is_empty()));
                let fam = f
                    .and_then(|f| str_of(f, b"Famy"))
                    .unwrap_or_default()
                    .to_string();
                (f, fam, false)
            }
        };
        let post = font
            .and_then(|f| str_of(f, b"Post"))
            .unwrap_or_default()
            .to_string();
        let weight = font.and_then(|f| i32_of(f, b"Wegt")).unwrap_or(400);
        let italic = font.and_then(|f| bool_of(f, b"Ital")).unwrap_or(false);
        let color = run_color(graph, run_item).unwrap_or([0, 0, 0, 255]);

        // Placement: the frame box, through the layer transform. Its
        // bottom edge is the first baseline (the box spans the visual
        // cap height), which is exactly what the rasterizer's layout
        // origin means.
        let frame = graph.child(node, b"TxtH")?;
        let frmb = f64s(frame, b"FrmB").filter(|b| b.len() == 4)?;
        let ctm = self.node_ctm(node);
        // Rotated or sheared text lays out in a de-rotated frame space
        // — the local frame box at document scale, origin at its top
        // left — and the finished raster is pushed through the rotation
        // at the end. Axis-aligned text keeps the direct path.
        let rotated = !ctm.axis_aligned();
        let doc_scale = ctm.scale_y().abs().max(1e-6);
        let (frame_left, frame_top, frame_width, frame_height) = if rotated {
            (
                0,
                0,
                ((frmb[2] - frmb[0]).abs() * doc_scale).round() as i32,
                ((frmb[3] - frmb[1]).abs() * doc_scale).round() as i32,
            )
        } else {
            // The frame box's bounding box through the transform; in
            // the axis-aligned case this is the box itself.
            let corners = [
                ctm.apply(frmb[0], frmb[1]),
                ctm.apply(frmb[2], frmb[1]),
                ctm.apply(frmb[0], frmb[3]),
                ctm.apply(frmb[2], frmb[3]),
            ];
            let min =
                |f: fn(&(f64, f64)) -> f64| corners.iter().map(f).fold(f64::INFINITY, f64::min);
            let max =
                |f: fn(&(f64, f64)) -> f64| corners.iter().map(f).fold(f64::NEG_INFINITY, f64::max);
            (
                min(|c| c.0).round() as i32,
                min(|c| c.1).round() as i32,
                (max(|c| c.0) - min(|c| c.0)).round() as i32,
                (max(|c| c.1) - min(|c| c.1)).round() as i32,
            )
        };
        let eff_size = (size * ctm.scale_y()) as f32;
        if !(0.5..=10_000.0).contains(&eff_size) {
            return None;
        }

        let mut spec = schist_text_engine::TextSpec {
            text,
            family,
            bold: weight >= 600
                || post.contains("Bold")
                || post.contains("Black")
                || post.contains("Heavy"),
            italic: italic || post.contains("Italic") || post.contains("Oblique"),
            size: eff_size,
            align,
            line_height: 1.0,
            tracking: 0.0,
            // Frame text reflows to its box; artistic text never wraps.
            wrap_width: (frame_text && frame_width > 8).then_some(frame_width as f32),
        };
        let mut raster = match schist_text_engine::rasterize(&spec) {
            Some(r) => r,
            None => {
                let fallback = schist_text_engine::default_family();
                log::warn!(
                    "affinity: font {:?} not installed; setting {name:?} in {fallback:?}",
                    spec.family
                );
                spec.family = fallback;
                schist_text_engine::rasterize(&spec)?
            }
        };
        if raster.is_empty() {
            return None;
        }
        // An artistic-text frame box records exactly how wide the
        // *writing* machine set this text — as a pen (advance) box, side
        // bearings and all, not an ink box (corpus files' own exports
        // adjudicated this: their ink starts one left side bearing
        // inside the frame). With the real family installed the natural
        // layout is already right and the frame may reflect someone
        // else's substitute, so leave it alone; when we substitute,
        // scale the size so our advance width still fills the recorded
        // box. Frame text keeps its size and reflows instead.
        if !frame_text && !family_installed && frame_width > 8 && raster.layout_width > 1.0 {
            let ratio = frame_width as f32 / raster.layout_width;
            if (ratio - 1.0).abs() > 0.002 {
                spec.size *= ratio;
                raster = schist_text_engine::rasterize(&spec)?;
                if raster.is_empty() {
                    return None;
                }
            }
        }

        // The frame box of multi-line artistic text runs from the first
        // line's cap down to the last line's baseline, so what it
        // records beyond that cap is exactly the leading Affinity used.
        // Our own leading is whatever the face's line metrics say, which
        // is a different number in every font and in no way the one this
        // document was set with — so solve for it.
        let lines = spec.text.lines().count();
        if !frame_text && lines > 1 && raster.line_advance > 0.0 {
            // The face's declared capital height, not the first line's
            // ink top: ascenders overshoot the cap, and the frame box is
            // measured from the cap.
            let cap = raster
                .cap_height
                .unwrap_or(raster.first_baseline - raster.bounds.top as f32);
            let wanted = (frame_height as f32 - cap) / (lines - 1) as f32;
            let scale = wanted / raster.line_advance;
            // A box that implies a collapsed or wildly stretched leading
            // is one we have misread; keep the face's own spacing.
            if (0.25..=4.0).contains(&scale) && (scale - 1.0).abs() > 0.01 {
                spec.line_height *= scale;
                raster = schist_text_engine::rasterize(&spec)?;
                if raster.is_empty() {
                    return None;
                }
            }
        }

        // Anchor our pen box to the frame box: the layout already put
        // the widest line's pen at 0 and aligned the rest, so the frame
        // left is the origin for every alignment, and the ink lands one
        // side bearing inside it exactly as Affinity draws it. (When the
        // engine measured no advances, fall back to ink anchoring.)
        let origin_x = if raster.layout_width > 1.0 {
            match spec.align {
                schist_text_engine::Align::Center if frame_width > 8 => {
                    frame_left + (frame_width - raster.layout_width.round() as i32) / 2
                }
                schist_text_engine::Align::Right if frame_width > 8 => {
                    frame_left + frame_width - raster.layout_width.round() as i32
                }
                _ => frame_left,
            }
        } else {
            frame_left - raster.bounds.left
        };
        // Vertically, artistic text anchors by baseline: the frame's
        // bottom edge is the last line's baseline (the first line's, for
        // one line), which no ascender-vs-cap disagreement can move.
        // Frame text keeps ink-top anchoring to its frame.
        let origin_y = if !frame_text && raster.first_baseline > 0.0 {
            let last_baseline =
                raster.first_baseline + (lines.max(1) - 1) as f32 * raster.line_advance;
            frame_top + frame_height - last_baseline.round() as i32
        } else {
            frame_top - raster.bounds.top
        };
        let origin = (origin_x, origin_y);
        // Affinity's panel names text layers after their content.
        let display_name = if name.is_empty() {
            let first_line = spec.text.lines().next().unwrap_or("Text").trim();
            let mut label: String = first_line.chars().take(32).collect();
            if label.len() < first_line.len() {
                label.push('…');
            }
            if label.is_empty() {
                label = "Text".to_string();
            }
            label
        } else {
            name.to_string()
        };
        let mut layer = Layer::new_raster(display_name);
        let bounds = raster.bounds.translated(origin.0, origin.1);
        let mut rgba = vec![0u8; raster.coverage.len() * 4];
        for (px, &cov) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&raster.coverage) {
            px[0] = color[0];
            px[1] = color[1];
            px[2] = color[2];
            px[3] = (cov as u16 * color[3] as u16 / 255) as u8;
        }
        if rotated {
            // Map layout space back through the rotation: a layout
            // pixel p sits at ctm · (frame_local_origin + p/scale).
            let c0 = ctm.apply(frmb[0].min(frmb[2]), frmb[1].min(frmb[3]));
            let lin = Mat([
                ctm.0[0] / doc_scale,
                ctm.0[1] / doc_scale,
                0.0,
                ctm.0[3] / doc_scale,
                ctm.0[4] / doc_scale,
                0.0,
            ]);
            let o = lin.apply(bounds.left as f64, bounds.top as f64);
            let map = Mat([
                lin.0[0],
                lin.0[1],
                o.0 + c0.0,
                lin.0[3],
                lin.0[4],
                o.1 + c0.1,
            ]);
            let img = RgbaImage {
                width: raster.bounds.width() as u32,
                height: raster.bounds.height() as u32,
                pixels: rgba,
            };
            let (rect, out) = affine_resample(&img, &map)?;
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                rect,
                &out.pixels,
            );
        } else {
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                bounds,
                &rgba,
            );
        }

        // The type tool's persistence block, so double-clicking with T
        // reopens this text for editing.
        #[derive(serde::Serialize)]
        struct StoredText<'a> {
            spec: &'a schist_text_engine::TextSpec,
            origin: (i32, i32),
            color: [u8; 4],
        }
        if let Ok(data) = serde_json::to_vec(&StoredText {
            spec: &spec,
            origin,
            color,
        }) {
            layer.extras.push(schist_core::RawBlock {
                key: *b"PsTx",
                data,
            });
        }
        self.report.text_layers += 1;
        Some(layer)
    }
}
