//! Common raster format codecs (PNG, JPEG, WebP, TIFF) via the `image`
//! crate, wrapped as `CodecPlugin`s, plus import-only Affinity support.
//! Import produces a single "Background" layer; export flattens through
//! the compositor.

pub use affinity::AffinityCodec;
use anyhow::Context as _;
use image::ImageFormat;
use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};
use schist_plugin_api::{CodecPlugin, ExportOptions, PluginManifest, PluginRegistry};

mod affinity;

fn import_with(format: ImageFormat, bytes: &[u8], title: &str) -> anyhow::Result<Document> {
    let img = image::load_from_memory_with_format(bytes, format)
        .with_context(|| format!("decoding {title}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    anyhow::ensure!(w > 0 && h > 0, "zero-sized image");
    let mut doc = Document::new(title, w, h, Depth::Eight);
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
        schist_colormgmt::dither_to_depth(
            &mut pixels,
            doc.width as usize,
            1 << options.bit_depth,
        );
    }
    let rgba: Vec<u8> = pixels
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect();
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(doc.width, doc.height, rgba).context("buffer size")?;
    let mut out = std::io::Cursor::new(Vec::new());
    match format {
        // JPEG has no alpha and takes a quality setting.
        ImageFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut out,
                options.quality.clamp(1, 100),
            );
            encoder.encode_image(&rgb)?;
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
