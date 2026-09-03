//! Mamiya MEF: the ZD, the only body that ever wrote one.
//!
//! A MEF is a plain big-endian TIFF/EP file with nothing private in
//! it at all. IFD0 is a 144x192 uncompressed RGB thumbnail carrying
//! the Exif and the orientation, and it points at three SubIFDs: the
//! sensor (4016x5344 samples of 12 bits, `PhotometricInterpretation`
//! 32803, `Compression` 1) and two more uncompressed RGB previews,
//! 992x1328 and 240x320. There is no JPEG anywhere in the file, so
//! [`RawImage::preview`](crate::RawImage::preview) comes out `None`:
//! the ZD stores every preview as raw interleaved bytes.
//!
//! The samples are packed twelve bits at a time, most significant
//! first, three bytes to two pixels, running straight through the
//! frame with no row padding. One thing has to be worked around: the
//! camera's `StripByteCounts` is 1024 bytes short of the frame it
//! describes — 32191232 where 4016x5344 twelve-bit samples need
//! 32192256 — and the missing kilobyte really is in the file, past the
//! declared end of the strip. The strip length is therefore treated as
//! a lower bound and the frame read to the length the dimensions ask
//! for, still inside the file.
//!
//! Nothing in the file records black level, saturation or an as-shot
//! white balance, and LibRaw reports none for it either; the levels
//! come from the sample depth.

use crate::formats::common;
use crate::tiff::{tags, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Result};

/// The IFD holding sensor samples.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Result<&'a Ifd> {
    tiff.all()
        .into_iter()
        .find(|ifd| ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) == Some(32803))
        .ok_or_else(|| Error::Unsupported("MEF with no CFA image directory".into()))
}

/// The 2x2 filter array from CFAPattern (0x828E), whose bytes are
/// 0 red, 1 green, 2 blue in row-major order. `None` when the tag is
/// missing or is not the 2x2 the ZD writes.
fn cfa_from_tag(ifd: &Ifd) -> Option<Cfa> {
    let dim = ifd.get(tags::CFA_REPEAT_PATTERN_DIM)?;
    if (dim.u32(0), dim.u32(1)) != (Some(2), Some(2)) {
        return None;
    }
    let pattern = ifd.get(tags::CFA_PATTERN)?;
    let mut colors = [CfaColor::Red; 4];
    for (i, color) in colors.iter_mut().enumerate() {
        *color = match pattern.u32(i)? {
            0 => CfaColor::Red,
            1 => CfaColor::Green,
            2 => CfaColor::Blue,
            _ => return None,
        };
    }
    Some(Cfa::Bayer(colors))
}

/// Unpack 12-bit big-endian samples, three bytes to two pixels.
fn unpack(data: &[u8], width: usize, height: usize) -> Result<Vec<u16>> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| Error::Corrupt("MEF frame too large".into()))?;
    let need = pixels
        .div_ceil(2)
        .checked_mul(3)
        .ok_or_else(|| Error::Corrupt("MEF frame too large".into()))?;
    if data.len() < need {
        return Err(Error::Corrupt(format!(
            "MEF holds {} bytes of samples, want {need} for {width}x{height}",
            data.len()
        )));
    }
    let mut out = vec![0u16; pixels];
    for (pair, triple) in out.chunks_mut(2).zip(data.as_chunks::<3>().0) {
        pair[0] = ((triple[0] as u16) << 4) | (triple[1] as u16 >> 4);
        if let Some(second) = pair.get_mut(1) {
            *second = ((triple[1] as u16 & 0x0f) << 8) | triple[2] as u16;
        }
    }
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = raw_ifd(&tiff)?;
    let layout = ImageLayout::of(&tiff, ifd)?;
    if layout.compression != 1 {
        return Err(Error::Unsupported(format!(
            "MEF with Compression {} (the ZD writes uncompressed packed samples)",
            layout.compression
        )));
    }
    if layout.bits_per_sample != 12 {
        return Err(Error::Unsupported(format!(
            "MEF with {}-bit samples",
            layout.bits_per_sample
        )));
    }
    let [(start, len)] = layout.chunks[..] else {
        return Err(Error::Unsupported(format!(
            "MEF sensor data in {} strips, want one",
            layout.chunks.len()
        )));
    };
    // See the module note: the declared strip is a kilobyte shorter
    // than the frame it describes, so the strip length is a lower
    // bound. The frame is read from the strip's start to whatever the
    // dimensions need, which `unpack` checks is inside the file.
    if len
        < layout
            .width
            .saturating_mul(layout.height)
            .div_ceil(2)
            .saturating_mul(3)
    {
        log::debug!(
            "MEF StripByteCounts {len} is short of the frame; reading to the end of the file"
        );
    }
    let data = unpack(&bytes[start..], layout.width, layout.height)?;

    let cfa = cfa_from_tag(ifd).unwrap_or(Cfa::RGGB);
    let mut raw = RawImage::new(
        Format::Mef,
        layout.width,
        layout.height,
        1,
        RawData::U16(data),
        cfa,
    );
    // Twelve bits, and the frame really does reach 4095.
    raw.white_level = 4095.0;

    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    // The ZD is a portrait-shaped sensor read out sideways: IFD0's
    // Orientation is 6 on the sample here, and the crop is the whole
    // frame — there are no masked borders to trim.
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = common::largest_jpeg(&tiff);

    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    Ok(common::largest_jpeg(&tiff))
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::Rect;
    use std::path::{Path, PathBuf};

    /// Every file under `$SCHIST_RAW_CORPUS` with one of `extensions`,
    /// recursively. Empty when the variable is unset, which is how the
    /// corpus tests skip on a machine without the samples.
    fn corpus(extensions: &[&str]) -> Vec<PathBuf> {
        let Ok(root) = std::env::var("SCHIST_RAW_CORPUS") else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut stack = vec![PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.iter().any(|want| e.eq_ignore_ascii_case(want)))
                {
                    found.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// LibRaw's `unprocessed_raw -T` output beside the sample.
    fn oracle(path: &Path) -> Option<(usize, usize, Vec<u16>)> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tiff");
        let image = image::open(PathBuf::from(name)).ok()?.into_luma16();
        let (width, height) = (image.width() as usize, image.height() as usize);
        Some((width, height, image.into_raw()))
    }

    /// `raw-identify -v -w` output beside the sample.
    fn identify(path: &Path) -> Option<String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".identify.txt");
        std::fs::read_to_string(PathBuf::from(name)).ok()
    }

    /// The text after `key:` on the line that starts with it.
    fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines()
            .map(str::trim)
            .find(|line| line.starts_with(key))
            .map(|line| line[key.len()..].trim())
    }

    /// A "W x H" pair.
    fn size(text: &str, key: &str) -> Option<(usize, usize)> {
        let value = field(text, key)?;
        let (w, h) = value.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    /// What LibRaw's "Image flip" means as an [`Orientation`].
    fn flip(text: &str) -> Option<crate::Orientation> {
        Some(match field(text, "Image flip:")?.parse::<u32>().ok()? {
            3 => crate::Orientation::Rotate180,
            5 => crate::Orientation::Rotate270CW,
            6 => crate::Orientation::Rotate90CW,
            _ => crate::Orientation::Normal,
        })
    }

    /// The first four letters of the "Filter pattern" line as a CFA.
    fn filter_pattern(text: &str) -> Option<Cfa> {
        let pattern = field(text, "Filter pattern:")?;
        let mut colors = [CfaColor::Red; 4];
        for (color, letter) in colors.iter_mut().zip(pattern.chars()) {
            *color = match letter {
                'R' => CfaColor::Red,
                'G' => CfaColor::Green,
                'B' => CfaColor::Blue,
                _ => return None,
            };
        }
        Some(Cfa::Bayer(colors))
    }

    /// The as-shot multipliers, normalised to green, from the
    /// "As shot" row of the makernote white-balance table.
    fn as_shot(text: &str) -> Option<[f32; 3]> {
        let row = field(text, "As shot")?;
        let numbers: Vec<f32> = row
            .split_whitespace()
            .take(4)
            .map_while(|v| v.parse::<f32>().ok())
            .collect();
        let [red, green, blue, ..] = numbers[..] else {
            return None;
        };
        (green > 0.0).then(|| [red / green, 1.0, blue / green])
    }

    /// Compare a decoded frame with the oracle sample for sample.
    fn compare(path: &Path, raw: &RawImage) {
        let Some((width, height, expect)) = oracle(path) else {
            eprintln!("{}: no oracle TIFF, data not checked", path.display());
            return;
        };
        assert_eq!(
            (raw.width, raw.height),
            (width, height),
            "{}: frame is {}x{}, oracle {width}x{height}",
            path.display(),
            raw.width,
            raw.height
        );
        let RawData::U16(got) = &raw.data else {
            panic!("{}: expected integer samples", path.display())
        };
        let mut wrong = 0usize;
        let mut first = Vec::new();
        for (i, (a, b)) in got.iter().zip(expect.iter()).enumerate() {
            if a != b {
                wrong += 1;
                if first.len() < 8 {
                    first.push(format!("({}, {}): {a} not {b}", i % width, i / width));
                }
            }
        }
        assert_eq!(
            wrong,
            0,
            "{}: {wrong} samples differ; {}",
            path.display(),
            first.join(", ")
        );
    }

    /// Levels, balance, crop, orientation and CFA against
    /// `raw-identify`.
    fn compare_metadata(path: &Path, raw: &RawImage) {
        let Some(text) = identify(path) else { return };
        if let Some(cfa) = filter_pattern(&text) {
            assert_eq!(raw.cfa, cfa, "{}: filter pattern", path.display());
        }
        if let Some(orientation) = flip(&text) {
            assert_eq!(
                raw.orientation,
                orientation,
                "{}: orientation",
                path.display()
            );
        }
        if let Some((width, height)) = size(&text, "Image size:") {
            let (x, y) = match field(&text, "Raw inset, width x height:") {
                Some(inset) => {
                    let left = inset
                        .split("left:")
                        .nth(1)
                        .and_then(|v| v.split_whitespace().next().and_then(|v| v.parse().ok()));
                    let top = inset
                        .split("top:")
                        .nth(1)
                        .and_then(|v| v.split_whitespace().next().and_then(|v| v.parse().ok()));
                    (left.unwrap_or(0), top.unwrap_or(0))
                }
                None => (0, 0),
            };
            let inset = field(&text, "Raw inset, width x height:")
                .and_then(|v| v.split_whitespace().next().map(str::to_string));
            let expect = match inset.and_then(|w| w.parse::<usize>().ok()) {
                Some(w) => {
                    let h = field(&text, "Raw inset, width x height:")
                        .and_then(|v| v.split_whitespace().nth(2))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(height);
                    Rect {
                        x,
                        y,
                        width: w,
                        height: h,
                    }
                }
                None => Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            };
            if raw.crop != expect {
                eprintln!("{}: crop {:?}, LibRaw {expect:?}", path.display(), raw.crop);
            }
        }
        if let Some(expect) = as_shot(&text) {
            for (got, want) in raw.wb_coeffs.iter().zip(expect.iter()) {
                if (got - want).abs() > want * 0.02 {
                    eprintln!(
                        "{}: white balance {:?}, LibRaw {expect:?}",
                        path.display(),
                        raw.wb_coeffs
                    );
                    break;
                }
            }
        }
    }

    /// Cutting a file short must never panic, whatever it does return.
    fn truncations(path: &Path) {
        let bytes = std::fs::read(path).expect("sample readable");
        let mut seed = bytes.len() as u64;
        for _ in 0..10 {
            // A cheap deterministic spread of cut points.
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let at = (seed >> 33) as usize % bytes.len().max(1);
            let _ = crate::decode(&bytes[..at]);
            let _ = crate::probe(&bytes[..at]);
        }
    }

    #[test]
    fn unpacks_twelve_bit_samples() {
        let data = [0xab, 0xcd, 0xef, 0x12, 0x34, 0x56];
        assert_eq!(
            unpack(&data, 4, 1).unwrap(),
            vec![0xabc, 0xdef, 0x123, 0x456]
        );
    }

    #[test]
    fn an_odd_pixel_count_takes_the_first_of_a_pair() {
        // Three pixels still occupy two whole three-byte groups.
        let data = [0xab, 0xcd, 0xef, 0x12, 0x30, 0x00];
        assert_eq!(unpack(&data, 3, 1).unwrap(), vec![0xabc, 0xdef, 0x123]);
    }

    #[test]
    fn short_data_is_corrupt_not_a_panic() {
        assert!(matches!(
            unpack(&[0; 8], 4016, 5344),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"MM\0*nonsense").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn corpus_matches_the_oracle() {
        for path in corpus(&["mef"]) {
            let bytes = std::fs::read(&path).expect("sample readable");
            assert_eq!(
                crate::probe(&bytes),
                Some(crate::Format::Mef),
                "{}: probed as something else",
                path.display()
            );
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) => {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let allowed: &[(&str, &str)] = &[];
                    let reason = allowed
                        .iter()
                        .find(|(file, _)| name.contains(file))
                        .unwrap_or_else(|| {
                            panic!("{}: unexpected Unsupported: {why}", path.display())
                        });
                    eprintln!(
                        "{}: unsupported as documented ({}): {why}",
                        path.display(),
                        reason.1
                    );
                    continue;
                }
                Err(other) => panic!("{}: {other}", path.display()),
            };
            raw.validate().expect("decoded frame is self-consistent");
            compare(&path, &raw);
            compare_metadata(&path, &raw);
            if let Some(preview) = &raw.preview {
                image::load_from_memory(preview)
                    .unwrap_or_else(|e| panic!("{}: preview will not decode: {e}", path.display()));
            }
        }
    }

    #[test]
    fn truncated_files_never_panic() {
        for path in corpus(&["mef"]) {
            truncations(&path);
        }
    }
}
