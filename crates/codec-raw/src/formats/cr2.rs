//! Canon CR2: a TIFF whose last IFD holds the sensor frame as a
//! lossless JPEG, cut into vertical slices.
//!
//! The container is an ordinary little-endian TIFF with a four-byte
//! extra signature (`CR`, major, minor) at byte 8 and, at byte 12, the
//! offset of the IFD that holds the sensor data. The four directories
//! are always the same four things: IFD0 the full-size JPEG the camera
//! would have written on its own, IFD1 the 160x120 thumbnail, IFD2 a
//! small uncompressed RGB preview, IFD3 the raw.
//!
//! The raw IFD carries no ImageWidth or ImageLength — the lossless
//! JPEG's own frame header carries the shape — and its `StripOffsets`
//! points at a complete SOF3 stream. That stream is *not* the sensor
//! frame in reading order. Canon splits the frame into vertical
//! slices, each of which is compressed as a full-height column and
//! written one after another, and describes the cut with tag 0xC640:
//! `[n, first, last]` means `n` slices `first` samples wide followed by
//! one `last` wide, so the frame is `n * first + last` samples across.
//! The JPEG's own width times its component count is that same number,
//! which is how a decoder that hands back samples in stream order (as
//! [`crate::ljpeg`] does) can be un-sliced with three integers and no
//! knowledge of how the encoder grouped columns into components.
//!
//! Everything else about the picture — the crop, the white balance, the
//! black level — lives in the Canon makernote, an ordinary IFD in the
//! file's byte order whose offsets are absolute.
//!
//! Clean-room: written from the TIFF 6.0 and ITU T.81 specifications,
//! published third-party CR2 write-ups, ExifTool's tag documentation,
//! and measurement of the sample files named in this module's tests.

use crate::formats::common;
use crate::tiff::{tags, Entry, Ifd, Tiff, Value};
use crate::{ljpeg, Cfa, CfaColor, Error, Format, RawData, RawImage, Rect, Result};

/// Tag 0xC640, `[slice count, first slice width, last slice width]`.
const CR2_SLICE: u16 = 0xC640;
/// Tag 0xC5E0, the Bayer phase of the sensor frame as a small enum.
const CR2_CFA_PATTERN: u16 = 0xC5E0;

/// Canon makernote tags this module reads.
mod canon {
    /// SensorInfo: the full sensor size, the borders of the area the
    /// camera itself would show, and the optically black mask.
    pub const SENSOR_INFO: u16 = 0x00E0;
    /// ColorData: a long, version-stamped block of colour numbers.
    pub const COLOR_DATA: u16 = 0x4001;
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let tiff = Tiff::parse(bytes)?;
    let raw_ifd =
        raw_ifd(&tiff).ok_or_else(|| Error::Corrupt("cr2: no IFD holds sensor data".into()))?;
    let stream = raw_stream(&tiff, raw_ifd)?;

    // sRAW and mRAW keep a subsampled YCbCr picture rather than a CFA,
    // in a three-component frame; every Bayer CR2 ever written has two
    // or four. Say so before the lossless decoder refuses the sampling
    // factors, which would be a much less helpful message.
    let shape = ljpeg::header(stream)?;
    if shape.components == 3 {
        return Err(Error::Unsupported(
            "cr2: sRAW/mRAW (subsampled YCbCr, not a colour filter array)".into(),
        ));
    }
    let frame = ljpeg::decode(stream)?;
    let row = frame
        .width
        .checked_mul(frame.components)
        .ok_or_else(|| Error::Corrupt("cr2: lossless JPEG frame too wide".into()))?;
    let (width, data) = deslice(&frame.data, row, frame.height, slices(raw_ifd, row))?;
    let height = frame.height;

    let mut raw = RawImage::new(
        Format::Cr2,
        width,
        height,
        1,
        RawData::U16(data),
        cfa(&tiff, raw_ifd),
    );
    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    raw.orientation = common::orientation(&tiff);
    raw.metadata = common::metadata(&tiff);
    raw.preview = common::largest_jpeg(&tiff);

    // The lossless JPEG's precision is the sensor's: 12-bit on the
    // compacts and the pre-2007 bodies, 14 since. Canon's real
    // saturation is a little under that on most bodies and only the
    // makernote or a camera table knows it, so a full-scale white here
    // is the safe end of the error: highlights stay neutral, they just
    // do not reach 1.0.
    raw.white_level = ((1u32 << shape.precision.clamp(1, 16)) - 1) as f32;

    if let Some(makernote) = makernote(&tiff) {
        let root = makernote.root();
        let little_endian = makernote.little_endian();
        let sensor = sensor_info(root, little_endian);
        if let Some(sensor) = &sensor {
            if let Some(crop) = sensor.crop(width, height) {
                raw.crop = crop;
            }
        }
        if let Some(color) = color_data(root, little_endian) {
            if let Some(wb) = color.as_shot_wb() {
                raw.wb_coeffs = wb;
            }
            if let Some(black) = color.black_levels() {
                raw.black_levels = spread_black(black, &raw.cfa);
            }
        }
        // Canon does not always record a black level, and even where it
        // does the masked border is the same number measured on this
        // frame at this temperature. Fall back to it.
        if raw.black_levels == [0.0; 4] {
            if let Some(black) = masked_black(&raw, sensor.as_ref()) {
                raw.black_levels = black;
            }
        }
    }
    if !raw.black_levels.iter().all(|b| *b < raw.white_level) {
        // A black level at or above saturation is a table or a parse
        // gone wrong; an unbalanced picture beats a refusal.
        raw.black_levels = [0.0; 4];
    }

    raw.apply_camera_table();
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(common::largest_jpeg(&Tiff::parse(bytes)?))
}

// ------------------------------------------------------------ container

/// The IFD holding the sensor data.
///
/// Byte 12 of a CR2 points straight at it, which is the only way to be
/// sure on a file whose IFD order is unusual; the search that follows
/// is for the odd file (and for the truncated ones a fuzzer builds)
/// whose header pointer does not land on a directory this parser read.
fn raw_ifd<'a>(tiff: &'a Tiff<'_>) -> Option<&'a Ifd> {
    let ifds = tiff.all();
    if let Some(offset) = tiff.u32_at(12) {
        let offset = offset as usize + tiff.base();
        if let Some(ifd) = ifds
            .iter()
            .find(|i| i.offset == offset && i.has(tags::STRIP_OFFSETS))
        {
            return Some(ifd);
        }
    }
    // The slice tag only ever appears on the raw IFD.
    if let Some(ifd) = ifds.iter().find(|i| i.has(CR2_SLICE)) {
        return Some(ifd);
    }
    // Last resort: the IFD whose single strip is a lossless JPEG.
    ifds.iter()
        .find(|i| raw_stream(tiff, i).map(is_lossless_jpeg).unwrap_or(false))
        .copied()
}

/// Whether a stream is SOF3 — a raw frame rather than a picture.
fn is_lossless_jpeg(stream: &[u8]) -> bool {
    ljpeg::header(stream).is_ok()
}

/// The raw IFD's one strip. [`crate::tiff::ImageLayout`] cannot be used
/// here: the raw IFD has no ImageWidth or ImageLength for it to read.
fn raw_stream<'a>(tiff: &Tiff<'a>, ifd: &Ifd) -> Result<&'a [u8]> {
    let offsets = ifd
        .get(tags::STRIP_OFFSETS)
        .map(|e| e.u64s())
        .unwrap_or_default();
    let counts = ifd
        .get(tags::STRIP_BYTE_COUNTS)
        .map(|e| e.u64s())
        .unwrap_or_default();
    let (&offset, &count) = match (offsets.first(), counts.first()) {
        (Some(offset), Some(count)) => (offset, count),
        _ => return Err(Error::Corrupt("cr2: raw IFD without a strip".into())),
    };
    let bytes = tiff.bytes();
    let start = usize::try_from(offset)
        .ok()
        .and_then(|o| o.checked_add(tiff.base()))
        .ok_or_else(|| Error::Corrupt("cr2: strip offset out of range".into()))?;
    let end = usize::try_from(count)
        .ok()
        .and_then(|c| start.checked_add(c))
        .ok_or_else(|| Error::Corrupt("cr2: strip length out of range".into()))?;
    bytes
        .get(start..end)
        .ok_or_else(|| Error::Corrupt(format!("cr2: strip {start}..{end} lies outside the file")))
}

/// Tag 0xC640, or the single full-width slice a file without it means.
fn slices(ifd: &Ifd, row: usize) -> [usize; 3] {
    match ifd.get(CR2_SLICE) {
        // A file that has the tag but has written it short is a file
        // whose slicing cannot be trusted; fall through to one slice.
        Some(entry) if entry.count >= 3 => {
            let at = |i| entry.u32(i).unwrap_or(0) as usize;
            [at(0), at(1), at(2)]
        }
        // The 20D and the 1D Mark II generation compress the whole
        // frame as one slice and leave the tag out.
        _ => [0, 0, row],
    }
}

/// Put Canon's vertical slices back into one raster.
///
/// `samples` is the lossless JPEG's output in stream order: `row`
/// samples a line, `height` lines. Each slice was compressed as a
/// full-height column, so the stream holds slice 0's `height` lines of
/// `first` samples, then slice 1's, and so on, with a final slice
/// `last` wide. Returns the frame width and the reassembled raster.
fn deslice(
    samples: &[u16],
    row: usize,
    height: usize,
    slices: [usize; 3],
) -> Result<(usize, Vec<u16>)> {
    let [count, first, last] = slices;
    let width = count
        .checked_mul(first)
        .and_then(|w| w.checked_add(last))
        .ok_or_else(|| Error::Corrupt("cr2: slice widths overflow".into()))?;
    if width != row {
        return Err(Error::Corrupt(format!(
            "cr2: slices {count}x{first}+{last} make {width} samples a row, the frame has {row}"
        )));
    }
    let total = width
        .checked_mul(height)
        .ok_or_else(|| Error::Corrupt("cr2: frame too large".into()))?;
    if samples.len() != total {
        return Err(Error::Corrupt(format!(
            "cr2: {} samples for a {width}x{height} frame",
            samples.len()
        )));
    }
    // One slice is the whole frame already in reading order.
    if count == 0 || first == 0 {
        return Ok((width, samples.to_vec()));
    }
    let mut out = vec![0u16; total];
    let mut read = 0;
    let mut x = 0;
    for slice in 0..=count {
        let slice_width = if slice < count { first } else { last };
        if slice_width == 0 {
            continue;
        }
        for y in 0..height {
            let from = samples
                .get(read..read + slice_width)
                .ok_or_else(|| Error::Corrupt("cr2: slice runs past the frame".into()))?;
            let at = y * width + x;
            out.get_mut(at..at + slice_width)
                .ok_or_else(|| Error::Corrupt("cr2: slice runs past the frame".into()))?
                .copy_from_slice(from);
            read += slice_width;
        }
        x += slice_width;
    }
    Ok((width, out))
}

/// The Bayer phase at the sensor frame's origin.
///
/// Tag 0xC5E0 names it as a small enum (ExifTool documents the four
/// values as CR2CFAPattern). Canon has never shipped anything but a
/// Bayer CR2, so an absent or unknown tag falls back to RGGB, the
/// commonest of the four, rather than failing the decode.
fn cfa(tiff: &Tiff<'_>, raw_ifd: &Ifd) -> Cfa {
    let value = raw_ifd
        .get(CR2_CFA_PATTERN)
        .or_else(|| tiff.find(CR2_CFA_PATTERN))
        .and_then(|e| e.u32(0));
    match value {
        Some(2) => Cfa::BGGR,
        Some(3) => Cfa::GBRG,
        Some(4) => Cfa::GRBG,
        _ => Cfa::RGGB,
    }
}

// ------------------------------------------------------------ makernote

/// The Canon makernote as a directory of its own.
///
/// It is a plain IFD in the file's byte order whose value offsets are
/// measured from the start of the file like any other, so it needs no
/// base and no header — only its position, which is where the
/// MakerNote tag's value sits.
fn makernote<'a>(tiff: &Tiff<'a>) -> Option<Tiff<'a>> {
    let entry = tiff.find(tags::MAKER_NOTE)?;
    Tiff::parse_at(tiff.bytes(), entry.offset, tiff.little_endian()).ok()
}

/// Makernote 0x00E0: where the picture sits on the sensor.
///
/// A SHORT array whose first element is the record's own length in
/// bytes; the rest are documented by ExifTool as SensorWidth,
/// SensorHeight, two reserved words, the four borders of the area the
/// camera would show, and the four borders of the optically black
/// mask (all zero on the bodies that have no mask inside the frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SensorInfo {
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
    mask: Option<Rect>,
}

/// A makernote record's values as 16-bit words.
///
/// Canon writes SensorInfo and ColorData as SHORT arrays on the EOS
/// bodies and as an UNDEFINED byte blob on the PowerShot compacts —
/// the same little-endian words either way, but a reader that trusted
/// the field type would take every compact's colour block for a list
/// of bytes and index it at half the stride.
fn shorts(entry: &Entry, little_endian: bool) -> Vec<u16> {
    match &entry.value {
        Value::Short(values) => values.clone(),
        Value::Byte(bytes) | Value::Undefined(bytes) => {
            let (pairs, _) = bytes.as_chunks::<2>();
            pairs
                .iter()
                .map(|pair| {
                    if little_endian {
                        u16::from_le_bytes(*pair)
                    } else {
                        u16::from_be_bytes(*pair)
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn sensor_info(mn: &Ifd, little_endian: bool) -> Option<SensorInfo> {
    let values = shorts(mn.get(canon::SENSOR_INFO)?, little_endian);
    let at = |i: usize| values.get(i).map(|v| *v as usize);
    let (left, top, right, bottom) = (at(5)?, at(6)?, at(7)?, at(8)?);
    if right <= left || bottom <= top {
        return None;
    }
    // The mask borders are inclusive like the picture's, and all four
    // are zero when the frame holds no mask.
    let mask = match (at(9), at(10), at(11), at(12)) {
        (Some(l), Some(t), Some(r), Some(b)) if r > l && b > t => Some(Rect {
            x: l,
            y: t,
            width: r - l + 1,
            height: b - t + 1,
        }),
        _ => None,
    };
    Some(SensorInfo {
        left,
        top,
        right,
        bottom,
        mask,
    })
}

impl SensorInfo {
    /// The borders as a rectangle, when they lie inside the frame.
    ///
    /// Canon's borders are inclusive, and the top border is odd on
    /// several bodies (the 1Ds Mark III's is 25) — the crop then starts
    /// on the other Bayer phase from the frame, which is exactly what
    /// [`crate::Cfa`] anchored at the frame origin is for. LibRaw
    /// rounds such a top up to keep its own single pattern usable and
    /// loses a row; this keeps Canon's number.
    fn crop(&self, width: usize, height: usize) -> Option<Rect> {
        let crop = Rect {
            x: self.left,
            y: self.top,
            width: self.right.checked_sub(self.left)? + 1,
            height: self.bottom.checked_sub(self.top)? + 1,
        };
        (crop.x + crop.width <= width && crop.y + crop.height <= height).then_some(crop)
    }
}

/// Makernote 0x4001: colour numbers, laid out by version.
///
/// The block is a SHORT array a few thousand entries long whose shape
/// changed with nearly every camera generation. Its first element is a
/// version stamp (negative on the PowerShot compacts), and ExifTool's
/// tag documentation splits the versions into a dozen tables. Only two
/// fields are wanted here, and only one of them at an offset that
/// varies: the as-shot white balance sits at element 25 in the oldest
/// layout (the 20D and 350D generation, 582 entries), at 24 in the next
/// (653), at 71 on the PowerShot compacts, and at 63 in every layout
/// since — which covers every EOS body from the 1D Mark II N on.
struct ColorData {
    values: Vec<u16>,
    version: i32,
}

fn color_data(mn: &Ifd, little_endian: bool) -> Option<ColorData> {
    let values = shorts(mn.get(canon::COLOR_DATA)?, little_endian);
    if values.len() < 32 {
        return None;
    }
    // The version stamp is signed: the compacts use -3 and -4.
    let version = *values.first()? as i16 as i32;
    Some(ColorData { values, version })
}

impl ColorData {
    fn count(&self) -> usize {
        self.values.len()
    }

    fn short(&self, i: usize) -> Option<u16> {
        self.values.get(i).copied()
    }

    /// Where `WB_RGGBLevelsAsShot` starts, or `None` for a block too
    /// short to hold it there.
    fn as_shot_offset(&self) -> Option<usize> {
        let offset = match self.count() {
            // ColorData1: EOS 20D, 350D. Verified on both.
            582 => 25,
            // ColorData2: the 1D Mark II and 1Ds Mark II. ExifTool's
            // documented offset; no sample of this generation was to
            // hand, so this one line is unverified.
            653 => 24,
            // ColorData5, the PowerShot compacts, which stamp a
            // negative version. Verified on the G11, S110 and G1 X
            // Mark III.
            _ if self.version < 0 => 71,
            // Every EOS layout from the 1D Mark II N on. Verified on
            // the 1D Mark II N, 40D, 1Ds Mark III, 450D, 50D, 550D,
            // M2 and Rebel T6 — versions 1, 3, 4, 5, 6, 7, 10 and 14.
            _ => 63,
        };
        (offset + 4 <= self.count()).then_some(offset)
    }

    /// R, G, B, G2 multipliers normalised so green is 1.
    ///
    /// Canon stores four levels in RGGB order — the two greens
    /// separately — as integers around 1024, which is the same
    /// convention as `RawImage::wb_coeffs` before normalisation.
    fn as_shot_wb(&self) -> Option<[f32; 4]> {
        let at = self.as_shot_offset()?;
        let levels: Vec<u16> = (0..4).map_while(|i| self.short(at + i)).collect();
        let [r, g1, g2, b] = <[u16; 4]>::try_from(levels).ok()?;
        if g1 == 0 || r == 0 || b == 0 {
            return None;
        }
        let g = g1 as f32;
        Some([r as f32 / g, 1.0, b as f32 / g, g2 as f32 / g])
    }

    /// `PerChannelBlackLevel`, in the same RGGB order.
    ///
    /// Its offset moves with the version rather than with the block
    /// length, and in the layouts here it is always followed by
    /// NormalWhiteLevel and SpecularWhiteLevel. Only versions measured
    /// against a real file are listed; anything else falls back to the
    /// masked border, which is what LibRaw measures anyway and is
    /// never more than two counts away from what the camera wrote on
    /// the files where both exist.
    ///
    /// The PowerShot layouts are deliberately absent: their blocks
    /// hold the black level twice at two different offsets and every
    /// compact in the corpus writes the same constant at both, so
    /// there is no way to tell from a sample which one ExifTool means.
    fn black_levels(&self) -> Option<[u16; 4]> {
        // ColorData1 (582 entries) has no black level at all, and its
        // first word is not a version — the 20D's reads 1164 — so a
        // file of that generation must never reach the table below.
        if self.count() < 653 {
            return None;
        }
        let at = match self.version {
            1 => 196,
            4 | 5 => 692,
            6 | 7 => 715,
            10 => 504,
            14 => 556,
            _ => return None,
        };
        if at + 4 > self.count() {
            return None;
        }
        let levels: Vec<u16> = (0..4).map_while(|i| self.short(at + i)).collect();
        let levels = <[u16; 4]>::try_from(levels).ok()?;
        // A plausible black is a small fraction of full scale and the
        // four channels agree closely; anything else means the offset
        // is wrong for this file and the masked border should decide.
        let (low, high) = (
            *levels.iter().min().unwrap_or(&0),
            *levels.iter().max().unwrap_or(&0),
        );
        (high < 8192 && high - low <= 64).then_some(levels)
    }
}

/// Canon's four RGGB levels put on the four positions of a Bayer tile.
///
/// The file names them by colour, the field wants them by position, and
/// which of the two greens is "first" depends on where the frame's
/// origin falls — so the greens are matched in raster order, which is
/// the only reading that agrees with itself whatever the phase. The two
/// greens differ by a count or two at most, so nothing rides on it.
fn spread_black(levels: [u16; 4], cfa: &Cfa) -> [f32; 4] {
    let mut out = [levels[1] as f32; 4];
    let mut greens = 0;
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = match cfa.color_at(i % 2, i / 2) {
            Some(CfaColor::Red) => levels[0] as f32,
            Some(CfaColor::Blue) => levels[3] as f32,
            _ => {
                greens += 1;
                levels[if greens > 1 { 2 } else { 1 }] as f32
            }
        };
    }
    out
}

/// The black level measured on the frame's own masked pixels.
///
/// Canon leaves a strip of the sensor under metal, and says where in
/// SensorInfo: either as an explicit mask rectangle, or — on the bodies
/// that leave those four words zero — as the space to the left of the
/// picture's own border. Both are read here as a median per Bayer
/// position, because the first row of a Canon frame is regularly junk
/// (on the 50D it averages nine times the black level) and a median
/// ignores it where a mean does not.
fn masked_black(raw: &RawImage, sensor: Option<&SensorInfo>) -> Option<[f32; 4]> {
    let sensor = sensor?;
    let RawData::U16(data) = &raw.data else {
        return None;
    };
    let area = match sensor.mask {
        Some(mask) => mask,
        // Two columns of guard on the picture side: the transition out
        // of the mask is not sharp.
        None if sensor.left >= 8 => Rect {
            x: 0,
            y: 0,
            width: sensor.left - 4,
            height: raw.height,
        },
        None => return None,
    };
    if area.x + area.width > raw.width || area.y + area.height > raw.height {
        return None;
    }
    let mut out = [0.0f32; 4];
    for (phase, slot) in out.iter_mut().enumerate() {
        let (px, py) = (phase % 2, phase / 2);
        let mut samples: Vec<u16> = Vec::new();
        let mut y = area.y + (py + 2 - area.y % 2) % 2;
        while y < area.y + area.height {
            let mut x = area.x + (px + 2 - area.x % 2) % 2;
            while x < area.x + area.width {
                samples.push(data[y * raw.width + x]);
                x += 2;
            }
            y += 2;
        }
        if samples.is_empty() {
            return None;
        }
        let mid = samples.len() / 2;
        samples.sort_unstable();
        *slot = samples[mid] as f32;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CfaColor::{Blue, Green, Red};
    use crate::Orientation;
    use std::path::PathBuf;

    // ------------------------------------------------------- mechanics

    /// A frame whose samples say where they belong, so a wrong
    /// reassembly is visible rather than merely unequal.
    fn ramp(width: usize, height: usize) -> Vec<u16> {
        (0..width * height).map(|i| i as u16).collect()
    }

    /// Slice a frame the way Canon's encoder does, for `deslice` to
    /// put back.
    fn slice_up(frame: &[u16], width: usize, height: usize, widths: &[usize]) -> Vec<u16> {
        let mut out = Vec::with_capacity(frame.len());
        let mut x = 0;
        for slice in widths {
            for y in 0..height {
                out.extend_from_slice(&frame[y * width + x..y * width + x + slice]);
            }
            x += slice;
        }
        out
    }

    #[test]
    fn deslice_rebuilds_the_frame_from_its_columns() {
        // The 50D's shape in miniature: two slices of one width and a
        // narrower last one.
        let (width, height) = (10, 4);
        let frame = ramp(width, height);
        let stream = slice_up(&frame, width, height, &[4, 4, 2]);
        assert_eq!(
            deslice(&stream, width, height, [2, 4, 2]).unwrap(),
            (width, frame)
        );
    }

    #[test]
    fn deslice_passes_a_single_slice_through() {
        // The G1 X Mark III writes [0, 0, width]; the 20D leaves the
        // tag out entirely and this module supplies the same triple.
        let (width, height) = (6, 3);
        let frame = ramp(width, height);
        assert_eq!(
            deslice(&frame, width, height, [0, 0, width]).unwrap(),
            (width, frame.clone())
        );
        assert_eq!(
            deslice(&frame, width, height, [1, 6, 0]).unwrap(),
            (width, frame)
        );
    }

    #[test]
    fn deslice_rejects_a_cut_that_does_not_add_up() {
        let frame = ramp(10, 4);
        // Slices that make a different width than the JPEG's raster.
        assert!(matches!(
            deslice(&frame, 10, 4, [2, 4, 4]),
            Err(Error::Corrupt(_))
        ));
        // A raster the sample count does not match.
        assert!(matches!(
            deslice(&frame, 10, 5, [0, 0, 10]),
            Err(Error::Corrupt(_))
        ));
        // Widths that would overflow rather than wrap.
        assert!(matches!(
            deslice(&frame, 10, 4, [usize::MAX, usize::MAX, 0]),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn slices_falls_back_to_one_full_width_slice() {
        // An IFD with no tag at all, as the 20D and 1D Mark II write.
        let empty = Ifd::default();
        assert_eq!(slices(&empty, 3596), [0, 0, 3596]);
    }

    #[test]
    fn spread_black_follows_the_bayer_phase() {
        // Canon names its four levels by colour in RGGB order; the
        // field wants them by position, so a GBRG frame moves them.
        let levels = [100, 200, 201, 300];
        assert_eq!(
            spread_black(levels, &Cfa::RGGB),
            [100.0, 200.0, 201.0, 300.0]
        );
        assert_eq!(
            spread_black(levels, &Cfa::GBRG),
            [200.0, 300.0, 100.0, 201.0]
        );
        assert_eq!(
            spread_black(levels, &Cfa::BGGR),
            [300.0, 200.0, 201.0, 100.0]
        );
    }

    #[test]
    fn cfa_reads_canons_pattern_tag() {
        assert_eq!(cfa_of(Some(1)), Cfa::RGGB);
        assert_eq!(cfa_of(Some(3)), Cfa::GBRG);
        // An unknown or absent value is RGGB, the commonest phase.
        assert_eq!(cfa_of(Some(99)), Cfa::RGGB);
        assert_eq!(cfa_of(None), Cfa::RGGB);
        assert_eq!(Cfa::GBRG, Cfa::Bayer([Green, Blue, Red, Green]));
    }

    /// `cfa` without a whole TIFF around it.
    fn cfa_of(value: Option<u32>) -> Cfa {
        match value {
            Some(2) => Cfa::BGGR,
            Some(3) => Cfa::GBRG,
            Some(4) => Cfa::GRBG,
            _ => Cfa::RGGB,
        }
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        for bytes in [
            &b""[..],
            &b"II*\0"[..],
            &b"II*\0\x08\0\0\0CR\x02\0\xff\xff\xff\xff"[..],
            &[0xff; 64][..],
        ] {
            assert!(decode(bytes).is_err());
            // A file whose directory reads but holds nothing has no
            // preview rather than a broken one.
            assert!(matches!(preview(bytes), Err(_) | Ok(None)));
        }
    }

    // ---------------------------------------------------------- corpus

    /// Files LibRaw's oracle covers but this decoder knowingly does
    /// not, with the reason. Both are Canon's subsampled raws: the
    /// frame is a YCbCr picture at half or quarter resolution, not a
    /// colour filter array, and `crate::ljpeg` refuses the sampling
    /// factors.
    const UNSUPPORTED: &[&str] = &["EOS_50D-IMG_9517.CR2", "EOS_50D-IMG_9518.CR2"];

    fn corpus() -> Option<PathBuf> {
        std::env::var_os("SCHIST_RAW_CORPUS").map(PathBuf::from)
    }

    /// Every CR2 under `dir`, recursively.
    fn cr2_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                cr2_files(&path, out);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("cr2"))
            {
                out.push(path);
            }
        }
    }

    /// The handful of lines of `raw-identify -v -w` this module checks
    /// itself against.
    #[derive(Debug, Default)]
    struct Identify {
        full: Option<(usize, usize)>,
        inset: Option<(usize, usize, usize, usize)>,
        flip: Option<u32>,
        pattern: Option<String>,
        as_shot: Option<[f64; 4]>,
    }

    fn identify(path: &std::path::Path) -> Option<Identify> {
        let text = std::fs::read_to_string(path.with_file_name(format!(
            "{}.identify.txt",
            path.file_name()?.to_string_lossy()
        )))
        .ok()?;
        let mut out = Identify::default();
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let word = |i: usize| words.get(i).map(|w| w.trim_end_matches(':'));
            let size = |i: usize| word(i).and_then(|w| w.parse::<usize>().ok());
            let level = |i: usize| word(i).and_then(|w| w.parse::<f64>().ok());
            match words.as_slice() {
                ["Full", "size:", ..] => out.full = Some((size(2)?, size(4)?)),
                // "Raw inset, width x height: W x H left: L top: T"
                ["Raw", "inset,", ..] => {
                    out.inset = Some((size(5)?, size(7)?, size(9)?, size(11)?))
                }
                ["Image", "flip:", ..] => out.flip = word(2).and_then(|w| w.parse().ok()),
                ["Filter", "pattern:", p] => out.pattern = Some(p.to_string()),
                ["As", "shot", ..] => {
                    out.as_shot = Some([level(2)?, level(3)?, level(4)?, level(5)?])
                }
                _ => {}
            }
        }
        Some(out)
    }

    /// LibRaw's `unprocessed_raw -T` output for a sample: the whole
    /// sensor frame, 16-bit grey, black not subtracted.
    fn oracle(path: &std::path::Path) -> Option<(usize, usize, Vec<u16>)> {
        let tiff = path.with_file_name(format!("{}.tiff", path.file_name()?.to_string_lossy()));
        let image = image::open(tiff).ok()?.into_luma16();
        let (width, height) = (image.width() as usize, image.height() as usize);
        Some((width, height, image.into_raw()))
    }

    #[test]
    fn corpus_decodes_and_matches_the_oracle() {
        let Some(dir) = corpus() else { return };
        let mut files = Vec::new();
        cr2_files(&dir, &mut files);
        files.sort();
        assert!(!files.is_empty(), "no CR2 under {}", dir.display());
        let mut checked = 0;
        for path in &files {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let bytes = std::fs::read(path).expect("read sample");
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Cr2),
                "{name} probes as CR2"
            );
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) if UNSUPPORTED.contains(&name.as_str()) => {
                    assert!(why.contains("sRAW"), "{name}: {why}");
                    // The picture is still there even when the sensor
                    // data cannot be read.
                    let jpeg = super::preview(&bytes).unwrap().expect("preview");
                    image::load_from_memory(&jpeg)
                        .unwrap_or_else(|e| panic!("{name} preview: {e}"));
                    continue;
                }
                Err(why) => panic!("{name}: {why}"),
            };
            raw.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(raw.format, Format::Cr2);
            assert_eq!(raw.make, "Canon", "{name}");
            assert!(!raw.model.is_empty(), "{name} has a model");

            if let Some((width, height, want)) = oracle(path) {
                assert_eq!(
                    (raw.width, raw.height),
                    (width, height),
                    "{name} frame size"
                );
                let RawData::U16(got) = &raw.data else {
                    panic!("{name} is not 16-bit")
                };
                let wrong: Vec<usize> = got
                    .iter()
                    .zip(&want)
                    .enumerate()
                    .filter(|(_, (a, b))| a != b)
                    .map(|(i, _)| i)
                    .take(8)
                    .collect();
                assert!(
                    wrong.is_empty(),
                    "{name}: samples differ from the oracle at {:?} (got {:?}, want {:?})",
                    wrong,
                    wrong.iter().map(|i| got[*i]).collect::<Vec<_>>(),
                    wrong.iter().map(|i| want[*i]).collect::<Vec<_>>(),
                );
                checked += 1;
            }

            if let Some(identify) = identify(path) {
                if let Some(full) = identify.full {
                    assert_eq!((raw.width, raw.height), full, "{name} full size");
                }
                if let Some((width, height, left, top)) = identify.inset {
                    // LibRaw rounds an odd top border up so that its
                    // single Bayer phase still describes the crop, and
                    // loses the last row doing it; this decoder keeps
                    // Canon's own borders, so a one-pixel disagreement
                    // in y is expected on those bodies.
                    assert_eq!((raw.crop.x, raw.crop.width), (left, width), "{name} crop x");
                    assert!(
                        raw.crop.y.abs_diff(top) <= 1 && raw.crop.height.abs_diff(height) <= 1,
                        "{name} crop y: {:?} against LibRaw's {top}+{height}",
                        raw.crop
                    );
                }
                if let Some(flip) = identify.flip {
                    let want = match flip {
                        3 => Orientation::Rotate180,
                        5 => Orientation::Rotate270CW,
                        6 => Orientation::Rotate90CW,
                        _ => Orientation::Normal,
                    };
                    assert_eq!(raw.orientation, want, "{name} orientation");
                }
                if let Some(pattern) = &identify.pattern {
                    let want: Vec<CfaColor> = pattern[..4]
                        .chars()
                        .map(|c| match c {
                            'R' => Red,
                            'B' => Blue,
                            _ => Green,
                        })
                        .collect();
                    let got: Vec<CfaColor> = (0..4)
                        .map(|i| raw.cfa.color_at(i % 2, i / 2).unwrap())
                        .map(|c| if c == CfaColor::Green2 { Green } else { c })
                        .collect();
                    assert_eq!(got, want, "{name} filter pattern");
                }
                if let Some(levels) = identify.as_shot {
                    let want = [
                        levels[0] / levels[1],
                        1.0,
                        levels[2] / levels[1],
                        levels[3] / levels[1],
                    ];
                    for (got, want) in raw.wb_coeffs.iter().zip(&want) {
                        assert!(
                            (*got as f64 - want).abs() < 1e-3,
                            "{name} white balance {:?} against LibRaw's {want:?}",
                            raw.wb_coeffs
                        );
                    }
                }
            }

            // Levels: the black must be a small, nearly uniform lift
            // and the white the bit depth's full scale.
            assert!(
                raw.white_level == 4095.0 || raw.white_level == 16383.0,
                "{name} white {}",
                raw.white_level
            );
            let black = raw.black_levels;
            let (low, high) = (
                black.iter().cloned().fold(f32::MAX, f32::min),
                black.iter().cloned().fold(0.0, f32::max),
            );
            assert!(
                high < raw.white_level / 4.0 && high - low <= 4.0,
                "{name} black {black:?}"
            );
            assert!(high > 0.0, "{name} found no black level");

            let preview = raw
                .preview
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has no preview"));
            image::load_from_memory(preview).unwrap_or_else(|e| panic!("{name} preview: {e}"));
            assert_eq!(
                super::preview(&bytes).unwrap().as_deref(),
                Some(&preview[..]),
                "{name}: the cheap preview path differs"
            );
        }
        assert!(checked > 0, "no oracle TIFF beside any sample");
    }

    #[test]
    fn truncated_corpus_files_never_panic() {
        let Some(dir) = corpus() else { return };
        let mut files = Vec::new();
        cr2_files(&dir, &mut files);
        files.sort();
        for path in &files {
            let bytes = std::fs::read(path).expect("read sample");
            // Cuts across the header, the directories, the makernote
            // and well into the compressed data.
            for cut in [0, 1, 15, 16, 64, 4096, 44338, 100_000, 1 << 20] {
                let cut = cut.min(bytes.len());
                let _ = decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
            for n in 1..=6 {
                let cut = bytes.len() * n / 7;
                let _ = decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
        }
    }
}
