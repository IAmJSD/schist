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

/// The bodies whose sRAW reconstruction has been checked sample for
/// sample against a reference frame, by CanonModelID.
///
/// The hue offset and the matrix in [`sraw_to_rgb`] are per generation,
/// and the trim in [`sraw_width`] is fitted, so a body that has not
/// been measured is refused outright: a frame that decodes to plausible
/// but wrong colour is the worst outcome, because nothing downstream
/// can tell. Of the bodies with samples to hand, the 5D Mark III wants
/// a matrix this module does not have. The 5D Mark II is closer: with
/// the other hue rule (`P << 1`) and the matrix below, both of its
/// samples reconstruct sample for sample *given* the right width — but
/// it trims to 3866 columns from a 3872-column frame whose last border
/// column is 3860, and to the full 2808 with borders 12..=2795, and no
/// rule over the numbers in the file produces either. Until the width
/// can be read rather than fitted, it stays refused.
const SRAW_VERIFIED_BODIES: &[(u32, &str)] = &[
    // Both modes, against the planar and full reference dumps.
    (0x8000_0261, "EOS 50D"),
    // Both modes, against the full reference dump.
    (0x8000_0250, "EOS 7D"),
];

/// Makernote 0x0010, the body as a number.
fn model_id(mn: &Ifd) -> Option<u32> {
    mn.get(canon::MODEL_ID)?.u32(0)
}

/// `Ok` for a body on [`SRAW_VERIFIED_BODIES`], the refusal otherwise.
fn sraw_verified_body(model_id: Option<u32>, model: &str) -> Result<()> {
    if model_id.is_some_and(|id| SRAW_VERIFIED_BODIES.iter().any(|(known, _)| *known == id)) {
        return Ok(());
    }
    let body = match (model.is_empty(), model_id) {
        (false, _) => model.to_string(),
        (true, Some(id)) => format!("model id {id:#x}"),
        (true, None) => "an unnamed body".to_string(),
    };
    Err(Error::Unsupported(format!(
        "Canon sRAW on {body}: colour reconstruction not verified for this body"
    )))
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
fn sraw(tiff: &Tiff<'_>, raw_ifd: &Ifd, stream: &[u8]) -> Result<RawImage> {
    let makernote = makernote(tiff);
    let (sensor, colour, model_id) = match &makernote {
        Some(mn) => {
            let (root, le) = (mn.root(), mn.little_endian());
            (sensor_info(root, le), color_data(root, le), model_id(root))
        }
        None => (None, None, None),
    };
    let (_, model) = tiff.make_model();
    // Before the entropy decode: refusing after it would only waste
    // the work.
    sraw_verified_body(model_id, &model)?;

    let sub = ljpeg::decode_subsampled(stream)?;
    let width = sraw_width(sensor.as_ref(), sub.width);
    let height = sub.height;
    let mut planes = sraw_planes(&sub, width, slices(raw_ifd, sub.row))?;
    sraw_upsample(&mut planes, width, height, sub.p);
    // Without the multipliers the three planes still reconstruct, just
    // on the matrix's own scale; unity is the honest fallback for a
    // ColorData layout this module has not measured, but it is a
    // visibly green picture, so it is not a silent one.
    let multipliers = match colour.as_ref().and_then(ColorData::sraw_multipliers) {
        Some(multipliers) => multipliers,
        None => {
            log::warn!(
                "cr2: {model}: ColorData version {} ({} words) holds no usable sRAW multipliers; \
                 reconstructing at unity gain",
                colour.as_ref().map_or(0, |c| c.version),
                colour.as_ref().map_or(0, ColorData::count)
            );
            [1024; 3]
        }
    };
    let data = sraw_to_rgb(&planes, sub.p, multipliers);

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
    raw.black_levels = [0.0; 4];
    raw.white_level = 16383.0;
    Ok(raw)
}

/// The width a subsampled frame is trimmed to.
///
/// The lossless JPEG codes the picture padded out to whole slices — the
/// 50D's mRAW writes 3344 luma columns for a 3272-column picture — and
/// no tag in the file states the real width outright. SensorInfo names
/// the picture's last column (3266 on that file), and rounding the
/// count of columns up to and including it to the next eight-column
/// boundary gives the width the picture is trimmed to. The frame keeps
/// its origin, so the left border plays no part: the trim is at the
/// right edge only.
///
/// This is the one quantity here fitted to the reference frames rather
/// than read from the file. It reproduces the 50D and 7D samples (3272,
/// 2376, 3888, 2592) and is known not to hold everywhere: the 5D Mark
/// II reconstructs to 3866 and 2808 columns, and no multiple-of-eight
/// rule over its borders (last column 3860; 12..=2795) gives either.
/// That body is refused by [`sraw_verified_body`] for this among other
/// reasons.
fn sraw_width(sensor: Option<&SensorInfo>, jpeg_width: usize) -> usize {
    let full = jpeg_width & !1;
    let Some(sensor) = sensor else { return full };
    match sensor
        .right
        .checked_add(1)
        .and_then(|a| a.checked_next_multiple_of(8))
    {
        Some(active) => (active.min(full)) & !1,
        None => full,
    }
}

/// Place the decoded MCUs, un-slicing as the Bayer path does.
///
/// Returns the three planes interleaved Y, Cb, Cr with the chroma
/// centred on zero and still present only at each block's anchor pixel
/// — the stage the `.sraw-planar` oracle captures, before any
/// interpolation or colour.
///
/// Canon cuts the frame into vertical strips exactly as it does a Bayer
/// one, and the same three-integer tag describes the cut; the only
/// difference is the unit. A slice is `first` samples of an entropy
/// row, and an MCU spends `components` samples on two columns, so a
/// slice spans `first * 2 / components` columns. Each strip is written
/// full height before the next begins, and the last is cut short where
/// the picture ends — the samples the encoder wrote past that are
/// padding and are stepped over, which is why the read position
/// advances by the slice's full width whatever the strip's width on
/// screen.
fn sraw_planes(sub: &ljpeg::SubsampledImage, width: usize, slices: [usize; 3]) -> Result<Vec<i32>> {
    let (height, components) = (sub.height, sub.components);
    let [count, first, last] = slices;
    // The identity the Bayer path holds the tag to: the slices together
    // are exactly one entropy row. It is checked before anything is
    // allocated because it ties the container's numbers to the frame
    // header's — a tag that lies about the cut, or a header forged to
    // claim a huge frame, fails here rather than getting its planes —
    // and because a count that does not add up is a count this loop
    // must not run to.
    let row = count
        .checked_mul(first)
        .and_then(|w| w.checked_add(last))
        .ok_or_else(|| Error::Corrupt("cr2: sRAW slice widths overflow".into()))?;
    if row != sub.row {
        return Err(Error::Corrupt(format!(
            "cr2: slices {count}x{first}+{last} make {row} samples a row, the sRAW frame has {}",
            sub.row
        )));
    }
    // A slice is whole MCUs, or its columns cannot be counted.
    if !first.is_multiple_of(components) || !last.is_multiple_of(components) {
        return Err(Error::Corrupt(format!(
            "cr2: sRAW slices of {first} and {last} samples are not whole {components}-sample MCUs"
        )));
    }
    // A zero first width is the single-slice spelling whatever the
    // count says (the identity above then makes `last` the whole row),
    // and must not be walked `count` times.
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
        for row in (0..height).step_by(sub.block_rows.max(1)) {
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

/// Y, Cb, Cr to the camera's RGB.
///
/// Canon shifts the chroma left two bits and adds a per-generation
/// *hue* offset before a fixed-point matrix over 2^14. The 50D and 7D
/// use `(P + 1) << 2` and the matrix below, and both are verified
/// against reference frames. Other bodies differ: the 5D Mark II wants
/// `P << 1` with this matrix, and the 5D Mark III a matrix this module
/// does not have — which is why [`sraw_verified_body`] admits only the
/// two. Each channel is then put on the sensor's scale by ColorData's
/// multipliers and clipped.
fn sraw_to_rgb(planes: &[i32], p: usize, multipliers: [i32; 3]) -> Vec<u16> {
    let hue = ((p as i64) + 1) << 2;
    let mut out = vec![0u16; planes.len()];
    let (pixels, _) = planes.as_chunks::<3>();
    let (out_pixels, _) = out.as_chunks_mut::<3>();
    for (pixel, rgb) in pixels.iter().zip(out_pixels.iter_mut()) {
        let y = pixel[0] as i64;
        let cb = ((pixel[1] as i64) << 2) + hue;
        let cr = ((pixel[2] as i64) << 2) + hue;
        // 64-bit throughout: the shifted chroma times the largest
        // coefficient is within a hair of overflowing 32 bits, and a
        // forged ColorData could make the final gain much larger.
        let camera = [
            y + ((50 * cb + 22929 * cr) >> 14),
            y + ((-5640 * cb - 11751 * cr) >> 14),
            y + ((29040 * cb - 101 * cr) >> 14),
        ];
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

    /// Where the sRAW channel multipliers start, by version.
    ///
    /// Like the black level, they move with the version stamp rather
    /// than the block length. Versions 6 (50D, 1250 words) and 7 (7D,
    /// 1337 words) are verified against reference frames; version 10
    /// (5D Mark III, 1312 words) has the quad's shape confirmed on a
    /// sample but no reconstruction to check it in; version 14 is
    /// documented, not measured. [`Self::sraw_multipliers`] checks the
    /// quad's shape, so a wrong line here reconstructs at unity with a
    /// warning rather than scaling by noise.
    fn sraw_mul_offset(&self) -> Option<usize> {
        let offset = match self.version {
            6 | 7 => 0x4E,
            10 => 0x7B,
            14 => 0x80,
            _ => return None,
        };
        (offset + 4 <= self.count()).then_some(offset)
    }

    /// The per-channel gains the sRAW reconstruction finishes with.
    ///
    /// Four words in sensor order R, Gr, Gb, B. The two greens are one
    /// number, so words 0, 1 and 3 are R, G and B, and every one is
    /// then multiplied by the largest of the four over 1024 and
    /// truncated.
    ///
    /// Despite sitting where a white balance would, these are not one:
    /// on the 50D they come out near 687, 1336, 855, leaving green a
    /// third above unity. They put the three reconstructed planes onto
    /// the sensor's own scale, which is why the frame still wants the
    /// as-shot multipliers applied on top of them.
    fn sraw_multipliers(&self) -> Option<[i32; 3]> {
        let at = self.sraw_mul_offset()?;
        let levels: Vec<u16> = (0..4).map_while(|i| self.short(at + i)).collect();
        let levels = <[u16; 4]>::try_from(levels).ok()?;
        // The quad's shape is the check that the offset is right for
        // this version: the two greens are equal — 1170 on every body
        // measured, 50D, 7D, 5D Mark II and 5D Mark III alike — and the
        // red and blue are positive numbers either side of them. The
        // as-shot white balance, which sits nearby in every layout,
        // has 1024 greens and fails this.
        let [r, g1, g2, b] = levels;
        if g1 != g2 || g1.abs_diff(1170) > 128 || r == 0 || b == 0 {
            return None;
        }
        let largest = *levels.iter().max()?;
        if largest == 0 {
            return None;
        }
        let scale = largest as f64 / 1024.0;
        // Clamped so that a forged block cannot make the final
        // multiply in `sraw_to_rgb` unbounded.
        let gain = |i: usize| ((levels[i] as f64 * scale) as i64).clamp(0, 1 << 20) as i32;
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

    #[test]
    fn sraw_width_rounds_the_sensor_borders_up() {
        let sensor = |left, right| SensorInfo {
            left,
            top: 0,
            right,
            bottom: 0,
            mask: None,
        };
        // The 50D's mRAW: 3267 columns of picture inside a frame the
        // encoder padded to 3344, trimmed to the next multiple of 8.
        assert_eq!(sraw_width(Some(&sensor(0, 3266)), 3344), 3272);
        // Its sRAW needs no trim: the borders already fill the frame.
        assert_eq!(sraw_width(Some(&sensor(0, 2375)), 2376), 2376);
        // The 7D's pair, likewise untrimmed.
        assert_eq!(sraw_width(Some(&sensor(0, 3887)), 3888), 3888);
        assert_eq!(sraw_width(Some(&sensor(0, 2591)), 2592), 2592);
        // The trim is at the right edge: a left border does not
        // narrow the frame, whose origin is still column 0.
        assert_eq!(sraw_width(Some(&sensor(12, 3266)), 3344), 3272);
        // The rounding never runs past what the JPEG actually codes.
        assert_eq!(sraw_width(Some(&sensor(0, 3340)), 3344), 3344);
        // No makernote at all: the whole coded frame, made even.
        assert_eq!(sraw_width(None, 3344), 3344);
        assert_eq!(sraw_width(None, 3345), 3344);
        // A border that overflows falls back rather than panicking.
        assert_eq!(sraw_width(Some(&sensor(0, usize::MAX)), 3344), 3344);
    }

    /// A subsampled frame built by hand, for `sraw_planes`.
    fn subsampled(p: usize, width: usize, height: usize, data: Vec<u16>) -> ljpeg::SubsampledImage {
        let components = 3 + p;
        let block_rows = p.div_ceil(2);
        let row = (width / 2) * components;
        let rows = height / block_rows;
        assert_eq!(data.len(), row * rows);
        ljpeg::SubsampledImage {
            width,
            height,
            p,
            components,
            block_rows,
            row,
            rows,
            precision: 15,
            data,
        }
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
        let planes = sraw_planes(&sub, 4, [1, 4, 4]).unwrap();
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
        let single = sraw_planes(&sub, 4, [0, 0, 8]).unwrap();
        assert_eq!(single, planes);
        assert_eq!(sraw_planes(&sub, 4, [usize::MAX, 0, 8]).unwrap(), planes);
        // Trimmed to two columns: the second MCU of each row is padding.
        let trimmed = sraw_planes(&sub, 2, [0, 0, 8]).unwrap();
        assert_eq!(trimmed.len(), 2 * 2 * 3);
        assert_eq!(trimmed[3], n(0, 1) as i32);
        assert_eq!(trimmed[6], n(1, 0) as i32);
    }

    #[test]
    fn sraw_planes_rejects_a_cut_that_does_not_add_up() {
        let sub = subsampled(1, 4, 2, vec![0; 16]);
        // Slices that do not make one entropy row, including the count
        // that used to spin for hours.
        for slices in [
            [2, 4, 4],
            [1, 4, 0],
            [usize::MAX, 4, 4],
            [usize::MAX, usize::MAX, 8],
        ] {
            assert!(
                matches!(sraw_planes(&sub, 4, slices), Err(Error::Corrupt(_))),
                "{slices:?}"
            );
        }
        // Slices that are not whole MCUs.
        assert!(matches!(
            sraw_planes(&sub, 4, [1, 3, 5]),
            Err(Error::Corrupt(_))
        ));
        // A frame short of samples is an error, not a read past the end.
        let short = subsampled(1, 4, 2, vec![0; 16]);
        let short = ljpeg::SubsampledImage {
            data: short.data[..8].to_vec(),
            ..short
        };
        assert!(matches!(
            sraw_planes(&short, 4, [0, 0, 8]),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn sraw_multipliers_come_from_the_version_offset() {
        let block = |version: i32, len: usize, at: usize, quad: [u16; 4]| {
            let mut values = vec![0u16; len];
            values[0] = version as u16;
            values[at..at + 4].copy_from_slice(&quad);
            ColorData { values, version }
        };
        // The 7D's block: version 7, gains at 0x4E, scaled by 1170/1024.
        let seven_d = block(7, 1337, 0x4E, [848, 1170, 1170, 490]);
        assert_eq!(seven_d.sraw_multipliers(), Some([968, 1336, 559]));
        // Version 10 moves them to 0x7B.
        let five_d3 = block(10, 1312, 0x7B, [836, 1170, 1170, 437]);
        assert_eq!(five_d3.sraw_multipliers(), Some([955, 1336, 499]));
        // A quad of the wrong shape — here the as-shot white balance,
        // whose greens are 1024 — means the offset is wrong for this
        // block, and unity is the answer.
        let wrong = block(7, 1337, 0x4E, [2166, 1024, 1024, 1524]);
        assert_eq!(wrong.sraw_multipliers(), None);
        let unequal = block(6, 1250, 0x4E, [602, 1170, 1171, 749]);
        assert_eq!(unequal.sraw_multipliers(), None);
        // A version this module has no offset for.
        let unknown = block(9, 1312, 0x7B, [836, 1170, 1170, 437]);
        assert_eq!(unknown.sraw_multipliers(), None);
    }

    #[test]
    fn sraw_bodies_are_gated_by_model_id() {
        assert!(sraw_verified_body(Some(0x8000_0261), "Canon EOS 50D").is_ok());
        assert!(sraw_verified_body(Some(0x8000_0250), "Canon EOS 7D").is_ok());
        for (id, model) in [
            (Some(0x8000_0218), "Canon EOS 5D Mark II"),
            (Some(0x8000_0285), "Canon EOS 5D Mark III"),
            (Some(0x8000_0285), ""),
            (None, "Canon EOS 60D"),
            (None, ""),
        ] {
            match sraw_verified_body(id, model) {
                Err(Error::Unsupported(why)) => {
                    assert!(
                        why.contains("sRAW") && why.contains("not verified"),
                        "{why}"
                    );
                    if !model.is_empty() {
                        assert!(why.contains(model), "{why}");
                    }
                }
                other => panic!("{model}: {other:?}"),
            }
        }
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
        assert_eq!(
            sraw_to_rgb(&planes, 3, [687, 1336, 855]),
            vec![368, 750, 512]
        );
        // Unity gains leave the matrix's own scale, and a neutral
        // pixel stays very nearly grey.
        let grey = vec![8000, 0, 0];
        let rgb = sraw_to_rgb(&grey, 3, [1024; 3]);
        assert!(
            rgb.iter().all(|v| v.abs_diff(8000) < 40),
            "a neutral pixel should stay grey, got {rgb:?}"
        );
        // Hostile values must clip, not wrap or panic.
        let extreme = vec![32767, 32767, -32768];
        let rgb = sraw_to_rgb(&extreme, 3, [1 << 20, 1 << 20, 1 << 20]);
        assert!(rgb.iter().all(|v| *v <= 32767));
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
    /// These are the only CR2s this decoder may refuse: a subsampled
    /// frame from a body whose colour reconstruction is not verified
    /// comes back `Unsupported` (see `SRAW_VERIFIED_BODIES`), and
    /// `corpus_sraw_matches_the_full_oracle_or_is_refused` checks that
    /// every subsampled sample is either exact or refused. The one
    /// other subsampled variant, sRAW inside a CR3 container, lives in
    /// `formats::cr3`, not here.
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
            let raw = match crate::decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) if is_subsampled(&bytes) => {
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

    /// The bodies whose subsampled frames must match their reference
    /// dumps sample for sample, by the TIFF model string. Every other
    /// body's subsampled file must be refused. This list is the test's
    /// own, so that the decoder's gate is checked against it rather
    /// than against itself.
    const SRAW_EXACT_MODELS: &[&str] = &["Canon EOS 50D", "Canon EOS 7D"];

    /// The shapes and worked values the specification records for the
    /// 50D pair, applied when a discovered file is one of them.
    ///
    /// Note which is which: 9517 is the *larger* picture — Canon's mRAW,
    /// or sRAW1 — and its luma is sampled 2x2 (factor byte 0x22, so
    /// P = 3); 9518 is the smaller sRAW (sRAW2), sampled 2x1 (0x21,
    /// P = 1). The written spec labels these the other way round in its
    /// prose; the factor byte in the file is what the arithmetic keys
    /// off, and the per-file worked values agree with the byte.
    struct Worked {
        name: &'static str,
        p: usize,
        width: usize,
        height: usize,
        /// Planar stage, in the sidecar's units: pixel 0's Y, Cb, Cr
        /// and the first four row-0 luma anchors.
        planar: (i32, i32, i32, [i32; 4]),
        /// Full stage: pixels 0, 1, 2 and the first of row 1.
        full: [(i32, i32, i32); 4],
    }

    const WORKED: &[Worked] = &[
        Worked {
            name: "EOS_50D-IMG_9517.CR2",
            p: 3,
            width: 3272,
            height: 2178,
            planar: (572, 8194, 8184, [572, 579, 565, 541]),
            full: [
                (368, 750, 512),
                (369, 752, 514),
                (373, 760, 512),
                (366, 725, 501),
            ],
        },
        Worked {
            name: "EOS_50D-IMG_9518.CR2",
            p: 1,
            width: 2376,
            height: 1584,
            planar: (584, 8192, 8188, [584, 571, 549, 522]),
            // The spec records only the first two pixels of this one.
            full: [(383, 764, 502), (374, 746, 497), (-1, -1, -1), (-1, -1, -1)],
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
    fn planar_stage(bytes: &[u8]) -> Result<(ljpeg::SubsampledImage, usize, Vec<i32>)> {
        let tiff = Tiff::parse(bytes)?;
        let raw_ifd = raw_ifd(&tiff).ok_or_else(|| Error::Corrupt("no raw IFD".into()))?;
        let stream = raw_stream(&tiff, raw_ifd)?;
        let sub = ljpeg::decode_subsampled(stream)?;
        let sensor = makernote(&tiff).and_then(|mn| {
            let (root, le) = (mn.root(), mn.little_endian());
            sensor_info(root, le)
        });
        let width = sraw_width(sensor.as_ref(), sub.width);
        let planes = sraw_planes(&sub, width, slices(raw_ifd, sub.row))?;
        Ok((sub, width, planes))
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
            let (sub, width, planes) =
                planar_stage(bytes).unwrap_or_else(|e| panic!("{name}: planar stage: {e}"));
            let elapsed = start.elapsed();
            let height = sub.height;
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
                let (y0, cb0, cr0, anchors) = worked.planar;
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
    /// for every subsampled sample in the corpus. A body on
    /// `SRAW_EXACT_MODELS` must match its sidecar sample for sample;
    /// any other body must be refused as unverified — a frame that
    /// decodes to plausible but wrong colour is the failure this
    /// guards against, so a decode on a gated body fails the test.
    #[test]
    fn corpus_sraw_matches_the_full_oracle_or_is_refused() {
        let Some(dir) = corpus() else { return };
        let files = subsampled_files(&dir);
        assert!(
            !files.is_empty(),
            "no subsampled CR2 under {}",
            dir.display()
        );
        let (mut exact, mut refused) = (0, 0);
        for (path, model, bytes) in &files {
            let name = file_name(path);
            let oracle = sidecar(path, "sraw-full.rgb16")
                .unwrap_or_else(|| panic!("{name}: no .sraw-full.rgb16 sidecar"));
            let start = std::time::Instant::now();
            let result = decode(bytes);
            let elapsed = start.elapsed();

            if !SRAW_EXACT_MODELS.contains(&model.as_str()) {
                match result {
                    Err(Error::Unsupported(why)) => {
                        assert!(
                            why.contains("sRAW") && why.contains("not verified"),
                            "{name} ({model}): refused for the wrong reason: {why}"
                        );
                        assert!(why.contains(model), "{name}: {why} does not name {model}");
                        // The picture is still there.
                        let jpeg = super::preview(bytes).unwrap().expect("preview");
                        image::load_from_memory(&jpeg)
                            .unwrap_or_else(|e| panic!("{name} preview: {e}"));
                        println!("{name} ({model}): refused as unverified: {why}");
                        refused += 1;
                    }
                    Ok(raw) => panic!(
                        "{name} ({model}): decoded {}x{} on a body whose colour reconstruction \
                         is not verified; it must be refused",
                        raw.width, raw.height
                    ),
                    Err(e) => panic!("{name} ({model}): {e}"),
                }
                continue;
            }

            let raw = result.unwrap_or_else(|e| panic!("{name} ({model}): {e}"));
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
                raw.wb_coeffs[0] > 1.5 && raw.wb_coeffs[1] == 1.0,
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

            if let Some(worked) = worked(&name) {
                assert_eq!(
                    (width, height),
                    (worked.width, worked.height),
                    "{name} shape"
                );
                // The worked first pixels the specification records.
                let px = |i: usize| {
                    (
                        got[i * 3] as i32,
                        got[i * 3 + 1] as i32,
                        got[i * 3 + 2] as i32,
                    )
                };
                for (i, want) in [0, 1, 2, width].into_iter().zip(worked.full) {
                    if want.0 >= 0 {
                        assert_eq!(px(i), want, "{name} pixel at index {i}");
                    }
                }
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
        }
        assert!(
            exact > 0,
            "no subsampled sample of a verified body under {}",
            dir.display()
        );
        println!("{exact} subsampled samples exact, {refused} refused as unverified bodies");
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
