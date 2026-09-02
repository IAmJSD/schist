//! One picture of a document, rendered without a window.
//!
//! This is what a file browser wants: not the editable document, just
//! the image, at some size, soon. macOS Quick Look is the caller that
//! shaped it — it gives an extension seconds, and a layered 2 GB PSB
//! does not open in seconds — but nothing here is macOS-specific.
//!
//! Two ways to the same picture, cheapest first:
//!
//! 1. **The embedded preview.** Photoshop writes a JPEG thumbnail into
//!    an image resource; Affinity writes a PNG of the page into the
//!    container. Both are the writing app's own render, so when one is
//!    large enough for the size asked for, it is both the fastest and
//!    the most faithful answer available.
//! 2. **A real composite.** Otherwise the file opens through the same
//!    codecs and compositor the editor uses — layers, masks, blend
//!    modes, adjustment layers and layer effects included — and the
//!    result is scaled down in bands, so peak memory follows the
//!    requested size rather than the document's.
//!
//! A camera raw is the same shape: the JPEG the camera embedded, else
//! the developed sensor data. Everything else (PNG, JPEG, WebP, TIFF)
//! decodes and scales directly.

use anyhow::{bail, Context as _, Result};
use image::RgbaImage;
use schist_core::{Document, IntRect, TILE_SIZE};
use schist_plugin_api::CodecPlugin as _;
use std::path::Path;

/// Ceiling on the longest edge of a rendered preview.
///
/// The downscale accumulator is four floats per output pixel, so this is
/// what bounds it: 2048 costs 67 MB, and no preview surface asks for
/// more (a Quick Look panel on a Retina display tops out around 1600).
pub const MAX_EDGE: u32 = 2048;

/// Composite no more than this many pixels at a time. A band of tiles
/// costs 16 bytes a pixel while it is being composited, so this is the
/// knob that keeps a 100-megapixel document inside 64 MB.
const BAND_PIXELS: u32 = 4 << 20;

/// Above this canvas size, an embedded preview wins even when it is
/// smaller than the size asked for: compositing a document this large
/// takes longer than any thumbnail is worth.
const COMPOSITE_PIXEL_BUDGET: u64 = 64 << 20;

/// Where a preview's pixels came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The preview the writing app embedded in the file.
    Embedded,
    /// A full composite of the document, through Schist's own codecs.
    Composited,
    /// A plain image format, decoded as-is.
    Decoded,
}

/// A rendered preview: straight-alpha RGBA8, `width * height * 4` bytes.
pub struct Preview {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub source: Source,
}

impl Preview {
    fn from_image(img: RgbaImage, source: Source) -> Preview {
        Preview {
            width: img.width(),
            height: img.height(),
            rgba: img.into_raw(),
            source,
        }
    }

    /// The preview as a PNG.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let img = RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .context("preview buffer had the wrong size")?;
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .context("encoding the preview as PNG")?;
        Ok(out.into_inner())
    }
}

/// Render a preview of the file at `path`, no larger than `max_edge` on
/// its longest side.
pub fn render_file(path: &Path, max_edge: u32) -> Result<Preview> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    render(&bytes, max_edge)
}

/// Render a preview of a file already in memory.
///
/// The format is detected from the bytes, never from a file name, and
/// `max_edge` is clamped to `16..=`[`MAX_EDGE`] — a thumbnail smaller
/// than that is an icon, and drawing one is the caller's business.
pub fn render(bytes: &[u8], max_edge: u32) -> Result<Preview> {
    let max_edge = max_edge.clamp(16, MAX_EDGE);
    if schist_codec_psd::is_psd(bytes) {
        psd_preview(bytes, max_edge)
    } else if schist_codecs_common::AffinityCodec.probe(bytes) {
        affinity_preview(bytes, max_edge)
    } else {
        // HEIC before the generic decoder: the `image` crate does not
        // read it, and an iPhone's camera roll is mostly HEIC — a
        // gallery of "no preview" was how that surfaced. The error when
        // libheif is absent is kept intact, so a caller can recognize
        // it and offer the managed download.
        #[cfg(not(target_arch = "wasm32"))]
        if schist_codecs_common::HeifCodec.probe(bytes) {
            use schist_plugin_api::CodecPlugin as _;
            let doc = schist_codecs_common::HeifCodec.import(bytes)?;
            return composite_preview(doc, max_edge);
        }
        // Camera raws likewise, and before the generic decoder for a
        // second reason: most of them are TIFF containers, which it
        // would open and hand back the thumbnail IFD of.
        #[cfg(not(target_arch = "wasm32"))]
        if schist_codecs_common::RawCodec.probe(bytes) {
            return raw_preview(bytes, max_edge);
        }
        let img = image::load_from_memory(bytes)
            .context("no Schist codec and no image decoder recognized this file")?
            .into_rgba8();
        Ok(Preview::from_image(fit(img, max_edge), Source::Decoded))
    }
}

/// PSD/PSB: the image resource thumbnail, else the composited document.
fn psd_preview(bytes: &[u8], max_edge: u32) -> Result<Preview> {
    let embedded = schist_codec_psd::read_thumbnail(bytes).and_then(decode_psd_thumbnail);
    let canvas_pixels = schist_codec_psd::read_dimensions(bytes)
        .map(|(w, h)| w as u64 * h as u64)
        .unwrap_or(0);
    if let Some(img) = &embedded {
        if img.width().max(img.height()) >= max_edge || canvas_pixels > COMPOSITE_PIXEL_BUDGET {
            return Ok(Preview::from_image(
                fit(img.clone(), max_edge),
                Source::Embedded,
            ));
        }
    }
    match schist_codec_psd::read_psd(bytes) {
        Ok(doc) => composite_preview(doc, max_edge),
        Err(e) => match embedded {
            // A file Schist cannot open still has a picture in it, and
            // showing that beats showing the generic document icon.
            Some(img) => {
                log::info!("psd: opening failed ({e}); falling back to the embedded thumbnail");
                Ok(Preview::from_image(fit(img, max_edge), Source::Embedded))
            }
            None => Err(e.into()),
        },
    }
}

/// The thumbnail resource holds a JPEG, in BGR order on files old enough.
fn decode_psd_thumbnail(thumb: schist_codec_psd::Thumbnail<'_>) -> Option<RgbaImage> {
    let mut img = image::load_from_memory_with_format(thumb.jpeg, image::ImageFormat::Jpeg)
        .map_err(|e| log::info!("psd: embedded thumbnail did not decode: {e}"))
        .ok()?
        .into_rgba8();
    if thumb.bgr {
        for px in img.pixels_mut() {
            px.0.swap(0, 2);
        }
    }
    Some(img)
}

/// Affinity: the container's own PNG preview, else the imported document.
fn affinity_preview(bytes: &[u8], max_edge: u32) -> Result<Preview> {
    let embedded = schist_codec_affinity::Archive::parse(bytes)
        .ok()
        .and_then(|archive| archive.thumbnail())
        .and_then(|png| {
            image::load_from_memory(png)
                .map_err(|e| log::info!("affinity: embedded thumbnail did not decode: {e}"))
                .ok()
        })
        .map(|img| img.into_rgba8());
    if let Some(img) = &embedded {
        if img.width().max(img.height()) >= max_edge {
            return Ok(Preview::from_image(
                fit(img.clone(), max_edge),
                Source::Embedded,
            ));
        }
    }
    // The Affinity codec already falls back to the flattened preview for
    // files whose layers it cannot recover, so a failure here is real.
    match schist_codecs_common::AffinityCodec.import(bytes) {
        Ok(doc) => composite_preview(doc, max_edge),
        Err(e) => match embedded {
            Some(img) => {
                log::info!("affinity: import failed ({e}); falling back to the embedded thumbnail");
                Ok(Preview::from_image(fit(img, max_edge), Source::Embedded))
            }
            None => Err(e),
        },
    }
}

/// Camera raw: the camera's own JPEG, else the developed sensor data.
///
/// The embedded preview is the camera's render of the same capture and
/// is often full size, while developing the raw takes seconds, so the
/// preview wins whenever it is big enough for the size asked for — and
/// stands in when development fails, the way the PSD thumbnail does.
/// A missing LibRaw is reported as it is, so a caller can tell it from
/// a broken file.
#[cfg(not(target_arch = "wasm32"))]
fn raw_preview(bytes: &[u8], max_edge: u32) -> Result<Preview> {
    let embedded = match schist_codecs_common::raw::embedded_preview(bytes) {
        Ok(img) => img,
        Err(e) if schist_codecs_common::raw::is_missing_library_error(&e) => return Err(e),
        Err(e) => {
            log::info!("raw: embedded preview did not decode: {e}");
            None
        }
    };
    if let Some(img) = &embedded {
        if img.width().max(img.height()) >= max_edge {
            return Ok(Preview::from_image(
                fit(img.clone(), max_edge),
                Source::Embedded,
            ));
        }
    }
    match schist_codecs_common::RawCodec.import(bytes) {
        Ok(doc) => composite_preview(doc, max_edge),
        Err(e) => match embedded {
            Some(img) => {
                log::info!("raw: developing failed ({e}); falling back to the embedded preview");
                Ok(Preview::from_image(fit(img, max_edge), Source::Embedded))
            }
            None => Err(e),
        },
    }
}

/// Composite a document to at most `max_edge` on its longest side.
fn composite_preview(mut doc: Document, max_edge: u32) -> Result<Preview> {
    // Layer effects live in a cache the compositor blends and does not
    // build; a freshly imported document has none yet.
    let mut damage = Vec::new();
    schist_compositor::restyle_layers(&mut doc.tree.layers, &mut damage);

    let canvas = doc.canvas_rect();
    if canvas.is_empty() {
        bail!("the document has no canvas to render");
    }
    let (w, h) = (canvas.width() as u32, canvas.height() as u32);
    let (tw, th) = fit_size(w, h, max_edge);

    let mut acc = Accumulator::new(w, h, tw, th);
    let rows = band_rows(w);
    let mut top = canvas.top;
    while top < canvas.bottom {
        let bottom = (top + rows as i32).min(canvas.bottom);
        let band = IntRect {
            left: canvas.left,
            top,
            right: canvas.right,
            bottom,
        };
        let pixels = schist_compositor::composite_region_rgba8(&doc, band);
        acc.add_band(&pixels, (top - canvas.top) as u32, (bottom - top) as u32);
        top = bottom;
    }

    let img = RgbaImage::from_raw(tw, th, acc.finish())
        .context("downscaled preview had the wrong size")?;
    Ok(Preview::from_image(img, Source::Composited))
}

/// How many document rows to composite at once.
fn band_rows(width: u32) -> u32 {
    let tile = TILE_SIZE as u32;
    let rows = (BAND_PIXELS / width.max(1)).max(tile);
    // Whole tile rows: the compositor works in tiles either way, so a
    // band that ends mid-tile only composites the same tile twice.
    (rows / tile) * tile
}

/// The size `w * h` scales to inside a `max_edge` box, never upscaling.
fn fit_size(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    let longest = w.max(h);
    if longest <= max_edge || longest == 0 {
        return (w.max(1), h.max(1));
    }
    let scale = max_edge as f64 / longest as f64;
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// Scale a decoded image into the `max_edge` box, if it is over it.
fn fit(img: RgbaImage, max_edge: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let (tw, th) = fit_size(w, h, max_edge);
    if (tw, th) == (w, h) {
        return img;
    }
    image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle)
}

/// A box-filter downscale that never holds the whole source.
///
/// Source pixel `(x, y)` falls in output cell `(x * tw / w, y * th / h)`,
/// a mapping that is separable, so the number of source pixels behind a
/// cell is the product of two small tables rather than a third image.
/// Colour is summed weighted by alpha (a transparent pixel must not drag
/// a cell's colour towards black) and unweighted at the end.
struct Accumulator {
    w: u32,
    h: u32,
    tw: u32,
    th: u32,
    /// Per output cell: alpha-weighted r, g, b, then plain alpha.
    sums: Vec<f32>,
    cols: Vec<u32>,
    rows: Vec<u32>,
}

impl Accumulator {
    fn new(w: u32, h: u32, tw: u32, th: u32) -> Accumulator {
        let mut cols = vec![0u32; tw as usize];
        for x in 0..w {
            cols[(x as u64 * tw as u64 / w as u64) as usize] += 1;
        }
        let mut rows = vec![0u32; th as usize];
        for y in 0..h {
            rows[(y as u64 * th as u64 / h as u64) as usize] += 1;
        }
        Accumulator {
            w,
            h,
            tw,
            th,
            sums: vec![0.0; tw as usize * th as usize * 4],
            cols,
            rows,
        }
    }

    /// Fold in `rows` rows of RGBA8 starting at document row `first_row`.
    fn add_band(&mut self, pixels: &[u8], first_row: u32, rows: u32) {
        // Precompute the column mapping once for the whole band.
        let col: Vec<u32> = (0..self.w)
            .map(|x| (x as u64 * self.tw as u64 / self.w as u64) as u32)
            .collect();
        for row in 0..rows {
            let ty = ((first_row + row) as u64 * self.th as u64 / self.h as u64) as usize;
            let src = row as usize * self.w as usize * 4;
            let src = &pixels[src..src + self.w as usize * 4];
            for (x, px) in src.as_chunks::<4>().0.iter().enumerate() {
                let a = px[3] as f32 / 255.0;
                let d = (ty * self.tw as usize + col[x] as usize) * 4;
                self.sums[d] += px[0] as f32 * a;
                self.sums[d + 1] += px[1] as f32 * a;
                self.sums[d + 2] += px[2] as f32 * a;
                self.sums[d + 3] += a;
            }
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut out = vec![0u8; self.tw as usize * self.th as usize * 4];
        for ty in 0..self.th as usize {
            for tx in 0..self.tw as usize {
                let i = (ty * self.tw as usize + tx) * 4;
                let count = (self.cols[tx] * self.rows[ty]) as f32;
                if count == 0.0 {
                    continue;
                }
                let alpha_sum = self.sums[i + 3];
                if alpha_sum > 0.0 {
                    for c in 0..3 {
                        out[i + c] = (self.sums[i + c] / alpha_sum).round().clamp(0.0, 255.0) as u8;
                    }
                }
                out[i + 3] = (self.sums[i + 3] / count * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_size_keeps_the_aspect_and_never_upscales() {
        assert_eq!(fit_size(800, 600, 200), (200, 150));
        assert_eq!(fit_size(600, 800, 200), (150, 200));
        // Already inside the box: left exactly as it is.
        assert_eq!(fit_size(120, 90, 200), (120, 90));
        // A sliver still has a pixel of height.
        assert_eq!(fit_size(4000, 3, 100), (100, 1));
    }

    #[test]
    fn transparent_pixels_dilute_coverage_without_tinting_colour() {
        let mut acc = Accumulator::new(2, 1, 1, 1);
        // Opaque red beside fully transparent green.
        acc.add_band(&[255, 0, 0, 255, 0, 255, 0, 0], 0, 1);
        let out = acc.finish();
        // The green never happened, as far as colour goes...
        assert_eq!(&out[..3], &[255, 0, 0]);
        // ... but the cell is only half covered.
        assert_eq!(out[3], 128);
    }

    #[test]
    fn a_banded_downscale_matches_the_rows_it_came_from() {
        let white = [255u8, 255, 255, 255];
        let black = [0u8, 0, 0, 255];
        let row = |c: [u8; 4]| c.repeat(4);
        let mut acc = Accumulator::new(4, 4, 2, 2);
        // Two bands of two rows: white on top, black underneath.
        acc.add_band(&[row(white), row(white)].concat(), 0, 2);
        acc.add_band(&[row(black), row(black)].concat(), 2, 2);
        let out = acc.finish();
        assert_eq!(&out[0..8], &[255, 255, 255, 255, 255, 255, 255, 255]);
        assert_eq!(&out[8..16], &[0, 0, 0, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn bands_cover_whole_tile_rows() {
        assert_eq!(band_rows(1024) % TILE_SIZE as u32, 0);
        assert_eq!(band_rows(100_000), TILE_SIZE as u32);
        assert!(band_rows(512) * 512 <= BAND_PIXELS);
    }
}
