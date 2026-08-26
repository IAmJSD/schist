//! Common raster format codecs (PNG, JPEG, WebP, TIFF) via the `image`
//! crate, wrapped as `CodecPlugin`s, plus layered Affinity
//! import/export. For the simple formats, import produces a single
//! "Background" layer and export flattens through the compositor.

pub use affinity::AffinityCodec;
use anyhow::Context as _;
use image::ImageFormat;
use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};
use schist_plugin_api::{CodecPlugin, ExportOptions, PluginManifest, PluginRegistry};

mod affinity;

/// The four cICP fields (primaries, transfer, matrix, full-range) from a
/// PNG's cICP chunk, if one is present before the image data.
fn png_cicp(bytes: &[u8]) -> Option<[u8; 4]> {
    let mut rest = bytes.get(8..)?;
    while rest.len() >= 12 {
        let len = u32::from_be_bytes(rest[0..4].try_into().ok()?) as usize;
        let kind = &rest[4..8];
        if kind == b"cICP" {
            return <[u8; 4]>::try_from(rest.get(8..8 + len)?).ok();
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return None;
        }
        rest = rest.get(8 + len + 4..)?;
    }
    None
}

fn import_with(format: ImageFormat, bytes: &[u8], title: &str) -> anyhow::Result<Document> {
    let mut decoder = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_decoder()
        .with_context(|| format!("decoding {title}"))?;
    use image::ImageDecoder as _;
    let mut icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|b| !b.is_empty());
    let img =
        image::DynamicImage::from_decoder(decoder).with_context(|| format!("decoding {title}"))?;
    let (w, h) = (img.width(), img.height());
    anyhow::ensure!(w > 0 && h > 0, "zero-sized image");

    // HDR PNGs (iPhone captures, HDR screenshots) mark BT.2100 PQ/HLG in
    // a cICP chunk, which overrides any iCCP profile; shown raw those
    // pixels come out flat and grey, so bake them down to sRGB. Everything
    // else keeps its embedded ICC profile for the display transform.
    let cicp = (format == ImageFormat::Png)
        .then(|| png_cicp(bytes))
        .flatten();
    let rgba = match cicp {
        Some([primaries, transfer @ (16 | 18), 0, 1]) => {
            // Bake from the decoder's full precision: HDR PNGs are
            // usually 16-bit and the shadows band if quantised first.
            let mut pixels = img.to_rgba32f().into_raw();
            match schist_colormgmt::bake_hdr_to_srgb(&mut pixels, primaries, transfer) {
                Ok(()) => {
                    icc = None; // the pixels are sRGB now
                    let bytes: Vec<u8> = pixels
                        .iter()
                        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
                        .collect();
                    image::RgbaImage::from_raw(w, h, bytes).context("buffer size")?
                }
                Err(err) => {
                    log::warn!("displaying HDR {title} unmapped: {err:#}");
                    img.to_rgba8()
                }
            }
        }
        _ => img.to_rgba8(),
    };

    let mut doc = Document::new(title, w, h, Depth::Eight);
    doc.icc_profile = icc;
    let mut layer = Layer::new_raster("Background");
    blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        Depth::Eight,
        IntRect::from_size(w, h),
        rgba.as_raw(),
    );
    doc.push_layer(layer);
    doc.damage_all();
    doc.dirty = false;
    Ok(doc)
}

fn export_flat(
    doc: &Document,
    format: ImageFormat,
    options: &ExportOptions,
) -> anyhow::Result<Vec<u8>> {
    let region = doc.canvas_rect();
    let mut pixels = schist_compositor::composite_region_f32(doc, region);
    // Reducing to 8 bits per channel bands smooth gradients; dither unless
    // the user turned it off.
    if options.dither && options.bit_depth <= 8 {
        schist_colormgmt::dither_to_depth(&mut pixels, doc.width as usize, 1 << options.bit_depth);
    }
    let rgba: Vec<u8> = pixels
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect();
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(doc.width, doc.height, rgba).context("buffer size")?;
    let mut out = std::io::Cursor::new(Vec::new());
    // A document profile changes what the numbers mean; a file without it
    // reads as sRGB elsewhere, so embed it wherever the format can.
    let icc = doc.icc_profile.clone();
    use image::ImageEncoder as _;
    match format {
        // JPEG has no alpha and takes a quality setting.
        ImageFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut out,
                options.quality.clamp(1, 100),
            );
            if let Some(icc) = icc {
                let _ = encoder.set_icc_profile(icc);
            }
            encoder.encode_image(&rgb)?;
        }
        ImageFormat::Png => {
            let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
            if let Some(icc) = icc {
                let _ = encoder.set_icc_profile(icc);
            }
            encoder.write_image(
                img.as_raw(),
                doc.width,
                doc.height,
                image::ExtendedColorType::Rgba8,
            )?;
        }
        _ => img.write_to(&mut out, format)?,
    }
    Ok(out.into_inner())
}

macro_rules! simple_codec {
    ($ty:ident, $id:literal, $name:literal, $format:expr, $exts:expr, $magic:expr) => {
        pub struct $ty;

        impl CodecPlugin for $ty {
            fn id(&self) -> &'static str {
                $id
            }
            fn name(&self) -> &'static str {
                $name
            }
            fn extensions(&self) -> &'static [&'static str] {
                $exts
            }
            fn probe(&self, bytes: &[u8]) -> bool {
                let magic: &[&[u8]] = $magic;
                magic.iter().any(|m| bytes.starts_with(m))
            }
            fn import(&self, bytes: &[u8]) -> anyhow::Result<Document> {
                import_with($format, bytes, $name)
            }
            fn can_export(&self) -> bool {
                true
            }
            fn export(&self, doc: &Document) -> anyhow::Result<Vec<u8>> {
                export_flat(doc, $format, &ExportOptions::default())
            }
            fn export_with(
                &self,
                doc: &Document,
                options: &ExportOptions,
            ) -> anyhow::Result<Vec<u8>> {
                export_flat(doc, $format, options)
            }
            fn supports_quality(&self) -> bool {
                matches!($format, ImageFormat::Jpeg)
            }
        }
    };
}

simple_codec!(
    PngCodec,
    "codec.png",
    "PNG",
    ImageFormat::Png,
    &["png"],
    &[b"\x89PNG"]
);
simple_codec!(
    JpegCodec,
    "codec.jpeg",
    "JPEG",
    ImageFormat::Jpeg,
    &["jpg", "jpeg"],
    &[b"\xFF\xD8\xFF"]
);
simple_codec!(
    WebPCodec,
    "codec.webp",
    "WebP",
    ImageFormat::WebP,
    &["webp"],
    &[b"RIFF"]
);
simple_codec!(
    TiffCodec,
    "codec.tiff",
    "TIFF",
    ImageFormat::Tiff,
    &["tif", "tiff"],
    &[b"II*\x00", b"MM\x00*"]
);

pub struct CommonCodecsPlugin;

impl PluginManifest for CommonCodecsPlugin {
    fn id(&self) -> &'static str {
        "schist.codecs-common"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_codec(Box::new(PngCodec));
        registry.register_codec(Box::new(JpegCodec));
        registry.register_codec(Box::new(WebPCodec));
        registry.register_codec(Box::new(TiffCodec));
        registry.register_codec(Box::new(AffinityCodec));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        // Encode a small test image, import as document, export, re-import.
        let mut img = image::RgbaImage::new(20, 10);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([x as u8 * 10, 100, 200, 255]);
        }
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let bytes = bytes.into_inner();

        let codec = PngCodec;
        assert!(codec.probe(&bytes));
        let doc = codec.import(&bytes).unwrap();
        assert_eq!((doc.width, doc.height), (20, 10));
        let layer = doc.tree.layers.first().unwrap();
        let px = layer.as_raster().unwrap().tiles.pixel(3, 0).to_u8();
        assert_eq!(px, [30, 100, 200, 255]);

        let out = codec.export(&doc).unwrap();
        let doc2 = codec.import(&out).unwrap();
        let px2 = doc2.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(3, 0)
            .to_u8();
        assert_eq!(px2, [30, 100, 200, 255]);
    }

    /// Insert a chunk right after IHDR (13-byte data + 12 bytes framing
    /// after the 8-byte signature).
    fn splice_chunk(png: &[u8], kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let at = 8 + 12 + 13;
        let mut out = png[..at].to_vec();
        out.extend((data.len() as u32).to_be_bytes());
        out.extend(kind);
        out.extend(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        out.extend(crc.finalize().to_be_bytes());
        out.extend(&png[at..]);
        out
    }

    #[test]
    fn png_iccp_profile_survives_import_and_export() {
        let display_p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();

        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 30, 40, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        image::ImageEncoder::set_icc_profile(&mut encoder, display_p3.clone()).unwrap();
        image::ImageEncoder::write_image(
            encoder,
            img.as_raw(),
            4,
            4,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();

        let doc = PngCodec.import(&bytes.into_inner()).unwrap();
        assert_eq!(doc.icc_profile.as_deref(), Some(display_p3.as_slice()));
        // Assigning a profile reinterprets the numbers; it must not touch them.
        let px = doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(0, 0)
            .to_u8();
        assert_eq!(px, [200, 30, 40, 255]);

        let out = PngCodec.export(&doc).unwrap();
        let doc2 = PngCodec.import(&out).unwrap();
        assert_eq!(doc2.icc_profile.as_deref(), Some(display_p3.as_slice()));
    }

    #[test]
    fn png_cicp_pq_bakes_to_srgb() {
        // Three PQ greys: black, ~203-nit reference white, ~1000 nits.
        let mut img = image::RgbaImage::new(3, 1);
        for (x, v) in [0u8, 148, 195].into_iter().enumerate() {
            img.put_pixel(x as u32, 0, image::Rgba([v, v, v, 255]));
        }
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        // BT.2020 primaries, PQ transfer, RGB, full-range.
        let bytes = splice_chunk(&bytes.into_inner(), b"cICP", &[9, 16, 0, 1]);
        assert_eq!(super::png_cicp(&bytes), Some([9, 16, 0, 1]));

        let doc = PngCodec.import(&bytes).unwrap();
        assert!(doc.icc_profile.is_none(), "baked pixels are sRGB");
        let tiles = &doc.tree.layers[0].as_raster().unwrap().tiles;
        let black = tiles.pixel(0, 0).to_u8()[0];
        let white = tiles.pixel(1, 0).to_u8()[0];
        let spec = tiles.pixel(2, 0).to_u8()[0];
        assert!(black < 5, "PQ black stays black: {black}");
        assert!(white > 240, "reference white bakes near white: {white}");
        assert!(spec >= white, "speculars roll off above white: {spec}");
    }

    #[test]
    fn plain_png_has_no_profile_and_exact_pixels() {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let doc = PngCodec.import(&bytes.into_inner()).unwrap();
        assert!(doc.icc_profile.is_none());
        let px = doc.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(1, 1)
            .to_u8();
        assert_eq!(px, [10, 20, 30, 255]);
    }

    #[test]
    fn registry_probe_dispatch() {
        let mut reg = PluginRegistry::new();
        CommonCodecsPlugin.register(&mut reg);
        assert_eq!(
            reg.codec_for(b"\x89PNG....", None).unwrap().id(),
            "codec.png"
        );
        assert_eq!(
            reg.codec_for(b"\xFF\xD8\xFF\xE0", None).unwrap().id(),
            "codec.jpeg"
        );
        assert_eq!(reg.codec_for(b"", Some("tiff")).unwrap().id(), "codec.tiff");
        assert_eq!(
            reg.codec_for(b"\x00\xFFKA....", None).unwrap().id(),
            "codec.affinity"
        );
        assert_eq!(
            reg.codec_for(b"", Some("afphoto")).unwrap().id(),
            "codec.affinity"
        );
        assert!(reg.codec_for(b"garbage", Some("xyz")).is_none());
    }
}
