//! Canon CRW: the CIFF container, and what can honestly be done with
//! what is inside it.
//!
//! CIFF is a heap of typed records read from the back. A 26-byte header
//! (`II`/`MM`, the header length, the signature `HEAPCCDR`, a version)
//! is followed by one big heap running to the end of the file; the last
//! four bytes of a heap give the offset of its directory, relative to
//! the heap's own start, and the directory is a count followed by
//! ten-byte entries of tag, length and offset. Offsets inside a heap
//! are relative to that heap, so a sub-heap is just the same structure
//! again with a new origin.
//!
//! The tag says how to read the value as well as what it means: bits
//! 11..13 are a type (ASCII, u16, u32, bytes, or a sub-heap) and bit 14
//! says the value is small enough to live in the entry's own eight
//! bytes instead of the heap. Everything a decoder wants is a record:
//! `0x080A` the make and model as two NUL-separated strings, `0x1810`
//! the picture's size and rotation, `0x1031` the sensor's size and the
//! borders of the picture within it, `0x10A9` the white balance table,
//! `0x2005` the sensor data, `0x2007` the full-size JPEG, `0x1835` the
//! index of the Huffman table set the sensor data was compressed with.
//!
//! # What is decoded
//!
//! The oldest bodies — the PowerShot Pro70 generation — store the
//! sensor uncompressed, ten bits a sample. Everything later — every
//! EOS CRW and every PowerShot from the G1 on — compresses, and that
//! codec is implemented here too; see [`decompress`].
//!
//! Its three Huffman table sets are firmware constants: record
//! `0x1835` carries only the *index* of the set a frame used, never
//! the tables. They are written out below as the DHT-style code-length
//! counts and symbol lists they are, and `set 0`'s assignment is
//! pinned by a unit test against the code words it must produce.
//!
//! [`preview`] works whatever the codec: the camera's own full-size
//! JPEG is a record like any other, and it is the whole picture.
//!
//! Clean-room: written from the public CIFF 1.0 specification and
//! third-party descriptions of it, ExifTool's tag documentation, a
//! functional description of the entropy coder, and measurement of the
//! sample files named in this module's tests.

use crate::bits::{BitPump, BitPumpJpeg, BitPumpMsb, HuffTable};
use crate::{Cfa, CfaColor, Error, Format, Metadata, Orientation, RawData, RawImage, Rect, Result};

/// Records this module reads. The low eleven bits are the identity;
/// the rest is type and storage, so the constants are whole tags.
mod tag {
    /// "Canon\0Canon EOS D60\0" — make and model in one ASCII record.
    pub const MAKE_MODEL: u16 = 0x080A;
    /// Focal length: record length, focal type, focal length, and the
    /// focal plane's size in the camera's own units.
    pub const FOCAL_LENGTH: u16 = 0x1029;
    /// The sensor frame and the picture's borders within it.
    pub const SENSOR_INFO: u16 = 0x1031;
    /// Which of the compressor's Huffman table sets the image used —
    /// present only on a compressed image, which makes it the marker.
    pub const DECODER_TABLE: u16 = 0x1835;
    /// White balance levels, several sets of four.
    pub const WHITE_BALANCE: u16 = 0x10A9;
    /// The picture's width, height, aspect and rotation.
    pub const IMAGE_INFO: u16 = 0x1810;
    /// The sensor data.
    pub const IMAGE_DATA: u16 = 0x2005;
    /// The camera's own full-size JPEG.
    pub const JPEG: u16 = 0x2007;
}

/// A record's type, from bits 11..13 of its tag.
const TYPE_HEAP1: u16 = 5;
const TYPE_HEAP2: u16 = 6;
/// Bit 14: the value is the entry's own last eight bytes.
const IN_RECORD: u16 = 0x4000;

/// Ceilings so a corrupt or hostile file cannot make the walk loop or
/// allocate. A real CRW has a few dozen records nested three deep.
const MAX_RECORDS: usize = 4096;
const MAX_DEPTH: usize = 6;

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let ciff = Ciff::parse(bytes)?;
    let (make, model) = ciff.make_model();

    let image = ciff
        .find(tag::IMAGE_DATA)
        .ok_or_else(|| Error::Corrupt("crw: no image record (0x2005)".into()))?;
    let sensor = ciff.sensor_info();
    let info = ciff.image_info();

    // Record 0x1835's presence *is* the compression flag; its first
    // field is the index of the Huffman table set the frame was coded
    // with. Nothing else in the file distinguishes the two layouts.
    let mut raw = if ciff.find(tag::DECODER_TABLE).is_some() {
        compressed(&ciff, image, sensor, &model)?
    } else {
        uncompressed(&ciff, image, sensor, info, &model)?
    };
    raw.set_camera(&make, &model);
    if let Some(info) = info {
        raw.orientation = info.orientation;
    }
    if let Some(wb) = ciff.as_shot_wb() {
        raw.wb_coeffs = wb;
    }
    raw.metadata = ciff.metadata();
    raw.preview = ciff.jpeg();
    raw.apply_camera_table();
    Ok(raw)
}

/// The ten-bits-a-sample layout of the Pro70 generation.
fn uncompressed(
    ciff: &Ciff<'_>,
    image: Record,
    sensor: Option<SensorInfo>,
    info: Option<ImageInfo>,
    model: &str,
) -> Result<RawImage> {
    // An uncompressed CIFF says its size in two places and neither is
    // the sensor frame: SensorInfo has it on the bodies that write one,
    // and on the bodies that do not (the Pro70 generation) only the
    // record's own length does — ten bits a sample over the picture's
    // number of rows.
    let (width, height) = match sensor {
        Some(sensor) => (sensor.width, sensor.height),
        None => {
            let height = info
                .map(|info| info.height)
                .filter(|h| *h > 0)
                .ok_or_else(|| Error::Corrupt("crw: no image dimensions".into()))?;
            let samples = image.len.checked_mul(8).unwrap_or(0) / 10;
            (samples / height, height)
        }
    };
    if width == 0 || height == 0 || image.len * 8 != width * height * 10 {
        return Err(Error::Unsupported(format!(
            "crw: {} bytes of uncompressed image for a {width}x{height} frame is \
             not the ten bits a sample this decoder knows",
            image.len
        )));
    }
    let cfa = uncompressed_cfa(model).ok_or_else(|| {
        Error::Unsupported(format!(
            "crw: uncompressed CIFF from {model}: this decoder does not know its filter array"
        ))
    })?;

    let data = unpack10(ciff.slice(image)?, width * height);
    let mut raw = RawImage::new(Format::Crw, width, height, 1, RawData::U16(data), cfa);
    // Ten bits a sample, and no record says the sensor saturates
    // earlier.
    raw.white_level = 1023.0;
    if let Some(sensor) = sensor {
        if let Some(crop) = sensor.crop(width, height) {
            raw.crop = crop;
        }
    }
    Ok(raw)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(Ciff::parse(bytes)?.jpeg())
}

/// The filter array of the bodies that wrote uncompressed CIFF.
///
/// Nothing in the file describes it, and the handful of cameras
/// concerned are all 1997–98 compacts with complementary-colour CCDs,
/// so it can only be recognised by name. The Pro70's array is the one
/// measured here — four rows of alternating pairs, yellow/cyan over
/// magenta/green, the pairs swapping every other row — against its
/// sample frame, where the four phases separate cleanly into two bright
/// and two dark.
///
/// `None` for anything else: a wrong filter array makes a picture that
/// looks decoded and is not, which is worse than refusing.
fn uncompressed_cfa(model: &str) -> Option<Cfa> {
    use CfaColor::{Cyan, Green, Magenta, Yellow};
    model.contains("Pro70").then(|| Cfa::Pattern {
        width: 2,
        height: 4,
        colors: vec![Yellow, Cyan, Magenta, Green, Cyan, Yellow, Green, Magenta],
    })
}

/// Ten-bit samples, most significant bit first, in little-endian
/// sixteen-bit words.
///
/// Canon wrote the frame as a big-endian bit stream and then stored it
/// as machine-order words, so every pair of bytes arrives swapped: the
/// bits of the first four samples of the Pro70's frame are the second
/// byte then the first, the fourth then the third. Undoing the swap
/// first leaves an ordinary MSB-first stream.
fn unpack10(bytes: &[u8], samples: usize) -> Vec<u16> {
    let mut swapped = Vec::with_capacity(bytes.len());
    for pair in bytes.chunks(2) {
        match pair {
            [a, b] => swapped.extend_from_slice(&[*b, *a]),
            rest => swapped.extend_from_slice(rest),
        }
    }
    let mut pump = BitPumpMsb::new(&swapped);
    (0..samples).map(|_| pump.get(10) as u16).collect()
}

// ----------------------------------------------- the compressed codec

/// Rows to a band. The codec decodes eight rows at a time because the
/// low-bit plane, when there is one, is interleaved a band at a time.
const BAND: usize = 8;
/// Samples to a Huffman block. Blocks run straight across row ends,
/// and `raw_width` is a multiple of eight but rarely of 64, so a row
/// boundary usually falls inside a block.
const BLOCK: usize = 64;
/// The implicit sample to the left of every row: the middle of the
/// ten-bit range, which is what both predictors are set to whenever a
/// row starts.
const EDGE_PREDICTOR: i32 = 512;
/// Bytes of zero padding between the low-bit plane and the start of
/// the coded stream. Verified all-zero on every sample; its purpose is
/// not known, and it is simply skipped.
const PAD: usize = 514;
/// How far into the file the low-bit-plane heuristic looks.
const PROBE_WINDOW: usize = 0x4000;

/// The geometry of one compressed frame: where the picture sits in the
/// sensor frame, and how far into the masked border the two black
/// rectangles are set.
///
/// None of this is in the file. The image record's own inset rectangle
/// (`0x1031` fields 5..8) describes a *smaller* picture than the one
/// the raw converter shows — 3072x2048 against 3088x2056 on the EOS
/// D60 — so the frame size is used as a key into [`GEOMETRY`] instead,
/// which is also what puts the black rectangles on the right columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    /// Insets of the left rectangle (fields 0 and 1) and of the right
    /// one (2 and 3) into the masked border. Zero everywhere but the
    /// EOS D60 / 10D / 300D frame, whose left rectangle starts sixteen
    /// columns further in.
    mask: [usize; 4],
}

/// `raw_width, raw_height, left, top, width shrink, height shrink,`
/// then the four black-rectangle insets. Keyed by the frame size
/// because a body writes exactly one.
const GEOMETRY: [[usize; 10]; 8] = [
    // PowerShot Pro90 IS
    [1944, 1416, 0, 0, 48, 0, 0, 0, 0, 0],
    // PowerShot S30, G1
    [2144, 1560, 4, 8, 52, 2, 0, 0, 0, 0],
    // EOS D30
    [2224, 1456, 48, 6, 0, 2, 0, 0, 0, 0],
    // PowerShot G2, S40, G3, S45
    [2376, 1728, 12, 6, 52, 2, 0, 0, 0, 0],
    // PowerShot G5, S50, S60
    [2672, 1968, 12, 6, 44, 2, 0, 0, 0, 0],
    // EOS D60, EOS 10D, EOS 300D
    [3152, 2068, 64, 12, 0, 0, 16, 0, 0, 0],
    // PowerShot G6, S70
    [3160, 2344, 44, 12, 4, 4, 0, 0, 0, 0],
    // PowerShot Pro1
    [3344, 2484, 4, 6, 52, 6, 0, 0, 0, 0],
];

fn geometry(width: usize, height: usize) -> Option<Geometry> {
    let row = GEOMETRY.iter().find(|r| r[0] == width && r[1] == height)?;
    Some(Geometry {
        left: row[2],
        top: row[3],
        width: width.checked_sub(row[2] + row[4])?,
        height: height.checked_sub(row[3] + row[5])?,
        mask: [row[6], row[7], row[8], row[9]],
    })
}

/// The frame size the `raw_width == 2672` low-bit correction applies
/// to; see [`merge_low_bits`].
const FUDGED_WIDTH: usize = 2672;

const TABLE_A_COUNTS: [[u8; 16]; 3] = [
    [0, 1, 4, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 2, 2, 3, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 6, 3, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];
const TABLE_A_SYMBOLS: [[u8; 13]; 3] = [
    [
        0x04, 0x03, 0x05, 0x06, 0x02, 0x07, 0x01, 0x08, 0x09, 0x00, 0x0A, 0x0B, 0xFF,
    ],
    [
        0x03, 0x02, 0x04, 0x01, 0x05, 0x00, 0x06, 0x07, 0x09, 0x08, 0x0A, 0x0B, 0xFF,
    ],
    [
        0x06, 0x05, 0x07, 0x04, 0x08, 0x03, 0x09, 0x02, 0x00, 0x0A, 0x01, 0x0B, 0xFF,
    ],
];
const TABLE_B_COUNTS: [[u8; 16]; 3] = [
    [0, 2, 2, 2, 1, 4, 2, 1, 2, 5, 1, 1, 0, 0, 0, 139],
    [0, 2, 2, 1, 4, 1, 4, 1, 3, 3, 1, 0, 0, 0, 0, 140],
    [0, 0, 6, 2, 1, 3, 3, 2, 5, 1, 2, 2, 8, 10, 0, 117],
];
const TABLE_B_SYMBOLS: [[u8; 162]; 3] = [
    [
        0x03, 0x04, 0x02, 0x05, 0x01, 0x06, 0x07, 0x08, 0x12, 0x13, 0x11, 0x14, 0x09, 0x15, 0x22,
        0x00, 0x21, 0x16, 0x0A, 0xF0, 0x23, 0x17, 0x24, 0x31, 0x32, 0x18, 0x19, 0x33, 0x25, 0x41,
        0x34, 0x42, 0x35, 0x51, 0x36, 0x37, 0x38, 0x29, 0x79, 0x26, 0x1A, 0x39, 0x56, 0x57, 0x28,
        0x27, 0x52, 0x55, 0x58, 0x43, 0x76, 0x59, 0x77, 0x54, 0x61, 0xF9, 0x71, 0x78, 0x75, 0x96,
        0x97, 0x49, 0xB7, 0x53, 0xD7, 0x74, 0xB6, 0x98, 0x47, 0x48, 0x95, 0x69, 0x99, 0x91, 0xFA,
        0xB8, 0x68, 0xB5, 0xB9, 0xD6, 0xF7, 0xD8, 0x67, 0x46, 0x45, 0x94, 0x89, 0xF8, 0x81, 0xD5,
        0xF6, 0xB4, 0x88, 0xB1, 0x2A, 0x44, 0x72, 0xD9, 0x87, 0x66, 0xD4, 0xF5, 0x3A, 0xA7, 0x73,
        0xA9, 0xA8, 0x86, 0x62, 0xC7, 0x65, 0xC8, 0xC9, 0xA1, 0xF4, 0xD1, 0xE9, 0x5A, 0x92, 0x85,
        0xA6, 0xE7, 0x93, 0xE8, 0xC1, 0xC6, 0x7A, 0x64, 0xE1, 0x4A, 0x6A, 0xE6, 0xB3, 0xF1, 0xD3,
        0xA5, 0x8A, 0xB2, 0x9A, 0xBA, 0x84, 0xA4, 0x63, 0xE5, 0xC5, 0xF3, 0xD2, 0xC4, 0x82, 0xAA,
        0xDA, 0xE4, 0xF2, 0xCA, 0x83, 0xA3, 0xA2, 0xC3, 0xEA, 0xC2, 0xE2, 0xE3,
    ],
    [
        0x02, 0x03, 0x01, 0x04, 0x05, 0x12, 0x11, 0x06, 0x13, 0x07, 0x08, 0x14, 0x22, 0x09, 0x21,
        0x00, 0x23, 0x15, 0x31, 0x32, 0x0A, 0x16, 0xF0, 0x24, 0x33, 0x41, 0x42, 0x19, 0x17, 0x25,
        0x18, 0x51, 0x34, 0x43, 0x52, 0x29, 0x35, 0x61, 0x39, 0x71, 0x62, 0x36, 0x53, 0x26, 0x38,
        0x1A, 0x37, 0x81, 0x27, 0x91, 0x79, 0x55, 0x45, 0x28, 0x72, 0x59, 0xA1, 0xB1, 0x44, 0x69,
        0x54, 0x58, 0xD1, 0xFA, 0x57, 0xE1, 0xF1, 0xB9, 0x49, 0x47, 0x63, 0x6A, 0xF9, 0x56, 0x46,
        0xA8, 0x2A, 0x4A, 0x78, 0x99, 0x3A, 0x75, 0x74, 0x86, 0x65, 0xC1, 0x76, 0xB6, 0x96, 0xD6,
        0x89, 0x85, 0xC9, 0xF5, 0x95, 0xB4, 0xC7, 0xF7, 0x8A, 0x97, 0xB8, 0x73, 0xB7, 0xD8, 0xD9,
        0x87, 0xA7, 0x7A, 0x48, 0x82, 0x84, 0xEA, 0xF4, 0xA6, 0xC5, 0x5A, 0x94, 0xA4, 0xC6, 0x92,
        0xC3, 0x68, 0xB5, 0xC8, 0xE4, 0xE5, 0xE6, 0xE9, 0xA2, 0xA3, 0xE3, 0xC2, 0x66, 0x67, 0x93,
        0xAA, 0xD4, 0xD5, 0xE7, 0xF8, 0x88, 0x9A, 0xD7, 0x77, 0xC4, 0x64, 0xE2, 0x98, 0xA5, 0xCA,
        0xDA, 0xE8, 0xF3, 0xF6, 0xA9, 0xB2, 0xB3, 0xF2, 0xD2, 0x83, 0xBA, 0xD3,
    ],
    [
        0x04, 0x05, 0x03, 0x06, 0x02, 0x07, 0x01, 0x08, 0x09, 0x12, 0x13, 0x14, 0x11, 0x15, 0x0A,
        0x16, 0x17, 0xF0, 0x00, 0x22, 0x21, 0x18, 0x23, 0x19, 0x24, 0x32, 0x31, 0x25, 0x33, 0x38,
        0x37, 0x34, 0x35, 0x36, 0x39, 0x79, 0x57, 0x58, 0x59, 0x28, 0x56, 0x78, 0x27, 0x41, 0x29,
        0x77, 0x26, 0x42, 0x76, 0x99, 0x1A, 0x55, 0x98, 0x97, 0xF9, 0x48, 0x54, 0x96, 0x89, 0x47,
        0xB7, 0x49, 0xFA, 0x75, 0x68, 0xB6, 0x67, 0x69, 0xB9, 0xB8, 0xD8, 0x52, 0xD7, 0x88, 0xB5,
        0x74, 0x51, 0x46, 0xD9, 0xF8, 0x3A, 0xD6, 0x87, 0x45, 0x7A, 0x95, 0xD5, 0xF6, 0x86, 0xB4,
        0xA9, 0x94, 0x53, 0x2A, 0xA8, 0x43, 0xF5, 0xF7, 0xD4, 0x66, 0xA7, 0x5A, 0x44, 0x8A, 0xC9,
        0xE8, 0xC8, 0xE7, 0x9A, 0x6A, 0x73, 0x4A, 0x61, 0xC7, 0xF4, 0xC6, 0x65, 0xE9, 0x72, 0xE6,
        0x71, 0x91, 0x93, 0xA6, 0xDA, 0x92, 0x85, 0x62, 0xF3, 0xC5, 0xB2, 0xA4, 0x84, 0xBA, 0x64,
        0xA5, 0xB3, 0xD2, 0x81, 0xE5, 0xD3, 0xAA, 0xC4, 0xCA, 0xF2, 0xB1, 0xE4, 0xD1, 0x83, 0x63,
        0xEA, 0xC3, 0xE2, 0x82, 0xF1, 0xA3, 0xC2, 0xA1, 0xC1, 0xE3, 0xA2, 0xE1,
    ],
];

/// Whether the image record carries a low-bit plane, and with it
/// whether the frame is twelve bits a sample rather than ten.
///
/// Nothing in the file says. What decides it is that the coded stream
/// is byte-stuffed — inside it every `0xFF` is followed by `0x00` —
/// while a low-bit plane is two raw bits a pixel and obeys no such
/// rule. So look at the window the coded stream would occupy if there
/// were no plane: a `0xFF` followed by anything but `0x00` there means
/// the window is really plane data, so the plane exists. A window with
/// no `0xFF` in it at all decides nothing, and a plane is assumed,
/// which is the behaviour every other decoder of this format has and
/// therefore what the reference frames were produced with.
fn has_low_bit_plane(bytes: &[u8], stream_start: usize) -> bool {
    let end = bytes.len().min(PROBE_WINDOW);
    let Some(window) = bytes.get(stream_start..end) else {
        return true;
    };
    let mut seen = false;
    for pair in window.windows(2) {
        if pair[0] == 0xFF {
            seen = true;
            if pair[1] != 0 {
                return true;
            }
        }
    }
    !seen
}

/// The compressed layout: a Huffman-coded stream of ten-bit samples,
/// optionally over a plane of two extra low bits each.
fn compressed(
    ciff: &Ciff<'_>,
    image: Record,
    sensor: Option<SensorInfo>,
    model: &str,
) -> Result<RawImage> {
    let sensor = sensor.ok_or_else(|| {
        Error::Corrupt("crw: compressed image with no sensor record (0x1031)".into())
    })?;
    let (width, height) = (sensor.width, sensor.height);
    let samples = crate::frame_samples(width, height, 1)?;
    let geometry = geometry(width, height).ok_or_else(|| {
        Error::Unsupported(format!(
            "crw: compressed {width}x{height} frame from {model}: no body known to \
             this decoder writes that size, so its margins are unknown"
        ))
    })?;

    // Every offset below is relative to the image record rather than
    // to the file. A camera puts the record at file offset 26 and
    // nothing else has ever been seen, but the record knows where it
    // is and the file's own header length says so too.
    let plane_len = samples / 4;
    let low = has_low_bit_plane(ciff.bytes, image.at + PAD).then_some(plane_len);
    let coded_at = image.at + low.unwrap_or(0) + PAD;
    let end = image.at + image.len;
    if coded_at >= end || end > ciff.bytes.len() {
        return Err(Error::Corrupt(format!(
            "crw: image record holds {} bytes, too few for a {width}x{height} frame",
            image.len
        )));
    }
    // A coded stream cannot be shorter than a bit a sample, so the
    // record's own length bounds the frame a forged header may claim.
    if (end - coded_at).saturating_mul(8) < samples {
        return Err(Error::Corrupt(format!(
            "crw: {} bytes of coded data for {samples} samples",
            end - coded_at
        )));
    }
    let set = ciff
        .longs(tag::DECODER_TABLE)
        .first()
        .map_or(0, |v| *v as usize)
        .min(TABLE_A_COUNTS.len() - 1);

    let mut frame = decompress(&ciff.bytes[coded_at..end], set, width, height)?;
    if let Some(plane_len) = low {
        let plane = ciff
            .bytes
            .get(image.at..image.at + plane_len)
            .ok_or_else(|| Error::Corrupt("crw: low-bit plane outside the file".into()))?;
        merge_low_bits(&mut frame, plane, width, height);
    }

    // The filter array is RGGB over the raw frame on every body that
    // wrote this codec; the margins are all even, so the picture's own
    // origin reads RGGB too.
    let mut raw = RawImage::new(
        Format::Crw,
        width,
        height,
        1,
        RawData::U16(frame),
        Cfa::RGGB,
    );
    raw.crop = Rect {
        x: geometry.left,
        y: geometry.top,
        width: geometry.width,
        height: geometry.height,
    };
    // Ten bits a sample without a plane, twelve with; nothing in the
    // file records a saturation point, so the depth is the ceiling.
    raw.white_level = if low.is_some() { 4095.0 } else { 1023.0 };
    if let Some(black) = black_level(&raw.data, width, &geometry) {
        if black.iter().all(|b| *b < raw.white_level) {
            raw.black_levels = black;
        }
    }
    Ok(raw)
}

/// The entropy-coded frame, ten bits a sample.
///
/// The stream is one continuous MSB-first bit sequence with JPEG's
/// byte stuffing (`FF 00` is a data `FF`; `FF` followed by anything
/// else ends it), never realigned at a band, row or block boundary.
///
/// It carries 64-sample blocks of signed differences coded exactly as
/// JPEG codes AC coefficients: a symbol packing a run of zeros in its
/// high nibble and a magnitude class in its low one, then that many
/// bits of value. Two things are peculiar to Canon. Difference 0 of a
/// block is *second order* — a difference against the previous block's
/// difference 0, through a carry that runs unbroken for the whole
/// frame — and the reconstruction keeps two predictors, one per Bayer
/// column parity, both reset to [`EDGE_PREDICTOR`] at the start of
/// every row. Because blocks straddle row ends, that reset lands in
/// the middle of a block more often than not.
fn decompress(coded: &[u8], set: usize, width: usize, height: usize) -> Result<Vec<u16>> {
    // Table A codes the first coefficient of a block, table B the
    // other 63. Both are firmware constants indexed by record 0x1835.
    let first = HuffTable::new(&TABLE_A_COUNTS[set], &TABLE_A_SYMBOLS[set])?;
    let rest = HuffTable::new(&TABLE_B_COUNTS[set], &TABLE_B_SYMBOLS[set])?;
    let mut out = vec![0u16; width * height];
    let mut pump = BitPumpJpeg::new(coded);
    let mut carry = 0i32;
    let mut predictor = [0i32; 2];
    let mut at = 0usize;

    for row in (0..height).step_by(BAND) {
        let rows = BAND.min(height - row);
        // Whole blocks only: a band whose sample count is not a
        // multiple of 64 simply leaves its tail uncoded.
        for _ in 0..rows * width / BLOCK {
            let mut difference = [0i32; BLOCK];
            let mut i = 0usize;
            while i < BLOCK {
                let symbol = if i == 0 {
                    first.decode(&mut pump)
                } else {
                    rest.decode(&mut pump)
                };
                // Zero ends the block, except as the first
                // coefficient, where it is the magnitude class 0.
                if symbol == 0 && i > 0 {
                    break;
                }
                // 0xFF skips one position. Its nominal run field is
                // fifteen, but the format does not use it that way.
                if symbol == 0xFF {
                    i += 1;
                    continue;
                }
                let run = (symbol >> 4) as usize;
                let size = (symbol & 15) as u32;
                i += run;
                if size == 0 {
                    i += 1;
                    continue;
                }
                let value = pump.get(size) as i32;
                // The lossless-JPEG sign convention: the top half of
                // the range is positive, the bottom half negative.
                let signed = if value & (1 << (size - 1)) != 0 {
                    value
                } else {
                    value - (1 << size) + 1
                };
                // A run can push the index past the end of the block.
                // Those differences are dropped, but their bits have
                // been consumed and the stream stays in step.
                if let Some(slot) = difference.get_mut(i) {
                    *slot = signed;
                }
                i += 1;
            }
            difference[0] += carry;
            carry = difference[0];

            for (i, d) in difference.iter().enumerate() {
                if at.is_multiple_of(width) {
                    predictor = [EDGE_PREDICTOR; 2];
                }
                let side = i & 1;
                // The carry is never reset, so a crafted stream can
                // drive the running value without bound; saturating
                // keeps a data error a data error rather than a panic.
                predictor[side] = predictor[side].saturating_add(*d);
                if let Some(sample) = out.get_mut(at) {
                    // A sample outside ten bits is a data error the
                    // format has no way to signal; it is stored as it
                    // falls rather than clamped, which is what the
                    // reference frames show.
                    *sample = predictor[side] as u16;
                }
                at += 1;
            }
        }
    }
    Ok(out)
}

/// Fold the two extra bits a sample of a twelve-bit body's low-bit
/// plane into the ten-bit frame.
///
/// The plane is raw two-bit fields, four to a byte, **least
/// significant pair first**, in raster order over the whole frame; it
/// is read straight out of the record and never touches the Huffman
/// reader's position.
fn merge_low_bits(frame: &mut [u16], plane: &[u8], width: usize, height: usize) {
    for row in (0..height).step_by(BAND) {
        // Each band of eight rows consumes `width * 2` bytes, which is
        // the same as stepping the plane by `row * width / 4`.
        let from = row * width / 4;
        let bytes = plane.get(from..).unwrap_or(&[]);
        let base = row * width;
        let count = (BAND * width).min(frame.len() - base);
        for (i, sample) in frame[base..base + count].iter_mut().enumerate() {
            let low = (bytes.get(i / 4).copied().unwrap_or(0) >> ((i % 4) * 2)) & 3;
            let mut value = ((*sample as u32) << 2) | low as u32;
            // Unexplained, and present in every decoder of this
            // format: on the 2672-wide bodies only, a merged value
            // under 512 is raised by two. Reproduced because the
            // reference frames carry it.
            if width == FUDGED_WIDTH && value < 512 {
                value += 2;
            }
            *sample = value as u16;
        }
    }
}

/// The black level, averaged over the sensor's optically black columns.
///
/// No record carries it. Two rectangles are used, both spanning the
/// picture's rows: one in the masked border to the left of the
/// picture, one in whatever is left to its right. Each CFA position is
/// averaged separately.
///
/// The result is rejected — leaving the black level at zero — when a
/// position collected no samples, or when the regions hold as many
/// exactly-zero samples as the first position has samples at all. That
/// second test is what makes the EOS D60's all-zero leading rows
/// harmless: a frame whose masked border is mostly zeros has no black
/// level worth measuring.
fn black_level(data: &RawData, width: usize, geometry: &Geometry) -> Option<[f32; 4]> {
    let RawData::U16(frame) = data else {
        return None;
    };
    let left = (2 + geometry.mask[0])..geometry.left.saturating_sub(2 + geometry.mask[1]);
    let right = (geometry.left + geometry.width + 2 + geometry.mask[2])
        ..width.saturating_sub(geometry.mask[3]);
    let mut sum = [0u64; 4];
    let mut count = [0usize; 4];
    let mut zeros = 0usize;
    for row in geometry.top..geometry.top + geometry.height {
        for columns in [left.clone(), right.clone()] {
            for col in columns {
                if col >= width {
                    break;
                }
                let Some(sample) = frame.get(row * width + col) else {
                    continue;
                };
                let position = (row & 1) * 2 + (col & 1);
                sum[position] += *sample as u64;
                count[position] += 1;
                zeros += usize::from(*sample == 0);
            }
        }
    }
    if count.contains(&0) || zeros >= count[0] {
        return None;
    }
    Some(std::array::from_fn(|i| sum[i] as f32 / count[i] as f32))
}

// ----------------------------------------------------------------- CIFF

/// One record, resolved to a position in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    tag: u16,
    /// Where the value's bytes are, absolute.
    at: usize,
    len: usize,
}

/// A parsed CIFF: every record in the file, in the order the heaps put
/// them, with the byte order the header names.
struct Ciff<'a> {
    bytes: &'a [u8],
    little_endian: bool,
    records: Vec<Record>,
}

impl<'a> Ciff<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Ciff<'a>> {
        if bytes.get(6..14) != Some(b"HEAPCCDR") {
            return Err(Error::Corrupt("crw: not a HEAPCCDR file".into()));
        }
        let little_endian = match bytes.get(0..2) {
            Some(b"II") => true,
            Some(b"MM") => false,
            _ => return Err(Error::Corrupt("crw: no byte order mark".into())),
        };
        let mut ciff = Ciff {
            bytes,
            little_endian,
            records: Vec::new(),
        };
        // The header length is where the root heap begins; it has been
        // 26 on every file ever written, but the field is there to be
        // read.
        let start = ciff.u32(2).unwrap_or(0) as usize;
        let len = bytes.len().saturating_sub(start);
        ciff.walk(start, len, 0);
        if ciff.records.is_empty() {
            return Err(Error::Corrupt("crw: no readable records".into()));
        }
        Ok(ciff)
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    /// Read one heap's directory and every sub-heap under it.
    ///
    /// A heap keeps its directory at the end and points at it with the
    /// last four bytes, so the walk is back to front; a record that
    /// does not lie inside its own heap is dropped rather than
    /// followed, which is what keeps a truncated file harmless.
    fn walk(&mut self, base: usize, len: usize, depth: usize) {
        if depth > MAX_DEPTH || len < 4 || self.records.len() >= MAX_RECORDS {
            return;
        }
        let Some(end) = base.checked_add(len) else {
            return;
        };
        if end > self.bytes.len() {
            return;
        }
        let Some(directory) = self
            .u32(end - 4)
            .map(|d| d as usize)
            .and_then(|d| base.checked_add(d))
        else {
            return;
        };
        let Some(count) = self.u16(directory) else {
            return;
        };
        for i in 0..count as usize {
            if self.records.len() >= MAX_RECORDS {
                return;
            }
            let entry = match directory.checked_add(2 + i * 10) {
                Some(entry) if entry + 10 <= end => entry,
                _ => return,
            };
            let (Some(tag), Some(length), Some(offset)) =
                (self.u16(entry), self.u32(entry + 2), self.u32(entry + 6))
            else {
                return;
            };
            let (at, length) = if tag & IN_RECORD != 0 {
                // The value replaces the length and offset fields.
                (entry + 2, 8)
            } else {
                let at = match base.checked_add(offset as usize) {
                    Some(at) => at,
                    None => continue,
                };
                let length = length as usize;
                if at
                    .checked_add(length)
                    .is_none_or(|value_end| value_end > end)
                {
                    continue;
                }
                (at, length)
            };
            self.records.push(Record {
                tag,
                at,
                len: length,
            });
            if matches!((tag >> 11) & 7, TYPE_HEAP1 | TYPE_HEAP2) && tag & IN_RECORD == 0 {
                self.walk(at, length, depth + 1);
            }
        }
    }

    fn find(&self, tag: u16) -> Option<Record> {
        self.records.iter().copied().find(|r| r.tag == tag)
    }

    fn slice(&self, record: Record) -> Result<&'a [u8]> {
        self.bytes
            .get(record.at..record.at + record.len)
            .ok_or_else(|| {
                Error::Corrupt(format!(
                    "crw: record {:04x} lies outside the file",
                    record.tag
                ))
            })
    }

    /// A record's u16 elements. Canon prefixes these arrays with their
    /// own length in bytes, which is left in place: every documented
    /// index into them counts from the length word.
    fn shorts(&self, tag: u16) -> Vec<u16> {
        let Some(record) = self.find(tag) else {
            return Vec::new();
        };
        (0..record.len / 2)
            .map_while(|i| self.u16(record.at + i * 2))
            .collect()
    }

    fn longs(&self, tag: u16) -> Vec<u32> {
        let Some(record) = self.find(tag) else {
            return Vec::new();
        };
        (0..record.len / 4)
            .map_while(|i| self.u32(record.at + i * 4))
            .collect()
    }

    /// `0x080A`: the make, a NUL, the model, a NUL, padding.
    fn make_model(&self) -> (String, String) {
        let Some(record) = self.find(tag::MAKE_MODEL) else {
            return (String::new(), String::new());
        };
        let Ok(text) = self.slice(record) else {
            return (String::new(), String::new());
        };
        let mut parts = text.split(|b| *b == 0).map(|part| {
            String::from_utf8_lossy(part)
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .trim()
                .to_string()
        });
        let make = parts.next().unwrap_or_default();
        let model = parts.next().unwrap_or_default();
        (make, model)
    }

    fn sensor_info(&self) -> Option<SensorInfo> {
        let values = self.shorts(tag::SENSOR_INFO);
        let at = |i: usize| values.get(i).map(|v| *v as usize);
        let info = SensorInfo {
            width: at(1)?,
            height: at(2)?,
            left: at(5)?,
            top: at(6)?,
            right: at(7)?,
            bottom: at(8)?,
        };
        (info.width > 0 && info.height > 0).then_some(info)
    }

    fn image_info(&self) -> Option<ImageInfo> {
        let values = self.longs(tag::IMAGE_INFO);
        Some(ImageInfo {
            height: *values.get(1)? as usize,
            // The rotation is in degrees clockwise, and negative
            // quarter turns appear as 270 and as -90 both.
            orientation: match (*values.get(3)? as i32).rem_euclid(360) {
                90 => Orientation::Rotate90CW,
                180 => Orientation::Rotate180,
                270 => Orientation::Rotate270CW,
                _ => Orientation::Normal,
            },
        })
    }

    /// The as-shot white balance, R G B G2 with green at 1.
    ///
    /// `0x10A9` is a run of four-level groups in Canon's RGGB order
    /// after the length word, the first of which is the balance the
    /// shot was taken at — checked against the EOS D60 sample, whose
    /// first group is exactly the multipliers LibRaw reports for it.
    fn as_shot_wb(&self) -> Option<[f32; 4]> {
        let values = self.shorts(tag::WHITE_BALANCE);
        let levels: Vec<u16> = values.get(1..5)?.to_vec();
        let [r, g1, g2, b] = <[u16; 4]>::try_from(levels).ok()?;
        if r == 0 || g1 == 0 || b == 0 {
            return None;
        }
        let g = g1 as f32;
        Some([r as f32 / g, 1.0, b as f32 / g, g2 as f32 / g])
    }

    /// `0x2007`, the camera's own full-size JPEG.
    fn jpeg(&self) -> Option<Vec<u8>> {
        let record = self.find(tag::JPEG)?;
        let stream = self.slice(record).ok()?;
        stream.starts_with(&[0xff, 0xd8]).then(|| stream.to_vec())
    }

    /// What little of the shot CIFF records in a form worth carrying:
    /// the focal length, in millimetres, from `0x1029`.
    ///
    /// The rest of a CRW's shooting data lives in `0x102A`/`0x102D` as
    /// arrays of camera-specific codes rather than physical units, and
    /// is left to a metadata reader that has ExifTool's tables.
    fn metadata(&self) -> Metadata {
        let focal = self.shorts(tag::FOCAL_LENGTH);
        Metadata {
            focal_length: focal.get(2).map(|v| *v as f32).filter(|v| *v > 0.0),
            ..Metadata::default()
        }
    }
}

/// `0x1031`: the sensor frame, and the picture's inclusive borders in
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SensorInfo {
    width: usize,
    height: usize,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
}

impl SensorInfo {
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

/// `0x1810`: what the camera would have made of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageInfo {
    height: usize,
    orientation: Orientation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ------------------------------------------------------- mechanics

    /// A CIFF around one heap of records, built by hand so the tests
    /// exercise the bytes a camera writes.
    fn ciff(records: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut heap = Vec::new();
        let mut directory = Vec::new();
        directory.extend_from_slice(&(records.len() as u16).to_le_bytes());
        for (tag, value) in records {
            directory.extend_from_slice(&tag.to_le_bytes());
            if tag & IN_RECORD != 0 {
                // The value is the entry's own eight bytes.
                let mut fixed = value.clone();
                fixed.resize(8, 0);
                directory.extend_from_slice(&fixed);
            } else {
                directory.extend_from_slice(&(value.len() as u32).to_le_bytes());
                directory.extend_from_slice(&(heap.len() as u32).to_le_bytes());
                heap.extend_from_slice(value);
            }
        }
        let at = heap.len() as u32;
        heap.extend_from_slice(&directory);
        heap.extend_from_slice(&at.to_le_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&26u32.to_le_bytes());
        out.extend_from_slice(b"HEAPCCDR");
        out.extend_from_slice(&0x0001_0002u32.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&heap);
        out
    }

    #[test]
    fn walks_a_heap_and_its_records() {
        let file = ciff(&[
            (
                tag::MAKE_MODEL,
                b"Canon\0Canon PowerShot Pro70\0\0\0\0\0".to_vec(),
            ),
            (tag::IMAGE_INFO, {
                let mut v = Vec::new();
                for word in [1536u32, 1024, 0x3f80_0000, 180, 8, 24, 257] {
                    v.extend_from_slice(&word.to_le_bytes());
                }
                v
            }),
        ]);
        let ciff = Ciff::parse(&file).unwrap();
        assert_eq!(
            ciff.make_model(),
            ("Canon".to_string(), "Canon PowerShot Pro70".to_string())
        );
        let info = ciff.image_info().unwrap();
        assert_eq!(info.height, 1024);
        assert_eq!(info.orientation, Orientation::Rotate180);
    }

    #[test]
    fn reads_a_value_stored_in_its_own_entry() {
        // Bit 14 says the eight bytes of length and offset *are* the
        // value; a walker that followed them as an offset would read
        // somewhere else entirely.
        let file = ciff(&[(0x5814, 1234u32.to_le_bytes().to_vec())]);
        let ciff = Ciff::parse(&file).unwrap();
        let record = ciff.find(0x5814).unwrap();
        assert_eq!(record.len, 8);
        assert_eq!(ciff.u32(record.at), Some(1234));
    }

    #[test]
    fn unpack10_undoes_the_word_swap() {
        // The first bytes of the Pro70 sample, whose first three
        // samples LibRaw unpacks as 496, 541 and 485.
        assert_eq!(unpack10(&[0x21, 0x7c, 0x96, 0xd7], 3), vec![496, 541, 485]);
        // Past the end the pump gives zeros rather than panicking.
        assert_eq!(unpack10(&[0x21], 2), vec![0x84, 0]);
    }

    // ---------------------------------------- the compressed codec

    /// The canonical code of one symbol in a DHT-style table, as
    /// `(code, length)`: start at 0 for the shortest length, count up
    /// within a length, shift left when the length grows.
    fn canonical(counts: &[u8; 16], symbols: &[u8], want: u8) -> Option<(u32, u32)> {
        let (mut code, mut index) = (0u32, 0usize);
        for length in 1..=16u32 {
            for k in 0..counts[length as usize - 1] as u32 {
                if symbols[index + k as usize] == want {
                    return Some((code + k, length));
                }
            }
            index += counts[length as usize - 1] as usize;
            code = (code + counts[length as usize - 1] as u32) << 1;
        }
        None
    }

    /// Bits MSB-first into whole bytes, with the byte stuffing the
    /// codec's reader expects: a data `0xFF` is written `FF 00`.
    #[derive(Default)]
    struct Stuffed {
        out: Vec<u8>,
        cache: u32,
        bits: u32,
    }

    impl Stuffed {
        fn put(&mut self, value: u32, bits: u32) {
            self.cache = (self.cache << bits) | (value & ((1u64 << bits) - 1) as u32);
            self.bits += bits;
            while self.bits >= 8 {
                self.bits -= 8;
                let byte = (self.cache >> self.bits) as u8;
                self.out.push(byte);
                if byte == 0xFF {
                    self.out.push(0);
                }
            }
        }
        /// One Huffman symbol from the named table of set 0.
        fn symbol(&mut self, first: bool, symbol: u8) {
            let (counts, symbols): (&[u8; 16], &[u8]) = if first {
                (&TABLE_A_COUNTS[0], &TABLE_A_SYMBOLS[0])
            } else {
                (&TABLE_B_COUNTS[0], &TABLE_B_SYMBOLS[0])
            };
            let (code, length) = canonical(counts, symbols, symbol).expect("symbol in the table");
            self.put(code, length);
        }
        /// A run/size symbol and its value bits, in the sign
        /// convention of section 4.2.
        fn difference(&mut self, first: bool, run: u8, size: u32, value: i32) {
            self.symbol(first, (run << 4) | size as u8);
            if size > 0 {
                let bits = if value > 0 {
                    value
                } else {
                    value + (1 << size) - 1
                };
                self.put(bits as u32, size);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.bits > 0 {
                let pad = 8 - self.bits;
                self.put(0, pad);
            }
            self.out
        }
    }

    /// The canonical assignment of the three first-coefficient tables,
    /// spelled out code word by code word. This is the one place the
    /// firmware tables can be checked without a file: get the code
    /// assignment wrong and every compressed frame is noise. Only set
    /// 0 appears in the corpus, so for sets 1 and 2 this is the whole
    /// of the evidence.
    #[test]
    fn the_first_coefficient_tables_have_the_codes_they_must() {
        const EXPANDED: [[(&str, u8); 13]; 3] = [
            [
                ("00", 0x04),
                ("010", 0x03),
                ("011", 0x05),
                ("100", 0x06),
                ("101", 0x02),
                ("1100", 0x07),
                ("1101", 0x01),
                ("11100", 0x08),
                ("11101", 0x09),
                ("11110", 0x00),
                ("111110", 0x0A),
                ("1111110", 0x0B),
                ("1111111", 0xFF),
            ],
            [
                ("00", 0x03),
                ("01", 0x02),
                ("100", 0x04),
                ("101", 0x01),
                ("1100", 0x05),
                ("1101", 0x00),
                ("1110", 0x06),
                ("11110", 0x07),
                ("111110", 0x09),
                ("1111110", 0x08),
                ("11111110", 0x0A),
                ("111111110", 0x0B),
                ("111111111", 0xFF),
            ],
            [
                ("000", 0x06),
                ("001", 0x05),
                ("010", 0x07),
                ("011", 0x04),
                ("100", 0x08),
                ("101", 0x03),
                ("1100", 0x09),
                ("1101", 0x02),
                ("1110", 0x00),
                ("11110", 0x0A),
                ("111110", 0x01),
                ("1111110", 0x0B),
                ("1111111", 0xFF),
            ],
        ];
        for (set, codes) in EXPANDED.iter().enumerate() {
            for (bits, symbol) in codes {
                let want = (u32::from_str_radix(bits, 2).unwrap(), bits.len() as u32);
                assert_eq!(
                    canonical(&TABLE_A_COUNTS[set], &TABLE_A_SYMBOLS[set], *symbol),
                    Some(want),
                    "set {set} table A symbol {symbol:#04x} should be {bits}"
                );
            }
        }
    }

    /// The six tables build, and each holds exactly the leaves its
    /// counts promise. A table that over-subscribes its code space is
    /// rejected by [`HuffTable::new`], which is the check that a
    /// mistyped count would trip.
    #[test]
    fn every_table_set_builds() {
        for set in 0..3 {
            assert_eq!(
                TABLE_A_COUNTS[set]
                    .iter()
                    .map(|c| *c as usize)
                    .sum::<usize>(),
                TABLE_A_SYMBOLS[set].len()
            );
            assert_eq!(
                TABLE_B_COUNTS[set]
                    .iter()
                    .map(|c| *c as usize)
                    .sum::<usize>(),
                TABLE_B_SYMBOLS[set].len()
            );
            HuffTable::new(&TABLE_A_COUNTS[set], &TABLE_A_SYMBOLS[set]).expect("table A builds");
            HuffTable::new(&TABLE_B_COUNTS[set], &TABLE_B_SYMBOLS[set]).expect("table B builds");
        }
    }

    /// `FF 00` is a data `0xFF`; `FF` followed by anything else ends
    /// the stream, and everything after it reads as zero.
    #[test]
    fn the_reader_unstuffs_and_stops_at_a_marker() {
        let mut pump = BitPumpJpeg::new(&[0xFF, 0x00, 0xFF, 0x00]);
        for _ in 0..16 {
            assert_eq!(pump.get(1), 1);
        }
        let mut pump = BitPumpJpeg::new(&[0xFF, 0x01, 0xFF, 0xFF]);
        assert_eq!(pump.get(8), 0);
        assert!(pump.at_marker());
    }

    /// Run/size symbols, end of block, the block-to-block carry and
    /// the per-row predictor reset, on a stream built here.
    ///
    /// Two blocks over a 64-wide, two-row frame, so each block is
    /// exactly one row and the reset falls on a block boundary.
    #[test]
    fn a_block_carries_runs_an_end_marker_and_a_carry() {
        let mut bits = Stuffed::default();
        // Block 0: +3 at position 0, then a run of one to position 2
        // carrying +1, then end of block.
        bits.difference(true, 0, 2, 3);
        bits.difference(false, 1, 1, 1);
        bits.symbol(false, 0x00);
        // Block 1: a first coefficient of zero, which the carry from
        // block 0 turns into +3, then end of block.
        bits.difference(true, 0, 0, 0);
        bits.symbol(false, 0x00);
        let frame = decompress(&bits.finish(), 0, 64, 2).expect("decodes");
        // Row 0: both predictors start at 512; the even one takes +3
        // then +1, the odd one nothing.
        assert_eq!(&frame[..5], &[515, 512, 516, 512, 516]);
        // Row 1 resets both predictors and applies the carried +3 to
        // the even one only.
        assert_eq!(&frame[64..68], &[515, 512, 515, 512]);
    }

    /// `0xFF` skips a single position. Its nominal run field is
    /// fifteen, and a decoder that honoured it would put the next
    /// difference fifteen columns further along.
    #[test]
    fn the_ff_symbol_skips_one_position_not_sixteen() {
        let mut bits = Stuffed::default();
        bits.symbol(true, 0xFF);
        bits.difference(false, 0, 1, 1);
        bits.symbol(false, 0x00);
        let frame = decompress(&bits.finish(), 0, 64, 1).expect("decodes");
        // Position 0 untouched, position 1 raised by one: the skip
        // moved the index on by exactly one.
        assert_eq!(&frame[..4], &[512, 513, 512, 513]);
    }

    /// A run that reaches past the end of a block loses its
    /// difference but still spends its bits, so the block that
    /// follows starts in step.
    #[test]
    fn a_run_past_the_end_of_a_block_still_spends_its_bits() {
        let mut bits = Stuffed::default();
        // Position 0, then a run of 15 from position 49 lands on 64.
        bits.difference(true, 0, 1, 1);
        for _ in 0..3 {
            bits.difference(false, 15, 0, 0);
        }
        bits.difference(false, 15, 1, 1);
        // No end-of-block symbol: the index has already run past 63,
        // so the block finishes on the run itself.
        // The next block must decode as a plain +1 at position 0.
        bits.difference(true, 0, 1, 1);
        bits.symbol(false, 0x00);
        let frame = decompress(&bits.finish(), 0, 64, 2).expect("decodes");
        assert_eq!(frame[0], 513);
        // Carry: block 1's own difference 0 is +1 on top of block 0's.
        assert_eq!(frame[64], 514);
    }

    /// The low-bit plane's four samples a byte, least significant pair
    /// first. The numbers are the EOS D60's row 8: ten-bit 55 55 55 52
    /// becoming twelve-bit 222 221 221 208.
    #[test]
    fn the_low_bit_plane_is_read_least_significant_pair_first() {
        let mut frame = vec![55u16, 55, 55, 52];
        let byte = 2 | (1 << 2) | (1 << 4);
        merge_low_bits(&mut frame, &[byte], 4, 1);
        assert_eq!(frame, vec![222, 221, 221, 208]);
    }

    /// The 2672-wide bodies raise a merged value under 512 by two.
    /// Unexplained, but every frame of theirs carries it.
    #[test]
    fn the_2672_wide_bodies_get_the_low_bit_fudge() {
        let mut frame = vec![100u16, 200];
        merge_low_bits(&mut frame, &[0], FUDGED_WIDTH, 1);
        assert_eq!(frame, vec![402, 800]);
        let mut frame = vec![100u16, 200];
        merge_low_bits(&mut frame, &[0], 2376, 1);
        assert_eq!(frame, vec![400, 800]);
    }

    /// The heuristic that decides whether there is a low-bit plane.
    #[test]
    fn the_low_bit_probe_reads_the_stuffing_rule() {
        let mut file = vec![0u8; PROBE_WINDOW];
        // Coded data: every 0xFF followed by a zero.
        file[600] = 0xFF;
        assert!(!has_low_bit_plane(&file, 540));
        // Plane data: an 0xFF followed by something else.
        file[601] = 1;
        assert!(has_low_bit_plane(&file, 540));
        // No 0xFF at all decides nothing, and a plane is assumed.
        assert!(has_low_bit_plane(&vec![0u8; PROBE_WINDOW], 540));
    }

    /// The per-frame-size geometry, on the two bodies the corpus has.
    #[test]
    fn geometry_is_keyed_by_the_frame_size() {
        let g2 = geometry(2376, 1728).expect("PowerShot G2");
        assert_eq!((g2.left, g2.top, g2.width, g2.height), (12, 6, 2312, 1720));
        let d60 = geometry(3152, 2068).expect("EOS D60");
        assert_eq!(
            (d60.left, d60.top, d60.width, d60.height),
            (64, 12, 3088, 2056)
        );
        // The D60 frame is the only one whose left black rectangle is
        // set in: columns 18..=61 rather than 2..=61.
        assert_eq!(d60.mask, [16, 0, 0, 0]);
        assert_eq!(geometry(1000, 1000), None);
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        for bytes in [
            &b""[..],
            &b"II"[..],
            &b"II\x1a\0\0\0HEAPCCDR"[..],
            // A heap whose directory offset points past its own end.
            &b"II\x1a\0\0\0HEAPCCDR\x02\0\x01\0\0\0\0\0\0\0\xff\xff\xff\xff"[..],
            &[0xff; 128][..],
        ] {
            assert!(decode(bytes).is_err());
            assert!(preview(bytes).is_err() || preview(bytes).unwrap().is_none());
        }
    }

    // ---------------------------------------------------------- corpus

    /// Files this module knowingly declines, with the reason. A
    /// corpus file that fails for any other reason fails the test.
    ///
    /// Nothing is on it today: every CIFF in the corpus decodes. The
    /// list stays because the two open cases — an uncompressed body
    /// other than the Pro70, whose filter array nothing records, and a
    /// compressed frame of a size no body in [`GEOMETRY`] writes —
    /// are refusals rather than bugs.
    const UNSUPPORTED: &[&str] = &["filter array", "no body known"];

    fn corpus() -> Option<PathBuf> {
        std::env::var_os("SCHIST_RAW_CORPUS").map(PathBuf::from)
    }

    fn crw_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                crw_files(&path, out);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("crw"))
            {
                out.push(path);
            }
        }
    }

    fn oracle(path: &std::path::Path) -> Option<(usize, usize, Vec<u16>)> {
        let tiff = path.with_file_name(format!("{}.tiff", path.file_name()?.to_string_lossy()));
        let image = image::open(tiff).ok()?.into_luma16();
        let (width, height) = (image.width() as usize, image.height() as usize);
        Some((width, height, image.into_raw()))
    }

    /// The ten-bit frame of a compressed sample, before its low-bit
    /// plane (if any) is merged, with where its coded stream began and
    /// whether a plane was found.
    fn ten_bit_frame(bytes: &[u8]) -> (Vec<u16>, usize, bool) {
        let ciff = Ciff::parse(bytes).expect("CIFF");
        let image = ciff.find(tag::IMAGE_DATA).expect("image record");
        let sensor = ciff.sensor_info().expect("sensor record");
        let (width, height) = (sensor.width, sensor.height);
        let plane = has_low_bit_plane(bytes, image.at + PAD);
        let coded_at = image.at + PAD + if plane { width * height / 4 } else { 0 };
        let end = image.at + image.len;
        let set = ciff
            .longs(tag::DECODER_TABLE)
            .first()
            .map_or(0, |v| *v as usize)
            .min(TABLE_A_COUNTS.len() - 1);
        let frame = decompress(&bytes[coded_at..end], set, width, height).expect("decodes");
        (frame, coded_at, plane)
    }

    fn sample(path: &str) -> Option<Vec<u8>> {
        let dir = corpus()?;
        let path = dir.join("Canon").join(path);
        std::fs::read(path).ok()
    }

    /// The PowerShot G2: ten bits a sample, no low-bit plane, and a
    /// row of dense non-zero differences.
    #[test]
    fn the_g2_decodes_its_first_band_sample_for_sample() {
        let Some(bytes) = sample("PowerShot_G2-RAW_CANON_G2.CRW") else {
            return;
        };
        let (frame, coded_at, plane) = ten_bit_frame(&bytes);
        assert!(!plane, "the G2 is a ten-bit body");
        assert_eq!(coded_at, 540);
        assert_eq!(
            &bytes[coded_at..coded_at + 16],
            &[
                0xE8, 0x7F, 0xE8, 0x7B, 0xFD, 0xEB, 0x8C, 0x4A, 0x50, 0xF7, 0x8C, 0x63, 0x3E, 0xE5,
                0x8F, 0x58
            ]
        );
        let width = 2376;
        assert_eq!(
            &frame[..16],
            &[32, 31, 32, 31, 32, 31, 32, 32, 31, 31, 33, 33, 30, 33, 31, 32]
        );
        assert_eq!(
            &frame[64..80],
            &[32, 31, 32, 30, 31, 31, 32, 31, 32, 30, 33, 32, 30, 32, 31, 32]
        );
        assert_eq!(
            &frame[width..width + 16],
            &[32, 32, 32, 34, 34, 32, 32, 33, 34, 32, 32, 30, 32, 33, 32, 33]
        );
        // Band 0 emits exactly 8 * 2376 samples, so the two predictors
        // at the end of it are the last two samples of row 7.
        assert_eq!(&frame[8 * width - 2..8 * width], &[32, 32]);
    }

    /// The EOS D60: twelve bits a sample, so a low-bit plane sits in
    /// front of a coded stream whose very first bytes are stuffed.
    #[test]
    fn the_d60_decodes_its_first_two_bands_sample_for_sample() {
        let Some(bytes) = sample("EOS_D60-CRW_0099.CRW") else {
            return;
        };
        let (mut frame, coded_at, plane) = ten_bit_frame(&bytes);
        assert!(plane, "the D60 is a twelve-bit body");
        assert_eq!(coded_at, 540 + 3152 * 2068 / 4);
        assert_eq!(
            &bytes[coded_at..coded_at + 8],
            // Two stuffed pairs in the first eight bytes.
            &[0xF9, 0xFF, 0x00, 0xFE, 0x9F, 0xFF, 0x00, 0xDF]
        );
        let width = 3152;
        // Block 0's differences are -512, -512 and block 1's +512,
        // which the carry cancels, so the frame opens with seven
        // entirely blank rows.
        assert!(
            frame[..7 * width].iter().all(|s| *s == 0),
            "rows 0..6 should be blank"
        );
        assert!(frame[7 * width..8 * width].iter().any(|s| *s != 0));
        // Both predictors stand at 55 at the end of band 0.
        assert_eq!(&frame[8 * width - 2..8 * width], &[55, 55]);
        assert_eq!(
            &frame[8 * width..8 * width + 16],
            &[55, 55, 55, 55, 55, 55, 55, 52, 55, 55, 55, 54, 55, 55, 55, 55]
        );
        assert_eq!(
            &frame[8 * width + 64..8 * width + 80],
            &[55, 55, 55, 54, 55, 55, 55, 55, 54, 55, 55, 55, 55, 55, 55, 55]
        );
        // Band 1 leaves the two predictors far apart, which is what
        // proves they are kept independently.
        assert_eq!(&frame[16 * width - 2..16 * width], &[455, 381]);

        let plane = &bytes[26..26 + 3152 * 2068 / 4];
        merge_low_bits(&mut frame, plane, width, 2068);
        assert_eq!(
            &frame[8 * width..8 * width + 16],
            &[222, 221, 221, 221, 221, 221, 221, 208, 221, 221, 221, 218, 221, 220, 221, 221]
        );
        assert_eq!(
            &frame[8 * width + 64..8 * width + 80],
            &[221, 221, 221, 219, 220, 221, 220, 221, 219, 221, 220, 221, 222, 221, 221, 220]
        );
    }

    /// The EOS 10D writes the same geometry and the same opening
    /// blocks as the D60 but ends band 0 with its two predictors at
    /// different values: an implementation that shared one predictor
    /// between the column parities would pass the D60 and fail here.
    #[test]
    fn the_10d_ends_its_first_band_with_two_different_predictors() {
        let Some(bytes) = sample("EOS_10D-CRW_7673.CRW") else {
            return;
        };
        let (frame, coded_at, plane) = ten_bit_frame(&bytes);
        assert!(plane);
        assert_eq!(coded_at, 540 + 3152 * 2068 / 4);
        let width = 3152;
        assert!(frame[..7 * width].iter().all(|s| *s == 0));
        assert_eq!(&frame[8 * width - 2..8 * width], &[51, 54]);
    }

    #[test]
    fn corpus_decodes_what_it_can_and_refuses_the_rest() {
        let Some(dir) = corpus() else { return };
        let mut files = Vec::new();
        crw_files(&dir, &mut files);
        files.sort();
        let (mut ciffs, mut decoded) = (0, 0);
        for path in &files {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let bytes = std::fs::read(path).expect("read sample");
            // Files named .CRW that are not CIFF at all: CHDK, the
            // third-party PowerShot firmware, writes headerless dumps
            // of the sensor under that extension. They carry nothing
            // that says what shape they are, so `probe` rejects them
            // and this decoder never sees them.
            if bytes.get(6..14) != Some(b"HEAPCCDR") {
                assert_eq!(
                    crate::probe(&bytes),
                    None,
                    "{name} is not CIFF but probes as raw"
                );
                continue;
            }
            assert_eq!(
                crate::probe(&bytes),
                Some(Format::Crw),
                "{name} probes as CRW"
            );
            // Every CIFF with an image record is one this module is
            // meant to decode, compressed or not.
            let ciff = Ciff::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            if ciff.find(tag::IMAGE_DATA).is_some() {
                ciffs += 1;
            }

            // The camera's own JPEG comes out of any CRW that has one,
            // compressed image or not.
            if let Some(jpeg) = preview(&bytes).expect("preview") {
                image::load_from_memory(&jpeg).unwrap_or_else(|e| panic!("{name} preview: {e}"));
            }

            let raw = match decode(&bytes) {
                Ok(raw) => raw,
                Err(Error::Unsupported(why)) => {
                    assert!(
                        UNSUPPORTED.iter().any(|allowed| why.contains(allowed)),
                        "{name}: {why}"
                    );
                    eprintln!("crw: {name} is unsupported: {why}");
                    continue;
                }
                Err(why) => panic!("{name}: {why}"),
            };
            raw.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(raw.format, Format::Crw);
            assert_eq!(raw.make, "Canon", "{name}");
            decoded += 1;

            let Some((width, height, want)) = oracle(path) else {
                continue;
            };
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
        }
        eprintln!(
            "crw: {decoded} of {ciffs} CIFF files decoded, {} files seen",
            files.len()
        );
        assert_eq!(
            decoded, ciffs,
            "every CIFF in the corpus with an image record should decode"
        );
    }

    #[test]
    fn truncated_corpus_files_never_panic() {
        let Some(dir) = corpus() else { return };
        let mut files = Vec::new();
        crw_files(&dir, &mut files);
        for path in &files {
            let bytes = std::fs::read(path).expect("read sample");
            for cut in [0, 1, 13, 14, 26, 27, 1024] {
                let cut = cut.min(bytes.len());
                let _ = decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
            for n in 1..=6 {
                let cut = bytes.len() * n / 7;
                let _ = decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
            // And the tail cut off, which moves the root directory.
            for n in 1..=4 {
                let cut = bytes.len() - n;
                let _ = decode(&bytes[..cut]);
                let _ = preview(&bytes[..cut]);
            }
        }
    }
}
