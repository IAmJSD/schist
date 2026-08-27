//! Affinity (.afphoto / .afdesign / .afpub / .af) import and export.
//!
//! Two readers, best first:
//!
//! 1. **Structured** — `schist-codec-affinity` parses the actual
//!    (reverse-engineered) format: container, object graph, layer tree,
//!    raster tiles. When it recovers every layer's pixels, the document
//!    opens layered, with names, opacity, visibility and blend modes.
//! 2. **Flattened preview** — layers Affinity would re-render live
//!    (shapes, text, adjustments) have no pixels in the file. Whenever
//!    the structured read can't show the complete picture, fall back to
//!    the flattened PNG/JPEG preview the apps embed in the clear,
//!    carved straight out of the raw bytes.
//!
//! Export writes a layered version-12 (unified ".af") container:
//! rasters as native tiles, groups, masks, clipping, blend modes, and
//! adjustment layers that carry preserved native parameters. Text and
//! vector layers export their rasterized pixels. The file embeds a
//! composited preview thumbnail, like Affinity's own saves.

use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};
use schist_plugin_api::CodecPlugin;

/// Every Affinity file starts with these bytes.
const MAGIC: [u8; 4] = [0x00, 0xFF, 0x4B, 0x41];

const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const PNG_IEND: [u8; 4] = *b"IEND";

/// A carved candidate preview stream, dimensions read from its header.
struct Candidate {
    width: u32,
    height: u32,
    range: std::ops::Range<usize>,
}

/// Import for the whole Affinity family. One codec covers Photo,
/// Designer and Publisher: the container (and its embedded preview) is
/// the same, only the object graph differs — and we never parse that.
pub struct AffinityCodec;

impl CodecPlugin for AffinityCodec {
    fn id(&self) -> &'static str {
        "codec.affinity"
    }
    fn name(&self) -> &'static str {
        "Affinity"
    }
    fn extensions(&self) -> &'static [&'static str] {
        // ".af" is the unified extension of Canva-era Affinity; the
        // container inside is the same family (version 12).
        &["afphoto", "afdesign", "afpub", "af"]
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        match schist_codec_affinity::read_affinity(bytes) {
            Ok((doc, report)) if report.complete() && !doc.tree.layers.is_empty() => {
                return Ok(doc);
            }
            // Partial recovery still beats the preview — real files have
            // been seen shipping stale, half-rendered previews — as long
            // as some actual pixels came through. The skipped layers
            // (text, shapes, fills) are covered by tucking the preview
            // underneath as a hidden reference layer.
            Ok((mut doc, report)) if report.raster_layers > 0 => {
                log::info!(
                    "affinity: {} layer(s) have no pixels in the file ({:?}); \
                     adding the flattened preview as a hidden reference layer",
                    report.skipped.len(),
                    report
                        .skipped
                        .iter()
                        .map(|(name, kind)| format!("{name} [{kind}]"))
                        .collect::<Vec<_>>()
                );
                if let Some(mut reference) = self.preview_layer(bytes, doc.width, doc.height) {
                    reference.visible = false;
                    doc.tree.layers.insert(0, reference);
                    doc.damage_all();
                    doc.mark_saved();
                }
                return Ok(doc);
            }
            Ok((_, report)) => log::info!(
                "affinity: no pixel layers recoverable ({} skipped); \
                 opening the flattened preview instead",
                report.skipped.len(),
            ),
            Err(e) => log::info!("affinity: structured parse failed ({e}); using preview"),
        }
        self.import_preview(bytes)
    }

    fn can_export(&self) -> bool {
        true
    }

    fn export(&self, doc: &Document) -> anyhow::Result<Vec<u8>> {
        let thumbnail = self.render_thumbnail(doc);
        let (bytes, report) = schist_codec_affinity::write_affinity(doc, thumbnail.as_deref())?;
        for (layer, why) in &report.skipped {
            log::warn!("affinity export: {layer:?}: {why}");
        }
        Ok(bytes)
    }
}

impl AffinityCodec {
    /// The embedded preview written on export: the composited document,
    /// scaled to Affinity's usual 512-pixel thumbnail box, as PNG.
    fn render_thumbnail(&self, doc: &Document) -> Option<Vec<u8>> {
        let region = doc.canvas_rect();
        if region.is_empty() {
            return None;
        }
        let pixels = schist_compositor::composite_region_f32(doc, region);
        let rgba: Vec<u8> = pixels
            .iter()
            .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
            .collect();
        let img: image::RgbaImage = image::ImageBuffer::from_raw(doc.width, doc.height, rgba)?;
        let scale = 512.0 / doc.width.max(doc.height).max(1) as f32;
        let img = if scale < 1.0 {
            image::imageops::resize(
                &img,
                ((doc.width as f32 * scale).round() as u32).max(1),
                ((doc.height as f32 * scale).round() as u32).max(1),
                image::imageops::Triangle,
            )
        } else {
            img
        };
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).ok()?;
        Some(out.into_inner())
    }

    /// The largest embedded PNG/JPEG preview that actually decodes.
    fn best_preview(&self, bytes: &[u8]) -> Option<image::RgbaImage> {
        let mut candidates = scan_pngs(bytes);
        candidates.extend(scan_jpegs(bytes));
        // Largest first; a candidate that fails to decode (truncated, or a
        // false positive inside the compressed object graph) just means we
        // fall through to the next one.
        candidates.sort_by_key(|c| std::cmp::Reverse(c.width as u64 * c.height as u64));
        for c in &candidates {
            let Ok(img) = image::load_from_memory(&bytes[c.range.clone()]) else {
                continue;
            };
            let rgba = img.to_rgba8();
            if rgba.width() > 0 && rgba.height() > 0 {
                return Some(rgba);
            }
        }
        None
    }

    /// The preview scaled to canvas size, as a reference layer.
    fn preview_layer(&self, bytes: &[u8], width: u32, height: u32) -> Option<Layer> {
        let mut rgba = self.best_preview(bytes)?;
        if (rgba.width(), rgba.height()) != (width, height) {
            rgba = image::imageops::resize(&rgba, width, height, image::imageops::Triangle);
        }
        let mut layer = Layer::new_raster("Flattened preview");
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(width, height),
            rgba.as_raw(),
        );
        Some(layer)
    }

    /// The flattened-preview fallback: open the preview as the document.
    fn import_preview(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        if let Some(rgba) = self.best_preview(bytes) {
            let (w, h) = rgba.dimensions();
            let mut doc = Document::new("Affinity import", w, h, Depth::Eight);
            let mut layer = Layer::new_raster("Background");
            blit_rgba8(
                &mut layer.as_raster_mut().unwrap().tiles,
                Depth::Eight,
                IntRect::from_size(w, h),
                rgba.as_raw(),
            );
            doc.push_layer(layer);
            doc.damage_all();
            doc.mark_saved();
            return Ok(doc);
        }
        anyhow::bail!(
            "no usable embedded preview. Affinity's layered format is \
             proprietary; Schist imports the flattened preview the file \
             carries. This file has none — re-save it in Affinity, or export \
             it as PSD for a full layered import."
        )
    }
}

/// Carve every complete PNG stream: signature through `IEND` + CRC.
/// Dimensions come straight from the `IHDR` that must follow the
/// signature (length + type + width at +16, height at +20).
fn scan_pngs(bytes: &[u8]) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = find(&bytes[search..], &PNG_SIG) {
        let start = search + rel;
        // Always move past this signature so a malformed candidate can't
        // wedge the scan.
        search = start + PNG_SIG.len();
        if start + 24 > bytes.len() {
            continue;
        }
        let width = u32::from_be_bytes(bytes[start + 16..start + 20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[start + 20..start + 24].try_into().unwrap());
        let Some(iend_rel) = find(&bytes[start..], &PNG_IEND) else {
            continue;
        };
        let end = start + iend_rel + PNG_IEND.len() + 4; // + CRC
        if end > bytes.len() {
            continue;
        }
        out.push(Candidate {
            width,
            height,
            range: start..end,
        });
    }
    out
}

/// Carve every complete JPEG stream by walking its marker segments.
///
/// A byte-scan for the `FF D9` end marker is not enough: an EXIF block
/// can hold a nested thumbnail JPEG whose end marker would truncate the
/// outer stream. Walking segments skips such blocks whole, picks
/// dimensions up from the frame header, and only treats `FF D9` as the
/// end once past the entropy-coded data.
fn scan_jpegs(bytes: &[u8]) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = find(&bytes[search..], &[0xFF, 0xD8, 0xFF]) {
        let start = search + rel;
        match walk_jpeg(bytes, start) {
            Some(c) => {
                search = c.range.end;
                out.push(c);
            }
            None => search = start + 3,
        }
    }
    out
}

/// Walk one JPEG's segments from its `FF D8` at `start`. Returns the
/// stream's extent and frame dimensions, or `None` if it isn't a
/// well-formed JPEG.
fn walk_jpeg(bytes: &[u8], start: usize) -> Option<Candidate> {
    let mut pos = start + 2;
    let mut dims: Option<(u32, u32)> = None;
    loop {
        if pos + 2 > bytes.len() || bytes[pos] != 0xFF {
            return None;
        }
        let marker = bytes[pos + 1];
        match marker {
            // EOI: done. Require dimensions, so a stray SOI in noise
            // followed by a stray EOI doesn't produce a 0×0 candidate.
            0xD9 => {
                let (width, height) = dims?;
                return Some(Candidate {
                    width,
                    height,
                    range: start..pos + 2,
                });
            }
            // Standalone markers with no payload.
            0x01 | 0xD0..=0xD7 => pos += 2,
            // A second SOI mid-stream is malformed.
            0xD8 => return None,
            // Start of scan: skip its header, then entropy-coded data,
            // where `FF` is followed by 00 (stuffing) or D0–D7 (restart)
            // until the next real marker (another scan, or EOI).
            0xDA => {
                pos += 2 + segment_len(bytes, pos + 2)?;
                loop {
                    if pos + 2 > bytes.len() {
                        return None;
                    }
                    if bytes[pos] == 0xFF && !matches!(bytes[pos + 1], 0x00 | 0xD0..=0xD7) {
                        break;
                    }
                    pos += 1;
                }
            }
            // Frame headers (SOF0–SOF15, minus DHT/JPG/DAC which share
            // the range): payload is precision(1), height(2), width(2).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let len = segment_len(bytes, pos + 2)?;
                if len < 7 {
                    return None;
                }
                let height = u16::from_be_bytes(bytes[pos + 5..pos + 7].try_into().unwrap());
                let width = u16::from_be_bytes(bytes[pos + 7..pos + 9].try_into().unwrap());
                dims = Some((width as u32, height as u32));
                pos += 2 + len;
            }
            // Everything else is a plain length-prefixed segment.
            _ => pos += 2 + segment_len(bytes, pos + 2)?,
        }
    }
}

/// The 2-byte big-endian segment length at `at` (which counts itself),
/// checked to fit inside `bytes`.
fn segment_len(bytes: &[u8], at: usize) -> Option<usize> {
    if at + 2 > bytes.len() {
        return None;
    }
    let len = u16::from_be_bytes([bytes[at], bytes[at + 1]]) as usize;
    (len >= 2 && at + len <= bytes.len()).then_some(len)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat;

    fn encode(img: &image::RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        match format {
            ImageFormat::Jpeg => {
                let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
                rgb.write_to(&mut out, format).unwrap()
            }
            _ => img.write_to(&mut out, format).unwrap(),
        }
        out.into_inner()
    }

    fn solid(w: u32, h: u32, color: [u8; 4]) -> image::RgbaImage {
        image::RgbaImage::from_pixel(w, h, image::Rgba(color))
    }

    /// A synthetic Affinity-shaped container: magic, header-ish filler,
    /// embedded payloads separated by noise, compressed-looking trailer.
    fn fake_affinity(payloads: &[&[u8]]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&MAGIC);
        f.extend_from_slice(b"\x0b\x00\x00\x00nsrP#InfG");
        for p in payloads {
            f.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
            f.extend_from_slice(p);
        }
        f.extend_from_slice(b"(zstd object graph would be here)");
        f
    }

    #[test]
    fn probes_magic_only() {
        assert!(AffinityCodec.probe(&fake_affinity(&[])));
        assert!(!AffinityCodec.probe(b"\x89PNG"));
        assert!(!AffinityCodec.probe(b""));
    }

    #[test]
    fn imports_largest_preview_across_formats() {
        // Small PNG thumbnail plus a larger JPEG preview — the JPEG wins.
        let png = encode(&solid(64, 64, [255, 0, 0, 255]), ImageFormat::Png);
        let jpeg = encode(&solid(320, 200, [0, 0, 255, 255]), ImageFormat::Jpeg);
        let f = fake_affinity(&[&png, &jpeg]);

        let doc = AffinityCodec.import(&f).unwrap();
        assert_eq!((doc.width, doc.height), (320, 200));
        let px = doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(10, 10)
            .to_u8();
        // JPEG is lossy; a solid field still lands within a hair of it.
        assert!(px[2] > 240 && px[0] < 16, "expected blue, got {px:?}");
        assert_eq!(px[3], 255);
    }

    #[test]
    fn falls_back_when_largest_candidate_is_truncated() {
        let good = encode(&solid(50, 40, [0, 255, 0, 255]), ImageFormat::Png);
        // A bigger PNG cut off mid-stream: carved (header intact, an IEND
        // from the following payload terminates it) but undecodable.
        let mut broken = encode(&solid(400, 400, [9, 9, 9, 255]), ImageFormat::Png);
        broken.truncate(60);
        let f = fake_affinity(&[&broken, &good]);

        let doc = AffinityCodec.import(&f).unwrap();
        assert_eq!((doc.width, doc.height), (50, 40));
    }

    #[test]
    fn no_preview_is_a_clear_error() {
        let err = AffinityCodec.import(&fake_affinity(&[])).unwrap_err();
        assert!(err.to_string().contains("preview"), "{err}");
    }

    #[test]
    fn jpeg_walker_skips_nested_exif_thumbnail() {
        // Outer JPEG carrying a whole JPEG inside an APP1 segment. A naive
        // scan-to-FF-D9 would end at the thumbnail's EOI; the segment
        // walker must return the full outer stream.
        let inner = encode(&solid(8, 8, [1, 2, 3, 255]), ImageFormat::Jpeg);
        let outer = encode(&solid(120, 90, [200, 150, 100, 255]), ImageFormat::Jpeg);
        let mut spliced = Vec::new();
        spliced.extend_from_slice(&outer[..2]); // SOI
        spliced.extend_from_slice(&[0xFF, 0xE1]); // APP1
        spliced.extend_from_slice(&((inner.len() + 2) as u16).to_be_bytes());
        spliced.extend_from_slice(&inner);
        spliced.extend_from_slice(&outer[2..]);

        let carved = scan_jpegs(&fake_affinity(&[&spliced]));
        assert!(
            carved.iter().any(|c| (c.width, c.height) == (120, 90)),
            "outer JPEG not carved whole"
        );
        let doc = AffinityCodec.import(&fake_affinity(&[&spliced])).unwrap();
        assert_eq!((doc.width, doc.height), (120, 90));
    }

    /// The real Affinity corpus in fixtures/affinity/ must open through
    /// the full codec (structured import or preview fallback). The
    /// corpus is optional, so an empty checkout still passes.
    #[test]
    fn fixture_corpus_imports() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/affinity");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("skipping: no fixtures at {}", dir.display());
            return;
        };
        for entry in entries.flatten() {
            let is_affinity_file = entry
                .path()
                .extension()
                .is_some_and(|x| x.to_string_lossy().starts_with("af"));
            if !is_affinity_file {
                continue; // the corpus README
            }
            let bytes = std::fs::read(entry.path()).unwrap();
            assert!(
                AffinityCodec.probe(&bytes),
                "{:?} lacks magic",
                entry.path()
            );
            let doc = AffinityCodec.import(&bytes).unwrap();
            assert!(doc.width > 0 && doc.height > 0);
        }
    }
}
