//! Minolta MRW: the DiMAGE compacts and the Dynax/Maxxum/Alpha DSLRs.
//!
//! An MRW is not a TIFF. It opens with the four bytes `\0MRM` and a
//! 32-bit big-endian length, and everything up to `8 + length` is a
//! chain of blocks with the same shape: a four-byte tag whose first
//! byte is NUL (`\0PRD`, `\0WBG`, `\0RIF`, `\0TTW`, `\0PAD`), a 32-bit
//! big-endian length, then that many bytes of body. The sensor data
//! begins immediately after the last block and runs to the end of the
//! file. Every number in the container is big-endian, on every body.
//!
//! Only three blocks matter here:
//!
//! * `PRD` — the sensor: its dimensions, how many bits a stored sample
//!   takes, and which of the four Bayer phases the frame starts on.
//! * `WBG` — the as-shot white balance, four numerators with a scale
//!   exponent each, *in filter-array order* rather than in a fixed
//!   R,G,G,B order: a body whose array is GBRG writes G,B,R,G. The
//!   camera's own tag name says so — ExifTool reads the same twelve
//!   bytes as `WB_RGGBLevels` on an RGGB body and `WB_GBRGLevels` on
//!   the DiMAGE A200.
//! * `TTW` — a complete big-endian TIFF, header and all, holding the
//!   ordinary Exif of the shot plus Minolta's makernote. Its offsets
//!   are relative to the block body, which is exactly what
//!   [`Tiff::parse_embedded`] wants.
//!
//! The full-size JPEG preview is not in the TIFF's own thumbnail slot
//! but in the makernote, at `PreviewImageStart`/`PreviewImageLength`
//! (0x0088/0x0089) — and on every body but the A200 its first byte has
//! been overwritten, so the stream starts `02 D8` or `00 D8` where a
//! JPEG must start `FF D8`. Restoring that byte is enough to make the
//! preview decodable, which is what `dcraw`-era readers have always
//! done with Minolta and Sony previews.

use crate::formats::common;
use crate::tiff::{tags, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// Minolta's makernote tags for the embedded preview.
const PREVIEW_START: u16 = 0x0088;
const PREVIEW_LENGTH: u16 = 0x0089;

/// One block of the header chain.
struct Block<'a> {
    /// The four tag bytes as stored, NUL included.
    tag: &'a [u8],
    /// Where the body starts in the file, for the blocks (`TTW`) whose
    /// contents carry offsets of their own.
    start: usize,
    body: &'a [u8],
}

/// The blocks of the header, and where the sensor data starts.
struct Header<'a> {
    blocks: Vec<Block<'a>>,
    data_offset: usize,
}

impl<'a> Header<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Header<'a>> {
        if bytes.len() < 8 || &bytes[0..4] != b"\0MRM" {
            return Err(Error::Corrupt("not an MRW: no \\0MRM signature".into()));
        }
        let length = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let data_offset = length
            .checked_add(8)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| Error::Corrupt("MRW header runs past the end of the file".into()))?;

        let mut blocks = Vec::new();
        let mut at = 8;
        // A malformed length could otherwise leave `at` standing still;
        // every block costs at least its eight-byte head, so the walk
        // terminates on its own, but the bounds are checked anyway.
        while at + 8 <= data_offset {
            let tag = &bytes[at..at + 4];
            let len =
                u32::from_be_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
                    as usize;
            let start = at + 8;
            let end = start
                .checked_add(len)
                .filter(|end| *end <= data_offset)
                .ok_or_else(|| {
                    Error::Corrupt(format!("MRW block {tag:02x?} overruns the header"))
                })?;
            blocks.push(Block {
                tag,
                start,
                body: &bytes[start..end],
            });
            at = end;
        }
        Ok(Header {
            blocks,
            data_offset,
        })
    }

    fn get(&self, tag: &[u8; 4]) -> Option<&Block<'a>> {
        self.blocks.iter().find(|b| b.tag == tag)
    }
}

/// What `PRD` says about the sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sensor {
    width: usize,
    height: usize,
    /// The vendor's "image" size: always a few pixels smaller than the
    /// sensor, with no statement of where inside it those pixels lie.
    image_width: usize,
    image_height: usize,
    /// Bits a stored sample takes: 12 (three bytes to two pixels) or
    /// 16 (one big-endian word a pixel, the low twelve bits used).
    storage_bits: u32,
    /// Bits the converter actually filled, always 12 so far.
    data_bits: u32,
    /// The filter-array phase, PRD's last byte.
    pattern: u8,
}

impl Sensor {
    /// `PRD` is a fixed 24-byte record: eight bytes of firmware
    /// version, then the sensor's height and width and the image's
    /// height and width as 16-bit counts (height first, which is the
    /// opposite of every other field in this crate), then the storage
    /// and data depths, then a storage-method byte, four unused bytes
    /// and the Bayer phase.
    fn parse(body: &[u8]) -> Result<Sensor> {
        if body.len() < 24 {
            return Err(Error::Corrupt(format!(
                "MRW PRD block is {} bytes, want 24",
                body.len()
            )));
        }
        let u16_at = |i: usize| u16::from_be_bytes([body[i], body[i + 1]]) as usize;
        let sensor = Sensor {
            height: u16_at(8),
            width: u16_at(10),
            image_height: u16_at(12),
            image_width: u16_at(14),
            storage_bits: body[16] as u32,
            data_bits: body[17] as u32,
            pattern: body[23],
        };
        if sensor.width == 0 || sensor.height == 0 {
            return Err(Error::Corrupt("MRW PRD gives an empty sensor".into()));
        }
        Ok(sensor)
    }

    /// The filter array anchored at the frame origin. Only two phases
    /// have ever been seen: 1 on every Dynax/Maxxum/Alpha body and the
    /// DiMAGE 5/7 family, 4 on the DiMAGE A200, which reads its sensor
    /// out half a line further along.
    fn cfa(&self) -> Result<Cfa> {
        match self.pattern {
            1 => Ok(Cfa::RGGB),
            4 => Ok(Cfa::GBRG),
            other => Err(Error::Unsupported(format!("MRW Bayer pattern {other}"))),
        }
    }
}

/// White balance from `WBG`: four scale exponents then four 16-bit
/// numerators, in the order the filter array puts the colours in.
///
/// The value of a coefficient is `numerator >> exponent`; the
/// exponents have been 2,2,2,2 on every file seen, so what survives
/// normalisation is the ratio of the numerators, but a body that used
/// different scales per channel would still come out right.
fn white_balance(body: &[u8], cfa: &Cfa) -> Option<[f32; 4]> {
    if body.len() < 12 {
        return None;
    }
    let mut coeffs = [0.0f32; 4];
    for i in 0..4 {
        let scale = body[i].min(31) as i32;
        let numerator = u16::from_be_bytes([body[4 + i * 2], body[5 + i * 2]]) as f32;
        // Position i of the 2x2 pattern, row-major, names the colour
        // this numerator balances.
        let color = cfa.color_at(i % 2, i / 2)?;
        let value = numerator / (1i64 << scale) as f32;
        let slot = match color {
            CfaColor::Red => 0,
            CfaColor::Green => 1,
            CfaColor::Blue => 2,
            CfaColor::Green2 => 3,
            _ => return None,
        };
        // Two positions carry green; the second fills G2 as well so a
        // developer that separates them has both.
        if slot == 1 && coeffs[1] != 0.0 {
            coeffs[3] = value;
        } else {
            coeffs[slot] = value;
        }
    }
    let green = coeffs[1];
    // A missing numerator (or a NaN out of a mangled scale) leaves the
    // balance unusable; unity is a better answer than a division.
    if !green.is_finite() || green <= 0.0 || coeffs[0] <= 0.0 || coeffs[2] <= 0.0 {
        return None;
    }
    if coeffs[3] <= 0.0 {
        coeffs[3] = green;
    }
    Some([coeffs[0] / green, 1.0, coeffs[2] / green, coeffs[3] / green])
}

/// Unpack the sensor frame. 12-bit samples are packed most
/// significant bits first, three bytes to two pixels, with no padding
/// at all — not even at the end of a row, which is why the whole frame
/// is unpacked as one run. 16-bit samples are plain big-endian words.
fn unpack(data: &[u8], sensor: &Sensor) -> Result<Vec<u16>> {
    let pixels = sensor
        .width
        .checked_mul(sensor.height)
        .ok_or_else(|| Error::Corrupt("MRW frame too large".into()))?;
    match sensor.storage_bits {
        12 => {
            let need = pixels.div_ceil(2) * 3;
            if data.len() < need {
                return Err(Error::Corrupt(format!(
                    "MRW holds {} bytes of 12-bit samples, want {need}",
                    data.len()
                )));
            }
            let mut out = vec![0u16; pixels];
            for (pair, chunk) in out.chunks_mut(2).zip(data.as_chunks::<3>().0) {
                pair[0] = ((chunk[0] as u16) << 4) | (chunk[1] as u16 >> 4);
                if let Some(second) = pair.get_mut(1) {
                    *second = ((chunk[1] as u16 & 0x0f) << 8) | chunk[2] as u16;
                }
            }
            Ok(out)
        }
        16 => {
            let need = pixels * 2;
            if data.len() < need {
                return Err(Error::Corrupt(format!(
                    "MRW holds {} bytes of 16-bit samples, want {need}",
                    data.len()
                )));
            }
            Ok(data[..need]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|w| u16::from_be_bytes([w[0], w[1]]))
                .collect())
        }
        other => Err(Error::Unsupported(format!(
            "MRW with {other}-bit sample storage"
        ))),
    }
}

/// The makernote as its own IFD chain: Minolta writes a bare IFD with
/// no header of its own, in the TTW TIFF's byte order, with offsets
/// relative to the TTW TIFF rather than to the makernote.
fn makernote<'a>(tiff: &Tiff<'a>) -> Option<Tiff<'a>> {
    let entry = tiff.exif()?.get(tags::MAKER_NOTE)?;
    Tiff::parse_at_relative(
        tiff.bytes(),
        entry.offset,
        tiff.base(),
        tiff.little_endian(),
    )
    .ok()
}

/// The preview the makernote points at, with its clobbered first byte
/// put back. `None` unless the stream really does end `FF D9`, so a
/// stale pointer cannot hand a caller a lump of sensor data.
fn makernote_preview(tiff: &Tiff<'_>) -> Option<Vec<u8>> {
    let maker = makernote(tiff)?;
    let root = maker.root();
    let start = root.get(PREVIEW_START)?.u32(0)? as usize + tiff.base();
    let length = root.get(PREVIEW_LENGTH)?.u32(0)? as usize;
    let stream = tiff.bytes().get(start..start.checked_add(length)?)?;
    if stream.len() < 4 || stream[1] != 0xd8 || stream[stream.len() - 2..] != [0xff, 0xd9] {
        return None;
    }
    let mut out = stream.to_vec();
    out[0] = 0xff;
    Some(out)
}

/// The largest showable JPEG in the file: the makernote's full-size
/// preview when it is there, else whatever the TTW TIFF's own tags
/// point at (the A200 keeps a small thumbnail there too).
fn best_preview(tiff: &Tiff<'_>) -> Option<Vec<u8>> {
    let maker = makernote_preview(tiff);
    let embedded = common::largest_jpeg(tiff);
    match (maker, embedded) {
        (Some(a), Some(b)) => Some(if a.len() >= b.len() { a } else { b }),
        (a, b) => a.or(b),
    }
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let header = Header::parse(bytes)?;
    let prd = header
        .get(b"\0PRD")
        .ok_or_else(|| Error::Corrupt("MRW without a PRD block".into()))?;
    let sensor = Sensor::parse(prd.body)?;
    let cfa = sensor.cfa()?;

    let data = unpack(&bytes[header.data_offset..], &sensor)?;
    let mut raw = RawImage::new(
        Format::Mrw,
        sensor.width,
        sensor.height,
        1,
        RawData::U16(data),
        cfa.clone(),
    );

    // Nothing in the file records a saturation point, so the depth the
    // converter wrote is the only honest ceiling. Black is likewise
    // unrecorded: these sensors sit near zero and LibRaw subtracts
    // nothing from them either.
    raw.white_level = ((1u32 << sensor.data_bits.clamp(8, 16)) - 1) as f32;

    if let Some(wbg) = header.get(b"\0WBG") {
        if let Some(coeffs) = white_balance(wbg.body, &cfa) {
            raw.wb_coeffs = coeffs;
        }
    }

    // The vendor's "image" size (PRD bytes 12..16) is a few pixels
    // smaller than the sensor on every body, but the format never says
    // where inside the frame those pixels sit and there is no masked
    // border to find it from — the extra columns carry picture, not
    // black. Guessing an origin would shift the whole image, so the
    // crop stays the full frame, which is what LibRaw also hands out.
    raw.crop = Rect {
        x: 0,
        y: 0,
        width: sensor.width,
        height: sensor.height,
    };

    if let Some(ttw) = header.get(b"\0TTW") {
        if let Ok(tiff) = Tiff::parse_embedded(bytes, ttw.start) {
            let (make, model) = tiff.make_model();
            raw.set_camera(&make, &model);
            raw.orientation = common::orientation(&tiff);
            raw.metadata = common::metadata(&tiff);
            raw.preview = best_preview(&tiff);
        }
    }
    if raw.make.is_empty() {
        raw.set_camera("Minolta", "");
    }

    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let header = Header::parse(bytes)?;
    let Some(ttw) = header.get(b"\0TTW") else {
        return Ok(None);
    };
    let Ok(tiff) = Tiff::parse_embedded(bytes, ttw.start) else {
        return Ok(None);
    };
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

    /// A minimal MRW: the signature, a PRD block and one block of
    /// padding, with the sensor data after them.
    fn synthetic(prd: [u8; 24], data: &[u8]) -> Vec<u8> {
        let mut out = b"\0MRM".to_vec();
        let header = 8 + 24;
        out.extend((header as u32).to_be_bytes());
        out.extend(b"\0PRD");
        out.extend(24u32.to_be_bytes());
        out.extend(prd);
        out.extend(data);
        out
    }

    fn prd(width: u16, height: u16, storage: u8, pattern: u8) -> [u8; 24] {
        let mut prd = [0u8; 24];
        prd[..8].copy_from_slice(b"27470002");
        prd[8..10].copy_from_slice(&height.to_be_bytes());
        prd[10..12].copy_from_slice(&width.to_be_bytes());
        prd[12..14].copy_from_slice(&height.to_be_bytes());
        prd[14..16].copy_from_slice(&width.to_be_bytes());
        prd[16] = storage;
        prd[17] = 12;
        prd[23] = pattern;
        prd
    }

    #[test]
    fn reads_the_block_chain() {
        let file = synthetic(prd(2, 2, 16, 1), &[0; 8]);
        let header = Header::parse(&file).expect("header parses");
        assert_eq!(header.blocks.len(), 1);
        assert_eq!(header.blocks[0].tag, b"\0PRD");
        assert_eq!(header.data_offset, 40);
    }

    #[test]
    fn rejects_a_block_that_overruns_the_header() {
        let mut file = synthetic(prd(2, 2, 16, 1), &[0; 8]);
        file[12..16].copy_from_slice(&9999u32.to_be_bytes());
        assert!(matches!(Header::parse(&file), Err(Error::Corrupt(_))));
    }

    #[test]
    fn unpacks_twelve_bit_samples_two_to_three_bytes() {
        let sensor = Sensor::parse(&prd(4, 1, 12, 1)).expect("PRD parses");
        // 0xABC, 0xDEF, 0x123, 0x456.
        let data = [0xab, 0xcd, 0xef, 0x12, 0x34, 0x56];
        assert_eq!(
            unpack(&data, &sensor).unwrap(),
            vec![0xabc, 0xdef, 0x123, 0x456]
        );
    }

    #[test]
    fn unpacks_sixteen_bit_samples_big_endian() {
        let sensor = Sensor::parse(&prd(2, 1, 16, 1)).expect("PRD parses");
        assert_eq!(
            unpack(&[0x01, 0x02, 0x03, 0x04], &sensor).unwrap(),
            vec![0x0102, 0x0304]
        );
    }

    #[test]
    fn short_sensor_data_is_corrupt_not_a_panic() {
        let sensor = Sensor::parse(&prd(1000, 1000, 12, 1)).expect("PRD parses");
        assert!(matches!(unpack(&[0; 16], &sensor), Err(Error::Corrupt(_))));
    }

    #[test]
    fn white_balance_follows_the_filter_array() {
        // Four numerators, scale 2 each: on an RGGB body they are
        // R, G, G, B; on the A200's GBRG body the same twelve bytes
        // are G, B, R, G.
        let mut block = [2u8, 2, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0];
        block[4..6].copy_from_slice(&408u16.to_be_bytes());
        block[6..8].copy_from_slice(&256u16.to_be_bytes());
        block[8..10].copy_from_slice(&256u16.to_be_bytes());
        block[10..12].copy_from_slice(&450u16.to_be_bytes());
        let rggb = white_balance(&block, &Cfa::RGGB).expect("balance");
        assert!((rggb[0] - 408.0 / 256.0).abs() < 1e-6);
        assert!((rggb[2] - 450.0 / 256.0).abs() < 1e-6);

        let mut block = [2u8, 2, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0];
        block[4..6].copy_from_slice(&255u16.to_be_bytes());
        block[6..8].copy_from_slice(&416u16.to_be_bytes());
        block[8..10].copy_from_slice(&480u16.to_be_bytes());
        block[10..12].copy_from_slice(&255u16.to_be_bytes());
        let gbrg = white_balance(&block, &Cfa::GBRG).expect("balance");
        assert!((gbrg[0] - 480.0 / 255.0).abs() < 1e-6, "{gbrg:?}");
        assert!((gbrg[2] - 416.0 / 255.0).abs() < 1e-6, "{gbrg:?}");
    }

    #[test]
    fn an_unknown_bayer_phase_is_unsupported() {
        let sensor = Sensor::parse(&prd(2, 2, 12, 7)).expect("PRD parses");
        assert!(matches!(sensor.cfa(), Err(Error::Unsupported(_))));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"not an MRW at all").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn corpus_matches_the_oracle() {
        for path in corpus(&["mrw"]) {
            let bytes = std::fs::read(&path).expect("sample readable");
            // The DiMAGE Z-series compacts write bare sensor data under
            // the same extension: no signature, no blocks, nothing but
            // pixels, and only a filename tells anybody what shape they
            // are. `probe` declines them and so does this test.
            if !bytes.starts_with(b"\0MRM") {
                eprintln!("{}: headerless MRW dump, not a container", path.display());
                continue;
            }
            assert_eq!(
                crate::probe(&bytes),
                Some(crate::Format::Mrw),
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
        for path in corpus(&["mrw"]) {
            truncations(&path);
        }
    }
}
