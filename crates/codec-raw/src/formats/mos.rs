//! Leaf MOS: the raw of the Leaf Valeo, Aptus and AFi backs (and of
//! the Aptus-II backs Phase One sold after buying Leaf).
//!
//! An ordinary TIFF, but one that says almost nothing about itself in
//! TIFF terms: IFD0 *is* the sensor image, marked BlackIsZero rather
//! than CFA, with no Make, no Model, no Exif IFD and no DNG tags. The
//! camera's own name arrives in the XMP packet (tag 0x02BC) and
//! everything else in a private blob under tag 0x8606, "LeafData".
//!
//! LeafData is a tree of records, each
//!
//! ```text
//! "PKTS" | u32 version | 40-byte NUL-padded name | u32 big-endian length | data
//! ```
//!
//! and a record whose data begins with `PKTS` is a container of more
//! of them — `camera_profile` holds `CamProf_capture_profile`, which
//! holds `CaptProf_*`, and so on. Leaf values are ASCII with
//! newline-separated fields, so `NeutObj_neutrals` reads
//! `"2096\n1659\n2096\n1507"`.
//!
//! The sensor data is either a plain 16-bit raster or, under TIFF
//! compression 99, a lossless JPEG with two components: each row of
//! the frame is one row of a half-width two-component scan, which is
//! the same as a plain raster of the row once the components are
//! interleaved — exactly what [`crate::ljpeg`] hands back.

use crate::formats::common;
use crate::tiff::{tags, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, Orientation, RawData, RawImage, Result};

/// The LeafData blob.
const LEAF_DATA: u16 = 0x8606;
/// The XMP packet, where the camera names itself.
const XMP: u16 = 0x02BC;

/// One record of the LeafData tree: its name and the bytes of its
/// value. Containers are flattened away — a caller wants
/// `NeutObj_neutrals`, not the four levels of profile above it — and
/// names are unique enough across the tree for that to be safe.
struct Leaf<'a> {
    records: Vec<(&'a str, &'a [u8])>,
}

impl<'a> Leaf<'a> {
    /// Walk the record tree depth-first. Bounded by the blob and by a
    /// nesting limit, so a file that claims a record contains itself
    /// cannot recurse without end.
    fn parse(blob: &'a [u8]) -> Leaf<'a> {
        let mut records = Vec::new();
        Leaf::walk(blob, 0, &mut records);
        Leaf { records }
    }

    fn walk(blob: &'a [u8], depth: usize, out: &mut Vec<(&'a str, &'a [u8])>) {
        if depth > 8 {
            return;
        }
        let mut at = 0usize;
        // 4 magic + 4 version + 40 name + 4 length.
        while at + 52 <= blob.len() {
            if &blob[at..at + 4] != b"PKTS" {
                break;
            }
            let name = &blob[at + 8..at + 48];
            let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
            let length =
                u32::from_be_bytes([blob[at + 48], blob[at + 49], blob[at + 50], blob[at + 51]])
                    as usize;
            let start = at + 52;
            let end = start.saturating_add(length).min(blob.len());
            let data = &blob[start..end];
            if let Ok(name) = std::str::from_utf8(name) {
                out.push((name, data));
            }
            if data.starts_with(b"PKTS") {
                Leaf::walk(data, depth + 1, out);
            }
            // A zero-length record would spin forever otherwise.
            at = end.max(at + 52);
        }
    }

    fn get(&self, name: &str) -> Option<&'a [u8]> {
        self.records
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    }

    /// A value's newline-separated fields, NUL trimmed.
    fn fields(&self, name: &str) -> Vec<&'a str> {
        let Some(value) = self.get(name) else {
            return Vec::new();
        };
        let end = value.iter().position(|b| *b == 0).unwrap_or(value.len());
        match std::str::from_utf8(&value[..end]) {
            Ok(text) => text
                .split('\n')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn numbers(&self, name: &str) -> Vec<f64> {
        self.fields(name)
            .iter()
            .filter_map(|f| f.parse().ok())
            .collect()
    }
}

/// The raw bytes an entry points at.
///
/// Not [`crate::tiff::Entry::bytes`]: Leaf types both of the blobs
/// this module wants — LeafData and the XMP packet — as ASCII, and an
/// ASCII value stops at its first NUL, which LeafData reaches four
/// bytes in. The entry's own offset and count are the whole thing.
fn blob<'a>(tiff: &Tiff<'a>, entry: &crate::tiff::Entry) -> Option<&'a [u8]> {
    let length = entry.count.checked_mul(entry.kind.size())?;
    tiff.bytes()
        .get(entry.offset..entry.offset.checked_add(length)?)
}

/// `tiff:Make` / `tiff:Model` out of the XMP packet, which Leaf
/// writes either as elements or as attributes depending on the
/// firmware.
fn xmp_value(xmp: &str, key: &str) -> Option<String> {
    for (open, close) in [(format!("<{key}>"), '<'), (format!("{key}=\""), '"')] {
        if let Some(start) = xmp.find(&open) {
            let rest = &xmp[start + open.len()..];
            if let Some(end) = rest.find(close) {
                let value = rest[..end].trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Leaf's lossless-JPEG encoder numbers the scan's components one
/// higher than the frame's: SOF3 declares components 0 and 1, SOS
/// then asks for 1 and 2. T.81 says a scan may only name components
/// the frame declared, and [`crate::ljpeg`] is right to refuse, so
/// the header is corrected here before the stream is handed over.
///
/// Returns `None` when the stream is already consistent, so the
/// ordinary case costs no copy.
fn fix_scan_component_ids(stream: &[u8]) -> Option<Vec<u8>> {
    if stream.get(0..2) != Some(&[0xFF, 0xD8]) {
        return None;
    }
    let mut frame: Vec<u8> = Vec::new();
    let mut at = 2usize;
    loop {
        while stream.get(at) == Some(&0xFF) && stream.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        if stream.get(at) != Some(&0xFF) {
            return None;
        }
        let marker = *stream.get(at + 1)?;
        let length = stream
            .get(at + 2..at + 4)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .filter(|l| *l >= 2)?;
        let body = stream.get(at + 4..at + 2 + length)?;
        match marker {
            // Any SOF: precision, height, width, then the component
            // count and three bytes each.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let count = *body.get(5)? as usize;
                frame = (0..count)
                    .map(|i| body.get(6 + i * 3).copied())
                    .collect::<Option<Vec<u8>>>()?;
            }
            0xDA => {
                let count = *body.first()? as usize;
                let ids: Vec<u8> = (0..count)
                    .map(|i| body.get(1 + i * 2).copied())
                    .collect::<Option<Vec<u8>>>()?;
                if frame.is_empty() || ids.iter().all(|id| frame.contains(id)) {
                    return None;
                }
                // Only the uniform off-by-one is worth correcting; a
                // stream wrong in some other way is not this bug.
                if !ids
                    .iter()
                    .all(|id| id.checked_sub(1).is_some_and(|c| frame.contains(&c)))
                {
                    return None;
                }
                let mut fixed = stream.to_vec();
                for i in 0..count {
                    fixed[at + 5 + i * 2] -= 1;
                }
                return Some(fixed);
            }
            0xD9 => return None,
            _ => {}
        }
        at += 2 + length;
    }
}

/// Whether the scan's rows are the frame's rows, or two halves read
/// from opposite edges of the sensor inward.
///
/// The AFi-II 12 reads its sensor from the top and the bottom at the
/// same time: scan row 0 is the frame's first row, scan row 1 its
/// last, scan row 2 its second, and so on. The Aptus 22 and Aptus 75
/// do not. Nothing in their LeafData says which — the two profiles
/// differ in a dozen fields (version, back type, dark-correction
/// type, an extra calibration block) and none of them names a
/// readout order — so the frame itself has to answer.
///
/// The test costs one pass over a few dozen rows and is not a close
/// call. Under a plain readout, scan rows two apart are the same two
/// CFA colours two rows apart on the sensor: nearly identical. Under
/// a split readout they are *adjacent* rows, so they carry the other
/// two colours and differ enormously, while rows four apart are the
/// same colours. The margin between the two on real frames is more
/// than twenty-fold, and a frame flat enough to score near the
/// threshold has no vertical structure for the choice to damage.
fn is_split_readout(scan: &[u16], width: usize, height: usize) -> bool {
    if height < 16 || !height.is_multiple_of(2) {
        return false;
    }
    let row = |r: usize| &scan[r * width..(r + 1) * width];
    let distance = |a: usize, b: usize| -> u64 {
        row(a)
            .iter()
            .zip(row(b))
            .step_by(width / 512 + 1)
            .map(|(x, y)| x.abs_diff(*y) as u64)
            .sum()
    };
    // Sample from the middle of the top half, where a real frame has
    // picture rather than masked border.
    let first = (height / 4) & !1;
    let count = 32.min((height / 2 - 4).saturating_sub(first) / 2);
    if count == 0 {
        return false;
    }
    let (mut two, mut four) = (0u64, 0u64);
    for i in 0..count {
        let r = first + i * 2;
        two += distance(r, r + 2);
        four += distance(r, r + 4);
    }
    two > four.saturating_mul(4)
}

/// The IFD holding the sensor: the largest 16-bit single-sample image
/// in the file. Leaf marks it BlackIsZero, so the CFA photometric
/// cannot be the test.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Result<&'a Ifd> {
    tiff.all()
        .into_iter()
        .filter(|ifd| {
            ifd.get(tags::BITS_PER_SAMPLE).and_then(|e| e.u32(0)) == Some(16)
                && ifd
                    .get(tags::SAMPLES_PER_PIXEL)
                    .and_then(|e| e.u32(0))
                    .unwrap_or(1)
                    == 1
        })
        .max_by_key(|ifd| {
            let side = |tag| ifd.get(tag).and_then(|e| e.u64(0)).unwrap_or(0);
            side(tags::IMAGE_WIDTH).saturating_mul(side(tags::IMAGE_LENGTH))
        })
        .ok_or_else(|| Error::Corrupt("no 16-bit image IFD in this Leaf file".into()))
}

/// Leaf's colour codes, as `CaptProf_mosaic_pattern` lists them for
/// the four positions of the 2x2, reading across then down.
///
/// Two backs pin the mapping down: the Aptus 75 writes `2 0 1 3` for
/// a frame LibRaw calls GBRG and the AFi-II 12 writes `1 3 2 0` for
/// one it calls RGGB, and only 0=blue, 1=red, 2 and 3 = the two
/// greens satisfies both. The greens are reported as plain green
/// rather than as green and [`CfaColor::Green2`], because a Leaf
/// sensor balances them together.
fn color(code: u32) -> Option<CfaColor> {
    Some(match code {
        0 => CfaColor::Blue,
        1 => CfaColor::Red,
        2 | 3 => CfaColor::Green,
        _ => return None,
    })
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = raw_ifd(&tiff)?;
    let layout = ImageLayout::of(&tiff, ifd)?;
    let (width, height) = (layout.width, layout.height);
    if width == 0 || height == 0 {
        return Err(Error::Corrupt("Leaf frame with no size".into()));
    }

    let leaf = ifd
        .get(LEAF_DATA)
        .or_else(|| tiff.find(LEAF_DATA))
        .and_then(|e| blob(&tiff, e))
        .map(Leaf::parse);
    // A Leaf frame is one plane of CFA samples; the backs can also
    // write three-plane files, which are a developed image rather
    // than a raw and have no business here.
    if let Some(planes) = leaf
        .as_ref()
        .map(|l| l.numbers("CaptProf_number_of_planes"))
    {
        if planes.first().is_some_and(|p| *p != 1.0) {
            return Err(Error::Unsupported(format!(
                "Leaf file with {} planes",
                planes[0]
            )));
        }
    }

    let samples = match layout.compression {
        1 => {
            // Reserve no more than the strips could hold: a forged
            // frame size must not turn into a gigabyte request.
            let stored: usize = layout.chunks.iter().map(|(_, len)| *len).sum();
            let mut out =
                Vec::with_capacity(crate::frame_samples(width, height, 1)?.min(stored / 2 + 1));
            let little_endian = tiff.little_endian();
            for (start, len) in &layout.chunks {
                out.extend(
                    bytes[*start..*start + *len]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| {
                            if little_endian {
                                u16::from_le_bytes([b[0], b[1]])
                            } else {
                                u16::from_be_bytes([b[0], b[1]])
                            }
                        }),
                );
            }
            if out.len() < width * height {
                return Err(Error::Corrupt(format!(
                    "Leaf raster holds {} of {} samples",
                    out.len(),
                    width * height
                )));
            }
            out.truncate(width * height);
            out
        }
        // 99 is Leaf's private number for lossless JPEG. The scan is
        // two components over half the frame's width, which comes
        // back interleaved — that is, as the frame's own rows.
        99 => {
            let stored: usize = layout.chunks.iter().map(|(_, len)| *len).sum();
            let mut out =
                Vec::with_capacity(crate::frame_samples(width, height, 1)?.min(stored * 4 + 1));
            for (start, len) in &layout.chunks {
                let stream = &bytes[*start..*start + *len];
                let fixed = fix_scan_component_ids(stream);
                let frame = crate::ljpeg::decode(fixed.as_deref().unwrap_or(stream))?;
                if frame.width * frame.components != width {
                    return Err(Error::Corrupt(format!(
                        "Leaf scan is {}x{} of {} components for a {width}-wide frame",
                        frame.width, frame.height, frame.components
                    )));
                }
                out.extend_from_slice(&frame.data);
            }
            if out.len() < width * height {
                return Err(Error::Corrupt(format!(
                    "Leaf scans hold {} of {} samples",
                    out.len(),
                    width * height
                )));
            }
            out.truncate(width * height);
            out
        }
        other => return Err(Error::Unsupported(format!("Leaf compression {other}"))),
    };

    // Put the two halves back in order when the back read them from
    // opposite edges.
    let samples = if is_split_readout(&samples, width, height) {
        let mut ordered = vec![0u16; samples.len()];
        for k in 0..height / 2 {
            let (top, bottom) = (k, height - 1 - k);
            ordered[top * width..(top + 1) * width]
                .copy_from_slice(&samples[2 * k * width..(2 * k + 1) * width]);
            ordered[bottom * width..(bottom + 1) * width]
                .copy_from_slice(&samples[(2 * k + 1) * width..(2 * k + 2) * width]);
        }
        ordered
    } else {
        samples
    };

    let cfa = leaf
        .as_ref()
        .map(|l| l.numbers("CaptProf_mosaic_pattern"))
        .filter(|pattern| pattern.len() == 4)
        .and_then(|pattern| {
            let colors: Option<Vec<CfaColor>> = pattern.iter().map(|c| color(*c as u32)).collect();
            colors.map(|c| Cfa::Bayer([c[0], c[1], c[2], c[3]]))
        })
        // Every Leaf back seen so far says which pattern it has; RGGB
        // is the family's usual one for a file that does not.
        .unwrap_or(Cfa::RGGB);

    let mut raw = RawImage::new(Format::Mos, width, height, 1, RawData::U16(samples), cfa);
    // Leaf backs are 14-bit sensors written into a 16-bit container:
    // both corpus frames saturate at exactly 16383 and neither the
    // TIFF nor LeafData records a level.
    raw.white_level = 16383.0;

    if let Some(leaf) = &leaf {
        // NeutObj_neutrals is four integers: a reference value and
        // then the neutral point of red, green and blue. The
        // multiplier that makes a channel neutral is the reference
        // over it.
        let neutrals = leaf.numbers("NeutObj_neutrals");
        if neutrals.len() >= 4 && neutrals.iter().all(|v| *v > 0.0) {
            let (value, green) = (neutrals[0], neutrals[2]);
            let mul = |n: f64| (value / n) as f32;
            let green = mul(green);
            if green > 0.0 {
                raw.wb_coeffs = [mul(neutrals[1]) / green, 1.0, mul(neutrals[3]) / green, 1.0];
            }
        }
        // The back records how far the frame is turned rather than an
        // EXIF orientation; a Leaf shot on a portrait-mounted back
        // says 90.
        if let Some(angle) = leaf.numbers("ImgProf_rotation_angle").first() {
            raw.orientation = match *angle as i32 {
                90 => Orientation::Rotate90CW,
                180 => Orientation::Rotate180,
                270 => Orientation::Rotate270CW,
                _ => Orientation::Normal,
            };
        }
        if let Some(jpeg) = leaf.get("JPEG_preview_data") {
            if jpeg.starts_with(&[0xFF, 0xD8]) {
                raw.preview = Some(jpeg.to_vec());
            }
        }
    }
    if raw.preview.is_none() {
        raw.preview = common::largest_jpeg(&tiff);
    }

    // The TIFF has no Make or Model; the XMP packet does.
    let xmp = tiff
        .root()
        .get(XMP)
        .and_then(|e| blob(&tiff, e))
        .map(String::from_utf8_lossy);
    match xmp.as_deref() {
        Some(xmp) => raw.set_camera(
            xmp_value(xmp, "tiff:Make").as_deref().unwrap_or("Leaf"),
            xmp_value(xmp, "tiff:Model").as_deref().unwrap_or(""),
        ),
        None => raw.set_camera("Leaf", ""),
    }
    raw.metadata = common::metadata(&tiff);
    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    if let Some(data) = tiff.find(LEAF_DATA).and_then(|e| blob(&tiff, e)) {
        if let Some(jpeg) = Leaf::parse(data).get("JPEG_preview_data") {
            if jpeg.starts_with(&[0xFF, 0xD8]) {
                return Ok(Some(jpeg.to_vec()));
            }
        }
    }
    Ok(common::largest_jpeg(&tiff))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::hasselblad::corpus;

    /// One PKTS record around a value.
    fn record(name: &str, value: &[u8]) -> Vec<u8> {
        let mut out = b"PKTS".to_vec();
        out.extend_from_slice(&1u32.to_be_bytes());
        let mut padded = name.as_bytes().to_vec();
        padded.resize(40, 0);
        out.extend_from_slice(&padded);
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn records_nest_and_flatten() {
        let inner = [
            record("NeutObj_neutrals", b"2096\n1659\n2096\n1507\0"),
            record("ImgProf_rotation_angle", b"90\0"),
        ]
        .concat();
        let blob = [record("camera_profile", &inner), record("tail", b"x\0")].concat();
        let leaf = Leaf::parse(&blob);
        assert_eq!(
            leaf.numbers("NeutObj_neutrals"),
            vec![2096.0, 1659.0, 2096.0, 1507.0]
        );
        assert_eq!(leaf.numbers("ImgProf_rotation_angle"), vec![90.0]);
        assert_eq!(leaf.fields("tail"), vec!["x"]);
        // The container itself is kept, so nothing is lost.
        assert!(leaf.get("camera_profile").is_some());
    }

    #[test]
    fn a_lying_length_does_not_run_away() {
        let mut blob = record("thing", b"value\0");
        // Claim far more data than the blob holds.
        let at = blob.len() - 6 - 4;
        blob[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        let leaf = Leaf::parse(&blob);
        assert_eq!(leaf.records.len(), 1);
    }

    #[test]
    fn zero_length_records_terminate() {
        let blob = [record("a", b""), record("b", b""), record("c", b"z")].concat();
        assert_eq!(Leaf::parse(&blob).records.len(), 3);
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // A record whose data is a copy of itself, sixteen deep.
        let mut blob = record("leaf", b"x\0");
        for _ in 0..16 {
            blob = record("nest", &blob);
        }
        // Terminates, and the limit is what stops it.
        assert!(Leaf::parse(&blob).records.len() <= 10);
    }

    #[test]
    fn the_mosaic_codes_match_the_two_known_backs() {
        let bayer = |codes: [u32; 4]| {
            let c: Vec<CfaColor> = codes.iter().map(|c| color(*c).unwrap()).collect();
            Cfa::Bayer([c[0], c[1], c[2], c[3]])
        };
        // Aptus 75: LibRaw calls this GBRG.
        assert_eq!(bayer([2, 0, 1, 3]), Cfa::GBRG);
        // AFi-II 12: RGGB.
        assert_eq!(bayer([1, 3, 2, 0]), Cfa::RGGB);
        assert_eq!(color(7), None);
    }

    #[test]
    fn xmp_is_read_as_element_or_attribute() {
        assert_eq!(
            xmp_value("<tiff:Make>Leaf</tiff:Make>", "tiff:Make").as_deref(),
            Some("Leaf")
        );
        assert_eq!(
            xmp_value(
                r#"<rdf:Description tiff:Model="Leaf Aptus 75"/>"#,
                "tiff:Model"
            )
            .as_deref(),
            Some("Leaf Aptus 75")
        );
        assert_eq!(xmp_value("<x/>", "tiff:Make"), None);
    }

    #[test]
    fn the_scan_component_off_by_one_is_corrected() {
        // SOI, SOF3 with components 0 and 1, SOS asking for 1 and 2.
        let mut stream = vec![0xFF, 0xD8, 0xFF, 0xC3, 0, 14, 16, 0, 2, 0, 4, 2];
        stream.extend_from_slice(&[0, 0x11, 0, 1, 0x11, 1]);
        stream.extend_from_slice(&[0xFF, 0xDA, 0, 10, 2, 1, 0x00, 2, 0x10, 1, 0, 0]);
        let fixed = fix_scan_component_ids(&stream).expect("a correction");
        assert_eq!(fixed[stream.len() - 7], 0);
        assert_eq!(fixed[stream.len() - 5], 1);
        // Corrected once, it needs no second pass.
        assert!(fix_scan_component_ids(&fixed).is_none());
    }

    #[test]
    fn a_split_readout_is_told_from_a_plain_one() {
        // A frame whose rows alternate between two levels, as a CFA
        // frame's do, laid out plainly.
        let (width, height) = (64, 64);
        let plain: Vec<u16> = (0..width * height)
            .map(|i| if (i / width) % 2 == 0 { 1000 } else { 4000 })
            .collect();
        assert!(!is_split_readout(&plain, width, height));
        // The same frame as the sensor's two halves would arrive.
        let mut split = vec![0u16; plain.len()];
        for k in 0..height / 2 {
            split[2 * k * width..(2 * k + 1) * width]
                .copy_from_slice(&plain[k * width..(k + 1) * width]);
            let bottom = height - 1 - k;
            split[(2 * k + 1) * width..(2 * k + 2) * width]
                .copy_from_slice(&plain[bottom * width..(bottom + 1) * width]);
        }
        assert!(is_split_readout(&split, width, height));
        // A frame with no vertical structure at all must not be
        // reordered: there is nothing to gain and the test would be
        // guessing.
        let flat = vec![2000u16; width * height];
        assert!(!is_split_readout(&flat, width, height));
    }

    #[test]
    fn garbage_is_not_a_leaf() {
        assert!(decode(&[0u8; 64]).is_err());
        assert!(decode(b"II*\0\x08\0\0\0").is_err());
    }

    #[test]
    fn corpus_matches_the_oracle() {
        let files = corpus::files(&["mos"]);
        for path in &files {
            let bytes = std::fs::read(path).unwrap();
            let name = corpus::name(path);
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Mos),
                "{name} did not probe as Leaf MOS"
            );
            let raw = decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            raw.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            corpus::check_against_oracle(path, &raw);
            corpus::check_against_identify(path, &raw, &[]);
            corpus::check_cfa(path, &raw);
            corpus::check_preview(path, &raw);
        }
        eprintln!("mos: {} corpus files checked", files.len());
    }

    #[test]
    fn corpus_truncations_do_not_panic() {
        for path in corpus::files(&["mos"]) {
            corpus::check_truncations(&path, decode);
        }
    }
}
