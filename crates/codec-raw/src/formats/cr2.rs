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
    /// CanonFirmwareVersion: an ASCII string, `Firmware Version 2.1.2`.
    pub const FIRMWARE: u16 = 0x0007;
    /// CanonModelID: the body as a LONG, `0x8000_0000` plus a small
    /// number for the EOS bodies.
    pub const MODEL_ID: u16 = 0x0010;
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

    // sRAW and mRAW keep a subsampled YCbCr picture rather than a CFA.
    // The luma component's sampling factors are the only mark of it,
    // and `ljpeg::header` refuses such a frame outright, so the branch
    // has to be taken before the frame header is read.
    if ljpeg::sampling(stream)? != 0x11 {
        return sraw(&tiff, raw_ifd, stream);
    }
    let shape = ljpeg::header(stream)?;
    // A three-component frame at 1:1 is not a Bayer CR2 either: Canon
    // groups a sensor row into two or four components, and three only
    // ever means Y, Cb, Cr. Without sampling factors there is no
    // telling how its blocks are shaped, so it is refused rather than
    // un-sliced into a single-channel frame of nonsense.
    if shape.components == 3 {
        return Err(Error::Unsupported(
            "cr2: a three-component lossless JPEG without sampling factors (an sRAW/mRAW shape \
             no Canon body writes)"
                .into(),
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
    common_fields(&mut raw, &tiff);

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

/// The camera, orientation, metadata and preview: the same for a Bayer
/// frame and a subsampled one.
fn common_fields(raw: &mut RawImage, tiff: &Tiff<'_>) {
    let (make, model) = tiff.make_model();
    raw.set_camera(&make, &model);
    raw.orientation = common::orientation(tiff);
    raw.metadata = common::metadata(tiff);
    raw.preview = common::largest_jpeg(tiff);
}

// ----------------------------------------------------------- sRAW/mRAW

/// The chroma level a neutral pixel decodes to, `1 << 14`. Subtracting
/// it centres the two chroma planes on zero.
const SRAW_NEUTRAL: i32 = 1 << 14;

/// The CanonModelIDs the sRAW colour rules turn on. Canon numbers the
/// EOS bodies `0x8000_0000` plus a small number that grows with the
/// generation, which is why two of the rules are inequalities.
mod body {
    /// The lowest id that takes the matrix, and the one body whose
    /// hue depends on its firmware.
    pub const EOS_5D_MARK_II: u32 = 0x8000_0218;
    pub const EOS_7D: u32 = 0x8000_0250;
    pub const EOS_50D: u32 = 0x8000_0261;
    /// From this id up the hue is `P << 1`. Note the 1D X (`0x269`)
    /// is *below* it despite being the newer camera.
    pub const EOS_1D_MARK_IV: u32 = 0x8000_0281;
    pub const EOS_60D: u32 = 0x8000_0287;
}

/// The last 5D Mark II firmware to use the older hue offset, as
/// [`parse_firmware`] numbers it (1.0.6).
const EOS_5D_MARK_II_OLD_HUE_FIRMWARE: u32 = 1_000_006;

/// How a body turns its Y, Cb, Cr into RGB. Two formulas exist and the
/// body's model id alone decides which — the ColorData version has no
/// say in it, and firmware matters for exactly one body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SrawColour {
    /// The 5D Mark II, 7D, 50D, 1D Mark IV and 60D: chroma shifted up
    /// two bits with a *hue* offset added, then a fixed-point matrix
    /// over 2^14.
    Matrix { hue: i32 },
    /// Every other body, older and newer alike: chroma added straight,
    /// with a 512 pedestal taken off the luma first on the bodies older
    /// than the 5D Mark II (the 1D Mark III, 40D and 1Ds Mark III).
    Plain { pedestal: i32 },
}

/// The colour rule for a body.
///
/// The hue is `P << 1` from the 1D Mark IV's id up, and on a 5D Mark II
/// whose firmware is past 1.0.6; the 50D, the 7D and an early 5D Mark II
/// use `(P + 1) << 2`. Only the five matrix bodies ever use it, so the
/// inequality really only separates the 1D Mark IV and 60D from the 50D
/// and 7D. A body with no model id at all gets the plain formula,
/// which is what the great majority of bodies use.
fn sraw_colour(model_id: Option<u32>, firmware: Option<u32>, p: usize) -> SrawColour {
    let Some(id) = model_id else {
        return SrawColour::Plain { pedestal: 0 };
    };
    let matrix = matches!(
        id,
        body::EOS_5D_MARK_II | body::EOS_7D | body::EOS_50D | body::EOS_1D_MARK_IV | body::EOS_60D
    );
    if matrix {
        let new_firmware = id == body::EOS_5D_MARK_II
            && firmware.is_some_and(|f| f > EOS_5D_MARK_II_OLD_HUE_FIRMWARE);
        let hue = if id >= body::EOS_1D_MARK_IV || new_firmware {
            (p as i32) << 1
        } else {
            ((p as i32) + 1) << 2
        };
        SrawColour::Matrix { hue }
    } else {
        SrawColour::Plain {
            pedestal: if id < body::EOS_5D_MARK_II { 512 } else { 0 },
        }
    }
}

/// Makernote 0x0010, the body as a number.
fn model_id(mn: &Ifd) -> Option<u32> {
    mn.get(canon::MODEL_ID)?.u32(0)
}

/// Makernote 0x0007, `Firmware Version 2.1.2`, as one comparable number.
fn firmware_version(mn: &Ifd) -> Option<u32> {
    let entry = mn.get(canon::FIRMWARE)?;
    let text = match entry.str() {
        Some(text) => text.to_string(),
        // A body that writes the tag as UNDEFINED rather than ASCII
        // still holds the same string in it.
        None => String::from_utf8_lossy(entry.bytes()?).into_owned(),
    };
    parse_firmware(&text)
}

/// `major * 1_000_000 + minor * 1_000 + patch` of the first
/// dotted number in `text`; parts past the first that are missing count
/// as zero, so `1.0` is 1.0.0.
fn parse_firmware(text: &str) -> Option<u32> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let mut parts = [0u32; 3];
    for (slot, piece) in parts.iter_mut().zip(text[start..].split('.').take(3)) {
        let digits: String = piece.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        *slot = digits.parse().ok()?;
    }
    Some(
        parts[0]
            .saturating_mul(1_000_000)
            .saturating_add(parts[1].saturating_mul(1_000))
            .saturating_add(parts[2]),
    )
}

/// Canon's sRAW and mRAW: a half- or quarter-resolution YCbCr picture
/// in place of a colour filter array.
///
/// The lossless JPEG carries, per minimum coded unit, the luma of a
/// two-pixel-wide block and the one chroma pair they share. The block
/// is two rows tall for mRAW, whose luma is sampled 2x2, and one row
/// for sRAW, sampled 2x1 — Canon's names run the other way from the
/// pixel counts: mRAW is the larger picture and subsamples its chroma
/// the more. This puts those blocks back where they belong, fills the
/// chroma the encoder did not send, and turns the result into the
/// camera's own RGB.
///
/// What comes out is *camera* RGB, not a white-balanced picture: the
/// per-channel gains in ColorData only put the three reconstructed
/// planes onto the sensor's scale (they leave green well above unity),
/// so the as-shot white balance still belongs in `wb_coeffs` where the
/// developer applies it, exactly as for a Bayer frame.
///
/// Only a CR2 ever holds one of these. Canon dropped both modes with
/// the DIGIC 8 generation that introduced CR3, whose bodies offer RAW
/// and C-RAW alone, so there is no CR3 case to route here.
fn sraw(tiff: &Tiff<'_>, raw_ifd: &Ifd, stream: &[u8]) -> Result<RawImage> {
    let makernote = makernote(tiff);
    let (sensor, colour, model_id, firmware) = match &makernote {
        Some(mn) => {
            let (root, le) = (mn.root(), mn.little_endian());
            (
                sensor_info(root, le),
                color_data(root, le),
                model_id(root),
                firmware_version(root),
            )
        }
        None => (None, None, None, None),
    };
    let (_, model) = tiff.make_model();
    // Before the entropy decode: refusing after it would only waste
    // the work.
    let multipliers = sraw_gains(colour.as_ref(), &model)?;
    if model_id.is_none() {
        log::warn!("cr2: {model}: no CanonModelID; reconstructing sRAW with the plain formula");
    }

    // SensorInfo is an independent description of the coded frame.
    // Compare areas rather than dimensions because several bodies wrap
    // or transpose the SOF. Do this before entropy decoding so a forged
    // SOF cannot turn an ordinary scan into a huge allocation.
    if let Some(sensor) = sensor
        .as_ref()
        .filter(|sensor| sensor.width > 1 && sensor.height > 1)
    {
        let jpeg_area = ljpeg::subsampled_frame_area(stream)?;
        let sensor_area = sensor
            .width
            .checked_mul(sensor.height)
            .ok_or_else(|| Error::Corrupt("cr2: SensorInfo frame area overflow".into()))?;
        if jpeg_area != sensor_area {
            return Err(Error::Corrupt(format!(
                "cr2: sRAW SOF holds {jpeg_area} pixels, SensorInfo says {}x{}",
                sensor.width, sensor.height
            )));
        }
    }

    let sub = ljpeg::decode_subsampled(stream)?;
    let slices = slices(raw_ifd, sub.row);
    let geometry = sraw_geometry(&sub, slices, sensor.as_ref())?;
    let SrawGeometry { width, height, .. } = geometry;
    let mut planes = sraw_planes(&sub, &geometry, slices)?;
    sraw_upsample(&mut planes, width, height, sub.p);
    let data = sraw_to_rgb(&planes, sraw_colour(model_id, firmware, sub.p), multipliers);

    let mut raw = RawImage::new(Format::Cr2, width, height, 3, RawData::U16(data), Cfa::None);
    common_fields(&mut raw, tiff);
    if let Some(colour) = &colour {
        if let Some(wb) = colour.as_shot_wb() {
            raw.wb_coeffs = wb;
        }
    }
    if let Some(sensor) = &sensor {
        if let Some(crop) = sensor.crop(width, height) {
            raw.crop = crop;
        }
    }
    raw.apply_camera_table();
    // After the camera table, because a Bayer body's tabulated levels
    // say nothing about a reconstructed frame: the YCbCr maths has
    // already centred the data, so there is no pedestal to subtract.
    // 16383 is the white point; the data does run past it on a bright
    // frame (the 80D reaches 19385), and those are clipped highlights.
    raw.black_levels = [0.0; 4];
    raw.white_level = 16383.0;
    Ok(raw)
}

/// The per-channel gains, or the refusal for a body whose ColorData
/// has no gain slot this module knows.
///
/// The gains are the one number in the reconstruction that cannot be
/// derived from the frame, and a layout whose slot is unknown would
/// reconstruct at a guessed scale — plausible, wrong, and undetectable
/// downstream — so it is the one thing here that is still refused. A
/// known layout whose quad is not shaped like one is a different
/// matter: the file is odd rather than unknown, and it reconstructs at
/// unity, visibly green, with a warning.
fn sraw_gains(colour: Option<&ColorData>, model: &str) -> Result<[i32; 3]> {
    let Some(colour) = colour else {
        return Err(Error::Unsupported(format!(
            "Canon sRAW on {model}: no ColorData makernote to take the channel gains from"
        )));
    };
    if colour.sraw_gain_offset().is_none() {
        return Err(Error::Unsupported(format!(
            "Canon sRAW on {model}: ColorData of {} words (version {}) is a layout with no known \
             sRAW gain slot",
            colour.count(),
            colour.version
        )));
    }
    Ok(colour.sraw_multipliers().unwrap_or_else(|| {
        log::warn!(
            "cr2: {model}: ColorData of {} words holds no usable sRAW gains where its layout \
             keeps them; reconstructing at unity gain",
            colour.count()
        );
        [1024; 3]
    }))
}

/// The shape of a subsampled picture, as the slice tag and the frame
/// header together decide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SrawGeometry {
    /// Columns the stream codes across one picture row: what the
    /// slices add up to. Even.
    coded_width: usize,
    /// Columns kept: the coded width, less the trim two bodies want.
    width: usize,
    /// Rows of the picture.
    height: usize,
}

/// Work out the picture's shape before placing anything.
///
/// The slice tag describes the *picture* row, not the frame header's.
/// On most bodies the two agree, but several write the frame wrapped:
/// the 6D family's mRAW declares 2736x4104 for a 4104x2736 picture, the
/// 5DS's 3888x7200 for 6480x4320, the 5D Mark IV's 2520x6720 for
/// 5040x3360, the 6D Mark II's 3888x3770 for 4680x3132 and the 80D's
/// 4032x3402 for 4536x3024. The stream is one continuous run of MCUs
/// either way, and the header's row length only says where the
/// entropy coder's column-0 predictors reset, so the picture is read
/// from the tag: the slices sum to `(Wc / 2) * C` samples for a coded
/// width `Wc`, and the height is the frame's area over that width —
/// exactly, or the tag and the header do not describe the same stream.
///
/// Two bodies then trim: the 50D's mRAW codes 3344 columns for a
/// 3272-column picture and the 5D Mark II's 3872 for 3866. Nothing in
/// the file states either (the SensorInfo border is 6 short of both, an
/// observation and not a rule), so they are the literal substitutions
/// the reference frames were made with; the columns past the trim are
/// still in the stream and are decoded and dropped.
///
/// SensorInfo's SensorWidth x SensorHeight equals the coded frame on
/// every body measured, wrapped ones included, so it is checked against
/// the tag. A disagreement means the two independent descriptions do
/// not identify one picture and is corrupt rather than something to
/// guess through.
fn sraw_geometry(
    sub: &ljpeg::SubsampledImage,
    slices: [usize; 3],
    sensor: Option<&SensorInfo>,
) -> Result<SrawGeometry> {
    let components = sub.components;
    let row = sraw_slice_row(slices, components)?;
    let coded_width = row / components * 2;
    let area = sub
        .width
        .checked_mul(sub.height)
        .ok_or_else(|| Error::Corrupt("cr2: sRAW frame too large".into()))?;
    if !area.is_multiple_of(coded_width) {
        return Err(Error::Corrupt(format!(
            "cr2: sRAW slices make a {coded_width}-column picture, which does not tile the \
             {}x{} frame",
            sub.width, sub.height
        )));
    }
    let height = area / coded_width;
    let block_rows = sub.block_rows.max(1);
    if height == 0 || !height.is_multiple_of(block_rows) {
        return Err(Error::Corrupt(format!(
            "cr2: sRAW picture of {height} rows, which {block_rows}-row blocks cannot tile"
        )));
    }
    let width = match coded_width {
        3344 => 3272,
        3872 => 3866,
        w => w,
    };
    if let Some(sensor) = sensor {
        if sensor.width > 1
            && sensor.height > 1
            && (sensor.width, sensor.height) != (coded_width, height)
        {
            return Err(Error::Corrupt(format!(
                "cr2: sRAW slices make a {coded_width}x{height} frame, SensorInfo says {}x{}",
                sensor.width, sensor.height
            )));
        }
    }
    Ok(SrawGeometry {
        coded_width,
        width,
        height,
    })
}

/// The samples one picture row holds, from the slice tag: `count`
/// slices `first` samples wide and one `last` wide, each a whole
/// number of `components`-sample MCUs.
fn sraw_slice_row(slices: [usize; 3], components: usize) -> Result<usize> {
    let [count, first, last] = slices;
    let row = count
        .checked_mul(first)
        .and_then(|w| w.checked_add(last))
        .ok_or_else(|| Error::Corrupt("cr2: sRAW slice widths overflow".into()))?;
    if row == 0 {
        return Err(Error::Corrupt("cr2: sRAW slices make an empty row".into()));
    }
    // A slice is whole MCUs, or its columns cannot be counted.
    if !first.is_multiple_of(components) || !last.is_multiple_of(components) {
        return Err(Error::Corrupt(format!(
            "cr2: sRAW slices of {first} and {last} samples are not whole {components}-sample MCUs"
        )));
    }
    Ok(row)
}

/// Place the decoded MCUs, un-slicing as the Bayer path does.
///
/// Returns the three planes interleaved Y, Cb, Cr with the chroma
/// centred on zero and still present only at each block's anchor pixel
/// — the stage the `.sraw-planar` oracle captures, before any
/// interpolation or colour.
///
/// Canon cuts the picture into vertical strips exactly as it does a
/// Bayer one, and the same three-integer tag describes the cut; the
/// only difference is the unit. A slice is `first` samples of a picture
/// row, and an MCU spends `components` samples on two columns, so a
/// slice spans `first * 2 / components` columns. Each strip is written
/// full height before the next begins, and the last is cut short where
/// the picture ends — the samples the encoder wrote past that are
/// padding and are stepped over, which is why the read position
/// advances by the slice's full width whatever the strip's width on
/// screen.
///
/// The stream must hold exactly one picture's worth of MCUs, which is
/// the check that bounds the loop: the tag's slice count is limited by
/// the row it must add up to, and the row count by the samples.
fn sraw_planes(
    sub: &ljpeg::SubsampledImage,
    geometry: &SrawGeometry,
    slices: [usize; 3],
) -> Result<Vec<i32>> {
    let SrawGeometry {
        coded_width,
        width,
        height,
    } = *geometry;
    let components = sub.components;
    let block_rows = sub.block_rows.max(1);
    let [count, first, last] = slices;
    let row = sraw_slice_row(slices, components)?;
    if row != coded_width / 2 * components || width > coded_width {
        return Err(Error::Corrupt(format!(
            "cr2: slices {count}x{first}+{last} make {row} samples a row, not a {coded_width}-column \
             picture"
        )));
    }
    if !height.is_multiple_of(block_rows) {
        return Err(Error::Corrupt(format!(
            "cr2: sRAW picture of {height} rows, which {block_rows}-row blocks cannot tile"
        )));
    }
    let needed = row
        .checked_mul(height / block_rows)
        .ok_or_else(|| Error::Corrupt("cr2: sRAW frame too large".into()))?;
    if sub.data.len() != needed {
        return Err(Error::Corrupt(format!(
            "cr2: {} samples for a {coded_width}x{height} sRAW picture that needs {needed}",
            sub.data.len()
        )));
    }
    // A zero first width is the single-slice spelling whatever the
    // count says (the row is then `last` alone), and must not be walked
    // `count` times.
    let count = if first == 0 { 0 } else { count };

    let stride = width
        .checked_mul(3)
        .ok_or_else(|| Error::Corrupt("cr2: sRAW frame too wide".into()))?;
    let mut out = vec![0i32; crate::frame_samples(width, height, 3)?];
    let last_column = width & !1;

    let mut read = 0usize;
    let mut ecol = 0usize;
    for slice in 0..=count {
        let scol = ecol;
        let slice_row = if slice < count { first } else { last };
        // The last slice always reaches the coded width, which is at
        // least the picture's; the clamp is where the trim happens.
        ecol = scol
            .saturating_add(slice_row * 2 / components)
            .min(last_column);
        if ecol == scol {
            // Nothing left to place: every column is in, and whatever
            // the stream still holds is padding.
            break;
        }
        for row in (0..height).step_by(block_rows) {
            let mut at = read;
            read = read.saturating_add(slice_row);
            let mut col = scol;
            while col < ecol {
                let mcu = sub.data.get(at..at + components).ok_or_else(|| {
                    Error::Corrupt("cr2: sRAW slice runs past the decoded frame".into())
                })?;
                at += components;
                // The luma of one MCU are the block in raster order:
                // two across, then two more on the row below when the
                // block is 2x2.
                for (k, luma) in mcu[..components - 2].iter().enumerate() {
                    let (y, x) = (row + (k >> 1), col + (k & 1));
                    if y < height && x < width {
                        out[y * stride + x * 3] = *luma as i32;
                    }
                }
                let anchor = row * stride + col * 3;
                out[anchor + 1] = mcu[components - 2] as i32 - SRAW_NEUTRAL;
                out[anchor + 2] = mcu[components - 1] as i32 - SRAW_NEUTRAL;
                col += 2;
            }
        }
    }
    Ok(out)
}

/// Fill in the chroma the encoder did not send, bilinearly.
///
/// A 2x2 block leaves every odd row without chroma, so those are
/// averaged from the rows above and below first; then every odd column
/// of every row is averaged from the even columns either side. Edges
/// copy their one neighbour.
fn sraw_upsample(planes: &mut [i32], width: usize, height: usize, p: usize) {
    let stride = width * 3;
    if p >> 1 != 0 {
        for row in (1..height).step_by(2) {
            let above = (row - 1) * stride;
            let below = if row + 1 < height {
                above + 2 * stride
            } else {
                above
            };
            let here = row * stride;
            for col in (0..width).step_by(2) {
                let x = col * 3;
                for c in 1..3 {
                    planes[here + x + c] = (planes[above + x + c] + planes[below + x + c] + 1) >> 1;
                }
            }
        }
    }
    for row in 0..height {
        let base = row * stride;
        for col in (1..width).step_by(2) {
            let left = base + (col - 1) * 3;
            let right = if col + 1 < width { left + 6 } else { left };
            let here = base + col * 3;
            for c in 1..3 {
                planes[here + c] = (planes[left + c] + planes[right + c] + 1) >> 1;
            }
        }
    }
}

/// Y, Cb, Cr to the camera's RGB, by the body's rule, then onto the
/// sensor's scale by ColorData's gains and clipped.
///
/// The matrix bodies shift the chroma left two bits and add their hue
/// offset before a fixed-point matrix over 2^14. Every other body adds
/// the chroma straight — Cr to red, Cb to blue, a mix over 2^12 to
/// green — after taking 512 off the luma on the three oldest. The
/// shifts are arithmetic: floor division, as the reference frames were
/// made.
fn sraw_to_rgb(planes: &[i32], colour: SrawColour, multipliers: [i32; 3]) -> Vec<u16> {
    let mut out = vec![0u16; planes.len()];
    let (pixels, _) = planes.as_chunks::<3>();
    let (out_pixels, _) = out.as_chunks_mut::<3>();
    for (pixel, rgb) in pixels.iter().zip(out_pixels.iter_mut()) {
        let (y, cb, cr) = (pixel[0] as i64, pixel[1] as i64, pixel[2] as i64);
        // 64-bit throughout: the shifted chroma times the largest
        // coefficient is within a hair of overflowing 32 bits, and a
        // forged ColorData could make the final gain much larger.
        let camera = match colour {
            SrawColour::Matrix { hue } => {
                let cb = (cb << 2) + hue as i64;
                let cr = (cr << 2) + hue as i64;
                [
                    y + ((50 * cb + 22929 * cr) >> 14),
                    y + ((-5640 * cb - 11751 * cr) >> 14),
                    y + ((29040 * cb - 101 * cr) >> 14),
                ]
            }
            SrawColour::Plain { pedestal } => {
                let y = y - pedestal as i64;
                [y + cr, y + ((-778 * cb - (cr << 11)) >> 12), y + cb]
            }
        };
        for c in 0..3 {
            rgb[c] = ((camera[c] * multipliers[c] as i64) >> 10).clamp(0, 32767) as u16;
        }
    }
    out
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
    /// SensorWidth and SensorHeight: the whole frame. On a subsampled
    /// file they are the coded picture, wrapped frames included.
    width: usize,
    height: usize,
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
        width: at(1).unwrap_or(0),
        height: at(2).unwrap_or(0),
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

    /// Where the sRAW channel gains start, by the block's length.
    ///
    /// Unlike the black level they move with the block length rather
    /// than the version stamp, which is ambiguous here: stamp 10 is
    /// shared by the 600D layout (gains at 0x62) and the 5D Mark III /
    /// 6D layout (0x7B). ExifTool's tag documentation splits the
    /// layouts by their length too. The bodies named are the ones a
    /// layout was measured on; the 600D and 1200D layouts have no
    /// subsampled body but their slot is documented, and the oldest
    /// three layouts (582, 653, 796 words) and every CR3-era one have
    /// no slot at all, so a body writing one of those is refused by
    /// [`sraw_gains`] before anything is decoded.
    fn sraw_gain_offset(&self) -> Option<usize> {
        let offset = match self.count() {
            // 1D Mark III, 40D, 1Ds Mark III, 450D, 5D Mark II and 50D,
            // 500D, 1D Mark IV and 7D, 550D, 60D. The 40D, 5D Mark II,
            // 50D, 1D Mark IV, 7D and 60D are verified.
            674 | 692 | 702 | 1227 | 1250 | 1251 | 1337 | 1338 | 1346 => 0x4E,
            // 600D, 1200D: documented, no subsampled mode to verify.
            1273 | 1275 => 0x62,
            // 5D Mark III, 650D and 700D; 6D, 70D and 100D; 1D X and
            // 1D C; 7D Mark II. Verified on the 5D Mark III, 6D, 70D
            // and 7D Mark II.
            1312 | 1313 | 1316 | 1506 => 0x7B,
            // 5DS and 5DS R; 5D Mark IV, 80D and 1D X Mark II; 1300D;
            // 6D Mark II and 800D. Verified on all six subsampled ones.
            1353 | 1560 | 1592 | 1602 => 0x80,
            _ => return None,
        };
        (offset + 4 <= self.count()).then_some(offset)
    }

    /// The per-channel gains the sRAW reconstruction finishes with.
    ///
    /// Four words in sensor order R, Gr, Gb, B. The two greens are one
    /// number, so words 0, 1 and 3 are R, G and B, and every one is
    /// then multiplied by the largest of the four over 1024 and
    /// truncated. The greens are 1170 on every body measured and always
    /// the largest, so the scale is 1170/1024 and green comes out 1336
    /// everywhere; the multiply is done in integers, which agrees with
    /// the reference's single-precision float whenever the product is
    /// exact, as it is for any real block.
    ///
    /// Despite sitting where a white balance would, these are not one:
    /// on the 50D they come out near 687, 1336, 855, leaving green a
    /// third above unity. They put the three reconstructed planes onto
    /// the sensor's own scale, which is why the frame still wants the
    /// as-shot multipliers applied on top of them.
    fn sraw_multipliers(&self) -> Option<[i32; 3]> {
        let at = self.sraw_gain_offset()?;
        let levels: Vec<u16> = (0..4).map_while(|i| self.short(at + i)).collect();
        let levels = <[u16; 4]>::try_from(levels).ok()?;
        // The quad's shape is the check that the offset is right for
        // this block: the two greens are equal — 1170 on every body
        // measured — and the red and blue are positive numbers either
        // side of them. The as-shot white balance, which sits nearby in
        // every layout, has 1024 greens and fails this.
        let [r, g1, g2, b] = levels;
        if g1 != g2 || g1.abs_diff(1170) > 128 || r == 0 || b == 0 {
            return None;
        }
        let largest = *levels.iter().max()? as u64;
        if largest == 0 {
            return None;
        }
        // Clamped so that a forged block cannot make the final
        // multiply in `sraw_to_rgb` unbounded.
        let gain = |i: usize| ((levels[i] as u64 * largest) >> 10).min(1 << 20) as i32;
        Some([gain(0), gain(1), gain(3)])
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

    /// A subsampled frame header without its samples, for the
    /// geometry, which never looks at them.
    fn sof(p: usize, width: usize, height: usize) -> ljpeg::SubsampledImage {
        let components = 3 + p;
        let block_rows = p.div_ceil(2);
        ljpeg::SubsampledImage {
            width,
            height,
            p,
            components,
            block_rows,
            row: (width / 2) * components,
            rows: height / block_rows,
            precision: 15,
            data: Vec::new(),
        }
    }

    /// A subsampled frame built by hand, for `sraw_planes`.
    fn subsampled(p: usize, width: usize, height: usize, data: Vec<u16>) -> ljpeg::SubsampledImage {
        let sub = sof(p, width, height);
        assert_eq!(data.len(), sub.row * sub.rows);
        ljpeg::SubsampledImage { data, ..sub }
    }

    fn geometry(coded_width: usize, width: usize, height: usize) -> SrawGeometry {
        SrawGeometry {
            coded_width,
            width,
            height,
        }
    }

    #[test]
    fn sraw_geometry_reads_the_picture_from_the_slice_tag() {
        // Every body and mode with a reference frame: the frame header's
        // width and height, the slice tag, and the picture the tag
        // describes. The coded width is `2 (n a + b) / C`, the height
        // the header's area over it, and the picture the coded width
        // less the two literal trims.
        // Name, P, the header's width and height, the slice tag, and
        // the coded width, picture width and height that follow.
        type Case = (
            &'static str,
            usize,
            (usize, usize),
            [usize; 3],
            (usize, usize, usize),
        );
        let table: &[Case] = &[
            (
                "50D mRAW",
                3,
                (3344, 2178),
                [7, 1254, 1254],
                (3344, 3272, 2178),
            ),
            (
                "50D sRAW",
                1,
                (2376, 1584),
                [3, 1440, 432],
                (2376, 2376, 1584),
            ),
            (
                "7D/60D mRAW",
                3,
                (3888, 2592),
                [8, 1296, 1296],
                (3888, 3888, 2592),
            ),
            (
                "7D/60D sRAW",
                1,
                (2592, 1728),
                [5, 864, 864],
                (2592, 2592, 1728),
            ),
            (
                "5D II mRAW",
                3,
                (3872, 2574),
                [10, 1056, 1056],
                (3872, 3866, 2574),
            ),
            (
                "5D II sRAW",
                1,
                (2808, 1872),
                [3, 1440, 1296],
                (2808, 2808, 1872),
            ),
            (
                "5D III mRAW",
                3,
                (3960, 2640),
                [5, 1980, 1980],
                (3960, 3960, 2640),
            ),
            (
                "5D III sRAW",
                1,
                (2880, 1920),
                [3, 1440, 1440],
                (2880, 2880, 1920),
            ),
            // The frames written taller than wide, or wrapped to a
            // different width altogether.
            (
                "6D family mRAW",
                3,
                (2736, 4104),
                [5, 2052, 2052],
                (4104, 4104, 2736),
            ),
            (
                "6D family sRAW",
                1,
                (2736, 1824),
                [3, 1368, 1368],
                (2736, 2736, 1824),
            ),
            (
                "5DS mRAW",
                3,
                (3888, 7200),
                [8, 2160, 2160],
                (6480, 6480, 4320),
            ),
            (
                "5DS sRAW",
                1,
                (4320, 2880),
                [5, 1440, 1440],
                (4320, 4320, 2880),
            ),
            (
                "5D IV mRAW",
                3,
                (2520, 6720),
                [7, 1890, 1890],
                (5040, 5040, 3360),
            ),
            (
                "5D IV sRAW",
                1,
                (3360, 2240),
                [4, 1344, 1344],
                (3360, 3360, 2240),
            ),
            (
                "6D II mRAW",
                3,
                (3888, 3770),
                [4, 2808, 2808],
                (4680, 4680, 3132),
            ),
            (
                "6D II sRAW",
                1,
                (3120, 2082),
                [3, 1560, 1560],
                (3120, 3120, 2082),
            ),
            (
                "80D mRAW",
                3,
                (4032, 3402),
                [8, 1512, 1512],
                (4536, 4536, 3024),
            ),
            (
                "80D sRAW",
                1,
                (3000, 2000),
                [5, 1000, 1000],
                (3000, 3000, 2000),
            ),
            (
                "1D IV mRAW",
                3,
                (3672, 2448),
                [8, 1224, 1224],
                (3672, 3672, 2448),
            ),
            (
                "1D IV sRAW",
                1,
                (2448, 1632),
                [5, 816, 816],
                (2448, 2448, 1632),
            ),
            (
                "40D sRAW",
                1,
                (1944, 1296),
                [3, 976, 960],
                (1944, 1944, 1296),
            ),
        ];
        for (name, p, (ws, hs), slices, (coded, width, height)) in table {
            let sub = sof(*p, *ws, *hs);
            let got = sraw_geometry(&sub, *slices, None).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(got, geometry(*coded, *width, *height), "{name}");
            // The stream the header describes is exactly the picture
            // the tag describes, MCU for MCU.
            let rows_per_block = sub.block_rows;
            assert_eq!(
                sub.row * sub.rows,
                (coded / 2) * sub.components * (height / rows_per_block),
                "{name}: stream and picture hold different MCU counts"
            );
        }
        // No tag at all is one slice the width of the header's row.
        let sub = sof(1, 2376, 1584);
        assert_eq!(
            sraw_geometry(&sub, [0, 0, sub.row], None).unwrap(),
            geometry(2376, 2376, 1584)
        );
        // SensorInfo is an independent cross-check: an absent size is
        // ignored, but a conflicting real size is corrupt.
        let sensor = |width, height| SensorInfo {
            width,
            height,
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
            mask: None,
        };
        let sub = sof(3, 3888, 7200);
        for s in [sensor(6480, 4320), sensor(0, 0)] {
            assert_eq!(
                sraw_geometry(&sub, [8, 2160, 2160], Some(&s)).unwrap(),
                geometry(6480, 6480, 4320)
            );
        }
        assert!(matches!(
            sraw_geometry(&sub, [8, 2160, 2160], Some(&sensor(3888, 7200))),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn sraw_geometry_rejects_a_tag_that_does_not_describe_the_frame() {
        let sub = sof(1, 2376, 1584);
        for slices in [
            // Rows that do not divide the frame's area.
            [3, 1440, 436],
            // Not whole MCUs.
            [3, 1441, 432],
            [3, 1440, 430],
            // Nothing at all.
            [0, 0, 0],
            [5, 0, 0],
            // Overflow.
            [usize::MAX, usize::MAX, 8],
            [usize::MAX, 4, usize::MAX],
        ] {
            assert!(
                matches!(sraw_geometry(&sub, slices, None), Err(Error::Corrupt(_))),
                "{slices:?}"
            );
        }
        // A picture whose rows 2x2 blocks cannot tile: 8x6 frame, tag
        // says 16 columns, so 3 rows.
        let sub = sof(3, 8, 6);
        assert!(matches!(
            sraw_geometry(&sub, [0, 0, 48], None),
            Err(Error::Corrupt(_))
        ));
        // A frame whose area overflows.
        let huge = ljpeg::SubsampledImage {
            width: usize::MAX,
            height: 2,
            ..sof(1, 4, 2)
        };
        assert!(matches!(
            sraw_geometry(&huge, [0, 0, 8], None),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn sraw_planes_unslices_the_blocks() {
        // A 2x1 frame, four columns by two rows, so two MCUs a row;
        // cut into two slices of one MCU each, so the stream holds
        // column pair 0 for both rows, then column pair 1 for both.
        let n = |y: u16, x: u16| 100 * y + x;
        let mcu = |y, x| [n(y, x), n(y, x + 1), 16384 + n(y, x), 16384 - n(y, x)];
        let stream: Vec<u16> = [mcu(0, 0), mcu(1, 0), mcu(0, 2), mcu(1, 2)].concat();
        let sub = subsampled(1, 4, 2, stream);
        let planes = sraw_planes(&sub, &geometry(4, 4, 2), [1, 4, 4]).unwrap();
        let px = |x: usize, y: usize| {
            let at = (y * 4 + x) * 3;
            (planes[at], planes[at + 1], planes[at + 2])
        };
        for y in 0..2u16 {
            for x in 0..4u16 {
                let (luma, cb, cr) = px(x as usize, y as usize);
                assert_eq!(luma, n(y, x) as i32, "luma at {x},{y}");
                // Chroma sits on the anchor (even) column only, centred
                // on zero; the odd column is left for the upsample.
                let anchor = x & !1;
                let want = if x % 2 == 0 { n(y, anchor) as i32 } else { 0 };
                assert_eq!((cb, cr), (want, -want), "chroma at {x},{y}");
            }
        }
        // The single-slice spellings place the same frame from a
        // stream in reading order.
        let reading: Vec<u16> = [mcu(0, 0), mcu(0, 2), mcu(1, 0), mcu(1, 2)].concat();
        let sub = subsampled(1, 4, 2, reading);
        let single = sraw_planes(&sub, &geometry(4, 4, 2), [0, 0, 8]).unwrap();
        assert_eq!(single, planes);
        assert_eq!(
            sraw_planes(&sub, &geometry(4, 4, 2), [usize::MAX, 0, 8]).unwrap(),
            planes
        );
        // Trimmed to two columns: the second MCU of each row is padding,
        // decoded and dropped.
        let trimmed = sraw_planes(&sub, &geometry(4, 2, 2), [0, 0, 8]).unwrap();
        assert_eq!(trimmed.len(), 2 * 2 * 3);
        assert_eq!(trimmed[3], n(0, 1) as i32);
        assert_eq!(trimmed[6], n(1, 0) as i32);

        // A wrapped frame: the header says 2 columns by 4 rows, the tag
        // says the picture is 4 columns wide, so it is 2 rows tall and
        // the stream is simply read on through the header's row ends.
        let sub = subsampled(
            1,
            2,
            4,
            [mcu(0, 0), mcu(0, 2), mcu(1, 0), mcu(1, 2)].concat(),
        );
        let wrapped = sraw_planes(&sub, &geometry(4, 4, 2), [0, 0, 8]).unwrap();
        assert_eq!(wrapped, planes);
        // And the transposed one: 2 wide by 4 tall in the header, cut
        // into two 1-MCU slices of the 4x2 picture.
        let sub = subsampled(
            1,
            2,
            4,
            [mcu(0, 0), mcu(1, 0), mcu(0, 2), mcu(1, 2)].concat(),
        );
        let transposed = sraw_planes(&sub, &geometry(4, 4, 2), [1, 4, 4]).unwrap();
        assert_eq!(transposed, planes);
    }

    #[test]
    fn sraw_planes_rejects_a_cut_that_does_not_add_up() {
        let sub = subsampled(1, 4, 2, vec![0; 16]);
        // Slices that do not make the picture row the geometry names,
        // including the count that used to spin for hours.
        for slices in [
            [2, 4, 4],
            [1, 4, 0],
            [usize::MAX, 4, 4],
            [usize::MAX, usize::MAX, 8],
        ] {
            assert!(
                matches!(
                    sraw_planes(&sub, &geometry(4, 4, 2), slices),
                    Err(Error::Corrupt(_))
                ),
                "{slices:?}"
            );
        }
        // Slices that are not whole MCUs.
        assert!(matches!(
            sraw_planes(&sub, &geometry(4, 4, 2), [1, 3, 5]),
            Err(Error::Corrupt(_))
        ));
        // A picture wider than the stream codes, or taller.
        assert!(matches!(
            sraw_planes(&sub, &geometry(4, 6, 2), [0, 0, 8]),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            sraw_planes(&sub, &geometry(4, 4, 3), [0, 0, 8]),
            Err(Error::Corrupt(_))
        ));
        // A frame short of samples is an error, not a read past the end.
        let short = ljpeg::SubsampledImage {
            data: sub.data[..8].to_vec(),
            ..sub.clone()
        };
        assert!(matches!(
            sraw_planes(&short, &geometry(4, 4, 2), [0, 0, 8]),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn sraw_multipliers_come_from_the_block_length() {
        let block = |version: i32, len: usize, at: usize, quad: [u16; 4]| {
            let mut values = vec![0u16; len];
            values[0] = version as u16;
            values[at..at + 4].copy_from_slice(&quad);
            ColorData { values, version }
        };
        // One block of each layout, with the quad the reference frames
        // were made with: the 7D (1337 words), 40D (692), 5D Mark III
        // (1312), 1D X Mark II (1592) and 6D Mark II (1602). The scale
        // is always 1170/1024, so green is always 1336.
        for (version, len, at, quad, want) in [
            (7, 1337, 0x4E, [848, 1170, 1170, 490], [968, 1336, 559]),
            (3, 692, 0x4E, [838, 1170, 1170, 505], [957, 1336, 577]),
            (6, 1250, 0x4E, [602, 1170, 1170, 749], [687, 1336, 855]),
            (9, 1346, 0x4E, [669, 1170, 1170, 619], [764, 1336, 707]),
            (10, 1273, 0x62, [700, 1170, 1170, 700], [799, 1336, 799]),
            (10, 1312, 0x7B, [836, 1170, 1170, 437], [955, 1336, 499]),
            (10, 1313, 0x7B, [539, 1170, 1170, 833], [615, 1336, 951]),
            (11, 1506, 0x7B, [552, 1170, 1170, 747], [630, 1336, 853]),
            (12, 1560, 0x80, [578, 1170, 1170, 528], [660, 1336, 603]),
            (13, 1592, 0x80, [630, 1170, 1170, 750], [719, 1336, 856]),
            (15, 1602, 0x80, [603, 1170, 1170, 625], [688, 1336, 714]),
        ] {
            let data = block(version, len, at, quad);
            assert_eq!(data.sraw_gain_offset(), Some(at), "{len} words");
            assert_eq!(data.sraw_multipliers(), Some(want), "{len} words");
        }
        // The stamp does not choose: two version-10 blocks of different
        // lengths keep their gains in different places.
        assert_ne!(
            block(10, 1273, 0, [0; 4]).sraw_gain_offset(),
            block(10, 1312, 0, [0; 4]).sraw_gain_offset()
        );
        // A quad of the wrong shape — here the as-shot white balance,
        // whose greens are 1024 — means the block is not what its
        // length says, and unity is the answer.
        let wrong = block(7, 1337, 0x4E, [2166, 1024, 1024, 1524]);
        assert_eq!(wrong.sraw_multipliers(), None);
        let unequal = block(6, 1250, 0x4E, [602, 1170, 1171, 749]);
        assert_eq!(unequal.sraw_multipliers(), None);
        // Layouts with no gain slot: the oldest three, the CR3 era, and
        // a length nobody has written.
        for len in [582, 653, 796, 1816, 4528, 1000] {
            let none = block(1, len, 0x4E, [602, 1170, 1170, 749]);
            assert_eq!(none.sraw_gain_offset(), None, "{len} words");
            assert_eq!(none.sraw_multipliers(), None, "{len} words");
        }
        // A forged quad cannot make the gains unbounded.
        let forged = block(7, 1337, 0x4E, [65535, 1170, 1170, 65535]);
        assert!(forged
            .sraw_multipliers()
            .unwrap()
            .iter()
            .all(|g| *g <= 1 << 20));
    }

    #[test]
    fn sraw_gains_refuse_only_an_unknown_layout() {
        let block = |len: usize| {
            let mut values = vec![0u16; len];
            if len > 0x52 {
                values[0x4E..0x52].copy_from_slice(&[602, 1170, 1170, 749]);
            }
            ColorData { values, version: 6 }
        };
        assert_eq!(
            sraw_gains(Some(&block(1250)), "Canon EOS 50D").unwrap(),
            [687, 1336, 855]
        );
        for (colour, what) in [
            (None, "no ColorData"),
            (Some(block(582)), "gain slot"),
            (Some(block(1816)), "gain slot"),
            (Some(block(999)), "gain slot"),
        ] {
            match sraw_gains(colour.as_ref(), "Canon EOS 50D") {
                Err(Error::Unsupported(why)) => {
                    assert!(
                        why.contains("sRAW") && why.contains("Canon EOS 50D"),
                        "{why}"
                    );
                    assert!(why.contains(what), "{why}");
                }
                other => panic!("{what}: {other:?}"),
            }
        }
        // A known layout with an odd quad is unity, not a refusal.
        let mut odd = block(1250);
        odd.values[0x4F] = 1024;
        assert_eq!(sraw_gains(Some(&odd), "Canon EOS 50D").unwrap(), [1024; 3]);
    }

    #[test]
    fn sraw_colour_is_chosen_by_model_id_and_firmware() {
        use SrawColour::{Matrix, Plain};
        let (mraw, sraw) = (3, 1);
        // The five matrix bodies and their hues, per mode.
        for (id, firmware, hues) in [
            (body::EOS_50D, Some(1_000_009), (16, 8)),
            (body::EOS_7D, Some(2_000_003), (16, 8)),
            (body::EOS_1D_MARK_IV, Some(1_001_000), (6, 2)),
            (body::EOS_60D, Some(1_001_001), (6, 2)),
            // The 5D Mark II by firmware: 2.1.2 is the corpus sample.
            (body::EOS_5D_MARK_II, Some(2_001_002), (6, 2)),
            (body::EOS_5D_MARK_II, Some(1_000_007), (6, 2)),
            (body::EOS_5D_MARK_II, Some(1_000_006), (16, 8)),
            (body::EOS_5D_MARK_II, Some(1_000_000), (16, 8)),
            // Unreadable firmware on a 5D Mark II is the older hue.
            (body::EOS_5D_MARK_II, None, (16, 8)),
            // Firmware means nothing on the others.
            (body::EOS_50D, None, (16, 8)),
            (body::EOS_60D, None, (6, 2)),
        ] {
            assert_eq!(
                sraw_colour(Some(id), firmware, mraw),
                Matrix { hue: hues.0 },
                "{id:#x}"
            );
            assert_eq!(
                sraw_colour(Some(id), firmware, sraw),
                Matrix { hue: hues.1 },
                "{id:#x}"
            );
        }
        // Everything else is the plain formula: the pedestal on the
        // three bodies below the 5D Mark II's id, none above it — the
        // 1D X sits between the 5D Mark II and the 1D Mark IV.
        for (id, pedestal) in [
            (0x8000_0169, 512), // 1D Mark III
            (0x8000_0190, 512), // 40D
            (0x8000_0215, 512), // 1Ds Mark III
            (0x8000_0269, 0),   // 1D X
            (0x8000_0285, 0),   // 5D Mark III
            (0x8000_0289, 0),   // 7D Mark II
            (0x8000_0302, 0),   // 6D
            (0x8000_0325, 0),   // 70D
            (0x8000_0328, 0),   // 1D X Mark II
            (0x8000_0349, 0),   // 5D Mark IV
            (0x8000_0350, 0),   // 80D
            (0x8000_0382, 0),   // 5DS
            (0x8000_0401, 0),   // 5DS R
            (0x8000_0406, 0),   // 6D Mark II
        ] {
            for p in [mraw, sraw] {
                assert_eq!(
                    sraw_colour(Some(id), Some(1_000_000), p),
                    Plain { pedestal },
                    "{id:#x}"
                );
            }
        }
        assert_eq!(sraw_colour(None, None, mraw), Plain { pedestal: 0 });
    }

    #[test]
    fn firmware_parses_canons_string() {
        assert_eq!(parse_firmware("Firmware Version 2.1.2"), Some(2_001_002));
        assert_eq!(parse_firmware("Firmware Version 1.0.6"), Some(1_000_006));
        assert_eq!(parse_firmware("1.0.7"), Some(1_000_007));
        // Missing parts are zero.
        assert_eq!(parse_firmware("Firmware 1.1"), Some(1_001_000));
        assert_eq!(parse_firmware("Version 3"), Some(3_000_000));
        assert_eq!(
            parse_firmware("Firmware Version 1.1.0 (beta)"),
            Some(1_001_000)
        );
        assert_eq!(parse_firmware("Firmware Version"), None);
        assert_eq!(parse_firmware(""), None);
        assert_eq!(parse_firmware("v99999999999.1"), None);
        assert!(
            parse_firmware("Firmware Version 1.0.6").unwrap() <= EOS_5D_MARK_II_OLD_HUE_FIRMWARE
        );
        assert!(
            parse_firmware("Firmware Version 1.0.7").unwrap() > EOS_5D_MARK_II_OLD_HUE_FIRMWARE
        );
    }

    #[test]
    fn sraw_upsample_fills_the_gaps_bilinearly() {
        // Two 2x2 blocks side by side: chroma only at (0,0) and (0,2),
        // so the odd column is their mean and the odd row copies down.
        let (width, height) = (4, 2);
        let mut planes = vec![0i32; width * height * 3];
        let set = |p: &mut Vec<i32>, x: usize, y: usize, cb, cr| {
            p[(y * width + x) * 3 + 1] = cb;
            p[(y * width + x) * 3 + 2] = cr;
        };
        set(&mut planes, 0, 0, 100, -40);
        set(&mut planes, 2, 0, 200, -50);
        sraw_upsample(&mut planes, width, height, 3);
        let cb = |x: usize, y: usize| planes[(y * width + x) * 3 + 1];
        let cr = |x: usize, y: usize| planes[(y * width + x) * 3 + 2];
        // Odd column: the mean of its neighbours, rounded half up.
        assert_eq!(cb(1, 0), 150);
        assert_eq!(cr(1, 0), -45);
        // Last column has only a left neighbour to copy.
        assert_eq!(cb(3, 0), 200);
        // Row 1 has no chroma of its own and no row below, so it takes
        // row 0 unchanged.
        assert_eq!((cb(0, 1), cb(1, 1), cb(2, 1)), (100, 150, 200));
        assert_eq!(cr(0, 1), -40);

        // A 2x1 frame carries chroma on every row, so only the odd
        // columns are filled and the rows are left alone.
        let mut planes = vec![0i32; width * height * 3];
        set(&mut planes, 0, 1, 60, 20);
        set(&mut planes, 2, 1, 80, 30);
        sraw_upsample(&mut planes, width, height, 1);
        assert_eq!(planes[(width + 1) * 3 + 1], 70);
        // Row 0 was never given chroma and must stay neutral.
        assert_eq!(planes[1], 0);
    }

    #[test]
    fn sraw_to_rgb_matches_the_worked_first_pixel() {
        // The 50D mRAW sample's (IMG_9517: Canon's sRAW1, luma sampled
        // 2x2, factor byte 0x22, P = 3) first pixel: Y 572 with the
        // chroma the stream carries (16386, 16376) centred on zero, the
        // hue offset (P + 1) << 2 = 16, and ColorData's gains
        // 687/1336/855.
        let planes = vec![572, 16386 - SRAW_NEUTRAL, 16376 - SRAW_NEUTRAL];
        let matrix = SrawColour::Matrix { hue: 16 };
        assert_eq!(
            sraw_to_rgb(&planes, matrix, [687, 1336, 855]),
            vec![368, 750, 512]
        );
        // Unity gains leave the matrix's own scale, and a neutral
        // pixel stays very nearly grey.
        let grey = vec![8000, 0, 0];
        let rgb = sraw_to_rgb(&grey, matrix, [1024; 3]);
        assert!(
            rgb.iter().all(|v| v.abs_diff(8000) < 40),
            "a neutral pixel should stay grey, got {rgb:?}"
        );

        // The plain formula: Cr straight onto red, Cb onto blue, and
        // green pulled the other way over 2^12, with the shifts
        // rounding towards minus infinity.
        let plain = SrawColour::Plain { pedestal: 0 };
        assert_eq!(
            sraw_to_rgb(&[1000, 0, 0], plain, [1024; 3]),
            vec![1000, 1000, 1000]
        );
        assert_eq!(
            sraw_to_rgb(&[1000, 100, -200], plain, [1024; 3]),
            // (-77800 + 409600) >> 12 is 81.
            vec![800, 1081, 1100]
        );
        assert_eq!(
            sraw_to_rgb(&[1000, -3, 1], plain, [1024; 3]),
            // (2334 - 2048) >> 12 is 0.
            vec![1001, 1000, 997]
        );
        assert_eq!(
            sraw_to_rgb(&[1000, -1, -1], plain, [1024; 3]),
            vec![999, 1000, 999]
        );
        // The pedestal comes off the luma before anything else, and
        // the gains scale the result over 1024.
        let old = SrawColour::Plain { pedestal: 512 };
        assert_eq!(
            sraw_to_rgb(&[1512, 0, 0], old, [1024; 3]),
            vec![1000, 1000, 1000]
        );
        assert_eq!(
            sraw_to_rgb(&[1512, 0, 0], old, [957, 1336, 577]),
            // 1000 * (957, 1336, 577) / 1024, truncated.
            vec![934, 1304, 563]
        );
        // Below zero clips to zero rather than wrapping.
        assert_eq!(sraw_to_rgb(&[100, 0, 0], old, [1024; 3]), vec![0, 0, 0]);
        // Hostile values must clip, not wrap or panic.
        let extreme = vec![32767, 32767, -32768];
        for colour in [matrix, plain, old] {
            let rgb = sraw_to_rgb(&extreme, colour, [1 << 20, 1 << 20, 1 << 20]);
            assert!(rgb.iter().all(|v| *v <= 32767));
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

    /// Whether a sample's lossless JPEG is subsampled — an sRAW or mRAW
    /// — read from its frame header rather than from a list of names.
    ///
    /// Every such file in the corpus must reconstruct sample for sample
    /// (`corpus_sraw_matches_the_full_oracle`); the only refusal left on
    /// this path is a ColorData layout with no known gain slot, which
    /// no body that writes a subsampled frame has.
    fn is_subsampled(bytes: &[u8]) -> bool {
        let Ok(tiff) = Tiff::parse(bytes) else {
            return false;
        };
        let Some(ifd) = raw_ifd(&tiff) else {
            return false;
        };
        raw_stream(&tiff, ifd)
            .and_then(ljpeg::sampling)
            .is_ok_and(|factor| factor != 0x11)
    }

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
        /// LibRaw's "Image size": what a subsampled frame reconstructs
        /// to, which is narrower than its padded "Full size".
        image: Option<(usize, usize)>,
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
                ["Image", "size:", ..] => out.image = Some((size(2)?, size(4)?)),
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
            let raw = crate::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
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
                // A subsampled frame's "Full size" is the padded luma
                // width the lossless JPEG codes (3344 on the 50D's
                // mRAW); the picture the reference hands on, and that
                // its sidecar holds, is the narrower "Image size".
                let want = if raw.cpp == 3 {
                    identify.image
                } else {
                    identify.full
                };
                if let Some(want) = want {
                    assert_eq!((raw.width, raw.height), want, "{name} frame size");
                }
                // For a subsampled frame LibRaw reports the makernote's
                // CroppedImageWidth/Height instead of SensorInfo's
                // borders (2352x1568 against 2376x1584 on the 50D's
                // sRAW), and those numbers are the same on both sample
                // files whatever the mode, so they cannot be the
                // frame's own crop. SensorInfo's borders are kept.
                if let Some((width, height, left, top)) = identify.inset.filter(|_| raw.cpp == 1) {
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
            // A reconstructed sRAW frame has no pedestal: the YCbCr
            // maths centred it, and its black level really is zero.
            assert!(high > 0.0 || raw.cpp == 3, "{name} found no black level");

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

    // ------------------------------------------------ sRAW / mRAW

    /// The shape and the worked values the specification records for a
    /// sample, applied when a discovered file is one of them: the
    /// picture's size, the full dump's pixels (0,0), (0,1) and (1,0)
    /// as R, G, B, the largest value in it, and — for the 50D pair —
    /// the planar stage's first pixel and luma anchors.
    ///
    /// Note which is which: 9517 is the *larger* picture — Canon's mRAW,
    /// or sRAW1 — and its luma is sampled 2x2 (factor byte 0x22, so
    /// P = 3); 9518 is the smaller sRAW (sRAW2), sampled 2x1 (0x21,
    /// P = 1). Part 1 of the written spec labels these the other way
    /// round in its prose; Part 2 corrects it, and the factor byte in
    /// the file is what the arithmetic keys off.
    struct Worked {
        name: &'static str,
        p: usize,
        width: usize,
        height: usize,
        /// Full stage: pixels (0,0), (0,1) and (1,0).
        full: [(i32, i32, i32); 3],
        max: i32,
        /// SensorInfo's crop where it is not the whole picture.
        crop: Option<(usize, usize, usize, usize)>,
        /// Planar stage, in the sidecar's units: pixel 0's Y, Cb, Cr
        /// and the first four row-0 luma anchors.
        planar: Option<(i32, i32, i32, [i32; 4])>,
    }

    const fn worked_row(
        name: &'static str,
        p: usize,
        (width, height): (usize, usize),
        full: [(i32, i32, i32); 3],
        max: i32,
    ) -> Worked {
        Worked {
            name,
            p,
            width,
            height,
            full,
            max,
            crop: None,
            planar: None,
        }
    }

    const WORKED: &[Worked] = &[
        // The two trimmed pictures keep a SensorInfo border 6 short of
        // their width (3266 and 3860), so their crops are narrower still.
        Worked {
            crop: Some((0, 0, 3267, 2178)),
            planar: Some((572, 8194, 8184, [572, 579, 565, 541])),
            ..worked_row(
                "EOS_50D-IMG_9517.CR2",
                3,
                (3272, 2178),
                [(368, 750, 512), (369, 752, 514), (366, 725, 501)],
                16730,
            )
        },
        Worked {
            crop: None,
            planar: Some((584, 8192, 8188, [584, 571, 549, 522])),
            ..worked_row(
                "EOS_50D-IMG_9518.CR2",
                1,
                (2376, 1584),
                [(383, 764, 502), (374, 746, 497), (370, 750, 490)],
                16737,
            )
        },
        worked_row(
            "EOS-1D_Mark_IV-IMG_5395-mraw.CR2",
            3,
            (3672, 2448),
            [(1267, 2686, 1626), (1308, 2690, 1626), (1265, 2673, 1614)],
            11075,
        ),
        worked_row(
            "EOS-1D_Mark_IV-IMG_5398-sraw.CR2",
            1,
            (2448, 1632),
            [(1665, 3638, 2277), (1677, 3668, 2289), (1658, 3636, 2280)],
            11817,
        ),
        worked_row(
            "EOS-1D_X_Mark_II-AZ1I2271-mraw.CR2",
            3,
            (4104, 2736),
            [(263, 772, 776), (259, 760, 763), (265, 774, 770)],
            17585,
        ),
        worked_row(
            "EOS-1D_X_Mark_II-AZ1I2272-sraw.CR2",
            1,
            (2736, 1824),
            [(264, 765, 761), (261, 761, 756), (258, 755, 754)],
            17446,
        ),
        Worked {
            crop: Some((4, 3, 1936, 1288)),
            ..worked_row(
                "EOS_40D-_MG_0154-sraw.CR2",
                1,
                (1944, 1296),
                [(878, 1103, 465), (876, 1092, 464), (940, 1182, 490)],
                10027,
            )
        },
        worked_row(
            "EOS_5DS-2K4A9928-mraw.CR2",
            3,
            (6480, 4320),
            [(29, 62, 25), (21, 52, 20), (23, 62, 25)],
            17585,
        ),
        worked_row(
            "EOS_5DS-2K4A9929-sraw.CR2",
            1,
            (4320, 2880),
            [(21, 54, 29), (32, 70, 35), (24, 61, 25)],
            17504,
        ),
        worked_row(
            "EOS_5DS_R-_DSR2003-mraw.CR2",
            3,
            (6480, 4320),
            [(981, 1319, 390), (974, 1302, 394), (977, 1315, 395)],
            16206,
        ),
        worked_row(
            "EOS_5DS_R-_DSR2004-sraw.CR2",
            1,
            (4320, 2880),
            [(998, 1328, 383), (988, 1317, 386), (986, 1339, 395)],
            16304,
        ),
        Worked {
            crop: Some((0, 0, 3861, 2574)),
            ..worked_row(
                "EOS_5D_Mark_II-sraw1.CR2",
                3,
                (3866, 2574),
                [(567, 804, 217), (567, 802, 220), (572, 799, 221)],
                14735,
            )
        },
        Worked {
            crop: Some((12, 8, 2784, 1856)),
            ..worked_row(
                "EOS_5D_Mark_II-sraw2.CR2",
                1,
                (2808, 1872),
                [(569, 778, 219), (570, 780, 219), (553, 785, 221)],
                15126,
            )
        },
        worked_row(
            "EOS_5D_Mark_III-5G4A9395.CR2",
            3,
            (3960, 2640),
            [(308, 404, 134), (303, 394, 132), (310, 405, 134)],
            17548,
        ),
        worked_row(
            "EOS_5D_Mark_III-5G4A9396.CR2",
            1,
            (2880, 1920),
            [(276, 352, 110), (276, 347, 110), (282, 348, 109)],
            17548,
        ),
        worked_row(
            "EOS_5D_Mark_IV-B13A0732-sraw.CR2",
            1,
            (3360, 2240),
            [(163, 529, 556), (170, 540, 565), (169, 532, 554)],
            18950,
        ),
        worked_row(
            "EOS_5D_Mark_IV-B13A0733-mraw.CR2",
            3,
            (5040, 3360),
            [(144, 450, 467), (146, 450, 468), (147, 455, 472)],
            18936,
        ),
        worked_row(
            "EOS_60D-IMG_2015-mraw.CR2",
            3,
            (3888, 2592),
            [(1482, 2747, 1710), (1600, 2965, 1818), (1510, 2758, 1706)],
            13648,
        ),
        worked_row(
            "EOS_60D-IMG_2016-sraw.CR2",
            1,
            (2592, 1728),
            [(514, 742, 352), (500, 738, 352), (500, 735, 357)],
            13648,
        ),
        worked_row(
            "EOS_6D-mRAW.CR2",
            3,
            (4104, 2736),
            [(3100, 7423, 5902), (3117, 7441, 5906), (3111, 7436, 5909)],
            17459,
        ),
        worked_row(
            "EOS_6D-sRAW.CR2",
            1,
            (2736, 1824),
            [(2947, 7027, 5525), (2933, 6997, 5497), (2950, 7008, 5513)],
            17429,
        ),
        Worked {
            crop: Some((0, 12, 4680, 3120)),
            ..worked_row(
                "EOS_6D_Mark_II-mRAW.CR2",
                3,
                (4680, 3132),
                [(0, 0, 0), (0, 1, 0), (3, 5, 0)],
                12785,
            )
        },
        Worked {
            crop: Some((0, 2, 3120, 2080)),
            ..worked_row(
                "EOS_6D_Mark_II-sRAW.CR2",
                1,
                (3120, 2082),
                [(142, 296, 115), (219, 427, 180), (125, 267, 108)],
                13829,
            )
        },
        worked_row(
            "EOS_70D-mRAW_01.CR2",
            3,
            (4104, 2736),
            [(505, 1505, 1332), (500, 1495, 1326), (508, 1508, 1328)],
            12325,
        ),
        worked_row(
            "EOS_70D-sRAW_01.CR2",
            1,
            (2736, 1824),
            [(766, 2071, 1678), (762, 2064, 1661), (781, 2105, 1689)],
            15046,
        ),
        worked_row(
            "EOS_7D-mraw.CR2",
            3,
            (3888, 2592),
            [(83, 69, 39), (74, 58, 30), (73, 41, 34)],
            13477,
        ),
        worked_row(
            "EOS_7D-sraw.CR2",
            1,
            (2592, 1728),
            [(86, 87, 40), (92, 93, 48), (96, 112, 49)],
            13579,
        ),
        worked_row(
            "EOS_7D_Mark_II-capture000107-mraw.CR2",
            3,
            (4104, 2736),
            [(1156, 2450, 1561), (1153, 2433, 1551), (1160, 2459, 1571)],
            13062,
        ),
        worked_row(
            "EOS_7D_Mark_II-capture000108-sraw.CR2",
            1,
            (2736, 1824),
            [(1273, 2687, 1723), (1267, 2677, 1722), (1267, 2672, 1708)],
            13255,
        ),
        worked_row(
            "EOS_80D-IMG_0096-sraw.CR2",
            1,
            (3000, 2000),
            [(118, 197, 97), (113, 189, 94), (112, 191, 89)],
            19281,
        ),
        Worked {
            crop: Some((36, 18, 4500, 3000)),
            ..worked_row(
                "EOS_80D-IMG_0097-mraw.CR2",
                3,
                (4536, 3024),
                [(0, 3, 0), (0, 3, 0), (0, 3, 0)],
                19385,
            )
        },
    ];

    fn worked(name: &str) -> Option<&'static Worked> {
        WORKED.iter().find(|w| w.name == name)
    }

    /// Every subsampled CR2 under the corpus, with its bytes and model.
    fn subsampled_files(dir: &std::path::Path) -> Vec<(PathBuf, String, Vec<u8>)> {
        let mut files = Vec::new();
        cr2_files(dir, &mut files);
        files.sort();
        files
            .into_iter()
            .filter_map(|path| {
                let bytes = std::fs::read(&path).ok()?;
                if !is_subsampled(&bytes) {
                    return None;
                }
                let model = Tiff::parse(&bytes).ok()?.make_model().1;
                Some((path, model, bytes))
            })
            .collect()
    }

    fn file_name(path: &std::path::Path) -> String {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    }

    /// A raw little-endian `u16` sidecar beside a sample.
    fn sidecar(path: &std::path::Path, which: &str) -> Option<Vec<u16>> {
        let name = format!("{}.{}", path.file_name()?.to_string_lossy(), which);
        let bytes = std::fs::read(path.with_file_name(name)).ok()?;
        Some(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect(),
        )
    }

    /// Decode a sample as far as the planar stage: the placed blocks,
    /// before any chroma interpolation or colour.
    fn planar_stage(bytes: &[u8]) -> Result<(ljpeg::SubsampledImage, SrawGeometry, Vec<i32>)> {
        let tiff = Tiff::parse(bytes)?;
        let raw_ifd = raw_ifd(&tiff).ok_or_else(|| Error::Corrupt("no raw IFD".into()))?;
        let stream = raw_stream(&tiff, raw_ifd)?;
        let sub = ljpeg::decode_subsampled(stream)?;
        let sensor = makernote(&tiff).and_then(|mn| {
            let (root, le) = (mn.root(), mn.little_endian());
            sensor_info(root, le)
        });
        let slices = slices(raw_ifd, sub.row);
        let geometry = sraw_geometry(&sub, slices, sensor.as_ref())?;
        let planes = sraw_planes(&sub, &geometry, slices)?;
        Ok((sub, geometry, planes))
    }

    /// Compare `got` with `want` sample for sample and fail with the
    /// first few disagreements.
    fn same_samples(name: &str, stage: &str, got: &[i32], want: &[i32]) {
        assert_eq!(got.len(), want.len(), "{name} {stage}: sample count");
        let wrong: Vec<usize> = got
            .iter()
            .zip(want)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .take(6)
            .collect();
        let total = got.iter().zip(want).filter(|(a, b)| a != b).count();
        assert!(
            wrong.is_empty(),
            "{name} {stage}: {total} of {} samples differ, first at {wrong:?} \
             (got {:?}, want {:?})",
            got.len(),
            wrong.iter().map(|i| got[*i]).collect::<Vec<_>>(),
            wrong.iter().map(|i| want[*i]).collect::<Vec<_>>(),
        );
    }

    /// Stage one: entropy decode and MCU placement, against
    /// `<file>.sraw-planar.rgb16`, for every subsampled sample that has
    /// one (the 50D pair). This stage has no body-specific constants in
    /// it, so it runs whether or not the body's colour is verified.
    ///
    /// The sidecar holds the luma straight and the two chroma with a
    /// bias of 8192 — a neutral block reads 8192, and a pixel the
    /// encoder sent no chroma for reads 8192 too. This module centres
    /// the chroma on zero instead, so the bias goes back on here.
    #[test]
    fn corpus_sraw_matches_the_planar_oracle() {
        let Some(dir) = corpus() else { return };
        let files = subsampled_files(&dir);
        assert!(
            !files.is_empty(),
            "no subsampled CR2 under {}",
            dir.display()
        );
        let mut checked = 0;
        for (path, _, bytes) in &files {
            let name = file_name(path);
            let Some(oracle) = sidecar(path, "sraw-planar.rgb16") else {
                continue;
            };
            let start = std::time::Instant::now();
            let (sub, geometry, planes) =
                planar_stage(bytes).unwrap_or_else(|e| panic!("{name}: planar stage: {e}"));
            let elapsed = start.elapsed();
            let SrawGeometry { width, height, .. } = geometry;
            assert_eq!(sub.components, 3 + sub.p, "{name} MCU components");
            assert_eq!(
                oracle.len(),
                width * height * 3,
                "{name}: the planar sidecar is not {width}x{height}x3"
            );

            if let Some(worked) = worked(&name) {
                assert_eq!(sub.p, worked.p, "{name} sraw parameter");
                assert_eq!(
                    (width, height),
                    (worked.width, worked.height),
                    "{name} shape"
                );
                // The worked first pixels the specification records,
                // in the sidecar's own units.
                let at = |i: usize| planes[i];
                let (y0, cb0, cr0, anchors) = worked
                    .planar
                    .unwrap_or_else(|| panic!("{name}: no worked planar values"));
                assert_eq!(
                    (at(0), at(1) + 8192, at(2) + 8192),
                    (y0, cb0, cr0),
                    "{name} first pixel of the planar stage"
                );
                let got: Vec<i32> = (0..4).map(|i| at(i * 2 * 3)).collect();
                assert_eq!(got, anchors, "{name} row-0 luma anchors");
            }

            let biased: Vec<i32> = planes
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|px| [px[0], px[1] + 8192, px[2] + 8192])
                .collect();
            let want: Vec<i32> = oracle.iter().map(|v| *v as i32).collect();
            same_samples(&name, "planar", &biased, &want);
            println!(
                "{name}: planar {width}x{height} P={} matches the oracle, entropy+placement {:.3}s",
                sub.p,
                elapsed.as_secs_f64()
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "no subsampled sample with a planar sidecar under {}",
            dir.display()
        );
        println!("checked {checked} subsampled samples against the planar oracle");
    }

    /// Stage two: the whole decode, against `<file>.sraw-full.rgb16`,
    /// for every subsampled sample in the corpus. Every one must match
    /// its sidecar sample for sample — there is no allow-list, because a
    /// frame that decodes to plausible but wrong colour is the failure
    /// this guards against and nothing downstream can tell.
    #[test]
    fn corpus_sraw_matches_the_full_oracle() {
        let Some(dir) = corpus() else { return };
        let files = subsampled_files(&dir);
        assert!(
            !files.is_empty(),
            "no subsampled CR2 under {}",
            dir.display()
        );
        let mut exact = 0;
        let mut bodies = std::collections::BTreeSet::new();
        for (path, model, bytes) in &files {
            let name = file_name(path);
            let oracle = sidecar(path, "sraw-full.rgb16")
                .unwrap_or_else(|| panic!("{name}: no .sraw-full.rgb16 sidecar"));
            let start = std::time::Instant::now();
            let raw = decode(bytes).unwrap_or_else(|e| panic!("{name} ({model}): {e}"));
            let elapsed = start.elapsed();
            raw.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            let (width, height) = (raw.width, raw.height);
            assert_eq!(raw.cpp, 3, "{name} samples a pixel");
            assert_eq!(raw.cfa, Cfa::None, "{name} has no filter array");
            assert_eq!(raw.white_level, 16383.0, "{name} white level");
            assert_eq!(raw.black_levels, [0.0; 4], "{name} black level");
            // The reconstruction is camera RGB, not a white-balanced
            // picture, so the as-shot multipliers must still be here
            // for the developer to apply.
            assert!(
                raw.wb_coeffs[1] == 1.0 && raw.wb_coeffs != [1.0; 4],
                "{name} kept no as-shot white balance: {:?}",
                raw.wb_coeffs
            );
            let RawData::U16(got) = &raw.data else {
                panic!("{name} is not 16-bit")
            };
            assert_eq!(
                oracle.len(),
                width * height * 3,
                "{name}: the full sidecar is not {width}x{height}x3"
            );

            let worked = worked(&name)
                .unwrap_or_else(|| panic!("{name}: no worked values; add the sample to WORKED"));
            assert_eq!(
                (width, height),
                (worked.width, worked.height),
                "{name} shape"
            );
            // The worked pixels the specification records: (0,0),
            // (0,1) and (1,0), and the largest value in the frame.
            let px = |i: usize| {
                (
                    got[i * 3] as i32,
                    got[i * 3 + 1] as i32,
                    got[i * 3 + 2] as i32,
                )
            };
            for (i, want) in [0, 1, width].into_iter().zip(worked.full) {
                assert_eq!(px(i), want, "{name} pixel at index {i}");
            }
            let max = got.iter().copied().max().unwrap_or(0) as i32;
            assert_eq!(max, worked.max, "{name} largest value");
            if let Some((x, y, w, h)) = worked.crop {
                assert_eq!(
                    raw.crop,
                    Rect {
                        x,
                        y,
                        width: w,
                        height: h
                    },
                    "{name} crop"
                );
            } else {
                assert_eq!(
                    (raw.crop.x, raw.crop.y, raw.crop.width, raw.crop.height),
                    (0, 0, width, height),
                    "{name} crop"
                );
            }

            let mine: Vec<i32> = got.iter().map(|v| *v as i32).collect();
            let want: Vec<i32> = oracle.iter().map(|v| *v as i32).collect();
            same_samples(&name, "full", &mine, &want);
            let mp = (width * height) as f64 / 1e6;
            println!(
                "{name} ({model}): full {width}x{height} RGB matches the oracle, {:.3}s ({:.1} MP/s)",
                elapsed.as_secs_f64(),
                mp / elapsed.as_secs_f64()
            );
            exact += 1;
            bodies.insert(model.clone());
        }
        assert!(exact > 0, "no subsampled sample under {}", dir.display());
        println!(
            "{exact} subsampled samples exact across {} bodies: {:?}",
            bodies.len(),
            bodies
        );
    }

    /// The white-balance decision, checked against the Bayer frame of
    /// the same scene.
    ///
    /// 9516, 9517 and 9518 are one subject photographed three times in
    /// a row: full Bayer, then the two subsampled modes. ColorData's
    /// sRAW multipliers sit where a white balance would and the written
    /// specification calls them one, but they are not: on this body
    /// they come out near 687, 1336, 855, so a neutral subject
    /// reconstructs strongly green — camera RGB, exactly like a Bayer
    /// frame before balancing. So this module bakes them in (they are
    /// part of the reconstruction, and the oracle contains them) and
    /// leaves the *as-shot* multipliers in `wb_coeffs` for `develop`,
    /// which is what makes the developed colour right.
    ///
    /// Had the choice gone the other way — `wb_coeffs` left at unity —
    /// the developed sRAW would come out roughly a stop green against
    /// the Bayer shot, which is what this measures.
    #[test]
    fn corpus_sraw_develops_like_the_bayer_shot_of_the_same_scene() {
        let Some(dir) = corpus() else { return };
        // Mean R/G and B/G of a developed frame.
        let balance = |name: &str| -> Option<(f64, f64)> {
            let bytes = std::fs::read(dir.join("Canon").join(name)).ok()?;
            let raw = decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            let out = crate::develop(&raw, &crate::DevelopOptions::default())
                .unwrap_or_else(|e| panic!("{name} develop: {e}"));
            let mut sums = [0f64; 3];
            for pixel in out.rgb.as_chunks::<3>().0 {
                for (s, v) in sums.iter_mut().zip(pixel) {
                    *s += *v as f64;
                }
            }
            (sums[1] > 0.0).then(|| (sums[0] / sums[1], sums[2] / sums[1]))
        };
        let Some(bayer) = balance("EOS_50D-IMG_9516.CR2") else {
            return;
        };
        for name in ["EOS_50D-IMG_9517.CR2", "EOS_50D-IMG_9518.CR2"] {
            let Some(sraw) = balance(name) else { continue };
            println!(
                "{name}: developed R/G {:.3} B/G {:.3} against the Bayer shot's {:.3} / {:.3}",
                sraw.0, sraw.1, bayer.0, bayer.1
            );
            // Chroma subsampling, a different demosaic and a slightly
            // different crop move the scene mean by a few per cent, so
            // the bar is loose. It is still nowhere near the failure
            // being guarded against: leaving `wb_coeffs` at unity puts
            // R/G out by the as-shot multiplier itself, about 2x.
            assert!(
                (sraw.0 / bayer.0 - 1.0).abs() < 0.15 && (sraw.1 / bayer.1 - 1.0).abs() < 0.15,
                "{name} develops to R/G {:.3} B/G {:.3}, the Bayer shot of the same scene to \
                 {:.3} / {:.3}: the white balance is being applied wrongly",
                sraw.0,
                sraw.1,
                bayer.0,
                bayer.1
            );
        }
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
