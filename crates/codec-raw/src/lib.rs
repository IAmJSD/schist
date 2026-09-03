//! Camera raw files, decoded and developed in pure Rust.
//!
//! Two steps, deliberately separate:
//!
//! 1. [`decode`] reads a file into a [`RawImage`]: the sensor data as
//!    the camera stored it (one sample a pixel under a colour filter
//!    array, or three for the few linear formats), plus everything a
//!    developer needs to make a picture of it — black and white levels,
//!    the as-shot white balance, the camera's colour matrix, the crop,
//!    the orientation — and the camera's own embedded JPEG.
//! 2. [`develop`] turns a `RawImage` into linear sRGB: black
//!    subtraction, white balance, demosaic, the matrix, then crop and
//!    orientation.
//!
//! Every vendor container is a module under [`formats`]; the shared
//! machinery (TIFF, bit readers, lossless JPEG, ISO-BMFF) sits beside
//! them. [`probe`] says which module a file belongs to without decoding
//! it.
//!
//! This is a clean-room implementation written from the public DNG
//! specification and published format descriptions. Nothing in it is
//! derived from dcraw, LibRaw, rawspeed, rawler or any other
//! copyleft decoder, and nothing here may read their source.

pub mod bits;
pub mod bmff;
pub mod cameras;
pub mod demosaic;
pub mod develop;
pub mod formats;
pub mod ljpeg;
pub mod tiff;

pub use develop::{develop, DevelopOptions, Developed};

/// Which container a file is. One variant per decoder module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// Adobe DNG, including Apple ProRAW, Pixel, Leica and Pentax DNGs.
    Dng,
    /// Nikon NEF and NRW.
    Nef,
    /// Sony ARW, SR2 and SRF.
    Arw,
    /// Canon CR2.
    Cr2,
    /// Canon CRW (CIFF).
    Crw,
    /// Canon CR3 (ISO-BMFF with the CRX codec).
    Cr3,
    /// Fujifilm RAF.
    Raf,
    /// Olympus / OM System ORF.
    Orf,
    /// Panasonic RW2 and Leica RWL.
    Rw2,
    /// Pentax PEF.
    Pef,
    /// Samsung SRW.
    Srw,
    /// Minolta MRW.
    Mrw,
    /// Kodak DCR and KDC.
    Kodak,
    /// Epson ERF.
    Erf,
    /// Mamiya MEF.
    Mef,
    /// Phase One IIQ.
    Iiq,
    /// Hasselblad 3FR and FFF.
    Hasselblad,
    /// Leaf MOS.
    Mos,
    /// Sigma X3F (Foveon).
    X3f,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file this crate recognises but cannot decode: an unknown
    /// compression variant, a camera it has no data for, a feature not
    /// yet implemented. The caller may have another decoder to try.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The file is not what its header says.
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// Not a raw file at all (from [`decode`] on bytes [`probe`] rejects).
    #[error("not a camera raw file")]
    NotRaw,
}

pub type Result<T> = std::result::Result<T, Error>;

/// The largest frame a decoder will allocate for, in samples of one
/// plane: 400 megapixels, past every sensor shipped (the largest
/// medium-format backs are 150 MP) while refusing the multi-gigabyte
/// requests a forged header can make, which the allocator answers by
/// aborting the process rather than by an error anyone can catch.
pub const MAX_FRAME_SAMPLES: usize = 400 << 20;

/// `width * height * cpp`, checked against overflow and against
/// [`MAX_FRAME_SAMPLES`]. Every decoder sizes its output through this
/// before allocating.
pub fn frame_samples(width: usize, height: usize, cpp: usize) -> Result<usize> {
    width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(cpp))
        .filter(|n| *n <= MAX_FRAME_SAMPLES)
        .ok_or_else(|| {
            Error::Corrupt(format!(
                "frame of {width}x{height}x{cpp} samples is not plausible"
            ))
        })
}

/// Sensor samples: 16-bit integers for nearly everything, floats for
/// floating-point DNGs.
#[derive(Debug, Clone, PartialEq)]
pub enum RawData {
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl RawData {
    pub fn len(&self) -> usize {
        match self {
            RawData::U16(v) => v.len(),
            RawData::F32(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Sample `i` as a float, unscaled.
    pub fn get(&self, i: usize) -> f32 {
        match self {
            RawData::U16(v) => v.get(i).map_or(0.0, |s| *s as f32),
            RawData::F32(v) => v.get(i).copied().unwrap_or(0.0),
        }
    }
}

/// One colour of a filter array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CfaColor {
    Red,
    Green,
    Blue,
    /// Panasonic and a few others record a second, separately
    /// balanced green; treat as green when in doubt.
    Green2,
    /// Cyan, magenta, yellow: old Kodak/Canon/Nikon CMYG sensors.
    Cyan,
    Magenta,
    Yellow,
    /// Emerald, on Sony's four-colour sensors.
    Emerald,
}

/// The colour filter array over the sensor, as a repeating pattern
/// anchored at the top-left of the *uncropped* `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cfa {
    /// Three samples a pixel already (linear DNG, Foveon, Canon sRAW).
    None,
    /// A 2x2 Bayer pattern, row-major: `[top-left, top-right,
    /// bottom-left, bottom-right]`.
    Bayer([CfaColor; 4]),
    /// Fujifilm X-Trans, 6x6 row-major.
    XTrans([[CfaColor; 6]; 6]),
    /// Anything else: `width` x `height` row-major.
    Pattern {
        width: usize,
        height: usize,
        colors: Vec<CfaColor>,
    },
}

impl Cfa {
    pub const RGGB: Cfa = Cfa::Bayer([
        CfaColor::Red,
        CfaColor::Green,
        CfaColor::Green,
        CfaColor::Blue,
    ]);
    pub const BGGR: Cfa = Cfa::Bayer([
        CfaColor::Blue,
        CfaColor::Green,
        CfaColor::Green,
        CfaColor::Red,
    ]);
    pub const GRBG: Cfa = Cfa::Bayer([
        CfaColor::Green,
        CfaColor::Red,
        CfaColor::Blue,
        CfaColor::Green,
    ]);
    pub const GBRG: Cfa = Cfa::Bayer([
        CfaColor::Green,
        CfaColor::Blue,
        CfaColor::Red,
        CfaColor::Green,
    ]);

    /// The colour under sensor pixel (`x`, `y`) of the uncropped data.
    pub fn color_at(&self, x: usize, y: usize) -> Option<CfaColor> {
        match self {
            Cfa::None => None,
            Cfa::Bayer(p) => Some(p[(y % 2) * 2 + (x % 2)]),
            Cfa::XTrans(p) => Some(p[y % 6][x % 6]),
            Cfa::Pattern {
                width,
                height,
                colors,
            } => {
                if *width == 0 || *height == 0 {
                    return None;
                }
                colors.get((y % height) * width + (x % width)).copied()
            }
        }
    }

    /// Whether the pattern describes itself consistently: non-zero
    /// dimensions and one colour per cell. `validate` checks it, so a
    /// decoder cannot hand the developer a pattern that indexes past
    /// its own colours.
    pub fn is_well_formed(&self) -> bool {
        match self {
            Cfa::Pattern {
                width,
                height,
                colors,
            } => *width > 0 && *height > 0 && colors.len() == width * height,
            _ => true,
        }
    }

    /// The same pattern with its origin moved by (`dx`, `dy`) pixels,
    /// for describing a crop of the data. A malformed `Pattern` comes
    /// back unchanged rather than panicking.
    pub fn shifted(&self, dx: usize, dy: usize) -> Cfa {
        if !self.is_well_formed() {
            return self.clone();
        }
        match self {
            Cfa::None => Cfa::None,
            Cfa::Bayer(_) => Cfa::Bayer(std::array::from_fn(|i| {
                self.color_at(dx + i % 2, dy + i / 2).unwrap()
            })),
            Cfa::XTrans(_) => Cfa::XTrans(std::array::from_fn(|y| {
                std::array::from_fn(|x| self.color_at(dx + x, dy + y).unwrap())
            })),
            Cfa::Pattern { width, height, .. } => Cfa::Pattern {
                width: *width,
                height: *height,
                colors: (0..height * width)
                    .map(|i| self.color_at(dx + i % width, dy + i / width).unwrap())
                    .collect(),
            },
        }
    }
}

/// The camera's orientation tag, TIFF/EXIF values 1..=8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Normal,
    MirrorHorizontal,
    Rotate180,
    MirrorVertical,
    /// Mirror horizontal then rotate 270° CW.
    Transpose,
    Rotate90CW,
    /// Mirror horizontal then rotate 90° CW.
    Transverse,
    Rotate270CW,
}

impl Orientation {
    pub fn from_exif(value: u32) -> Orientation {
        match value {
            2 => Orientation::MirrorHorizontal,
            3 => Orientation::Rotate180,
            4 => Orientation::MirrorVertical,
            5 => Orientation::Transpose,
            6 => Orientation::Rotate90CW,
            7 => Orientation::Transverse,
            8 => Orientation::Rotate270CW,
            _ => Orientation::Normal,
        }
    }
    /// Whether width and height swap when applied.
    pub fn transposes(self) -> bool {
        matches!(
            self,
            Orientation::Transpose
                | Orientation::Rotate90CW
                | Orientation::Transverse
                | Orientation::Rotate270CW
        )
    }
}

/// A rectangle in sensor pixels of the uncropped data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Shooting metadata worth carrying out, when the container has it.
/// Everything optional; the gallery reads EXIF separately.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub iso: Option<f32>,
    pub exposure_time: Option<f32>,
    pub f_number: Option<f32>,
    pub focal_length: Option<f32>,
    pub lens: Option<String>,
    /// "YYYY:MM:DD HH:MM:SS" as EXIF writes it.
    pub date_time: Option<String>,
}

/// A decoded raw: the sensor data and what it takes to develop it.
#[derive(Debug, Clone, PartialEq)]
pub struct RawImage {
    pub format: Format,
    /// The make and model strings as the file carries them.
    pub make: String,
    pub model: String,
    /// The same, normalised for the camera table: `cameras::normalize`.
    pub clean_make: String,
    pub clean_model: String,

    /// Dimensions of `data` — the full decoded frame, masked borders
    /// included, so `cfa` and `crop` are anchored at its origin.
    pub width: usize,
    pub height: usize,
    /// Samples per pixel: 1 under a CFA, 3 for `Cfa::None`.
    pub cpp: usize,
    pub data: RawData,
    pub cfa: Cfa,

    /// Black level per CFA position (row-major 2x2 for Bayer; all four
    /// equal is the common case), in `data`'s units. For `Cfa::None`,
    /// per channel with the fourth unused.
    pub black_levels: [f32; 4],
    /// Saturation level in `data`'s units, one value for the sensor.
    pub white_level: f32,
    /// As-shot white balance as multipliers for R, G, B, G2, normalised
    /// so green is 1.0. `[1.0; 4]` when the file records none.
    pub wb_coeffs: [f32; 4],
    /// XYZ (D65) to camera colour, rows camera R/G/B (the DNG
    /// ColorMatrix convention). From the file where it carries one
    /// (DNG), else the camera table, else `None`.
    pub color_matrix: Option<[[f32; 3]; 3]>,

    /// The area meant to be shown: the sensor's active region minus
    /// the masked borders and any vendor crop. As the file records
    /// it: the origin may be odd, so a consumer re-deriving a filter
    /// pattern for the cropped region must shift `cfa` by it (`develop`
    /// interpolates the whole frame and crops afterwards, and needs no
    /// such care).
    pub crop: Rect,
    pub orientation: Orientation,
    /// The camera's own JPEG preview, the largest one in the file, as
    /// its bytes.
    pub preview: Option<Vec<u8>>,
    pub metadata: Metadata,
}

impl RawImage {
    /// A `RawImage` with every field at its plainest, for decoders to
    /// fill in: no crop (the whole frame), no orientation, unit white
    /// balance, no matrix.
    pub fn new(
        format: Format,
        width: usize,
        height: usize,
        cpp: usize,
        data: RawData,
        cfa: Cfa,
    ) -> RawImage {
        RawImage {
            format,
            make: String::new(),
            model: String::new(),
            clean_make: String::new(),
            clean_model: String::new(),
            width,
            height,
            cpp,
            data,
            cfa,
            black_levels: [0.0; 4],
            white_level: 65535.0,
            wb_coeffs: [1.0; 4],
            color_matrix: None,
            crop: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            orientation: Orientation::Normal,
            preview: None,
            metadata: Metadata::default(),
        }
    }

    /// Set make and model, deriving the normalised pair.
    pub fn set_camera(&mut self, make: &str, model: &str) {
        self.make = make.trim().to_string();
        self.model = model.trim().to_string();
        let (clean_make, clean_model) = cameras::normalize(&self.make, &self.model);
        self.clean_make = clean_make;
        self.clean_model = clean_model;
    }

    /// Fill `color_matrix` (and black/white levels, when the decoder
    /// left them at their defaults and the table has values) from the
    /// camera table. Decoders call this last; a matrix the file itself
    /// carries wins.
    pub fn apply_camera_table(&mut self) {
        if let Some(camera) = cameras::lookup(&self.clean_make, &self.clean_model) {
            if self.color_matrix.is_none() {
                self.color_matrix = Some(camera.color_matrix);
            }
            if self.black_levels == [0.0; 4] {
                if let Some(black) = camera.black_level {
                    self.black_levels = [black as f32; 4];
                }
            }
            if self.white_level == 65535.0 {
                if let Some(white) = camera.white_level {
                    self.white_level = white as f32;
                }
            }
        }
    }

    /// Internal consistency: data length matches the dimensions, the
    /// crop lies inside the frame, levels are sane. Decoders' tests
    /// call this on everything they produce.
    pub fn validate(&self) -> Result<()> {
        // Checked arithmetic throughout: this is the last guard between
        // a decoder and the developer, and a product that wraps to the
        // data length would pass garbage through in release builds.
        let expect = frame_samples(self.width, self.height, self.cpp)?;
        if self.data.len() != expect {
            return Err(Error::Corrupt(format!(
                "data holds {} samples for {}x{}x{}",
                self.data.len(),
                self.width,
                self.height,
                self.cpp
            )));
        }
        let inside = self
            .crop
            .x
            .checked_add(self.crop.width)
            .is_some_and(|right| right <= self.width)
            && self
                .crop
                .y
                .checked_add(self.crop.height)
                .is_some_and(|bottom| bottom <= self.height);
        if !inside || self.crop.width == 0 || self.crop.height == 0 {
            return Err(Error::Corrupt(format!(
                "crop {:?} outside {}x{}",
                self.crop, self.width, self.height
            )));
        }
        if self.cpp == 1 && self.cfa == Cfa::None || self.cpp == 3 && self.cfa != Cfa::None {
            return Err(Error::Corrupt(format!(
                "cpp {} with cfa {:?}",
                self.cpp, self.cfa
            )));
        }
        if !self.cfa.is_well_formed() {
            return Err(Error::Corrupt(format!("malformed cfa {:?}", self.cfa)));
        }
        // Written to catch NaN as well as the wrong sign.
        let bad = |v: f32| v.is_nan() || v <= 0.0;
        if bad(self.white_level)
            || self
                .black_levels
                .iter()
                .any(|b| b.is_nan() || *b >= self.white_level)
        {
            return Err(Error::Corrupt(format!(
                "levels black {:?} white {}",
                self.black_levels, self.white_level
            )));
        }
        Ok(())
    }
}

/// Which format a file is, from its bytes, without decoding it. `None`
/// for anything that is not a camera raw (a plain TIFF from a
/// converter included: it has no sensor data).
pub fn probe(bytes: &[u8]) -> Option<Format> {
    let at = |offset: usize, want: &[u8]| bytes.get(offset..offset + want.len()) == Some(want);
    if at(0, b"FUJIFILMCCD-RAW") {
        return Some(Format::Raf);
    }
    if at(0, b"IIRO") || at(0, b"IIRS") || at(0, b"MMOR") {
        return Some(Format::Orf);
    }
    if at(0, b"IIU\0") {
        return Some(Format::Rw2);
    }
    if at(6, b"HEAPCCDR") {
        return Some(Format::Crw);
    }
    if at(0, b"FOVb") {
        return Some(Format::X3f);
    }
    if at(0, b"\0MRM") {
        return Some(Format::Mrw);
    }
    if at(4, b"ftyp") && at(8, b"crx ") {
        return Some(Format::Cr3);
    }
    if at(0, b"II*\0") && at(8, b"CR") {
        return Some(Format::Cr2);
    }
    // Phase One: an ordinary little-endian TIFF header (IFD0 is a
    // thumbnail at the end of the file) with the signature "IIIICwaR"
    // right after it; the private directory the iiq module reads is
    // reached from there.
    if at(0, b"II*\0") && at(8, b"IIIICwaR") {
        return Some(Format::Iiq);
    }
    probe_tiff(bytes)
}

/// The TIFF-shaped formats, told apart by the maker and by whether
/// any IFD holds sensor data. This is a minimal walk that does not
/// depend on the full parser, so a truncated or odd file cannot make
/// probing fail loudly.
fn probe_tiff(bytes: &[u8]) -> Option<Format> {
    let le = match bytes.get(0..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    const MAKE: u16 = 0x010F;
    const PHOTOMETRIC: u16 = 0x0106;
    const BITS_PER_SAMPLE: u16 = 0x0102;
    const STRIP_OFFSETS: u16 = 0x0111;
    const STRIP_BYTE_COUNTS: u16 = 0x0117;
    const SUB_IFDS: u16 = 0x014A;
    const LEAF_DATA: u16 = 0x8606;
    const DNG_VERSION: u16 = 0xC612;
    const DNG_PRIVATE_DATA: u16 = 0xC634;

    let mut make = String::new();
    let mut sensor_data = false;
    let mut dng = false;
    let mut leaf = false;
    // Sony's first bodies (DSLR-A100) point SubIFDs at bare data and
    // the DSC-F828 writes strips with no offsets; both carry Sony's
    // private block, which no converter's TIFF does.
    let mut private = false;
    let mut queue = vec![u32_at(4)? as usize];
    let mut visited = 0;
    while let Some(ifd) = queue.pop() {
        if visited >= 12 {
            break;
        }
        visited += 1;
        let Some(count) = u16_at(ifd) else { continue };
        let count = count.min(512) as usize;
        // An IFD of strips more than 8 bits deep is sensor data in the
        // formats that never say so with PhotometricInterpretation
        // (Samsung SRW, Sony SRF).
        let (mut deep, mut strips) = (false, false);
        for i in 0..count {
            let entry = ifd + 2 + i * 12;
            let (Some(tag), Some(kind), Some(n)) =
                (u16_at(entry), u16_at(entry + 2), u32_at(entry + 4))
            else {
                break;
            };
            match tag {
                // One SHORT sits in the entry; three (RGB) live at the
                // offset the entry holds, and reading the offset as the
                // value would call every colour thumbnail deep.
                BITS_PER_SAMPLE if kind == 3 => {
                    let Some(at) = (if n <= 2 {
                        Some(entry + 8)
                    } else {
                        u32_at(entry + 8).map(|o| o as usize)
                    }) else {
                        break;
                    };
                    deep = u16_at(at).is_some_and(|b| b > 8);
                }
                STRIP_OFFSETS | STRIP_BYTE_COUNTS => strips = true,
                LEAF_DATA => leaf = true,
                DNG_PRIVATE_DATA => private = true,
                MAKE if kind == 2 && make.is_empty() => {
                    let n = n as usize;
                    let Some(at) = (if n <= 4 {
                        Some(entry + 8)
                    } else {
                        u32_at(entry + 8).map(|o| o as usize)
                    }) else {
                        break;
                    };
                    if let Some(s) = bytes.get(at..at + n) {
                        make = String::from_utf8_lossy(s)
                            .trim_end_matches('\0')
                            .trim()
                            .to_string();
                    }
                }
                PHOTOMETRIC if kind == 3 => {
                    if matches!(u16_at(entry + 8), Some(32803 | 34892)) {
                        sensor_data = true;
                    }
                }
                DNG_VERSION => dng = true,
                SUB_IFDS if kind == 4 || kind == 13 => {
                    let n = n.min(8) as usize;
                    let Some(at) = (if n == 1 {
                        Some(entry + 8)
                    } else {
                        u32_at(entry + 8).map(|o| o as usize)
                    }) else {
                        break;
                    };
                    for j in 0..n {
                        if let Some(off) = u32_at(at + j * 4) {
                            queue.push(off as usize);
                        }
                    }
                }
                _ => {}
            }
        }
        if deep && strips {
            sensor_data = true;
        }
        if let Some(next) = u32_at(ifd + 2 + count * 12) {
            if next != 0 {
                queue.push(next as usize);
            }
        }
    }
    if dng {
        return Some(Format::Dng);
    }
    // Leaf backs name themselves only in XMP; their private metadata
    // tag is the mark.
    if leaf && make.is_empty() {
        return Some(Format::Mos);
    }
    let make = make.to_ascii_uppercase();
    let by_make = if make.starts_with("NIKON") {
        Format::Nef
    } else if make.starts_with("SONY") {
        Format::Arw
    } else if make.starts_with("CANON") {
        Format::Cr2
    } else if make.starts_with("PENTAX") || make.starts_with("RICOH") || make.starts_with("ASAHI") {
        Format::Pef
    } else if make.starts_with("SAMSUNG") {
        Format::Srw
    } else if make.starts_with("KODAK") || make.starts_with("EASTMAN") {
        Format::Kodak
    } else if make.starts_with("SEIKO EPSON") || make.starts_with("EPSON") {
        Format::Erf
    } else if make.starts_with("MAMIYA") {
        Format::Mef
    } else if make.starts_with("HASSELBLAD") {
        Format::Hasselblad
    } else if make.starts_with("LEAF") {
        Format::Mos
    } else if make.starts_with("PHASE ONE") {
        Format::Iiq
    } else if make.starts_with("OLYMPUS") || make.starts_with("OM DIGITAL") {
        Format::Orf
    } else if make.starts_with("PANASONIC") || make.starts_with("LEICA") {
        Format::Rw2
    } else if make.starts_with("MINOLTA") || make.starts_with("KONICA") {
        Format::Mrw
    } else {
        return None;
    };
    // Sony ARW, Samsung SRW, Leaf MOS and Hasselblad files mark their
    // sensor IFDs as CFA; a converter's TIFF with the same Make does
    // not. Nikon, Canon and Pentax likewise. The makers whose raws do
    // not use the CFA photometric are trusted on make alone.
    let cfa_optional = matches!(
        by_make,
        Format::Mos | Format::Kodak | Format::Mef | Format::Erf
    ) || (by_make == Format::Arw && private);
    (sensor_data || cfa_optional).then_some(by_make)
}

/// The camera's orientation tag without decoding sensor data, for
/// turning the embedded preview upright: IFD0's tag for the
/// TIFF-shaped formats, the CMT1 block for CR3, the embedded JPEG's
/// EXIF for RAF. `Normal` for the containers that carry none, and for
/// anything unreadable.
pub fn orientation(bytes: &[u8]) -> Orientation {
    fn tiff_orientation(bytes: &[u8], base: usize) -> Option<Orientation> {
        let tiff = tiff::Tiff::parse_embedded(bytes, base).ok()?;
        Some(formats::common::orientation(&tiff))
    }
    /// The orientation inside a JPEG's Exif APP1 segment.
    fn jpeg_orientation(jpeg: &[u8], base: usize) -> Option<Orientation> {
        let mut at = 2;
        while at + 4 <= jpeg.len() && jpeg[at] == 0xFF {
            let marker = jpeg[at + 1];
            let len = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
            if marker == 0xE1 && jpeg.get(at + 4..at + 10) == Some(b"Exif\0\0") {
                return tiff_orientation(jpeg, base + at + 10);
            }
            if marker == 0xDA {
                break;
            }
            at += 2 + len;
        }
        None
    }
    let found = match probe(bytes) {
        Some(Format::Cr3) => bmff::parse(bytes).ok().and_then(|boxes| {
            boxes
                .iter()
                .flat_map(|b| std::iter::once(b).chain(b.find_all(b"CMT1")))
                .find(|b| &b.kind == b"CMT1")
                .and_then(|cmt1| tiff_orientation(bytes, cmt1.data.start))
        }),
        Some(Format::Raf) => {
            let field = |at: usize| -> Option<usize> {
                Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize)
            };
            field(84)
                .zip(field(88))
                .and_then(|(at, len)| bytes.get(at..at.checked_add(len)?))
                .and_then(|jpeg| jpeg_orientation(jpeg, 0))
        }
        Some(Format::Crw) | Some(Format::X3f) | Some(Format::Mrw) | None => None,
        Some(_) => tiff_orientation(bytes, 0),
    };
    found.unwrap_or_default()
}

/// Decode a raw file into its sensor data and metadata.
pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let format = probe(bytes).ok_or(Error::NotRaw)?;
    let mut raw = match format {
        Format::Dng => formats::dng::decode(bytes)?,
        Format::Nef => formats::nef::decode(bytes)?,
        Format::Arw => formats::arw::decode(bytes)?,
        Format::Cr2 => formats::cr2::decode(bytes)?,
        Format::Crw => formats::crw::decode(bytes)?,
        Format::Cr3 => formats::cr3::decode(bytes)?,
        Format::Raf => formats::raf::decode(bytes)?,
        Format::Orf => formats::orf::decode(bytes)?,
        Format::Rw2 => formats::rw2::decode(bytes)?,
        Format::Pef => formats::pef::decode(bytes)?,
        Format::Srw => formats::srw::decode(bytes)?,
        Format::Mrw => formats::mrw::decode(bytes)?,
        Format::Kodak => formats::kodak::decode(bytes)?,
        Format::Erf => formats::erf::decode(bytes)?,
        Format::Mef => formats::mef::decode(bytes)?,
        Format::Iiq => formats::iiq::decode(bytes)?,
        Format::Hasselblad => formats::hasselblad::decode(bytes)?,
        Format::Mos => formats::mos::decode(bytes)?,
        Format::X3f => formats::x3f::decode(bytes)?,
    };
    raw.format = format;
    raw.validate()?;
    Ok(raw)
}

/// The camera's embedded JPEG preview, without decoding the sensor
/// data. Decoders that can find it cheaply implement
/// `formats::<x>::preview`; the default goes through `decode`.
pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let format = probe(bytes).ok_or(Error::NotRaw)?;
    match format {
        Format::Dng => formats::dng::preview(bytes),
        Format::Nef => formats::nef::preview(bytes),
        Format::Arw => formats::arw::preview(bytes),
        Format::Cr2 => formats::cr2::preview(bytes),
        Format::Crw => formats::crw::preview(bytes),
        Format::Cr3 => formats::cr3::preview(bytes),
        Format::Raf => formats::raf::preview(bytes),
        Format::Orf => formats::orf::preview(bytes),
        Format::Rw2 => formats::rw2::preview(bytes),
        Format::Pef => formats::pef::preview(bytes),
        Format::Srw => formats::srw::preview(bytes),
        Format::Mrw => formats::mrw::preview(bytes),
        Format::Kodak => formats::kodak::preview(bytes),
        Format::Erf => formats::erf::preview(bytes),
        Format::Mef => formats::mef::preview(bytes),
        Format::Iiq => formats::iiq::preview(bytes),
        Format::Hasselblad => formats::hasselblad::preview(bytes),
        Format::Mos => formats::mos::preview(bytes),
        Format::X3f => formats::x3f::preview(bytes),
    }
}
