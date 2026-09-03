//! ORF — Olympus and OM System raw.
//!
//! An ORF is an ordinary little-endian TIFF wearing a private magic
//! (`IIRO`, `IIRS`, or `MMOR` for the big-endian variant), which
//! [`Tiff::parse`] already accepts. IFD0 *is* the sensor image: it
//! carries ImageWidth/Length, BitsPerSample and StripOffsets like any
//! other TIFF image, but its PhotometricInterpretation is a plain 1 or
//! 2 rather than 32803, so there is nothing in the directory itself
//! that says "this is a raw". The strips hold one of four things:
//!
//! * 12-bit samples packed big-endian, in strips of a few rows — the
//!   C-series and SP-series compacts, the ones whose magic is `IIRS`
//!   and whose BitsPerSample is 12;
//! * 12-bit samples packed *little*-endian in one strip — the early
//!   Four Thirds bodies, which claim BitsPerSample 16 and then store
//!   1.5 bytes a pixel;
//! * 16-bit words, one a pixel;
//! * Olympus's own lossless compression, the adaptive Golomb-like
//!   scheme every body since the E-300 era uses. Nothing distinguishes
//!   it in the directory — Compression still says 1 — so the byte
//!   count decides: fewer bytes than 1.5 a pixel and it must be coded.
//!
//! Everything a developer needs beyond the samples lives in the
//! makernote, which comes in three shapes ("OLYMP\0", "OLYMPUS\0II"
//! and "OM SYSTEM\0\0\0II") described at [`MakerNote`].

use crate::bits::{BitPump, BitPumpLsb, BitPumpMsb};
use crate::formats::common;
use crate::tiff::{tags, Entry, Ifd, ImageLayout, Tiff};
use crate::{Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// Olympus's ImageProcessing sub-IFD (makernote 0x2040) and the
/// CameraSettings one (0x2020), by the names ExifTool's tag
/// documentation gives them.
mod mn {
    /// Makernote: CameraSettings sub-IFD.
    pub const CAMERA_SETTINGS: u16 = 0x2020;
    /// Makernote: ImageProcessing sub-IFD.
    pub const IMAGE_PROCESSING: u16 = 0x2040;
    /// Makernote: RawInfo sub-IFD. The compacts that have no
    /// ImageProcessing put the same numbers, under the same tags, in
    /// this one instead.
    pub const RAW_INFO: u16 = 0x3000;
    /// Makernote: a small JPEG of the frame, on the older bodies.
    pub const THUMBNAIL_IMAGE: u16 = 0x0100;

    /// CameraSettings: offset of the full-size preview JPEG, relative
    /// to the makernote's own start.
    pub const PREVIEW_START: u16 = 0x0101;
    /// CameraSettings: that JPEG's length.
    pub const PREVIEW_LENGTH: u16 = 0x0102;

    /// ImageProcessing: as-shot red and blue levels, x256 with green 256.
    pub const WB_RB_LEVELS: u16 = 0x0100;
    /// ImageProcessing: black level per CFA position.
    pub const BLACK_LEVEL_2: u16 = 0x0600;
    /// ImageProcessing: how many of the 16 bits carry signal.
    pub const VALID_BITS: u16 = 0x0611;
    pub const CROP_LEFT: u16 = 0x0612;
    pub const CROP_TOP: u16 = 0x0613;
    pub const CROP_WIDTH: u16 = 0x0614;
    pub const CROP_HEIGHT: u16 = 0x0615;
    /// ImageProcessing: `[saturation, ...]` — the level above which the
    /// sensor stops being linear, which is what a developer wants as
    /// the white point.
    pub const SENSOR_CALIBRATION: u16 = 0x0805;
    /// ImageProcessing: `[left, top, right, bottom]`, inclusive, of the
    /// frame the chosen aspect ratio keeps.
    pub const ASPECT_FRAME: u16 = 0x1113;

    /// RawInfo: offset of the full-size preview JPEG, from the start of
    /// the file. (In ImageProcessing the same numbers are a tone curve,
    /// so this pair is only ever read out of RawInfo.)
    pub const RAW_PREVIEW_START: u16 = 0x0801;
    /// RawInfo: that JPEG's length.
    pub const RAW_PREVIEW_LENGTH: u16 = 0x0802;

    /// Old (v1) makernote: red balance, x256.
    pub const OLD_RED_BALANCE: u16 = 0x1017;
    /// Old (v1) makernote: blue balance, x256.
    pub const OLD_BLUE_BALANCE: u16 = 0x1018;
    /// Old (v1) makernote: black level per CFA position.
    pub const OLD_BLACK_LEVEL: u16 = 0x1012;
    /// Old (v1) makernote: how many of the 16 bits carry signal.
    pub const OLD_VALID_BITS: u16 = 0x102C;
}

/// EXIF's CFAPattern (0xA302). Olympus is one of the few makers that
/// fills it in honestly on a raw, which saves this module a model
/// table: the compacts are BGGR and the Four Thirds bodies RGGB, and
/// the tag says so.
const CFA_PATTERN: u16 = 0xA302;

/// A parsed Olympus makernote: the entries plus the base its internal
/// offsets are measured from.
///
/// Three generations, told apart by the bytes at its start:
///
/// * `OLYMP\0` + two bytes — the original. The IFD begins eight bytes
///   in and its offsets are relative to the *file*, so `base` is 0.
///   Its "sub-IFDs" (0x2010..0x2050) are UNDEFINED blobs whose bytes
///   are simply an IFD, again with file-relative offsets.
/// * `OLYMPUS\0` + `II`/`MM` + a version word — an embedded TIFF
///   without the usual 42: the IFD begins twelve bytes in and every
///   offset inside it, sub-IFDs included, counts from the makernote's
///   own first byte.
/// * `OM SYSTEM\0\0\0` + `II` + a version word — the same, four bytes
///   longer, on the bodies OM Digital Solutions ships.
struct MakerNote<'a> {
    tiff: Tiff<'a>,
}

impl<'a> MakerNote<'a> {
    fn parse(bytes: &'a [u8], entry: &Entry, file_little_endian: bool) -> Option<MakerNote<'a>> {
        let at = entry.offset;
        let head = bytes.get(at..)?;
        let (ifd_at, base, little_endian) = if head.starts_with(b"OLYMPUS\0") {
            (at.checked_add(12)?, at, byte_order(head.get(8..10)?)?)
        } else if head.starts_with(b"OM SYSTEM\0\0\0") {
            (at.checked_add(16)?, at, byte_order(head.get(12..14)?)?)
        } else if head.starts_with(b"OLYMP\0") {
            // The first generation shares the file's offsets *and* its
            // byte order; the two bytes after the signature are a
            // version, not a byte order of its own.
            (at.checked_add(8)?, 0, file_little_endian)
        } else {
            return None;
        };
        let tiff = Tiff::parse_at_relative(bytes, ifd_at, base, little_endian).ok()?;
        Some(MakerNote { tiff })
    }

    fn root(&self) -> &Ifd {
        self.tiff.root()
    }

    /// One of the makernote's sub-directories, whichever way this
    /// generation stores it: an IFD-typed pointer (relative to the
    /// makernote base) on the newer ones, a blob of IFD bytes on the
    /// first.
    fn sub(&self, tag: u16) -> Option<Tiff<'a>> {
        let entry = self.root().get(tag)?;
        let bytes = self.tiff.bytes();
        let base = self.tiff.base();
        let at = match entry.kind {
            crate::tiff::Kind::Ifd | crate::tiff::Kind::Long | crate::tiff::Kind::Ifd8 => {
                base.checked_add(entry.u32(0)? as usize)?
            }
            crate::tiff::Kind::Undefined | crate::tiff::Kind::Byte => entry.offset,
            _ => return None,
        };
        Tiff::parse_at_relative(bytes, at, base, self.tiff.little_endian()).ok()
    }
}

fn byte_order(sig: &[u8]) -> Option<bool> {
    match sig {
        b"II" => Some(true),
        b"MM" => Some(false),
        _ => None,
    }
}

/// Everything about the uncompressed layouts that is not in
/// [`ImageLayout`]: how a sample is stored, how wide it really is, and
/// whether the rows arrive in two interlaced passes.
#[derive(Debug, Clone, Copy)]
struct Unpacking {
    packing: Packing,
    valid_bits: u32,
    little_endian: bool,
    interlaced: bool,
}

/// How the strips hold their samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Packing {
    /// Twelve bits a sample, most significant bit first.
    Packed12Be,
    /// Twelve bits a sample, least significant bit first, in blocks
    /// of sixteen bytes that carry ten samples and a spare byte.
    Packed12Blocks,
    /// One 16-bit word a sample, in the file's byte order.
    Words16,
    /// Olympus's adaptive lossless coding.
    Compressed,
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let ifd = raw_ifd(&tiff)?;
    let layout = ImageLayout::of(&tiff, ifd)?;
    let width = layout.width;
    // A few compacts record an odd number of rows and then store an
    // even number: rounding up keeps the CFA whole, and the frame then
    // matches what LibRaw reports.
    let height = layout.height + (layout.height & 1);
    if width == 0 || height == 0 || width > 1 << 16 || height > 1 << 16 {
        return Err(Error::Corrupt(format!("ORF frame {width}x{height}")));
    }
    // The strips bound the frame: every layout stores at least a bit a
    // sample, so a header claiming more than the file could hold is a
    // forgery, not a big camera — and must not size an allocation.
    let samples = crate::frame_samples(width, height, 1)?;
    let stored: usize = layout.chunks.iter().map(|(_, len)| *len).sum();
    if stored.saturating_mul(8) < samples {
        return Err(Error::Corrupt(format!(
            "ORF frame of {samples} samples in {stored} bytes of strips"
        )));
    }
    if layout.compression != 1 {
        return Err(Error::Unsupported(format!(
            "ORF with compression {}",
            layout.compression
        )));
    }
    // The heuristic below weighs the strips against the frame the
    // directory declares, not the row-padded one.
    let samples = width * layout.height;
    let total: usize = layout.chunks.iter().map(|(_, len)| len).sum();
    let packing = match layout.bits_per_sample {
        // The compacts say 12 and mean it, one strip every few rows.
        12 => Packing::Packed12Be,
        16 if total >= samples * 2 => Packing::Words16,
        // Between 1.5 and 2 bytes a pixel there is only one thing it
        // can be: 12-bit samples, ten to a sixteen-byte block — 1.6
        // bytes a pixel.
        16 if total >= samples * 3 / 2 => Packing::Packed12Blocks,
        16 => Packing::Compressed,
        other => {
            return Err(Error::Unsupported(format!(
                "ORF with {other} bits a sample"
            )))
        }
    };

    // Everything beyond the samples is in the makernote, and a file
    // without one is still perfectly decodable — it just develops with
    // defaults. It is read first because how wide a sample is decides
    // how the 16-bit layout is aligned.
    let maker = tiff
        .exif()
        .and_then(|exif| exif.get(tags::MAKER_NOTE))
        .and_then(|entry| MakerNote::parse(bytes, entry, tiff.little_endian()));
    let processing = maker
        .as_ref()
        .and_then(|m| m.sub(mn::IMAGE_PROCESSING).or_else(|| m.sub(mn::RAW_INFO)));
    let valid_bits = processing
        .as_ref()
        .and_then(|ip| ip.root().get(mn::VALID_BITS).and_then(|e| e.u32(0)))
        .or_else(|| {
            maker
                .as_ref()
                .and_then(|m| m.root().get(mn::OLD_VALID_BITS).and_then(|e| e.u32(0)))
        })
        .filter(|bits| (8..=16).contains(bits))
        .unwrap_or(12);

    let data = match packing {
        Packing::Compressed => {
            let (start, len) = *layout
                .chunks
                .first()
                .ok_or_else(|| Error::Corrupt("ORF with no strip".into()))?;
            decompress(&bytes[start..start + len], width, height)
        }
        // The compacts that stamp their files `IIRS` read their sensor
        // out as two interlaced fields and store them that way.
        _ => unpack(
            bytes,
            &layout,
            width,
            height,
            Unpacking {
                packing,
                valid_bits,
                little_endian: tiff.little_endian(),
                interlaced: bytes.starts_with(b"IIRS"),
            },
        ),
    };

    let mut raw = RawImage::new(
        Format::Orf,
        width,
        height,
        1,
        RawData::U16(data),
        cfa(&tiff),
    );
    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);

    if let Some(ip) = processing.as_ref().map(|t| t.root()) {
        if let (Some(r), Some(b)) = (
            ip.get(mn::WB_RB_LEVELS).and_then(|e| e.f64(0)),
            ip.get(mn::WB_RB_LEVELS).and_then(|e| e.f64(1)),
        ) {
            if r > 0.0 && b > 0.0 {
                raw.wb_coeffs = [(r / 256.0) as f32, 1.0, (b / 256.0) as f32, 1.0];
            }
        }
        if let Some(black) = ip.get(mn::BLACK_LEVEL_2) {
            read_black(black, &mut raw.black_levels);
        }
        // The saturation level the camera measured beats the width of
        // the field: sensors clip below their full scale, and Olympus
        // records where.
        if let Some(max) = ip.get(mn::SENSOR_CALIBRATION).and_then(|e| e.f64(0)) {
            if max > 0.0 && max < 65536.0 {
                raw.white_level = max as f32;
            }
        }
        raw.crop = crop(ip, width, height).unwrap_or(raw.crop);
    }
    if let Some(root) = maker.as_ref().map(|m| m.root()) {
        // The first-generation makernote keeps the same numbers loose
        // in its root directory, for the bodies that have no
        // ImageProcessing or RawInfo at all.
        if raw.wb_coeffs == [1.0; 4] {
            if let (Some(r), Some(b)) = (
                root.get(mn::OLD_RED_BALANCE).and_then(|e| e.f64(0)),
                root.get(mn::OLD_BLUE_BALANCE).and_then(|e| e.f64(0)),
            ) {
                if r > 0.0 && b > 0.0 {
                    raw.wb_coeffs = [(r / 256.0) as f32, 1.0, (b / 256.0) as f32, 1.0];
                }
            }
        }
        if raw.black_levels == [0.0; 4] {
            if let Some(black) = root.get(mn::OLD_BLACK_LEVEL) {
                read_black(black, &mut raw.black_levels);
            }
        }
    }
    if raw.white_level == 65535.0 {
        raw.white_level = ((1u32 << valid_bits) - 1) as f32;
    }
    // A black level at or above the white point would make `validate`
    // reject the frame; trust the white point and drop the black.
    if raw.black_levels.iter().any(|b| *b >= raw.white_level) {
        raw.black_levels = [0.0; 4];
    }

    raw.preview = preview_from(&tiff, maker.as_ref());
    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let tiff = Tiff::parse(bytes)?;
    let maker = tiff
        .exif()
        .and_then(|exif| exif.get(tags::MAKER_NOTE))
        .and_then(|entry| MakerNote::parse(bytes, entry, tiff.little_endian()));
    Ok(preview_from(&tiff, maker.as_ref()))
}

/// The biggest JPEG the file offers. On the Four Thirds and Micro Four
/// Thirds bodies the full-size preview is only reachable through the
/// makernote's CameraSettings, which points at it with an offset
/// relative to the makernote's own start; the TIFF directories know
/// nothing about it.
fn preview_from(tiff: &Tiff<'_>, maker: Option<&MakerNote<'_>>) -> Option<Vec<u8>> {
    let bytes = tiff.bytes();
    let mut best = common::largest_jpeg(tiff);
    let mut consider = |start: usize, len: usize| {
        let stream = bytes.get(start..start.checked_add(len)?)?;
        if !stream.starts_with(&[0xFF, 0xD8]) {
            return None;
        }
        if best.as_ref().is_none_or(|found| stream.len() > found.len()) {
            best = Some(stream.to_vec());
        }
        Some(())
    };
    if let Some(maker) = maker {
        let base = maker.tiff.base();
        if let Some(settings) = maker.sub(mn::CAMERA_SETTINGS) {
            let root = settings.root();
            if let (Some(start), Some(len)) = (
                root.get(mn::PREVIEW_START).and_then(|e| e.u32(0)),
                root.get(mn::PREVIEW_LENGTH).and_then(|e| e.u32(0)),
            ) {
                if let Some(start) = base.checked_add(start as usize) {
                    consider(start, len as usize);
                }
            }
        }
        // The compacts point at theirs from RawInfo instead, and by an
        // offset from the start of the file rather than the makernote.
        if let Some(info) = maker.sub(mn::RAW_INFO) {
            let root = info.root();
            if let (Some(start), Some(len)) = (
                root.get(mn::RAW_PREVIEW_START).and_then(|e| e.u32(0)),
                root.get(mn::RAW_PREVIEW_LENGTH).and_then(|e| e.u32(0)),
            ) {
                consider(start as usize, len as usize);
            }
        }
        // The oldest bodies keep their only JPEG right in the
        // makernote's root, as a blob rather than a pointer.
        if let Some(thumb) = maker.root().get(mn::THUMBNAIL_IMAGE) {
            consider(thumb.offset, thumb.count);
        }
    }
    best
}

/// Which IFD holds the sensor. It is IFD0 on every ORF seen, but the
/// file also carries a thumbnail IFD with strips of its own, so pick by
/// depth rather than by position: the raw is the deepest-strip IFD that
/// is not JPEG-compressed.
fn raw_ifd<'t>(tiff: &'t Tiff<'_>) -> Result<&'t Ifd> {
    let mut best: Option<(&Ifd, u64)> = None;
    for ifd in tiff.all() {
        if !ifd.has(tags::STRIP_OFFSETS) {
            continue;
        }
        let bits = ifd
            .get(tags::BITS_PER_SAMPLE)
            .and_then(|e| e.u32(0))
            .unwrap_or(8);
        let compression = ifd
            .get(tags::COMPRESSION)
            .and_then(|e| e.u32(0))
            .unwrap_or(1);
        if bits < 12 || compression != 1 {
            continue;
        }
        let size = ifd
            .get(tags::STRIP_BYTE_COUNTS)
            .map(|e| e.u64s().iter().sum::<u64>())
            .unwrap_or(0);
        if best.is_none_or(|(_, best)| size > best) {
            best = Some((ifd, size));
        }
    }
    best.map(|(ifd, _)| ifd)
        .ok_or_else(|| Error::Corrupt("ORF without a sensor IFD".into()))
}

/// The CFA, from EXIF's CFAPattern: two 16-bit repeat counts in the
/// file's byte order, then one byte a cell (0 red, 1 green, 2 blue).
/// RGGB when the tag is missing — every Olympus interchangeable-lens
/// body is RGGB, and only the compacts differ.
fn cfa(tiff: &Tiff<'_>) -> Cfa {
    let color = |c: u8| match c {
        0 => Some(CfaColor::Red),
        1 => Some(CfaColor::Green),
        2 => Some(CfaColor::Blue),
        3 => Some(CfaColor::Cyan),
        4 => Some(CfaColor::Magenta),
        5 => Some(CfaColor::Yellow),
        _ => None,
    };
    let parsed = (|| {
        let entry = tiff.exif()?.get(CFA_PATTERN)?;
        let raw = entry.bytes()?;
        let n = |at: usize| -> Option<usize> {
            let pair: [u8; 2] = raw.get(at..at + 2)?.try_into().ok()?;
            Some(if tiff.little_endian() {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            } as usize)
        };
        let (w, h) = (n(0)?, n(2)?);
        if w != 2 || h != 2 {
            return None;
        }
        let cells: Option<Vec<CfaColor>> = raw.get(4..8)?.iter().map(|c| color(*c)).collect();
        let cells = cells?;
        Some(Cfa::Bayer([cells[0], cells[1], cells[2], cells[3]]))
    })();
    parsed.unwrap_or(Cfa::RGGB)
}

/// Black level per CFA position. Olympus writes the four values in
/// raster order (top-left, top-right, bottom-left, bottom-right),
/// which is exactly [`RawImage::black_levels`]'s convention.
fn read_black(entry: &Entry, out: &mut [f32; 4]) {
    for (i, slot) in out.iter_mut().enumerate() {
        if let Some(v) = entry.f64(i.min(entry.count.saturating_sub(1))) {
            *slot = v as f32;
        }
    }
}

/// The area meant to be shown, from ImageProcessing's crop tags (or
/// AspectFrame when a body records only that).
///
/// An odd origin is nudged to the next even pixel. A crop that started
/// on an odd column would put a green under what the CFA calls red;
/// the bodies that do it (the early Four Thirds ones) are the same
/// bodies whose crop LibRaw reports one pixel further in, so this
/// matches the oracle as well as the sensor.
fn crop(ip: &Ifd, width: usize, height: usize) -> Option<Rect> {
    let num = |tag: u16| ip.get(tag).and_then(|e| e.u32(0)).map(|v| v as usize);
    let (mut x, mut y, mut w, mut h) = match (
        num(mn::CROP_LEFT),
        num(mn::CROP_TOP),
        num(mn::CROP_WIDTH),
        num(mn::CROP_HEIGHT),
    ) {
        (Some(x), Some(y), Some(w), Some(h)) => (x, y, w, h),
        _ => {
            let frame = ip.get(mn::ASPECT_FRAME)?;
            let (l, t, r, b) = (
                frame.u32(0)? as usize,
                frame.u32(1)? as usize,
                frame.u32(2)? as usize,
                frame.u32(3)? as usize,
            );
            (l, t, r.checked_sub(l)? + 1, b.checked_sub(t)? + 1)
        }
    };
    if x & 1 == 1 {
        x += 1;
        w = w.saturating_sub(1);
    }
    if y & 1 == 1 {
        y += 1;
        h = h.saturating_sub(1);
    }
    if w == 0 || h == 0 || x >= width || y >= height {
        return None;
    }
    Some(Rect {
        x,
        y,
        width: w.min(width - x),
        height: h.min(height - y),
    })
}

/// The three uncompressed layouts, strip by strip.
///
/// The strips are walked in order and their rows counted as they go,
/// rather than assumed to be RowsPerStrip apart: the interlaced
/// compacts end one strip early at the boundary between their two
/// fields, so a strip's position in the stream is the sum of what came
/// before it and nothing else.
///
/// `interlaced` maps that stream back onto the frame. Those sensors
/// are read out in two passes, all the even rows and then all the odd
/// ones, and the file keeps them in that order; the even field is the
/// longer one when the frame has an odd number of rows.
///
/// A strip that runs short simply leaves the rest of its rows at zero:
/// a truncated file gives a short picture rather than an error, and
/// never a panic.
fn unpack(
    bytes: &[u8],
    layout: &ImageLayout,
    width: usize,
    height: usize,
    how: Unpacking,
) -> Vec<u16> {
    let Unpacking {
        packing,
        valid_bits,
        little_endian,
        interlaced,
    } = how;
    let mut out = vec![0u16; width * height];
    let row_bytes = match packing {
        Packing::Words16 => width * 2,
        Packing::Packed12Be => (width * 12).div_ceil(8),
        // Ten samples to sixteen bytes; every frame seen has a whole
        // number of blocks in a row.
        Packing::Packed12Blocks => width.div_ceil(10) * 16,
        Packing::Compressed => width * 2,
    }
    .max(1);
    let half = height.div_ceil(2);
    let frame_row = |stream_row: usize| -> Option<usize> {
        let row = if !interlaced {
            stream_row
        } else if stream_row < half {
            stream_row * 2
        } else {
            (stream_row - half) * 2 + 1
        };
        (row < height).then_some(row)
    };

    let mut stream_row = 0usize;
    for (start, len) in &layout.chunks {
        let rows = layout.rows_per_chunk.max(1).min(len / row_bytes);
        let src = &bytes[*start..*start + *len];
        match packing {
            Packing::Packed12Be => {
                let mut pump = BitPumpMsb::new(src);
                for row in 0..rows {
                    read_row(&mut out, width, frame_row(stream_row + row), |i| {
                        let _ = i;
                        pump.get(12) as u16
                    });
                }
            }
            Packing::Packed12Blocks => {
                let payload = block_payload(src);
                let mut pump = BitPumpLsb::new(&payload);
                for row in 0..rows {
                    read_row(&mut out, width, frame_row(stream_row + row), |i| {
                        let _ = i;
                        pump.get(12) as u16
                    });
                }
            }
            Packing::Words16 => {
                let word_at = |at: usize| -> u16 {
                    let Some(word) = src.get(at..at + 2) else {
                        return 0;
                    };
                    let word: [u8; 2] = word.try_into().unwrap();
                    if little_endian {
                        u16::from_le_bytes(word)
                    } else {
                        u16::from_be_bytes(word)
                    }
                };
                // The bodies that spend a whole word on a sample leave
                // the sample's bits at the *top* of it: the E-1's
                // twelve read sixteen times too high, the E-10's ten
                // sixty-four times. The makernote's ValidBits is what
                // says how far down to push them — and it has to be
                // trusted rather than sniffed from the data, because
                // these sensors do set the odd bit below their own
                // depth, which LibRaw discards along with the padding.
                let shift = 16 - valid_bits.min(16);
                for row in 0..rows {
                    let base = row * row_bytes;
                    read_row(&mut out, width, frame_row(stream_row + row), |i| {
                        word_at(base + i * 2) >> shift
                    });
                }
            }
            Packing::Compressed => unreachable!("compressed strips take the other path"),
        }
        stream_row += rows;
    }
    out
}

/// The bits of a block-padded stream, with the sixteenth byte of every
/// block dropped.
///
/// The early Four Thirds bodies write their 12-bit samples ten at a
/// time: fifteen bytes hold exactly ten samples, and a sixteenth byte
/// (always zero on every file seen) rounds the block up to a power of
/// two. That is 1.6 bytes a pixel, which is how a frame this shape is
/// told apart from a compressed one.
fn block_payload(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 16 * 15 + 15);
    for block in src.chunks(16) {
        out.extend_from_slice(&block[..block.len().min(15)]);
    }
    out
}

/// Fill one frame row from `next`, which must be called `width` times
/// whether or not the row lands inside the frame — the bit pumps'
/// position is the stream's position, and skipping a row would lose
/// the ones after it.
fn read_row(out: &mut [u16], width: usize, row: Option<usize>, mut next: impl FnMut(usize) -> u16) {
    match row {
        Some(row) => {
            let start = row * width;
            for i in 0..width {
                out[start + i] = next(i);
            }
        }
        None => {
            for i in 0..width {
                next(i);
            }
        }
    }
}

/// Olympus's lossless compression.
///
/// Each sample is stored as the difference from a predictor, and the
/// difference is split three ways: its bottom two bits go out raw, its
/// top bits as a count of leading zeros, and the middle `nbits` bits
/// straight. `nbits` is not in the file — both ends derive it from the
/// magnitude of the last difference in the same column parity, so the
/// code adapts to the noise without spending a bit on saying so.
///
/// Three integers of state ride along per column parity: the last
/// magnitude (which sets `nbits`), a running bias that is added to
/// every difference, and a count of how many small differences have
/// gone by in a row, which buys two extra bits of headroom while the
/// image is quiet.
///
/// The predictor is the classic gradient one, over neighbours two
/// pixels away so it always compares like colour with like: with the
/// pixel to the left (`w`), the one above (`n`) and the one above-left
/// (`nw`), a monotone run takes a planar prediction and anything else
/// takes whichever neighbour the gradient favours.
fn decompress(strip: &[u8], width: usize, height: usize) -> Vec<u16> {
    let mut out = vec![0u16; width * height];
    // Seven bytes of preamble sit in front of the coded data.
    let mut pump = BitPumpMsb::new(strip.get(7..).unwrap_or(&[]));
    for row in 0..height {
        // Two rows back is where the same colour last appeared, and
        // splitting the buffer there gives the predictor two plain
        // slices instead of arithmetic on one.
        let (earlier, rest) = out.split_at_mut(row * width);
        let here = &mut rest[..width];
        let above = (row >= 2).then(|| &earlier[(row - 2) * width..][..width]);
        // The state starts afresh on every row: rows are independently
        // recoverable even though the bitstream is not.
        let mut carry = [[0i32; 3]; 2];
        for col in 0..width {
            // The predictor, before any bits are read: it depends only
            // on samples already decoded.
            let pred = match (above, col >= 2) {
                (None, false) => 0,
                (None, true) => here[col - 2] as i32,
                (Some(above), false) => above[col] as i32,
                (Some(above), true) => {
                    let w = here[col - 2] as i32;
                    let n = above[col] as i32;
                    let nw = above[col - 2] as i32;
                    if (w < nw && nw < n) || (n < nw && nw < w) {
                        // A steep monotone run: extrapolate the plane
                        // through the three, unless the step is small
                        // enough that the average is the safer guess.
                        if (w - nw).abs() > 32 || (n - nw).abs() > 32 {
                            w + n - nw
                        } else {
                            (w + n) >> 1
                        }
                    } else if (w - nw).abs() > (n - nw).abs() {
                        w
                    } else {
                        n
                    }
                }
            };

            let carry = &mut carry[col & 1];
            // Two extra bits of field width while the run of small
            // differences lasts, and `nbits` wide enough to hold the
            // last magnitude on top of that.
            let extra = if carry[2] < 3 { 2 } else { 0 };
            let magnitude = 32 - (carry[0] as u32 & 0xFFFF).leading_zeros();
            let nbits = (2 + extra).max(magnitude.saturating_sub(extra));

            // Fifteen bits cover a whole difference's preamble, so one
            // look at the stream is enough: a sign bit, the two low
            // bits of the difference, then a leading-zero run
            // terminated by a one, up to twelve. Twelve zeros is the
            // escape for a difference too wide for the run to be worth
            // coding.
            let window = pump.peek(15);
            let head = (window >> 12) as i32;
            let low = head & 3;
            // All-ones when negative, so `^` below flips the magnitude
            // into its two's complement.
            let sign = -(head >> 2);
            let run = window & 0xFFF;
            let high = if run == 0 {
                pump.consume(15);
                (pump.get(16 - nbits) >> 1) as i32
            } else {
                let zeros = run.leading_zeros() - 20;
                pump.consume(3 + zeros + 1);
                zeros as i32
            };

            carry[0] = (high << nbits) | pump.get(nbits) as i32;
            let diff = (carry[0] ^ sign).wrapping_add(carry[1]);
            // The bias tracks about a sixteenth of the recent
            // differences, which is what keeps `nbits` honest when the
            // signal has a slope.
            carry[1] = (diff.wrapping_mul(3).wrapping_add(carry[1])) >> 5;
            carry[2] = if carry[0] > 16 { 0 } else { carry[2] + 1 };

            let value = pred + (diff << 2) + low;
            here[col] = value.clamp(0, u16::MAX as i32) as u16;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ORF: an `IIRO` header, IFD0 with the tags the decoder
    /// needs, and `strip` as the sensor data.
    fn build(
        bits: u16,
        width: u32,
        height: u32,
        strip: &[u8],
        extra: &[(u16, u16, u32)],
    ) -> Vec<u8> {
        let mut entries: Vec<(u16, u16, u32)> = vec![
            (tags::IMAGE_WIDTH, 4, width),
            (tags::IMAGE_LENGTH, 4, height),
            (tags::BITS_PER_SAMPLE, 3, bits as u32),
            (tags::COMPRESSION, 3, 1),
            (tags::PHOTOMETRIC, 3, 1),
            (tags::STRIP_OFFSETS, 4, 0),
            (tags::ROWS_PER_STRIP, 4, height),
            (tags::STRIP_BYTE_COUNTS, 4, strip.len() as u32),
        ];
        entries.extend_from_slice(extra);
        entries.sort_by_key(|e| e.0);
        let ifd_at = 8usize;
        let strip_at = ifd_at + 2 + entries.len() * 12 + 4;
        let mut out = Vec::new();
        out.extend_from_slice(b"IIRO");
        out.extend_from_slice(&(ifd_at as u32).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, kind, value) in &entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
            let value = if *tag == tags::STRIP_OFFSETS {
                strip_at as u32
            } else {
                *value
            };
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(out.len(), strip_at);
        out.extend_from_slice(strip);
        out
    }

    #[test]
    fn twelve_bit_big_endian_strips() {
        // 0x123, 0x456, 0x789, 0xabc packed MSB-first.
        let strip = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
        let file = build(12, 4, 1, &strip, &[]);
        let raw = decode(&file).expect("decodes");
        // The odd row count is rounded up, so the frame is 4x2 with a
        // blank second row.
        assert_eq!((raw.width, raw.height), (4, 2));
        let RawData::U16(data) = &raw.data else {
            panic!("u16")
        };
        assert_eq!(&data[..4], &[0x123, 0x456, 0x789, 0xabc]);
        assert_eq!(&data[4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn twelve_bit_blocks_when_the_count_says_so() {
        // BitsPerSample 16 but 1.6 bytes a pixel: the early Four
        // Thirds layout, low bit first, ten samples to a block whose
        // last byte is skipped. Ten samples, so the second block's
        // first sample must come from byte 16, not byte 15.
        let mut strip = vec![0u8; 32];
        strip[..3].copy_from_slice(&[0x12, 0x34, 0x56]);
        strip[15] = 0xff; // the spare byte, ignored
        strip[16..19].copy_from_slice(&[0x9a, 0xbc, 0xde]);
        let file = build(16, 10, 2, &strip, &[]);
        let raw = decode(&file).expect("decodes");
        let RawData::U16(data) = &raw.data else {
            panic!("u16")
        };
        assert_eq!(&data[..2], &[0x412, 0x563]);
        assert_eq!(&data[10..12], &[0xc9a, 0xdeb]);
    }

    /// A whole word a sample, with the sample at the top of it: with
    /// no makernote to say otherwise the samples are twelve bits, so
    /// each word comes down by four.
    #[test]
    fn sixteen_bit_words_are_top_aligned() {
        let strip = [0x34, 0x12, 0x78, 0x56, 0xbc, 0x0a, 0xff, 0x0f];
        let file = build(16, 4, 1, &strip, &[]);
        let raw = decode(&file).expect("decodes");
        let RawData::U16(data) = &raw.data else {
            panic!("u16")
        };
        assert_eq!(&data[..4], &[0x123, 0x567, 0x0ab, 0x0ff]);
    }

    /// The compressed coder, hand-encoded: with all state at zero the
    /// first sample is `((high << 4) | extra) * 4 + low`, and the
    /// second uses the same fresh state because it is the other column
    /// parity.
    #[test]
    fn compressed_first_samples() {
        // Seven bytes of preamble, then the bits
        //   0 00 000000111110 1111  -> sign +, low 0, high 6, extra 15
        //   0 11 000000000001 0101  -> sign +, low 3, high 11, extra 5
        let mut strip = vec![0u8; 7];
        strip.extend_from_slice(&[0x00, 0x7d, 0x80, 0x0a, 0xea, 0xfd, 0x65, 0x17]);
        let out = decompress(&strip, 4, 1);
        assert_eq!(out[0], (6 * 16 + 15) * 4);
        assert_eq!(out[1], (11 * 16 + 5) * 4 + 3);
    }

    #[test]
    fn hostile_input_never_panics() {
        assert!(decode(&[]).is_err());
        assert!(decode(b"IIRO").is_err());
        let file = build(12, 4, 2, &[0x12, 0x34, 0x56], &[]);
        for cut in 0..file.len() {
            let _ = decode(&file[..cut]);
            let _ = preview(&file[..cut]);
        }
    }

    #[test]
    fn a_crop_on_an_odd_column_moves_in_not_out() {
        let mut ip = Ifd::default();
        for (tag, value) in [
            (mn::CROP_LEFT, 55u32),
            (mn::CROP_TOP, 49),
            (mn::CROP_WIDTH, 3136),
            (mn::CROP_HEIGHT, 2352),
        ] {
            ip.entries.push(Entry {
                tag,
                kind: crate::tiff::Kind::Long,
                count: 1,
                offset: 0,
                value: crate::tiff::Value::Long(vec![value]),
            });
        }
        assert_eq!(
            crop(&ip, 3280, 2450),
            Some(Rect {
                x: 56,
                y: 50,
                width: 3135,
                height: 2351
            })
        );
    }
}

/// The LibRaw oracle, for this module's corpus tests and the RW2
/// module's — the two vendors share one set of reference tools, so the
/// parsing of their output lives in one place rather than twice.
///
/// `SCHIST_RAW_CORPUS` names a directory walked recursively. Beside
/// every sample sit `<name>.tiff` (`unprocessed_raw -T`: the whole
/// uncropped frame as 16-bit grey, black not subtracted) and
/// `<name>.identify.txt` (`raw-identify -v -w`). Both are optional;
/// what is there is checked.
#[cfg(test)]
pub(super) mod oracle {
    use crate::{Cfa, CfaColor, Orientation, RawData, RawImage};
    use std::path::{Path, PathBuf};

    /// Every file under `SCHIST_RAW_CORPUS` with one of `extensions`
    /// (compared case-insensitively). Empty when the variable is unset,
    /// which is how the corpus tests skip themselves.
    pub fn corpus_files(extensions: &[&str]) -> Vec<PathBuf> {
        let Ok(root) = std::env::var("SCHIST_RAW_CORPUS") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let matches = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| extensions.iter().any(|want| e.eq_ignore_ascii_case(want)));
                if matches {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// What `raw-identify -v -w` says about a file.
    #[derive(Debug, Default)]
    pub struct Identify {
        /// "Full size", LibRaw's `raw_width x raw_height`.
        pub full: Option<(usize, usize)>,
        /// "Raw inset": the vendor crop, origin and size.
        pub inset: Option<(usize, usize, usize, usize)>,
        /// "Image flip".
        pub flip: Option<u32>,
        /// The first four letters of "Filter pattern".
        pub filters: Option<String>,
        /// "black:", the level common to every colour.
        pub black: f32,
        /// "cblack[0 .. 3]", the per-colour extra, R G B G2.
        pub cblack: Option<[f32; 4]>,
        /// "Highlight linearity limits", the first value.
        pub linear_max: Option<f32>,
        /// The "As shot" row of the white-balance table, R G B G2.
        pub as_shot: Option<[f32; 4]>,
    }

    pub fn identify(path: &Path) -> Option<Identify> {
        let text = std::fs::read_to_string(with_suffix(path, "identify.txt")).ok()?;
        let mut out = Identify::default();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Full size:") {
                out.full = pair(rest);
            } else if let Some(rest) = line.strip_prefix("Raw inset, width x height:") {
                let numbers = numbers(rest);
                if let [w, h, x, y] = numbers[..] {
                    out.inset = Some((x as usize, y as usize, w as usize, h as usize));
                }
            } else if let Some(rest) = line.strip_prefix("Image flip:") {
                out.flip = rest.trim().parse().ok();
            } else if let Some(rest) = line.strip_prefix("Filter pattern:") {
                out.filters = Some(rest.trim().chars().take(4).collect());
            } else if let Some(rest) = line.strip_prefix("black:") {
                out.black = rest.trim().parse().unwrap_or(0.0);
            } else if let Some(rest) = line.strip_prefix("cblack[0 .. 3]:") {
                let n = numbers(rest);
                if let [a, b, c, d] = n[..] {
                    out.cblack = Some([a, b, c, d]);
                }
            } else if let Some(rest) = line.strip_prefix("Highlight linearity limits:") {
                out.linear_max = numbers(rest).first().copied();
            } else if let Some(rest) = line.strip_prefix("As shot") {
                let n = numbers(rest);
                if n.len() >= 3 && n[1] > 0.0 {
                    out.as_shot = Some([n[0] / n[1], 1.0, n[2] / n[1], 1.0]);
                }
            }
        }
        Some(out)
    }

    fn numbers(text: &str) -> Vec<f32> {
        text.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    fn pair(text: &str) -> Option<(usize, usize)> {
        let n = numbers(text);
        (n.len() >= 2).then(|| (n[0] as usize, n[1] as usize))
    }

    fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".");
        name.push(suffix);
        PathBuf::from(name)
    }

    /// Compare `raw.data` with `unprocessed_raw`'s frame, sample for
    /// sample.
    ///
    /// One family of differences is accepted, because it is LibRaw
    /// throwing information away rather than this crate inventing it:
    /// on a handful of bodies LibRaw's `width` is a few pixels short of
    /// the frame the file actually stores, and the columns past it come
    /// out zero. Those columns hold real sensor readings, so they are
    /// kept here; the test insists the oracle's side is zero and that
    /// they are within 64 pixels of the right edge.
    pub fn compare_samples(path: &Path, raw: &RawImage) {
        let oracle = with_suffix(path, "tiff");
        if !oracle.exists() {
            eprintln!("{}: no oracle frame, samples unchecked", path.display());
            return;
        }
        let image = image::open(&oracle)
            .unwrap_or_else(|e| panic!("{}: {e}", oracle.display()))
            .into_luma16();
        assert_eq!(
            (image.width() as usize, image.height() as usize),
            (raw.width, raw.height),
            "{}: frame size",
            path.display()
        );
        let RawData::U16(mine) = &raw.data else {
            panic!("{}: expected 16-bit samples", path.display())
        };
        let theirs = image.as_raw();
        let mut mismatches = Vec::new();
        for (i, (a, b)) in mine.iter().zip(theirs.iter()).enumerate() {
            if a != b {
                mismatches.push((i / raw.width, i % raw.width, *a, *b));
            }
        }
        if mismatches.is_empty() {
            return;
        }
        let edge = raw.width.saturating_sub(64);
        if mismatches
            .iter()
            .all(|(_, col, _, theirs)| *theirs == 0 && *col >= edge)
        {
            let first = mismatches
                .iter()
                .map(|(_, col, _, _)| *col)
                .min()
                .unwrap_or(0);
            eprintln!(
                "{}: {} samples in columns {}..{} kept where LibRaw writes zero",
                path.display(),
                mismatches.len(),
                first,
                raw.width
            );
            return;
        }
        // The other accepted difference runs the other way: where a
        // file declares an odd number of rows, the frame is rounded up
        // to keep the CFA whole and the last row has no data behind it.
        // This crate leaves it at zero; LibRaw leaves whatever its own
        // buffer held.
        if mismatches
            .iter()
            .all(|(row, _, mine, _)| *row + 1 == raw.height && *mine == 0)
        {
            eprintln!(
                "{}: the padding row past the file's own row count is left blank",
                path.display()
            );
            return;
        }
        let shown: Vec<_> = mismatches.iter().take(8).collect();
        panic!(
            "{}: {} of {} samples differ from the oracle, first {:?} (row, col, mine, theirs)",
            path.display(),
            mismatches.len(),
            mine.len(),
            shown
        );
    }

    /// Levels, white balance, crop, orientation and CFA against
    /// `raw-identify`. Deviations this crate makes deliberately are
    /// named in each check.
    pub fn compare_metadata(path: &Path, raw: &RawImage) {
        let Some(identify) = identify(path) else {
            eprintln!("{}: no identify output, metadata unchecked", path.display());
            return;
        };
        let name = path.display();
        if let Some((w, h)) = identify.full {
            assert_eq!((raw.width, raw.height), (w, h), "{name}: full size");
        }
        if let Some(filters) = &identify.filters {
            let mine: String = (0..4)
                .map(|i| match raw.cfa.color_at(i % 2, i / 2) {
                    Some(CfaColor::Red) => 'R',
                    Some(CfaColor::Blue) => 'B',
                    Some(CfaColor::Green) | Some(CfaColor::Green2) => 'G',
                    Some(CfaColor::Cyan) => 'C',
                    Some(CfaColor::Magenta) => 'M',
                    Some(CfaColor::Yellow) => 'Y',
                    _ => '?',
                })
                .collect();
            assert_eq!(&mine, filters, "{name}: filter pattern");
        }
        if let Some(flip) = identify.flip {
            let want = match flip {
                3 => Orientation::Rotate180,
                5 => Orientation::Rotate270CW,
                6 => Orientation::Rotate90CW,
                _ => Orientation::Normal,
            };
            assert_eq!(raw.orientation, want, "{name}: orientation");
        }
        if let Some(shot) = identify.as_shot {
            for (i, want) in shot.iter().enumerate().take(3) {
                let got = raw.wb_coeffs[i];
                assert!(
                    (got - want).abs() <= 0.01 * want.max(1.0),
                    "{name}: white balance {:?} want {shot:?}",
                    raw.wb_coeffs
                );
            }
        }
        // LibRaw reports black as a base plus a per-colour extra,
        // indexed by colour (R, G, B, G2); this crate keeps one value a
        // CFA position, so the two are compared through the pattern.
        let cblack = identify.cblack.unwrap_or([0.0; 4]);
        for position in 0..4 {
            let want = identify.black + cblack[libraw_color(&raw.cfa, position)];
            assert!(
                (raw.black_levels[position] - want).abs() < 0.5,
                "{name}: black {:?} want {want} at position {position}",
                raw.black_levels
            );
        }
        if let Some(max) = identify.linear_max {
            assert!(
                (raw.white_level - max).abs() <= 1.0 + 0.01 * max,
                "{name}: white level {} want about {max}",
                raw.white_level
            );
        }
        if let Some((x, y, w, h)) = identify.inset {
            assert_eq!(
                (raw.crop.x, raw.crop.y, raw.crop.width, raw.crop.height),
                (x, y, w, h),
                "{name}: crop"
            );
        }
    }

    /// LibRaw's colour index (0 red, 1 green, 2 blue, 3 the second
    /// green) for a CFA position 0..4 in raster order.
    fn libraw_color(cfa: &Cfa, position: usize) -> usize {
        let color = cfa.color_at(position % 2, position / 2);
        let first_green = (0..4).find(|i| {
            matches!(
                cfa.color_at(i % 2, i / 2),
                Some(CfaColor::Green) | Some(CfaColor::Green2)
            )
        });
        match color {
            Some(CfaColor::Red) => 0,
            Some(CfaColor::Blue) => 2,
            Some(CfaColor::Green) | Some(CfaColor::Green2) => {
                if first_green == Some(position) {
                    1
                } else {
                    3
                }
            }
            _ => 0,
        }
    }

    /// The preview has to be a JPEG a viewer can actually show.
    pub fn check_preview(path: &Path, raw: &RawImage) {
        let Some(preview) = &raw.preview else {
            eprintln!("{}: no preview", path.display());
            return;
        };
        image::load_from_memory(preview)
            .unwrap_or_else(|e| panic!("{}: preview does not decode: {e}", path.display()));
    }

    /// Run `body` on `count` truncation lengths spread over a file,
    /// including the awkward ends.
    pub fn truncations(len: usize, count: usize, mut body: impl FnMut(usize)) {
        for i in 0..count {
            // A cheap deterministic spread: prime steps through the
            // file, so the cuts land in headers, in the middle of the
            // directory and inside the sensor data alike.
            let cut = (len / (count + 1)) * (i + 1) + (i * 7919) % 1024;
            body(cut.min(len));
        }
        for cut in [0, 1, 4, 8, 16, len.saturating_sub(1)] {
            body(cut.min(len));
        }
    }
}

/// Corpus tests: every ORF under `SCHIST_RAW_CORPUS`, checked against
/// the LibRaw oracle files beside it.
#[cfg(test)]
mod corpus {
    use super::oracle;
    use super::*;

    fn files() -> Vec<std::path::PathBuf> {
        oracle::corpus_files(&["orf"])
    }

    #[test]
    fn every_file_matches_the_oracle() {
        for path in &files() {
            let bytes = std::fs::read(path).expect("corpus file readable");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Orf),
                "{} did not probe as ORF",
                path.display()
            );
            let raw = crate::decode(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            raw.validate().expect("valid");
            oracle::compare_samples(path, &raw);
            oracle::compare_metadata(path, &raw);
            oracle::check_preview(path, &raw);
        }
    }

    #[test]
    fn truncation_never_panics() {
        for path in &files() {
            let bytes = std::fs::read(path).expect("corpus file readable");
            oracle::truncations(bytes.len(), 10, |cut| {
                let _ = crate::decode(&bytes[..cut]);
                let _ = crate::preview(&bytes[..cut]);
            });
        }
    }
}
