//! Epson ERF: the R-D1, R-D1s and R-D1x rangefinders.
//!
//! An ERF is an ordinary TIFF/EP file. IFD0 is a 160x120 uncompressed
//! RGB thumbnail and its single SubIFD is the sensor: 3040x2024
//! samples of 12 bits under an RGGB array, `PhotometricInterpretation`
//! 32803 and `Compression` 32769.
//!
//! That compression number promises nothing — the samples are not
//! compressed, only packed, and the packing has a quirk that no TIFF
//! reader would guess. Twelve-bit samples are written most
//! significant bits first, three bytes to two pixels, but after every
//! *ten* pixels the camera writes one extra byte, so ten pixels
//! occupy sixteen bytes rather than fifteen. That is where the
//! declared strip length comes from: 3040 pixels a row is 304 groups,
//! 4864 bytes a row, 9844736 bytes for the frame, and reading it as a
//! plain 12-bit stream goes wrong at the eleventh pixel. The spare
//! byte has been zero in every file examined.
//!
//! Epson bought the R-D1's firmware plumbing from Olympus, and it
//! shows: the makernote is `EPSON\0` plus a two-byte version and then
//! an Olympus-style IFD, with Olympus's tag numbers and offsets
//! relative to the file rather than to the note. The ones that matter
//! are 0x0280 (the full-size JPEG preview, its leading `FF` byte
//! clobbered exactly as Minolta and Sony clobber theirs), 0x0400
//! (the sensor rectangle), 0x0401 (black level, four values in
//! R, G, B, G2 order rather than in filter-array order) and
//! 0x020b/0x020c, the size of the picture inside the sensor frame.
//!
//! What the file does *not* record anywhere this decoder could find is
//! the as-shot white balance; see [`decode`].

use crate::formats::common;
use crate::tiff::{tags, Ifd, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// Epson's (Olympus's) makernote tags.
const EPSON_IMAGE_WIDTH: u16 = 0x020b;
const EPSON_IMAGE_HEIGHT: u16 = 0x020c;
const EPSON_PREVIEW: u16 = 0x0280;
const EPSON_SENSOR_AREA: u16 = 0x0400;
const EPSON_BLACK_LEVEL: u16 = 0x0401;

/// Epson's private "packed, not compressed" value for `Compression`.
const COMPRESSION_PACKED: u32 = 32769;

/// How many pixels share a packing group, and how many bytes the group
/// takes. Fifteen bytes hold the ten 12-bit samples; the sixteenth is
/// padding the camera writes and the decoder steps over.
const GROUP_PIXELS: usize = 10;
const GROUP_BYTES: usize = 16;

/// The IFD holding sensor samples: the one that says so with
/// `PhotometricInterpretation` 32803 (CFA).
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Result<&'a Ifd> {
    tiff.all()
        .into_iter()
        .find(|ifd| ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) == Some(32803))
        .ok_or_else(|| Error::Unsupported("ERF with no CFA image directory".into()))
}

/// Unpack the ten-pixels-in-sixteen-bytes frame.
fn unpack(data: &[u8], width: usize, height: usize) -> Result<Vec<u16>> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| Error::Corrupt("ERF frame too large".into()))?;
    let groups = pixels.div_ceil(GROUP_PIXELS);
    let need = groups
        .checked_mul(GROUP_BYTES)
        .ok_or_else(|| Error::Corrupt("ERF frame too large".into()))?;
    if data.len() < need {
        return Err(Error::Corrupt(format!(
            "ERF strip holds {} bytes, want {need} for {width}x{height}",
            data.len()
        )));
    }
    let mut out = vec![0u16; pixels];
    for (group, chunk) in out
        .chunks_mut(GROUP_PIXELS)
        .zip(data.as_chunks::<GROUP_BYTES>().0)
    {
        for (pair, triple) in group.chunks_mut(2).zip(chunk.as_chunks::<3>().0) {
            pair[0] = ((triple[0] as u16) << 4) | (triple[1] as u16 >> 4);
            if let Some(second) = pair.get_mut(1) {
                *second = ((triple[1] as u16 & 0x0f) << 8) | triple[2] as u16;
            }
        }
    }
    Ok(out)
}

/// The makernote as its own IFD: `EPSON\0` and a two-byte version,
/// then a bare directory in the file's byte order whose offsets are
/// relative to the file, not to the note.
fn makernote<'a>(tiff: &Tiff<'a>) -> Option<Tiff<'a>> {
    let entry = tiff.exif()?.get(tags::MAKER_NOTE)?;
    let head = tiff.bytes().get(entry.offset..entry.offset + 8)?;
    if !head.starts_with(b"EPSON\0") {
        return None;
    }
    Tiff::parse_at(tiff.bytes(), entry.offset + 8, tiff.little_endian()).ok()
}

/// Black level per filter-array position from makernote 0x0401, whose
/// four values are ordered R, G, B, G2 whatever the array is.
fn black_levels(maker: &Ifd, cfa: &Cfa) -> Option<[f32; 4]> {
    let entry = maker.get(EPSON_BLACK_LEVEL)?;
    let values: Vec<u32> = (0..4).map_while(|i| entry.u32(i)).collect();
    let [red, green, blue, green2] = values[..] else {
        return None;
    };
    let mut levels = [0.0f32; 4];
    for (position, level) in levels.iter_mut().enumerate() {
        *level = match cfa.color_at(position % 2, position / 2)? {
            CfaColor::Red => red,
            CfaColor::Green => green,
            CfaColor::Blue => blue,
            CfaColor::Green2 => green2,
            _ => return None,
        } as f32;
    }
    // The two greens of an RGGB array are not the same number here
    // (64 and 60 on the R-D1): the second green position takes the
    // fourth value, which is why the mapping goes through the colour
    // rather than straight down the list.
    Some(levels)
}

/// The preview the makernote points at, its clobbered first byte put
/// back. Verified to be a whole JPEG before it is handed out.
fn makernote_preview(tiff: &Tiff<'_>, maker: &Ifd) -> Option<Vec<u8>> {
    let entry = maker.get(EPSON_PREVIEW)?;
    let stream = tiff
        .bytes()
        .get(entry.offset..entry.offset.checked_add(entry.count)?)?;
    if stream.len() < 4 || stream[1] != 0xd8 || stream[stream.len() - 2..] != [0xff, 0xd9] {
        return None;
    }
    let mut out = stream.to_vec();
    out[0] = 0xff;
    Some(out)
}

fn best_preview(tiff: &Tiff<'_>) -> Option<Vec<u8>> {
    let maker = makernote(tiff);
    let from_maker = maker
        .as_ref()
        .and_then(|m| makernote_preview(tiff, m.root()));
    let embedded = common::largest_jpeg(tiff);
    match (from_maker, embedded) {
        (Some(a), Some(b)) => Some(if a.len() >= b.len() { a } else { b }),
        (a, b) => a.or(b),
    }
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = raw_ifd(&tiff)?;
    let layout = crate::tiff::ImageLayout::of(&tiff, ifd)?;
    if layout.compression != COMPRESSION_PACKED {
        return Err(Error::Unsupported(format!(
            "ERF with Compression {} (only 32769, Epson's packed raw, is known)",
            layout.compression
        )));
    }
    if layout.bits_per_sample != 12 {
        return Err(Error::Unsupported(format!(
            "ERF with {}-bit samples",
            layout.bits_per_sample
        )));
    }
    let [(start, len)] = layout.chunks[..] else {
        return Err(Error::Unsupported(format!(
            "ERF sensor data in {} strips, want one",
            layout.chunks.len()
        )));
    };
    let data = unpack(&bytes[start..start + len], layout.width, layout.height)?;

    // Every R-D1 body reads out RGGB, and the file says so in
    // CFAPattern; there is no other layout to fall back to.
    let cfa = Cfa::RGGB;
    let mut raw = RawImage::new(
        Format::Erf,
        layout.width,
        layout.height,
        1,
        RawData::U16(data),
        cfa.clone(),
    );
    raw.white_level = 4095.0;

    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = best_preview(&tiff);

    if let Some(maker) = makernote(&tiff) {
        let root = maker.root();
        if let Some(levels) = black_levels(root, &cfa) {
            raw.black_levels = levels;
        }
        // The picture is centred in the sensor frame: the camera
        // records only its size (0x020b/0x020c), and half the
        // difference on each side is the margin LibRaw also reports
        // (16 columns, 12 rows on the R-D1).
        let inner = |tag| root.get(tag).and_then(|e| e.u32(0)).map(|v| v as usize);
        if let (Some(width), Some(height)) = (inner(EPSON_IMAGE_WIDTH), inner(EPSON_IMAGE_HEIGHT)) {
            if width <= layout.width && height <= layout.height && width > 0 && height > 0 {
                raw.crop = Rect {
                    x: (layout.width - width) / 2,
                    y: (layout.height - height) / 2,
                    width,
                    height,
                };
            }
        }
        // 0x0400 is the sensor rectangle itself (0 0 3040 2024 on both
        // bodies). It is read only to check the frame is the whole
        // sensor; nothing here needs it otherwise.
        if let Some(area) = root.get(EPSON_SENSOR_AREA) {
            if let (Some(width), Some(height)) = (area.u32(2), area.u32(3)) {
                if width as usize != layout.width || height as usize != layout.height {
                    log::debug!(
                        "ERF sensor area {}x{} disagrees with the frame {}x{}",
                        width,
                        height,
                        layout.width,
                        layout.height
                    );
                }
            }
        }
    }

    // The as-shot white balance is deliberately left at unity. The
    // R-D1's makernote carries no pair of numbers anywhere in its
    // 1508-byte header whose ratio is the multiplier LibRaw reports —
    // an exhaustive search over every 1-, 2-, 3- and 4-byte integer
    // and every IEEE float in the header, in both byte orders, finds
    // nothing, and the same search across two bodies with very
    // different balances finds nothing either. Whatever LibRaw does
    // with the R-D1 is not reading a stored multiplier, and inventing
    // one here would be worse than saying the file records none.
    //
    // Without a balance the colours cannot be right, so no colour
    // matrix is offered either: a caller with another decoder (the
    // plugin has LibRaw) treats a missing matrix as "let the other one
    // render this", which is the right outcome until the balance is
    // found. The sensor data itself is exact.
    raw.apply_camera_table();
    raw.color_matrix = None;
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    Ok(best_preview(&tiff))
}

#[cfg(test)]
mod tests {

    use super::*;
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
    fn unpacks_ten_pixels_from_sixteen_bytes() {
        // Fifteen bytes of 12-bit samples then the padding byte the
        // camera writes; the eleventh pixel restarts in the next group.
        let mut data = vec![0u8; 32];
        data[0] = 0xab;
        data[1] = 0xcd;
        data[2] = 0xef;
        data[15] = 0xff; // padding, must be stepped over
        data[16] = 0x12;
        data[17] = 0x34;
        let out = unpack(&data, 10, 2).unwrap();
        assert_eq!(out[0], 0xabc);
        assert_eq!(out[1], 0xdef);
        assert_eq!(out[10], 0x123);
    }

    #[test]
    fn short_strips_are_corrupt_not_a_panic() {
        assert!(matches!(
            unpack(&[0; 16], 3040, 2024),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"II*\0garbage").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn corpus_matches_the_oracle() {
        for path in corpus(&["erf"]) {
            let bytes = std::fs::read(&path).expect("sample readable");
            assert_eq!(
                crate::probe(&bytes),
                Some(crate::Format::Erf),
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
        for path in corpus(&["erf"]) {
            truncations(&path);
        }
    }
}
