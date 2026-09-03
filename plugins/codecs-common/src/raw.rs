//! Camera raw import through the pure-Rust `schist-codec-raw` crate.
//!
//! A raw file is a sensor dump in one of some dozens of vendor
//! containers: most are TIFF with a proprietary compression (Nikon,
//! Sony, Pentax, DNG), Canon's CR3 is ISO-BMFF, and Fuji, Olympus and
//! Panasonic use private headers. The crate reads all of them, with
//! no library to load, so this module is the same on the desktop and
//! in the browser; the one file it refuses is Nikon's licensed High
//! Efficiency NEF, which nothing reads without the vendor's SDK.
//!
//! Import only: a raw is a capture, and nothing writes one.
//!
//! The development is a plain one: camera white balance, the sensor
//! matrix to sRGB, 16 bits a channel so the shadows survive being
//! pushed, and an exposure lift with a soft shoulder (`expose_and_encode`)
//! in place of dcraw's automatic brightening, which stretches the
//! histogram until 1% of the pixels clip. The result lands near the
//! camera's own JPEG in brightness with the highlights still there.

use schist_core::{Document, RawDevelopment, RawSettings};
use schist_plugin_api::CodecPlugin;

pub use schist_codec_raw::demosaic::Quality as RawQuality;

/// A camera capture developed into straight-alpha, sRGB-encoded floats.
/// Kept flat so the app can apply its remaining Camera Raw controls before
/// turning the result into document tiles.
#[derive(Debug)]
pub struct DevelopedRaw {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<f32>,
}

/// File suffixes dispatched to the camera-raw codec. Shared with the app so
/// a direct capture open can start in the development dialog, while a PSD
/// containing a RAW-backed layer simply opens as a document.
pub const RAW_EXTENSIONS: &[&str] = &[
    "dng", "nef", "nrw", "arw", "srf", "sr2", "cr2", "cr3", "crw", "raf", "orf", "rw2", "rwl",
    "pef", "srw", "erf", "kdc", "dcr", "mrw", "mos", "iiq", "3fr", "fff", "mef", "x3f", "raw",
];
/// Bring linear developed pixels up to display brightness and encode
/// them as sRGB, in place.
///
/// A raw's linear values sit where the sensor put them, which is a stop
/// or more under the camera's own JPEG: cameras expose to protect the
/// highlights and lift the picture afterwards. dcraw's answer is to
/// stretch the histogram until 1% of the samples clip. This does the
/// lift without the clipping: the gain that brings the 99th percentile
/// of the brightest channel to white — capped at +2 EV, and never a
/// darkening, so an exposed-to-the-right frame is left alone — and
/// above the knee an exponential shoulder (the one HDR captures get)
/// that compresses the top towards white instead of cutting it off.
fn expose_and_encode_with(rgba: &mut [f32], exposure: f32) {
    const KNEE: f32 = 0.85;
    const MAX_GAIN: f32 = 4.0;
    const BINS: usize = 4096;

    let pixels = rgba.as_chunks_mut::<4>().0;
    if pixels.is_empty() {
        return;
    }
    let mut histogram = vec![0u32; BINS];
    for px in pixels.iter() {
        let brightest = px[0].max(px[1]).max(px[2]).clamp(0.0, 1.0);
        histogram[((brightest * (BINS - 1) as f32) as usize).min(BINS - 1)] += 1;
    }
    let total = pixels.len() as u64;
    let mut seen = 0u64;
    let mut p99 = 1.0f32;
    for (bin, count) in histogram.iter().enumerate() {
        seen += *count as u64;
        if seen * 100 >= total * 99 {
            p99 = bin as f32 / (BINS - 1) as f32;
            break;
        }
    }
    let auto_gain = if p99 > 0.0 {
        (1.0 / p99).clamp(1.0, MAX_GAIN)
    } else {
        1.0
    };
    // Exposure belongs in scene-linear light, before the shoulder and the
    // sRGB transfer curve. Non-finite public input is neutral rather than a
    // way to turn the entire frame into NaNs.
    let exposure = if exposure.is_finite() {
        exposure.clamp(-5.0, 5.0)
    } else {
        0.0
    };
    let gain = auto_gain * 2.0f32.powf(exposure);

    // The shoulder leaves everything in 0..=1, and the tiles hold 16
    // bits, so the curve is a table over that range rather than a
    // `powf` per sample.
    let curve: Vec<f32> = (0..=u16::MAX)
        .map(|i| srgb_encode(i as f32 / u16::MAX as f32))
        .collect();
    for px in pixels.iter_mut() {
        for c in px.iter_mut().take(3) {
            let v = (*c * gain).max(0.0);
            let v = if v <= KNEE {
                v
            } else {
                KNEE + (1.0 - KNEE) * (1.0 - (-(v - KNEE) / (1.0 - KNEE)).exp())
            };
            *c = curve[(v.min(1.0) * u16::MAX as f32 + 0.5) as usize];
        }
    }
}

#[cfg(test)]
fn expose_and_encode(rgba: &mut [f32]) {
    expose_and_encode_with(rgba, 0.0);
}

/// The sRGB transfer curve, linear light to signal.
fn srgb_encode(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// The camera's own preview — most raws embed a JPEG, often full size —
/// as straight RGBA, turned upright. `None` when the file has no usable
/// one. Orders of magnitude cheaper than developing, which is what a
/// thumbnail wants.
pub fn embedded_preview(bytes: &[u8]) -> anyhow::Result<Option<image::RgbaImage>> {
    guarded(|| native::embedded_preview(bytes))
}

/// Re-develop an original capture with RAW-domain controls.
///
/// Temperature and tint alter the camera's white-balance multipliers before
/// demosaic; exposure is applied in scene-linear light before the display
/// curve. The remaining [`RawSettings`] controls operate on the developed
/// image and are applied by the Camera Raw filter in the host.
pub fn develop_rgba(
    bytes: &[u8],
    settings: RawSettings,
    quality: RawQuality,
) -> anyhow::Result<DevelopedRaw> {
    guarded(|| native::develop_rgba(bytes, settings, quality))
}

/// Camera raw files, import only.
pub struct RawCodec;

impl CodecPlugin for RawCodec {
    fn id(&self) -> &'static str {
        "codec.raw"
    }
    fn name(&self) -> &'static str {
        "Camera Raw"
    }
    fn extensions(&self) -> &'static [&'static str] {
        RAW_EXTENSIONS
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        // The native crate's probe knows every container, including
        // the ones (Phase One, Leaf, Samsung, Kodak) whose TIFFs carry
        // no CFA photometric and are told apart by maker.
        schist_codec_raw::probe(bytes).is_some()
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        guarded(|| native::develop_document(bytes))
    }
}

/// Run the native decoder with a panic turned into an error. The
/// crate promises never to panic on hostile input, and its tests fuzz
/// for that; this is the belt to those braces, since a panic here
/// would take the whole editor down with the file.
fn guarded<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(anyhow::anyhow!("native raw decoder panicked: {what}"))
        }
    }
}

/// The pure-Rust path.
mod native {
    use super::{expose_and_encode_with, DevelopedRaw, RawDevelopment, RawQuality, RawSettings};
    use anyhow::Context as _;
    use schist_codec_raw::{DevelopOptions, Orientation};
    use schist_color::Depth;
    use schist_core::Document;
    use std::sync::Arc;

    /// Decode and develop through `schist-codec-raw`, then the
    /// exposure lift and encoding. A body the camera table has no
    /// colour matrix for develops in white-balanced camera RGB —
    /// recognisable, not right — and says so in the log; adding its
    /// matrix to the table is the fix.
    pub(super) fn develop_document(bytes: &[u8]) -> anyhow::Result<Document> {
        let developed = develop_rgba(bytes, RawSettings::default(), RawQuality::Best)?;
        let mut doc = crate::deep_document(
            "Raw",
            developed.width as u32,
            developed.height as u32,
            &developed.rgba,
            Depth::Sixteen,
            None,
        )
        .context("assembling document")?;
        if let Some(layer) = doc.tree.layers.first_mut() {
            layer.raw = Some(Box::new(RawDevelopment {
                source: Arc::from(bytes),
                settings: RawSettings::default(),
            }));
        }
        Ok(doc)
    }

    pub(super) fn develop_rgba(
        bytes: &[u8],
        settings: RawSettings,
        quality: RawQuality,
    ) -> anyhow::Result<DevelopedRaw> {
        let settings = settings.sanitized();
        let raw = schist_codec_raw::decode(bytes).context("decoding")?;
        if raw.color_matrix.is_none() {
            log::warn!(
                "raw: no colour matrix for {} {}; developing in camera RGB",
                raw.make,
                raw.model
            );
        }
        let options = DevelopOptions {
            quality,
            white_balance: Some(adjusted_white_balance(raw.wb_coeffs, settings)),
            ..DevelopOptions::default()
        };
        let developed = schist_codec_raw::develop(&raw, &options).context("developing")?;
        let mut rgba = Vec::with_capacity(developed.rgb.len() / 3 * 4);
        for px in developed.rgb.as_chunks::<3>().0 {
            rgba.extend_from_slice(&[px[0], px[1], px[2], 1.0]);
        }
        expose_and_encode_with(&mut rgba, settings.exposure);
        Ok(DevelopedRaw {
            width: developed.width,
            height: developed.height,
            rgba,
        })
    }

    fn adjusted_white_balance(mut wb: [f32; 4], settings: RawSettings) -> [f32; 4] {
        let finite = |value: f32| if value.is_finite() { value } else { 0.0 };
        let temperature = finite(settings.temperature).clamp(-100.0, 100.0) / 100.0;
        let tint = finite(settings.tint).clamp(-100.0, 100.0) / 100.0;

        // Work in stops so opposite slider directions are reciprocal. A
        // warmer setting trades blue gain for red; positive tint trades
        // green for equal red/blue (magenta). `develop` normalises green to
        // one after validating all four coefficients.
        wb[0] *= 2.0f32.powf(temperature * 0.5 + tint * 0.125);
        wb[2] *= 2.0f32.powf(-temperature * 0.5 + tint * 0.125);
        wb[1] *= 2.0f32.powf(-tint * 0.25);
        wb[3] *= 2.0f32.powf(-tint * 0.25);
        wb
    }

    /// The embedded JPEG, decoded and turned upright.
    pub(super) fn embedded_preview(bytes: &[u8]) -> anyhow::Result<Option<image::RgbaImage>> {
        let Some(jpeg) = schist_codec_raw::preview(bytes).context("finding preview")? else {
            return Ok(None);
        };
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
            .context("decoding embedded preview")?
            .into_rgba8();
        let upright = match schist_codec_raw::orientation(bytes) {
            Orientation::Rotate180 => image::imageops::rotate180(&img),
            Orientation::Rotate90CW => image::imageops::rotate90(&img),
            Orientation::Rotate270CW => image::imageops::rotate270(&img),
            Orientation::MirrorHorizontal => image::imageops::flip_horizontal(&img),
            Orientation::MirrorVertical => image::imageops::flip_vertical(&img),
            Orientation::Transpose => {
                image::imageops::rotate270(&image::imageops::flip_horizontal(&img))
            }
            Orientation::Transverse => {
                image::imageops::rotate90(&image::imageops::flip_horizontal(&img))
            }
            Orientation::Normal => img,
        };
        Ok(Some(upright))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A DNG built by hand: an uncompressed 16-bit RGGB mosaic whose
    /// left half is a flat grey and whose right half is red, with a
    /// neutral as-shot white balance and the sRGB matrix as the camera
    /// matrix, so the developed picture's colours are predictable.
    pub(crate) fn synthetic_dng(width: u16, height: u16) -> Vec<u8> {
        const BYTE: u16 = 1;
        const ASCII: u16 = 2;
        const SHORT: u16 = 3;
        const LONG: u16 = 4;
        const RATIONAL: u16 = 5;
        const SRATIONAL: u16 = 10;
        let short = |v: u16| v.to_le_bytes().to_vec();
        let long = |v: u32| v.to_le_bytes().to_vec();
        let shorts = |v: &[u16]| v.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<_>>();
        let rationals = |v: &[(i32, i32)]| {
            v.iter()
                .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
                .collect::<Vec<_>>()
        };

        let (w, h) = (width as usize, height as usize);
        let mut pixels = Vec::with_capacity(w * h * 2);
        for y in 0..h {
            for x in 0..w {
                let red_site = y % 2 == 0 && x % 2 == 0;
                let value: u16 = if x < w / 2 {
                    20000
                } else if red_site {
                    40000
                } else {
                    8000
                };
                pixels.extend(value.to_le_bytes());
            }
        }

        // XYZ (D65) to linear sRGB, as ColorMatrix1 (XYZ to camera).
        let srgb_from_xyz: [i32; 9] = [32406, -15372, -4986, -9689, 18758, 415, 557, -2040, 10570];
        let matrix: Vec<(i32, i32)> = srgb_from_xyz.iter().map(|v| (*v, 10000)).collect();

        // Tags in ascending order, as TIFF requires. The strip offset
        // is patched in once the layout is known.
        let entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
            (0x00FE, LONG, 1, long(0)),
            (0x0100, LONG, 1, long(width as u32)),
            (0x0101, LONG, 1, long(height as u32)),
            (0x0102, SHORT, 1, short(16)),
            (0x0103, SHORT, 1, short(1)),
            (0x0106, SHORT, 1, short(32803)),
            (0x010F, ASCII, 7, b"Schist\0".to_vec()),
            (0x0110, ASCII, 10, b"Synthetic\0".to_vec()),
            (0x0111, LONG, 1, long(0)),
            (0x0115, SHORT, 1, short(1)),
            (0x0116, LONG, 1, long(height as u32)),
            (0x0117, LONG, 1, long((w * h * 2) as u32)),
            (0x011C, SHORT, 1, short(1)),
            (0x828D, SHORT, 2, shorts(&[2, 2])),
            (0x828E, BYTE, 4, vec![0, 1, 1, 2]),
            (0xC612, BYTE, 4, vec![1, 4, 0, 0]),
            (0xC614, ASCII, 17, b"Schist Synthetic\0".to_vec()),
            (0xC61A, SHORT, 1, short(0)),
            (0xC61D, LONG, 1, long(65535)),
            (0xC621, SRATIONAL, 9, rationals(&matrix)),
            (0xC628, RATIONAL, 3, rationals(&[(1, 1), (1, 1), (1, 1)])),
            (0xC65A, SHORT, 1, short(21)),
        ];
        let ifd_at = 8usize;
        let ifd_len = 2 + entries.len() * 12 + 4;
        let extra_at = ifd_at + ifd_len;
        let mut extra = Vec::new();
        let mut placed = Vec::new();
        for (tag, kind, count, payload) in &entries {
            if payload.len() <= 4 {
                let mut field = payload.clone();
                field.resize(4, 0);
                placed.push((*tag, *kind, *count, field));
            } else {
                placed.push((*tag, *kind, *count, long((extra_at + extra.len()) as u32)));
                extra.extend_from_slice(payload);
            }
        }
        let strip_at = extra_at + extra.len();
        for (tag, _, _, field) in &mut placed {
            if *tag == 0x0111 {
                *field = long(strip_at as u32);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"II*\0");
        out.extend(long(ifd_at as u32));
        out.extend(short(placed.len() as u16));
        for (tag, kind, count, field) in &placed {
            out.extend(short(*tag));
            out.extend(short(*kind));
            out.extend(long(*count));
            out.extend_from_slice(field);
        }
        out.extend(long(0));
        assert_eq!(out.len(), extra_at);
        out.extend(extra);
        assert_eq!(out.len(), strip_at);
        out.extend(pixels);
        out
    }

    #[test]
    fn probe_recognises_the_vendor_containers() {
        let cases: &[(&str, Vec<u8>)] = &[
            ("raf", b"FUJIFILMCCD-RAW 0201FF393103".to_vec()),
            ("orf", b"IIRO\x08\0\0\0rest".to_vec()),
            ("orf big-endian", b"MMOR\0\0\0\x08rest".to_vec()),
            ("rw2", b"IIU\0\x18\0\0\0rest".to_vec()),
            ("crw", b"II\x1a\0\0\0HEAPCCDR".to_vec()),
            ("x3f", b"FOVb\0\0\0\0".to_vec()),
            ("mrw", b"\0MRM\0\0\0\0".to_vec()),
            ("cr3", b"\0\0\0\x18ftypcrx \0\0\0\x01".to_vec()),
            ("cr2", b"II*\0\x10\0\0\0CR\x02\0".to_vec()),
        ];
        for (name, bytes) in cases {
            assert!(RawCodec.probe(bytes), "{name} should probe as raw");
        }
        assert!(!RawCodec.probe(b"\x89PNG\r\n\x1a\n"));
        assert!(
            !RawCodec.probe(b"II*\0\x08\0\0\0"),
            "a truncated TIFF is not a raw"
        );
        assert!(
            !RawCodec.probe(b"\0\0\0\x18ftypheic"),
            "HEIC is another codec's"
        );
    }

    #[test]
    fn probe_tells_a_mosaic_tiff_from_a_plain_one() {
        assert!(RawCodec.probe(&synthetic_dng(8, 8)), "a DNG is a raw");

        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let mut plain = std::io::Cursor::new(Vec::new());
        img.write_to(&mut plain, image::ImageFormat::Tiff).unwrap();
        let plain = plain.into_inner();
        assert!(crate::TiffCodec.probe(&plain));
        assert!(!RawCodec.probe(&plain), "an RGB TIFF is not a raw");
    }

    /// The developed picture, or None when the crate declines the
    /// file (`Unsupported`): the sweep reports those rather than
    /// failing, since a decline is a documented gap, not a bug.
    fn develop_or_skip(bytes: &[u8]) -> Option<Document> {
        match RawCodec.import(bytes) {
            Ok(doc) => Some(doc),
            Err(err) => {
                let declined = err.chain().any(|e| {
                    matches!(
                        e.downcast_ref::<schist_codec_raw::Error>(),
                        Some(schist_codec_raw::Error::Unsupported(_))
                    )
                });
                assert!(declined, "{err:#}");
                eprintln!("declined: {err:#}");
                None
            }
        }
    }

    #[test]
    fn develops_a_synthetic_dng() {
        let Some(doc) = develop_or_skip(&synthetic_dng(64, 32)) else {
            return;
        };
        assert_eq!((doc.width, doc.height), (64, 32));
        assert_eq!(doc.depth, schist_color::Depth::Sixteen, "raws keep 16 bits");
        assert!(doc.icc_profile.is_none(), "developed to sRGB, no profile");
        let raw = doc.tree.layers[0]
            .raw
            .as_deref()
            .expect("the original capture should stay attached");
        assert_eq!(raw.source.as_ref(), synthetic_dng(64, 32));
        assert_eq!(raw.settings, RawSettings::default());
        let tiles = &doc.tree.layers[0].as_raster().unwrap().tiles;
        // Grey in, grey out: the as-shot balance is neutral and the
        // matrix is sRGB's own.
        let grey = tiles.pixel(12, 12);
        assert!(
            (grey.r - grey.g).abs() < 0.03 && (grey.g - grey.b).abs() < 0.03,
            "left half should be neutral, got {:?}",
            (grey.r, grey.g, grey.b)
        );
        // 20000/65535 linear is 0.59 in sRGB before the exposure lift,
        // and the lift is bounded by the red half's brightest channel,
        // so the grey lands between that and just under white.
        assert!(
            (0.55..0.97).contains(&grey.g),
            "left half should be a light grey, got {}",
            grey.g
        );
        let red = tiles.pixel(52, 12);
        assert!(
            red.r > red.g + 0.2 && red.r > red.b + 0.2,
            "right half should be red, got {:?}",
            (red.r, red.g, red.b)
        );
        assert_eq!(tiles.pixel(12, 12).a, 1.0);
    }

    #[test]
    fn raw_controls_run_before_display_encoding() {
        let bytes = synthetic_dng(64, 32);
        let neutral = develop_rgba(&bytes, RawSettings::default(), RawQuality::Best).unwrap();
        let exposed = develop_rgba(
            &bytes,
            RawSettings {
                exposure: 1.0,
                ..RawSettings::default()
            },
            RawQuality::Best,
        )
        .unwrap();
        let warm = develop_rgba(
            &bytes,
            RawSettings {
                temperature: 100.0,
                ..RawSettings::default()
            },
            RawQuality::Best,
        )
        .unwrap();
        let at = (12 * 64 + 12) * 4;
        assert!(
            exposed.rgba[at + 1] > neutral.rgba[at + 1],
            "positive EV should lift linear-light green before encoding"
        );
        assert!(
            warm.rgba[at] > neutral.rgba[at] && warm.rgba[at + 2] < neutral.rgba[at + 2],
            "warmer white balance should trade blue for red"
        );
    }

    #[test]
    fn exposure_lifts_a_dark_frame_and_rolls_the_top_off() {
        // 99% of the frame at 0.2 linear, 1% at 0.9: the lift is the
        // full +2 EV, the bulk lands at 0.8, and the bright percent
        // compresses towards white without reaching it.
        let mut px = [0.2f32, 0.2, 0.2, 1.0].repeat(99);
        px.extend([0.9, 0.9, 0.9, 1.0]);
        expose_and_encode(&mut px);
        let bulk = px[0];
        let bright = px[99 * 4];
        assert!((bulk - srgb_encode(0.8)).abs() < 0.01, "bulk {bulk}");
        assert!(bright > bulk && bright < 1.0, "bright {bright}");
        assert_eq!(px[3], 1.0, "alpha untouched");
    }

    #[test]
    fn exposure_leaves_a_bright_frame_alone() {
        // Exposed to the right already: nothing is darkened, and the
        // shoulder keeps white white.
        let mut px = [0.95f32, 0.5, 0.1, 1.0].repeat(50);
        expose_and_encode(&mut px);
        assert!(px[0] > srgb_encode(0.9) && px[0] <= 1.0, "red {}", px[0]);
        assert!((px[1] - srgb_encode(0.5)).abs() < 0.03, "green {}", px[1]);
        let mut white = vec![1.0f32, 1.0, 1.0, 1.0];
        expose_and_encode(&mut white);
        assert!(white[0] > 0.95, "white {}", white[0]);
    }

    #[test]
    fn a_dng_without_a_preview_yields_none() {
        assert!(embedded_preview(&synthetic_dng(64, 32)).unwrap().is_none());
    }

    #[test]
    fn a_broken_raw_is_an_error() {
        let mut bytes = synthetic_dng(64, 32);
        bytes.truncate(bytes.len() / 2);
        assert!(
            RawCodec.import(&bytes).is_err(),
            "half a file should not develop"
        );
    }

    /// Every file in `SCHIST_RAW_CORPUS` (a directory of real camera
    /// files) must probe as raw, develop, and yield an upright preview
    /// when it embeds one. Skipped without the variable.
    #[test]
    fn corpus_sweep() {
        let Ok(dir) = std::env::var("SCHIST_RAW_CORPUS") else {
            return;
        };
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut entries = Vec::new();
        walk(std::path::Path::new(&dir), &mut entries);
        entries.sort();
        for path in entries {
            // The oracle sidecars (`.tiff`, `.json`, `.identify.txt`)
            // live beside the samples and are not raws; nor are the
            // headerless dumps and scanner files the probe declines.
            let sidecar = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("tiff" | "json" | "txt" | "png" | "ppm" | "pgm" | "sh")
            );
            if sidecar {
                continue;
            }
            if !RawCodec.probe(&std::fs::read(&path).unwrap()) {
                eprintln!("{}: not probed as raw, skipped", path.display());
                continue;
            }
            // A sample with no `.tiff` sidecar is one the oracle tools
            // could not unpack; only files with an oracle are required
            // to develop here (the crate's own tests cover the rest).
            let oracle = path.with_file_name(format!(
                "{}.tiff",
                path.file_name().unwrap().to_string_lossy()
            ));
            if !oracle.exists() {
                eprintln!("{}: no oracle, skipped", path.display());
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                RawCodec.probe(&bytes),
                "{} should probe as raw",
                path.display()
            );
            let started = std::time::Instant::now();
            let Some(doc) = develop_or_skip(&bytes) else {
                continue;
            };
            let developed = started.elapsed();
            let started = std::time::Instant::now();
            let preview = embedded_preview(&bytes).unwrap();
            eprintln!(
                "{}: {}x{} developed in {:.2?}; preview {} in {:.2?}",
                path.display(),
                doc.width,
                doc.height,
                developed,
                preview
                    .as_ref()
                    .map(|p| format!("{}x{}", p.width(), p.height()))
                    .unwrap_or_else(|| "none".into()),
                started.elapsed()
            );
            // A square-mode frame (the D-LUX 5's 2752x2754 with a
            // 1920x1920 preview) has no orientation to disagree on.
            // Nor does a thumbnail-sized one: a few backs store those
            // the sensor's way round (Phase One's 409x545).
            let square = doc.width.abs_diff(doc.height) * 50 < doc.width.max(doc.height);
            if let Some(preview) = preview.filter(|p| !square && p.width().max(p.height()) >= 640) {
                let landscape = |w: u32, h: u32| w >= h;
                assert_eq!(
                    landscape(preview.width(), preview.height()),
                    landscape(doc.width, doc.height),
                    "{}: preview orientation disagrees with the developed picture",
                    path.display()
                );
            }
        }
    }
}
