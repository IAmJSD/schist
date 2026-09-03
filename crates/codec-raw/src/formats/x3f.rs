//! Sigma X3F: the raw of the Foveon cameras, whose sensor stacks
//! three photodiodes at every pixel instead of putting a colour
//! filter over each one. An X3F frame therefore has three samples a
//! pixel and no CFA at all — [`Cfa::None`], `cpp = 3`.
//!
//! The container is Sigma's own. A file opens with `FOVb`, a version
//! and a header whose shape depends on that version, and *ends* with
//! a `u32` pointing at the directory: `SECd`, a version, a count, and
//! then twelve bytes an entry — offset, length, and a four-character
//! type.
//!
//! The sections that matter:
//!
//!  * `IMA2` (and `IMAG` on the oldest bodies) — an image, headed
//!    `SECi` with a type (1 raw, 2 processed), a data format, the
//!    dimensions and a row size. A camera writes two or three of
//!    them: a full-size JPEG, sometimes a small one, and the sensor
//!    data.
//!  * `PROP` — a property list, UTF-16 keys and values, holding the
//!    camera's name, exposure and white-balance mode. The Quattro
//!    bodies dropped it and put an Exif block in their preview JPEG
//!    instead, so both are read.
//!  * `CAMF` — the camera's calibration, including the matrices that
//!    turn Foveon's three stacked responses into colour. It is
//!    obfuscated, and decoding it is out of this module's scope:
//!    `color_matrix` is left to the camera table.
//!
//! What this module does **not** do is the sensor data of any modern
//! body. The Merrill's format 30 and the Quattro's format 35 are
//! Foveon's "TRUE" codes, and the Quattro's top layer carries four
//! times the resolution of the two below it — a frame that does not
//! fit [`RawImage`]'s one-size-per-plane shape without a resampling
//! policy this crate has not chosen. Both return
//! [`Error::Unsupported`]; the preview and the metadata still come
//! out, which is what a gallery needs from a file it cannot develop.
//!
//! LibRaw 0.21 cannot read X3F at all, so there is no oracle frame
//! for any of these files and nothing here is checked against one.

use crate::tiff::Tiff;
use crate::{Cfa, Error, Format, Metadata, Orientation, RawData, RawImage, Result};

/// Every X3F begins here.
const MAGIC: &[u8; 4] = b"FOVb";

/// One directory entry: where a section is and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Section {
    offset: usize,
    length: usize,
    kind: [u8; 4],
}

/// An image section's header, and where its data starts.
#[derive(Debug, Clone, Copy)]
struct Image {
    /// 1 for the sensor's own data, 2 for a processed picture.
    kind: u32,
    /// How the data is coded; see [`formats`].
    format: u32,
    columns: usize,
    rows: usize,
    data: usize,
    length: usize,
}

/// The data formats an image section declares.
mod formats {
    /// Three 16-bit samples a pixel, uncompressed.
    pub const PLAIN: u32 = 3;
    /// The Huffman code of the SD9/SD10/SD14 era.
    pub const HUFFMAN: u32 = 11;
    // 18 and 25 are the baseline JPEGs a camera writes for its
    // previews; they are recognised by their SOI rather than by
    // number, because the code varies with the body and any of them
    // may be the largest one.
    /// Foveon's "TRUE" entropy code, on the DP/SD Merrill bodies.
    pub const TRUE: u32 = 30;
    pub const MERRILL: u32 = 32;
    /// The Quattro's code, whose top layer is twice as wide and twice
    /// as tall as the two below it.
    pub const QUATTRO: u32 = 35;
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The file's sections, in directory order.
fn directory(bytes: &[u8]) -> Result<Vec<Section>> {
    if bytes.get(0..4) != Some(MAGIC) {
        return Err(Error::Corrupt("not an X3F: no FOVb signature".into()));
    }
    // The directory's position is the last four bytes of the file.
    let end = bytes
        .len()
        .checked_sub(4)
        .ok_or_else(|| Error::Corrupt("X3F too short to hold a directory pointer".into()))?;
    let start = u32_at(bytes, end).unwrap_or(0) as usize;
    if bytes.get(start..start + 4) != Some(b"SECd") {
        return Err(Error::Corrupt(
            "X3F directory pointer does not lead to a SECd".into(),
        ));
    }
    let count = u32_at(bytes, start + 8).unwrap_or(0) as usize;
    // Twelve bytes an entry: a count larger than the file could hold
    // is a lie, and capping it keeps the allocation honest.
    let count = count.min(bytes.len() / 12);
    let mut sections = Vec::with_capacity(count);
    for i in 0..count {
        let at = start + 12 + i * 12;
        let (Some(offset), Some(length)) = (u32_at(bytes, at), u32_at(bytes, at + 4)) else {
            break;
        };
        let Some(kind) = bytes.get(at + 8..at + 12) else {
            break;
        };
        let (offset, length) = (offset as usize, length as usize);
        // A section reaching past the end of the file is not one.
        if offset
            .checked_add(length)
            .is_none_or(|end| end > bytes.len())
        {
            continue;
        }
        sections.push(Section {
            offset,
            length,
            kind: [kind[0], kind[1], kind[2], kind[3]],
        });
    }
    if sections.is_empty() {
        return Err(Error::Corrupt(
            "X3F directory holds no readable section".into(),
        ));
    }
    Ok(sections)
}

/// An image section's header. Seven `u32`s — magic, version, type,
/// format, columns, rows, row size — and then the data.
fn image(bytes: &[u8], section: &Section) -> Option<Image> {
    if bytes.get(section.offset..section.offset + 4) != Some(b"SECi") {
        return None;
    }
    let word = |i: usize| u32_at(bytes, section.offset + i * 4);
    let data = section.offset + 28;
    Some(Image {
        kind: word(2)?,
        format: word(3)?,
        columns: word(4)? as usize,
        rows: word(5)? as usize,
        data,
        length: section.length.checked_sub(28)?,
    })
}

/// Whether a stream is a JPEG a viewer could show: a real frame
/// header, not a lossless or hierarchical one. Walks marker segments
/// only, and stops at the first frame or at anything malformed.
fn is_displayable_jpeg(stream: &[u8]) -> bool {
    if !stream.starts_with(&[0xFF, 0xD8]) {
        return false;
    }
    let mut at = 2;
    while at + 4 <= stream.len() {
        if stream[at] != 0xFF {
            return false;
        }
        match stream[at + 1] {
            0xFF => {
                at += 1;
                continue;
            }
            0x01 | 0xD0..=0xD8 => {
                at += 2;
                continue;
            }
            0xC0 | 0xC1 | 0xC2 | 0xC9 | 0xCA => return true,
            0xC3 | 0xC5..=0xC7 | 0xCB | 0xCD..=0xCF => return false,
            0xDA | 0xD9 => return false,
            _ => {}
        }
        let length = u16::from_be_bytes([stream[at + 2], stream[at + 3]]) as usize;
        if length < 2 {
            return false;
        }
        at += 2 + length;
    }
    false
}

/// The largest showable JPEG among the file's image sections.
fn largest_jpeg(bytes: &[u8], sections: &[Section]) -> Option<Vec<u8>> {
    sections
        .iter()
        .filter(|s| s.kind == *b"IMA2" || s.kind == *b"IMAG")
        .filter_map(|s| image(bytes, s))
        .filter_map(|i| bytes.get(i.data..i.data + i.length))
        .filter(|stream| is_displayable_jpeg(stream))
        .max_by_key(|stream| stream.len())
        .map(<[u8]>::to_vec)
}

/// The PROP section: a header of six `u32`s (magic, version, entry
/// count, character format, reserved, total length), then a pair of
/// `u32` offsets an entry, then a pool of NUL-terminated UTF-16
/// strings the offsets index **in characters**.
fn properties(bytes: &[u8], section: &Section) -> Vec<(String, String)> {
    if bytes.get(section.offset..section.offset + 4) != Some(b"SECp") {
        return Vec::new();
    }
    let count = u32_at(bytes, section.offset + 8).unwrap_or(0) as usize;
    // Eight bytes an entry, and the pool has to hold something.
    let count = count.min(section.length / 8);
    let pool = section.offset + 24 + count * 8;
    let string = |chars: usize| -> Option<String> {
        let mut at = pool.checked_add(chars.checked_mul(2)?)?;
        let mut out = String::new();
        loop {
            let unit = u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?);
            if unit == 0 {
                return Some(out);
            }
            // The camera writes plain ASCII in UTF-16 units; anything
            // outside the basic plane would be a surrogate, and there
            // is no camera that writes one.
            out.push(char::from_u32(unit as u32)?);
            at += 2;
            if out.len() > 512 {
                return Some(out);
            }
        }
    };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = section.offset + 24 + i * 8;
        let (Some(name), Some(value)) = (u32_at(bytes, at), u32_at(bytes, at + 4)) else {
            break;
        };
        if let (Some(name), Some(value)) = (string(name as usize), string(value as usize)) {
            if !name.is_empty() {
                out.push((name, value));
            }
        }
    }
    out
}

/// The Exif block inside a preview JPEG, which is where the Quattro
/// bodies put what earlier ones put in PROP.
fn preview_exif(jpeg: &[u8]) -> Option<Tiff<'_>> {
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xFF {
            return None;
        }
        let marker = jpeg[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        if matches!(marker, 0xD8..=0xDA) {
            return None;
        }
        let length = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        if length < 2 {
            return None;
        }
        if marker == 0xE1 && jpeg.get(at + 4..at + 10) == Some(b"Exif\0\0") {
            // The TIFF header sits at the end of the marker's own
            // identifier, and every offset in it is relative to there.
            return Tiff::parse_embedded(jpeg, at + 10).ok();
        }
        at += 2 + length;
    }
    None
}

/// A property list, with the lookups the rest of the module wants.
struct Properties(Vec<(String, String)>);

impl Properties {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    }
    fn number(&self, key: &str) -> Option<f32> {
        self.get(key).and_then(|v| v.parse().ok())
    }
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let sections = directory(bytes)?;
    let raw = sections
        .iter()
        .filter(|s| s.kind == *b"IMA2" || s.kind == *b"IMAG")
        .filter_map(|s| image(bytes, s))
        // Type 1 is the sensor's own data; type 2 is a picture the
        // camera made from it.
        .find(|i| i.kind == 1)
        .ok_or_else(|| Error::Unsupported("X3F with no sensor image section".into()))?;

    let samples = match raw.format {
        formats::PLAIN => {
            let count = raw
                .columns
                .checked_mul(raw.rows)
                .and_then(|n| n.checked_mul(3))
                .ok_or_else(|| Error::Corrupt("X3F frame too large".into()))?;
            let end = count
                .checked_mul(2)
                .and_then(|n| raw.data.checked_add(n))
                .ok_or_else(|| Error::Corrupt("X3F frame too large".into()))?;
            let data = bytes
                .get(raw.data..end)
                .ok_or_else(|| Error::Corrupt("X3F plain frame runs past the file".into()))?;
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect()
        }
        formats::HUFFMAN | formats::TRUE | formats::MERRILL => {
            return Err(Error::Unsupported(format!(
                "Foveon entropy-coded sensor data (X3F image format {}); the preview and \
                 the metadata are still readable",
                raw.format
            )))
        }
        formats::QUATTRO => {
            return Err(Error::Unsupported(
                "Foveon Quattro sensor data (X3F image format 35): the top layer has four \
                 times the pixels of the two below it, which needs a resampling policy this \
                 crate has not chosen"
                    .into(),
            ))
        }
        other => return Err(Error::Unsupported(format!("X3F image format {other}"))),
    };

    let mut image = RawImage::new(
        Format::X3f,
        raw.columns,
        raw.rows,
        3,
        RawData::U16(samples),
        // Three stacked photodiodes: every pixel already has all
        // three colours, so there is no filter array to describe.
        Cfa::None,
    );
    describe(bytes, &sections, &mut image);
    image.apply_camera_table();
    Ok(image)
}

/// Everything about a frame that is not its samples: the camera, the
/// exposure, the orientation and the preview. Shared by [`decode`]
/// and used on its own for the frames this module cannot unpack.
fn describe(bytes: &[u8], sections: &[Section], image: &mut RawImage) {
    let properties = Properties(
        sections
            .iter()
            .find(|s| s.kind == *b"PROP")
            .map(|s| properties(bytes, s))
            .unwrap_or_default(),
    );
    let preview = largest_jpeg(bytes, sections);
    // The Quattro bodies dropped PROP and put an ordinary Exif block
    // in the preview instead.
    let exif = preview.as_deref().and_then(preview_exif);

    let (mut make, mut model) = (
        properties.get("CAMMANUF").unwrap_or("").to_string(),
        properties.get("CAMMODEL").unwrap_or("").to_string(),
    );
    if let Some(tiff) = &exif {
        let (exif_make, exif_model) = tiff.make_model();
        if make.is_empty() {
            make = exif_make;
        }
        if model.is_empty() {
            model = exif_model;
        }
    }
    image.set_camera(&make, &model);

    image.metadata = Metadata {
        iso: properties.number("ISO"),
        // SHUTTER is seconds; EXPTIME is the same in microseconds.
        exposure_time: properties
            .number("SHUTTER")
            .or_else(|| properties.number("EXPTIME").map(|t| t / 1e6)),
        f_number: properties.number("APERTURE"),
        focal_length: properties.number("FLENGTH"),
        lens: properties.get("LENSMODEL").map(str::to_string),
        date_time: None,
    };
    if let Some(tiff) = &exif {
        // Exif fills whatever the properties did not, and is the only
        // source of a formatted timestamp: PROP's TIME is a Unix
        // second count, which this crate has no clock to format.
        let from_exif = crate::formats::common::metadata(tiff);
        let take = |a: &mut Option<f32>, b: Option<f32>| {
            if a.is_none() {
                *a = b;
            }
        };
        take(&mut image.metadata.iso, from_exif.iso);
        take(&mut image.metadata.exposure_time, from_exif.exposure_time);
        take(&mut image.metadata.f_number, from_exif.f_number);
        take(&mut image.metadata.focal_length, from_exif.focal_length);
        if image.metadata.lens.is_none() {
            image.metadata.lens = from_exif.lens;
        }
        image.metadata.date_time = from_exif.date_time;
    }

    // ROTATION is degrees clockwise, the way the camera was held.
    image.orientation = match properties.number("ROTATION").map(|r| r as i32) {
        Some(90) => Orientation::Rotate90CW,
        Some(180) => Orientation::Rotate180,
        Some(270) => Orientation::Rotate270CW,
        Some(_) => Orientation::Normal,
        None => exif
            .as_ref()
            .map(crate::formats::common::orientation)
            .unwrap_or_default(),
    };
    // The white balance a Foveon needs lives in the obfuscated CAMF
    // block; PROP only names the mode ("Auto", "Daylight"). Unit
    // multipliers are the honest answer, and the developer's own
    // white balance is what a Foveon frame gets in practice.
    drop(exif);
    image.preview = preview;
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(largest_jpeg(bytes, &directory(bytes)?))
}

/// The camera and exposure of an X3F whose sensor data this module
/// cannot unpack — everything [`decode`] would have filled in.
///
/// A gallery that has a preview and a name for a file is not stuck
/// just because the Foveon entropy code is unread.
pub fn metadata(bytes: &[u8]) -> Result<(String, String, Metadata, Option<Vec<u8>>)> {
    let sections = directory(bytes)?;
    let mut image = RawImage::new(Format::X3f, 1, 1, 3, RawData::U16(vec![0; 3]), Cfa::None);
    describe(bytes, &sections, &mut image);
    Ok((image.make, image.model, image.metadata, image.preview))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::hasselblad::corpus;

    /// A file with one image section and one property list.
    fn build(image_kind: u32, format: u32, columns: u32, rows: u32, data: &[u8]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&0x0003_0000u32.to_le_bytes());
        out.resize(104, 0);
        let image_at = out.len();
        out.extend_from_slice(b"SECi");
        for word in [0x0002_0000u32, image_kind, format, columns, rows, 0] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(data);
        let image_len = out.len() - image_at;

        let property_at = out.len();
        let entries: [(&str, &str); 2] = [("CAMMODEL", "SIGMA DP2 Merrill"), ("ISO", "400")];
        out.extend_from_slice(b"SECp");
        out.extend_from_slice(&0x0002_0000u32.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut pool: Vec<u16> = Vec::new();
        let mut offsets = Vec::new();
        for (name, value) in entries {
            let name_at = pool.len();
            pool.extend(name.encode_utf16());
            pool.push(0);
            let value_at = pool.len();
            pool.extend(value.encode_utf16());
            pool.push(0);
            offsets.push((name_at as u32, value_at as u32));
        }
        for (name, value) in &offsets {
            out.extend_from_slice(&name.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        for unit in &pool {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        let property_len = out.len() - property_at;

        let directory_at = out.len();
        out.extend_from_slice(b"SECd");
        out.extend_from_slice(&0x0002_0000u32.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        for (offset, length, kind) in [
            (image_at, image_len, b"IMA2"),
            (property_at, property_len, b"PROP"),
        ] {
            out.extend_from_slice(&(offset as u32).to_le_bytes());
            out.extend_from_slice(&(length as u32).to_le_bytes());
            out.extend_from_slice(kind);
        }
        out.extend_from_slice(&(directory_at as u32).to_le_bytes());
        out
    }

    #[test]
    fn a_plain_frame_is_three_samples_a_pixel() {
        let pixels: Vec<u16> = (0..2 * 2 * 3).map(|i| 1000 + i as u16).collect();
        let data: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
        let file = build(1, formats::PLAIN, 2, 2, &data);
        let raw = decode(&file).unwrap();
        raw.validate().unwrap();
        assert_eq!((raw.width, raw.height, raw.cpp), (2, 2, 3));
        assert_eq!(raw.cfa, Cfa::None);
        assert_eq!(raw.data, RawData::U16(pixels));
        assert_eq!(raw.clean_model, "DP2 Merrill");
        assert_eq!(raw.metadata.iso, Some(400.0));
    }

    #[test]
    fn the_true_codes_are_unsupported_by_name() {
        for format in [
            formats::HUFFMAN,
            formats::TRUE,
            formats::MERRILL,
            formats::QUATTRO,
        ] {
            let file = build(1, format, 8, 8, &[0; 64]);
            match decode(&file) {
                Err(Error::Unsupported(message)) => {
                    assert!(message.contains("Foveon"), "{message}")
                }
                other => panic!("format {format}: {other:?}"),
            }
        }
    }

    #[test]
    fn properties_are_utf16_indexed_by_character() {
        let file = build(1, formats::PLAIN, 1, 1, &[0; 6]);
        let sections = directory(&file).unwrap();
        let list = properties(&file, sections.iter().find(|s| s.kind == *b"PROP").unwrap());
        assert_eq!(
            list,
            vec![
                ("CAMMODEL".to_string(), "SIGMA DP2 Merrill".to_string()),
                ("ISO".to_string(), "400".to_string()),
            ]
        );
    }

    #[test]
    fn a_lossless_jpeg_is_not_a_preview() {
        // SOF3 is sensor data wearing a JPEG's clothes.
        let lossless = [
            0xFFu8, 0xD8, 0xFF, 0xC3, 0, 11, 16, 0, 1, 0, 1, 1, 0, 0x11, 0,
        ];
        assert!(!is_displayable_jpeg(&lossless));
        let baseline = [
            0xFFu8, 0xD8, 0xFF, 0xC0, 0, 11, 8, 0, 1, 0, 1, 1, 0, 0x11, 0,
        ];
        assert!(is_displayable_jpeg(&baseline));
        assert!(!is_displayable_jpeg(&[]));
    }

    #[test]
    fn garbage_is_not_an_x3f() {
        assert!(decode(&[0u8; 64]).is_err());
        assert!(decode(b"FOVb").is_err());
        let file = build(1, formats::PLAIN, 2, 2, &[0; 24]);
        for cut in 0..file.len() {
            let _ = decode(&file[..cut]);
            let _ = preview(&file[..cut]);
        }
    }

    #[test]
    fn corpus_previews_and_metadata() {
        let files = corpus::files(&["x3f"]);
        for path in &files {
            let bytes = std::fs::read(path).unwrap();
            let name = corpus::name(path);
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::X3f),
                "{name} did not probe as X3F"
            );
            // No LibRaw build reads X3F, so there is no oracle frame
            // for any of these: the checks are the ones that can be
            // made without one.
            let (make, model, metadata, jpeg) = super::metadata(&bytes).unwrap();
            assert_eq!(make, "SIGMA", "{name}: make");
            assert!(model.starts_with("SIGMA"), "{name}: model {model:?}");
            let jpeg = jpeg.unwrap_or_else(|| panic!("{name}: no preview"));
            let decoded = image::load_from_memory(&jpeg)
                .unwrap_or_else(|e| panic!("{name}: preview will not decode: {e}"));
            assert!(
                decoded.width() >= 640,
                "{name}: preview is only {}px wide",
                decoded.width()
            );
            assert_eq!(preview(&bytes).unwrap(), Some(jpeg), "{name}");
            match decode(&bytes) {
                Ok(raw) => raw.validate().unwrap_or_else(|e| panic!("{name}: {e}")),
                Err(Error::Unsupported(message)) => {
                    eprintln!("{name}: {model}, {metadata:?} — unsupported: {message}")
                }
                Err(e) => panic!("{name}: {e}"),
            }
        }
        eprintln!("x3f: {} corpus files checked", files.len());
    }

    #[test]
    fn corpus_truncations_do_not_panic() {
        for path in corpus::files(&["x3f"]) {
            corpus::check_truncations(&path, decode);
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            for cut in [0, 1, 4, 64, 1024, bytes.len() / 2, bytes.len() - 1] {
                let _ = preview(&bytes[..cut.min(bytes.len())]);
            }
        }
    }
}
