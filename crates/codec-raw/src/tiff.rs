//! TIFF structure: the container of DNG and most vendor raws.
//!
//! Classic and BigTIFF headers in both byte orders, the vendor header
//! variants that are TIFF under another signature (Olympus `IIRO` /
//! `IIRS` / `MMOR`, Panasonic `IIU\0`), the IFD chain, SubIFDs, the
//! Exif IFD, and the GPS IFD. Makernotes are *not* parsed here: each
//! vendor's is its own dialect, and the vendor module parses it with
//! [`Tiff::parse_at`] or its own reader.
//!
//! Every accessor is bounds-checked and never panics on a hostile
//! file; a truncated IFD is simply an IFD with fewer entries.

use crate::{Error, Result};

/// TIFF field types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Byte = 1,
    Ascii = 2,
    Short = 3,
    Long = 4,
    Rational = 5,
    SByte = 6,
    Undefined = 7,
    SShort = 8,
    SLong = 9,
    SRational = 10,
    Float = 11,
    Double = 12,
    Ifd = 13,
    Long8 = 16,
    SLong8 = 17,
    Ifd8 = 18,
}

impl Kind {
    pub fn from_u16(v: u16) -> Option<Kind> {
        Some(match v {
            1 => Kind::Byte,
            2 => Kind::Ascii,
            3 => Kind::Short,
            4 => Kind::Long,
            5 => Kind::Rational,
            6 => Kind::SByte,
            7 => Kind::Undefined,
            8 => Kind::SShort,
            9 => Kind::SLong,
            10 => Kind::SRational,
            11 => Kind::Float,
            12 => Kind::Double,
            13 => Kind::Ifd,
            16 => Kind::Long8,
            17 => Kind::SLong8,
            18 => Kind::Ifd8,
            _ => return None,
        })
    }
    /// Bytes per element.
    pub fn size(self) -> usize {
        match self {
            Kind::Byte | Kind::Ascii | Kind::SByte | Kind::Undefined => 1,
            Kind::Short | Kind::SShort => 2,
            Kind::Long | Kind::SLong | Kind::Float | Kind::Ifd => 4,
            Kind::Rational
            | Kind::SRational
            | Kind::Double
            | Kind::Long8
            | Kind::SLong8
            | Kind::Ifd8 => 8,
        }
    }
}

/// One IFD entry, with its value decoded eagerly (byte order applied)
/// and its position kept for the blobs a vendor wants to re-read.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub tag: u16,
    pub kind: Kind,
    pub count: usize,
    /// Absolute offset of the value bytes in the file (inside the entry
    /// when they fit, else where the entry points).
    pub offset: usize,
    pub value: Value,
}

/// Entry values, one vector per type family. Rationals are kept as
/// pairs; use [`Entry::f64`] for the quotient.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Byte(Vec<u8>),
    Ascii(String),
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    SByte(Vec<i8>),
    Undefined(Vec<u8>),
    SShort(Vec<i16>),
    SLong(Vec<i32>),
    SRational(Vec<(i32, i32)>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    Long8(Vec<u64>),
    SLong8(Vec<i64>),
}

impl Entry {
    /// Element `i` as an unsigned integer, for any integer kind
    /// (`Ifd` counts as `Long`). `None` past the end or for non-integers.
    pub fn u32(&self, i: usize) -> Option<u32> {
        // Signed kinds are widened through i64 so a negative value
        // saturates to 0 rather than wrapping to something enormous:
        // callers use this for offsets and counts.
        self.u64(i).map(|v| v.min(u32::MAX as u64) as u32)
    }

    pub fn u64(&self, i: usize) -> Option<u64> {
        let signed = |v: i64| -> u64 { v.max(0) as u64 };
        Some(match &self.value {
            Value::Byte(v) | Value::Undefined(v) => *v.get(i)? as u64,
            Value::Short(v) => *v.get(i)? as u64,
            Value::Long(v) => *v.get(i)? as u64,
            Value::Long8(v) => *v.get(i)?,
            Value::SByte(v) => signed(*v.get(i)? as i64),
            Value::SShort(v) => signed(*v.get(i)? as i64),
            Value::SLong(v) => signed(*v.get(i)? as i64),
            Value::SLong8(v) => signed(*v.get(i)?),
            // Rationals, floats and text are not integers: a caller
            // that wants their numeric value asks for `f64`.
            Value::Ascii(_)
            | Value::Rational(_)
            | Value::SRational(_)
            | Value::Float(_)
            | Value::Double(_) => return None,
        })
    }

    /// Element `i` as a float: rationals divided out, integers widened.
    pub fn f64(&self, i: usize) -> Option<f64> {
        Some(match &self.value {
            Value::Rational(v) => {
                let (n, d) = *v.get(i)?;
                // A zero denominator is EXIF's way of saying "unknown"
                // (Nikon writes 0/0 for an unset lens aperture).
                if d == 0 {
                    return None;
                }
                n as f64 / d as f64
            }
            Value::SRational(v) => {
                let (n, d) = *v.get(i)?;
                if d == 0 {
                    return None;
                }
                n as f64 / d as f64
            }
            Value::Float(v) => *v.get(i)? as f64,
            Value::Double(v) => *v.get(i)?,
            Value::SByte(v) => *v.get(i)? as f64,
            Value::SShort(v) => *v.get(i)? as f64,
            Value::SLong(v) => *v.get(i)? as f64,
            Value::SLong8(v) => *v.get(i)? as f64,
            Value::Byte(v) | Value::Undefined(v) => *v.get(i)? as f64,
            Value::Short(v) => *v.get(i)? as f64,
            Value::Long(v) => *v.get(i)? as f64,
            Value::Long8(v) => *v.get(i)? as f64,
            Value::Ascii(_) => return None,
        })
    }

    /// The string of an ASCII entry, NUL-terminated and trimmed.
    pub fn str(&self) -> Option<&str> {
        match &self.value {
            // TIFF ASCII is NUL-terminated and cameras pad with spaces
            // ("NIKON CORPORATION\0", "Canon EOS 50D\0"); some write
            // several NUL-separated strings in one entry, of which the
            // first is the one anybody means.
            Value::Ascii(s) => Some(s.split('\0').next().unwrap_or("").trim()),
            _ => None,
        }
    }

    /// Every integer element widened, for offset/count lists.
    ///
    /// BigTIFF offsets can exceed 32 bits; use [`Entry::u64s`] there.
    pub fn u32s(&self) -> Vec<u32> {
        self.u64s()
            .into_iter()
            .map(|v| v.min(u32::MAX as u64) as u32)
            .collect()
    }

    /// The same as [`Entry::u32s`] without the 32-bit ceiling.
    pub fn u64s(&self) -> Vec<u64> {
        (0..self.count).map_while(|i| self.u64(i)).collect()
    }

    /// The raw bytes of a `Byte`/`Undefined` entry (or the ASCII bytes).
    pub fn bytes(&self) -> Option<&[u8]> {
        match &self.value {
            Value::Byte(v) | Value::Undefined(v) => Some(v),
            Value::Ascii(s) => Some(s.as_bytes()),
            _ => None,
        }
    }
}

/// One image file directory with the directories it points to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ifd {
    /// Absolute offset of this IFD.
    pub offset: usize,
    pub entries: Vec<Entry>,
    /// Tag 0x014A, in order.
    pub sub_ifds: Vec<Ifd>,
    /// Tag 0x8769.
    pub exif: Option<Box<Ifd>>,
    /// Tag 0x8825.
    pub gps: Option<Box<Ifd>>,
}

impl Ifd {
    pub fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }
    pub fn has(&self, tag: u16) -> bool {
        self.get(tag).is_some()
    }
    /// This IFD, then its SubIFDs (depth-first), then Exif, then GPS.
    pub fn walk(&self) -> Vec<&Ifd> {
        let mut out = vec![self];
        for sub in &self.sub_ifds {
            out.extend(sub.walk());
        }
        if let Some(exif) = &self.exif {
            out.extend(exif.walk());
        }
        if let Some(gps) = &self.gps {
            out.extend(gps.walk());
        }
        out
    }
}

/// A parsed TIFF: the header and every IFD reachable from it.
#[derive(Debug, Clone)]
pub struct Tiff<'a> {
    bytes: &'a [u8],
    little_endian: bool,
    big_tiff: bool,
    /// Where this TIFF's offsets are measured from: 0 for a plain
    /// file, the header position for an embedded one, the makernote's
    /// own start for the dialects that number from there.
    base: usize,
    /// The IFD chain from the header (IFD0, IFD1, ...).
    pub ifds: Vec<Ifd>,
}

/// Limits that stop a hostile or truncated file from making the parser
/// loop or allocate without bound. Real raws are far below all of
/// them: the fattest IFD in the sample corpus has a few hundred
/// entries and the largest eagerly decoded value is a makernote of a
/// few hundred kilobytes.
const MAX_IFDS_PER_CHAIN: usize = 32;
const MAX_IFDS_TOTAL: usize = 256;
const MAX_ENTRIES: usize = 4096;
const MAX_SUB_IFDS: usize = 32;
const MAX_DEPTH: usize = 4;
const MAX_VALUE_BYTES: usize = 32 << 20;
const VALUE_BUDGET: usize = 64 << 20;

/// The mechanics of walking IFDs, shared by every entry point.
struct Parser<'a> {
    bytes: &'a [u8],
    le: bool,
    big: bool,
    base: usize,
    ifds_left: usize,
    value_budget: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8], le: bool, big: bool, base: usize) -> Parser<'a> {
        Parser {
            bytes,
            le,
            big,
            base,
            ifds_left: MAX_IFDS_TOTAL,
            value_budget: VALUE_BUDGET,
        }
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn u64(&self, at: usize) -> Option<u64> {
        let b: [u8; 8] = self.bytes.get(at..at.checked_add(8)?)?.try_into().ok()?;
        Some(if self.le {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    }

    /// An offset field as the file stores it: 4 bytes classic, 8 in
    /// BigTIFF. Kept raw because 0 means "no more" for the next-IFD
    /// pointer, which `base` must not turn into a real position.
    fn offset_raw(&self, at: usize) -> Option<u64> {
        if self.big {
            self.u64(at)
        } else {
            self.u32(at).map(|v| v as u64)
        }
    }

    /// A file offset turned into a position in `bytes`.
    fn absolute(&self, raw: u64) -> Option<usize> {
        usize::try_from(raw).ok()?.checked_add(self.base)
    }

    /// Follow a chain of IFDs from `start`. `ancestors` holds every
    /// IFD offset already on the path here, so a file whose next-IFD
    /// pointer loops back (a real corruption, and a favourite of
    /// fuzzers) simply ends the chain.
    fn chain(&mut self, start: usize, ancestors: &[usize], depth: usize) -> Vec<Ifd> {
        let mut visited: Vec<usize> = ancestors.to_vec();
        let mut out = Vec::new();
        let mut at = start;
        while out.len() < MAX_IFDS_PER_CHAIN {
            if self.ifds_left == 0 || visited.contains(&at) {
                break;
            }
            visited.push(at);
            self.ifds_left -= 1;
            let Some((ifd, next)) = self.ifd(at, &visited, depth) else {
                break;
            };
            out.push(ifd);
            if next == 0 {
                break;
            }
            match self.absolute(next) {
                Some(next) => at = next,
                None => break,
            }
        }
        out
    }

    /// One IFD, ignoring any next-IFD pointer: SubIFDs, Exif and GPS
    /// are single directories, not chains.
    fn one(&mut self, at: usize, ancestors: &[usize], depth: usize) -> Option<Ifd> {
        if depth > MAX_DEPTH || self.ifds_left == 0 || ancestors.contains(&at) {
            return None;
        }
        self.ifds_left -= 1;
        let mut path = ancestors.to_vec();
        path.push(at);
        self.ifd(at, &path, depth).map(|(ifd, _)| ifd)
    }

    /// One IFD and the raw next-IFD pointer that follows it.
    fn ifd(&mut self, at: usize, path: &[usize], depth: usize) -> Option<(Ifd, u64)> {
        let (count, header, entry_size) = if self.big {
            (self.u64(at)? as usize, 8, 20)
        } else {
            (self.u16(at)? as usize, 2, 12)
        };
        // A count past the cap is either corruption or a file we would
        // spend forever on; keep what fits and stop the chain there
        // (the next-IFD pointer would be somewhere else entirely).
        let clamped = count.min(MAX_ENTRIES);
        let mut entries = Vec::with_capacity(clamped.min(256));
        for i in 0..clamped {
            let at = match at.checked_add(header + i * entry_size) {
                Some(at) => at,
                None => break,
            };
            // A truncated IFD is an IFD with fewer entries.
            let (Some(tag), Some(kind), Some(count)) = (
                self.u16(at),
                self.u16(at + 2),
                if self.big {
                    self.u64(at + 4)
                } else {
                    self.u32(at + 4).map(|v| v as u64)
                },
            ) else {
                break;
            };
            // An unknown field type carries an unknown element size, so
            // the value cannot be found at all: drop the entry.
            let Some(kind) = Kind::from_u16(kind) else {
                continue;
            };
            let value_field = at + 4 + if self.big { 8 } else { 4 };
            let Ok(count) = usize::try_from(count) else {
                continue;
            };
            let Some(total) = count.checked_mul(kind.size()) else {
                continue;
            };
            // Values up to the size of the offset field live inside the
            // entry; anything larger is pointed at.
            let offset = if total <= if self.big { 8 } else { 4 } {
                value_field
            } else {
                match self
                    .offset_raw(value_field)
                    .and_then(|raw| self.absolute(raw))
                {
                    Some(offset) => offset,
                    None => continue,
                }
            };
            let value = self.value(kind, total, offset);
            entries.push(Entry {
                tag,
                kind,
                count,
                offset,
                value,
            });
        }

        let mut ifd = Ifd {
            offset: at,
            entries,
            ..Default::default()
        };
        self.children(&mut ifd, path, depth);

        let next = if count > clamped {
            0
        } else {
            at.checked_add(header + count * entry_size)
                .and_then(|at| self.offset_raw(at))
                .unwrap_or(0)
        };
        Some((ifd, next))
    }

    /// The directories an IFD points at: SubIFDs (0x014A), Exif
    /// (0x8769) and GPS (0x8825).
    fn children(&mut self, ifd: &mut Ifd, path: &[usize], depth: usize) {
        // SubIFDs is LONG in the DNG specification but IFD (type 13) in
        // plenty of real files, and holds one offset or many.
        let subs: Vec<u64> = ifd
            .get(tags::SUB_IFDS)
            .filter(|e| matches!(e.kind, Kind::Long | Kind::Ifd | Kind::Long8 | Kind::Ifd8))
            .map(|e| e.u64s())
            .unwrap_or_default();
        for raw in subs.into_iter().take(MAX_SUB_IFDS) {
            let Some(at) = self.absolute(raw) else {
                continue;
            };
            if let Some(sub) = self.one(at, path, depth + 1) {
                ifd.sub_ifds.push(sub);
            }
        }
        for (tag, slot) in [(tags::EXIF_IFD, 0), (tags::GPS_IFD, 1)] {
            let Some(raw) = ifd.get(tag).and_then(|e| e.u64(0)) else {
                continue;
            };
            let Some(at) = self.absolute(raw) else {
                continue;
            };
            if let Some(child) = self.one(at, path, depth + 1) {
                if slot == 0 {
                    ifd.exif = Some(Box::new(child));
                } else {
                    ifd.gps = Some(Box::new(child));
                }
            }
        }
    }

    /// Decode one entry's value. Anything that does not lie inside the
    /// file, or that would blow the parser's memory budget, becomes an
    /// empty value of the right kind: the tag is still visible (a
    /// decoder can see that the file claims it) but reads nothing.
    fn value(&mut self, kind: Kind, total: usize, at: usize) -> Value {
        let empty = empty_value(kind);
        if total > MAX_VALUE_BYTES || total > self.value_budget {
            return empty;
        }
        let Some(end) = at.checked_add(total) else {
            return empty;
        };
        let Some(raw) = self.bytes.get(at..end) else {
            return empty;
        };
        self.value_budget -= total;
        decode_value(kind, raw, self.le)
    }
}

fn empty_value(kind: Kind) -> Value {
    match kind {
        Kind::Byte => Value::Byte(Vec::new()),
        Kind::Ascii => Value::Ascii(String::new()),
        Kind::Short => Value::Short(Vec::new()),
        Kind::Long | Kind::Ifd => Value::Long(Vec::new()),
        Kind::Rational => Value::Rational(Vec::new()),
        Kind::SByte => Value::SByte(Vec::new()),
        Kind::Undefined => Value::Undefined(Vec::new()),
        Kind::SShort => Value::SShort(Vec::new()),
        Kind::SLong => Value::SLong(Vec::new()),
        Kind::SRational => Value::SRational(Vec::new()),
        Kind::Float => Value::Float(Vec::new()),
        Kind::Double => Value::Double(Vec::new()),
        Kind::Long8 | Kind::Ifd8 => Value::Long8(Vec::new()),
        Kind::SLong8 => Value::SLong8(Vec::new()),
    }
}

/// Turn `raw` (a whole number of elements of `kind`) into a
/// [`Value`], byte order applied.
fn decode_value(kind: Kind, raw: &[u8], le: bool) -> Value {
    macro_rules! ints {
        ($t:ty, $n:literal) => {
            raw.chunks_exact($n)
                .map(|c| {
                    let b: [u8; $n] = c.try_into().unwrap_or([0; $n]);
                    if le {
                        <$t>::from_le_bytes(b)
                    } else {
                        <$t>::from_be_bytes(b)
                    }
                })
                .collect()
        };
    }
    // Rationals are two 32-bit halves each, numerator first.
    macro_rules! rationals {
        ($t:ty) => {
            raw.chunks_exact(8)
                .map(|c| {
                    let n: [u8; 4] = c[0..4].try_into().unwrap_or([0; 4]);
                    let d: [u8; 4] = c[4..8].try_into().unwrap_or([0; 4]);
                    if le {
                        (<$t>::from_le_bytes(n), <$t>::from_le_bytes(d))
                    } else {
                        (<$t>::from_be_bytes(n), <$t>::from_be_bytes(d))
                    }
                })
                .collect()
        };
    }
    match kind {
        Kind::Byte => Value::Byte(raw.to_vec()),
        Kind::Undefined => Value::Undefined(raw.to_vec()),
        // Not necessarily UTF-8: cameras have shipped Latin-1 and
        // Shift-JIS in Artist and Copyright. Lossy keeps the string
        // usable instead of dropping it.
        // A lossy conversion can triple the size (every bad byte becomes
        // a 3-byte replacement), so over-long strings are cut at the
        // value cap to keep the budget honest.
        Kind::Ascii => {
            let mut text = String::from_utf8_lossy(raw).into_owned();
            if text.len() > MAX_VALUE_BYTES {
                let mut cut = MAX_VALUE_BYTES;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
            }
            Value::Ascii(text)
        }
        Kind::SByte => Value::SByte(raw.iter().map(|b| *b as i8).collect()),
        Kind::Short => Value::Short(ints!(u16, 2)),
        Kind::SShort => Value::SShort(ints!(i16, 2)),
        Kind::Long | Kind::Ifd => Value::Long(ints!(u32, 4)),
        Kind::SLong => Value::SLong(ints!(i32, 4)),
        Kind::Float => Value::Float(ints!(f32, 4)),
        Kind::Double => Value::Double(ints!(f64, 8)),
        Kind::Long8 | Kind::Ifd8 => Value::Long8(ints!(u64, 8)),
        Kind::SLong8 => Value::SLong8(ints!(i64, 8)),
        Kind::Rational => Value::Rational(rationals!(u32)),
        Kind::SRational => Value::SRational(rationals!(i32)),
    }
}

/// The byte order and flavour a TIFF header names.
fn header(bytes: &[u8], base: usize) -> Result<(bool, bool)> {
    let sig = bytes
        .get(base..base + 4)
        .ok_or_else(|| Error::Corrupt("file too short for a TIFF header".into()))?;
    Ok(match [sig[0], sig[1], sig[2], sig[3]] {
        [b'I', b'I', 42, 0] => (true, false),
        [b'M', b'M', 0, 42] => (false, false),
        // BigTIFF's magic is 43; its header carries the offset size.
        [b'I', b'I', 43, 0] => (true, true),
        [b'M', b'M', 0, 43] => (false, true),
        // Olympus stamps ORF with its own magic (and changed it twice:
        // "RO" on the E-1 era, "RS" on later bodies) and Panasonic
        // stamps RW2 with "U\0". Both are ordinary little-endian TIFF
        // behind the signature, offset to IFD0 at byte 4 as usual.
        [b'I', b'I', b'R', b'O'] | [b'I', b'I', b'R', b'S'] | [b'I', b'I', b'U', 0] => {
            (true, false)
        }
        // The big-endian Olympus variant, on a handful of bodies.
        [b'M', b'M', b'O', b'R'] => (false, false),
        _ => return Err(Error::Corrupt(format!("not a TIFF header: {:02x?}", sig))),
    })
}

impl<'a> Tiff<'a> {
    /// Parse from the header at byte 0. Accepts `II*\0`, `MM\0*`, the
    /// BigTIFF magics, and the Olympus/Panasonic signatures; the IFD
    /// chain is followed (bounded, loop-safe), SubIFDs and the Exif and
    /// GPS IFDs are parsed recursively.
    pub fn parse(bytes: &'a [u8]) -> Result<Tiff<'a>> {
        Tiff::parse_embedded(bytes, 0)
    }

    /// Parse a TIFF whose header sits at `base` inside `bytes` (an
    /// embedded TIFF, as makernotes and Canon CR3's CMT boxes hold),
    /// with all offsets inside it relative to `base`.
    pub fn parse_embedded(bytes: &'a [u8], base: usize) -> Result<Tiff<'a>> {
        let (le, big) = header(bytes, base)?;
        let mut parser = Parser::new(bytes, le, big, base);
        let first = if big {
            // BigTIFF: u16 offset size (always 8 so far), u16 zero,
            // then the 64-bit offset to IFD0.
            match parser.u16(base + 4) {
                Some(8) => {}
                Some(other) => {
                    return Err(Error::Unsupported(format!(
                        "BigTIFF with {other}-byte offsets"
                    )))
                }
                None => return Err(Error::Corrupt("truncated BigTIFF header".into())),
            }
            parser
                .u64(base + 8)
                .ok_or_else(|| Error::Corrupt("truncated BigTIFF header".into()))?
        } else {
            parser
                .u32(base + 4)
                .ok_or_else(|| Error::Corrupt("truncated TIFF header".into()))? as u64
        };
        let start = parser
            .absolute(first)
            .ok_or_else(|| Error::Corrupt("IFD0 offset out of range".into()))?;
        let ifds = parser.chain(start, &[], 0);
        Tiff::finish(bytes, le, big, base, ifds)
    }

    /// Parse one IFD chain starting at an absolute offset, with a given
    /// byte order and no header — for makernote IFDs that share the
    /// file's byte order and offsets (Nikon's after its own header,
    /// Pentax's, Sony's).
    pub fn parse_at(bytes: &'a [u8], offset: usize, little_endian: bool) -> Result<Tiff<'a>> {
        Tiff::parse_at_relative(bytes, offset, 0, little_endian)
    }

    /// The same, for makernotes whose offsets are relative to their own
    /// start rather than the file: entries are read from `bytes` but
    /// every offset has `base` added.
    pub fn parse_at_relative(
        bytes: &'a [u8],
        offset: usize,
        base: usize,
        little_endian: bool,
    ) -> Result<Tiff<'a>> {
        let mut parser = Parser::new(bytes, little_endian, false, base);
        let ifds = parser.chain(offset, &[], 0);
        Tiff::finish(bytes, little_endian, false, base, ifds)
    }

    /// Every entry point ends here: a TIFF with no IFD at all is not
    /// one, and `root()` may then assume there is an IFD0.
    fn finish(
        bytes: &'a [u8],
        le: bool,
        big: bool,
        base: usize,
        ifds: Vec<Ifd>,
    ) -> Result<Tiff<'a>> {
        if ifds.is_empty() {
            return Err(Error::Corrupt("no readable IFD".into()));
        }
        Ok(Tiff {
            bytes,
            little_endian: le,
            big_tiff: big,
            base,
            ifds,
        })
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
    pub fn little_endian(&self) -> bool {
        self.little_endian
    }
    pub fn big_tiff(&self) -> bool {
        self.big_tiff
    }
    /// What this TIFF's stored offsets are relative to; add it to any
    /// offset read out of an entry's value (`StripOffsets` and friends)
    /// to get a position in [`Tiff::bytes`].
    pub fn base(&self) -> usize {
        self.base
    }
    /// IFD0.
    pub fn root(&self) -> &Ifd {
        &self.ifds[0]
    }
    /// Every IFD in the file, chain order then depth-first.
    pub fn all(&self) -> Vec<&Ifd> {
        self.ifds.iter().flat_map(|i| i.walk()).collect()
    }
    /// The first entry with `tag` anywhere in the file.
    pub fn find(&self, tag: u16) -> Option<&Entry> {
        self.all().into_iter().find_map(|i| i.get(tag))
    }
    /// The first IFD anywhere holding `tag`.
    pub fn find_ifd(&self, tag: u16) -> Option<&Ifd> {
        self.all().into_iter().find(|i| i.has(tag))
    }
    /// The first Exif IFD found.
    pub fn exif(&self) -> Option<&Ifd> {
        self.all().into_iter().find_map(|i| i.exif.as_deref())
    }
    /// Make (0x010F) and Model (0x0110) from IFD0, empty when absent.
    pub fn make_model(&self) -> (String, String) {
        let s = |tag| {
            self.root()
                .get(tag)
                .and_then(|e| e.str())
                .unwrap_or("")
                .to_string()
        };
        (s(0x010F), s(0x0110))
    }

    // Byte-order-aware readers at absolute offsets, for decoders that
    // walk the file themselves.
    pub fn u8_at(&self, at: usize) -> Option<u8> {
        self.bytes.get(at).copied()
    }
    pub fn u16_at(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if self.little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }
    pub fn u32_at(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if self.little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }
    pub fn u64_at(&self, at: usize) -> Option<u64> {
        let b: [u8; 8] = self.bytes.get(at..at + 8)?.try_into().ok()?;
        Some(if self.little_endian {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    }
}

/// Strip or tile layout of an image IFD, resolved to absolute byte
/// ranges, for decoders that read pixel data from a TIFF IFD.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageLayout {
    pub width: usize,
    pub height: usize,
    pub bits_per_sample: u32,
    pub samples_per_pixel: usize,
    pub compression: u32,
    pub photometric: u32,
    /// Each chunk (strip or tile) as `(offset, length)`, in order.
    pub chunks: Vec<(usize, usize)>,
    /// Tile dimensions when tiled; `None` for strips (then
    /// `rows_per_chunk` is RowsPerStrip).
    pub tile: Option<(usize, usize)>,
    pub rows_per_chunk: usize,
}

impl ImageLayout {
    /// Read ImageWidth/Length, BitsPerSample, SamplesPerPixel,
    /// Compression, PhotometricInterpretation and the strip or tile
    /// offsets/byte counts. `Err(Corrupt)` when a chunk lies outside
    /// the file.
    pub fn of(tiff: &Tiff<'_>, ifd: &Ifd) -> Result<ImageLayout> {
        let int = |tag: u16| ifd.get(tag).and_then(|e| e.u32(0));
        let width = int(tags::IMAGE_WIDTH)
            .ok_or_else(|| Error::Corrupt("image IFD without ImageWidth".into()))?;
        let height = int(tags::IMAGE_LENGTH)
            .ok_or_else(|| Error::Corrupt("image IFD without ImageLength".into()))?;
        // BitsPerSample is per sample; raw IFDs are single-sample, and
        // where the tag is missing (old Kodak and Minolta thumbnails)
        // 8 is the only value that has ever been meant. SamplesPerPixel
        // and Compression take the TIFF defaults.
        let bits_per_sample = int(tags::BITS_PER_SAMPLE).unwrap_or(8);
        let samples_per_pixel = int(tags::SAMPLES_PER_PIXEL).unwrap_or(1) as usize;
        let compression = int(tags::COMPRESSION).unwrap_or(1);
        let photometric = int(tags::PHOTOMETRIC).unwrap_or(1);

        let tiled = ifd.has(tags::TILE_OFFSETS);
        let (offsets, counts) = if tiled {
            (
                ifd.get(tags::TILE_OFFSETS)
                    .map(|e| e.u64s())
                    .unwrap_or_default(),
                ifd.get(tags::TILE_BYTE_COUNTS)
                    .map(|e| e.u64s())
                    .unwrap_or_default(),
            )
        } else {
            (
                ifd.get(tags::STRIP_OFFSETS)
                    .map(|e| e.u64s())
                    .unwrap_or_default(),
                ifd.get(tags::STRIP_BYTE_COUNTS)
                    .map(|e| e.u64s())
                    .unwrap_or_default(),
            )
        };
        if offsets.is_empty() {
            return Err(Error::Corrupt(
                "image IFD with no strip or tile offsets".into(),
            ));
        }
        let file_len = tiff.bytes().len();
        let base = tiff.base();
        // A single strip with no byte count runs to the end of the file:
        // several pre-2005 raws (and Olympus's C-series ORF) omit
        // StripByteCounts entirely.
        let counts = if counts.len() == offsets.len() {
            counts
        } else if counts.is_empty() && offsets.len() == 1 {
            let start = base.saturating_add(offsets[0] as usize);
            if start > file_len {
                return Err(Error::Corrupt(
                    "strip offset past the end of the file".into(),
                ));
            }
            vec![(file_len - start) as u64]
        } else {
            return Err(Error::Corrupt(format!(
                "{} chunk offsets but {} byte counts",
                offsets.len(),
                counts.len()
            )));
        };

        // Reserve for what a real file could hold, not what the count
        // claims: a strip needs a byte, so the file length bounds it.
        let mut chunks = Vec::with_capacity(offsets.len().min(file_len));
        for (offset, count) in offsets.iter().zip(counts.iter()) {
            let start = usize::try_from(*offset)
                .ok()
                .and_then(|o| o.checked_add(base))
                .ok_or_else(|| Error::Corrupt("chunk offset out of range".into()))?;
            let len = usize::try_from(*count)
                .map_err(|_| Error::Corrupt("chunk length out of range".into()))?;
            let end = start
                .checked_add(len)
                .ok_or_else(|| Error::Corrupt("chunk length out of range".into()))?;
            if end > file_len {
                return Err(Error::Corrupt(format!(
                    "chunk {start}..{end} lies outside the {file_len}-byte file"
                )));
            }
            chunks.push((start, len));
        }

        let tile = tiled.then(|| {
            (
                int(tags::TILE_WIDTH).unwrap_or(0) as usize,
                int(tags::TILE_LENGTH).unwrap_or(0) as usize,
            )
        });
        if let Some((tw, th)) = tile {
            if tw == 0 || th == 0 {
                return Err(Error::Corrupt("tiled image without tile dimensions".into()));
            }
        }
        let rows_per_chunk = match tile {
            Some((_, tile_height)) => tile_height,
            // RowsPerStrip defaults to "the whole image in one strip",
            // which is what raw IFDs almost always are anyway.
            None => int(tags::ROWS_PER_STRIP).unwrap_or(height).max(1) as usize,
        };

        Ok(ImageLayout {
            width: width as usize,
            height: height as usize,
            bits_per_sample,
            samples_per_pixel,
            compression,
            photometric,
            chunks,
            tile,
            rows_per_chunk,
        })
    }
}

/// Tags used across modules.
pub mod tags {
    pub const NEW_SUBFILE_TYPE: u16 = 0x00FE;
    pub const IMAGE_WIDTH: u16 = 0x0100;
    pub const IMAGE_LENGTH: u16 = 0x0101;
    pub const BITS_PER_SAMPLE: u16 = 0x0102;
    pub const COMPRESSION: u16 = 0x0103;
    pub const PHOTOMETRIC: u16 = 0x0106;
    pub const MAKE: u16 = 0x010F;
    pub const MODEL: u16 = 0x0110;
    pub const STRIP_OFFSETS: u16 = 0x0111;
    pub const ORIENTATION: u16 = 0x0112;
    pub const SAMPLES_PER_PIXEL: u16 = 0x0115;
    pub const ROWS_PER_STRIP: u16 = 0x0116;
    pub const STRIP_BYTE_COUNTS: u16 = 0x0117;
    pub const PLANAR_CONFIGURATION: u16 = 0x011C;
    pub const SOFTWARE: u16 = 0x0131;
    pub const DATE_TIME: u16 = 0x0132;
    pub const SUB_IFDS: u16 = 0x014A;
    pub const JPEG_INTERCHANGE_FORMAT: u16 = 0x0201;
    pub const JPEG_INTERCHANGE_FORMAT_LENGTH: u16 = 0x0202;
    pub const TILE_WIDTH: u16 = 0x0142;
    pub const TILE_LENGTH: u16 = 0x0143;
    pub const TILE_OFFSETS: u16 = 0x0144;
    pub const TILE_BYTE_COUNTS: u16 = 0x0145;
    pub const CFA_REPEAT_PATTERN_DIM: u16 = 0x828D;
    pub const CFA_PATTERN: u16 = 0x828E;
    pub const EXIF_IFD: u16 = 0x8769;
    pub const GPS_IFD: u16 = 0x8825;
    pub const EXPOSURE_TIME: u16 = 0x829A;
    pub const F_NUMBER: u16 = 0x829D;
    pub const ISO: u16 = 0x8827;
    pub const DATE_TIME_ORIGINAL: u16 = 0x9003;
    pub const MAKER_NOTE: u16 = 0x927C;
    pub const FOCAL_LENGTH: u16 = 0x920A;
    pub const LENS_MODEL: u16 = 0xA434;
    pub const DNG_VERSION: u16 = 0xC612;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Hand-built TIFFs. Everything here is written by hand rather than
    // by a TIFF library so the tests exercise exactly the bytes a
    // camera writes, including the shapes libraries refuse to produce.
    // ---------------------------------------------------------------

    fn w16(le: bool, v: u16) -> [u8; 2] {
        if le {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }
    fn w32(le: bool, v: u32) -> [u8; 4] {
        if le {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }
    fn w64(le: bool, v: u64) -> [u8; 8] {
        if le {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    }

    enum Payload {
        /// Value bytes, already in the file's byte order.
        Raw(Vec<u8>),
        /// LONG offsets of other IFDs in the same build, filled in once
        /// their positions are known (SubIFDs, Exif, GPS).
        Refs(Vec<usize>),
    }

    struct TestEntry {
        tag: u16,
        kind: u16,
        count: u32,
        payload: Payload,
    }

    enum Next {
        End,
        Ifd(usize),
        Raw(u32),
    }

    struct TestIfd {
        entries: Vec<TestEntry>,
        next: Next,
    }

    fn ifd(entries: Vec<TestEntry>) -> TestIfd {
        TestIfd {
            entries,
            next: Next::End,
        }
    }

    fn ascii(tag: u16, text: &str) -> TestEntry {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        TestEntry {
            tag,
            kind: Kind::Ascii as u16,
            count: bytes.len() as u32,
            payload: Payload::Raw(bytes),
        }
    }
    fn shorts(le: bool, tag: u16, values: &[u16]) -> TestEntry {
        TestEntry {
            tag,
            kind: Kind::Short as u16,
            count: values.len() as u32,
            payload: Payload::Raw(values.iter().flat_map(|v| w16(le, *v)).collect()),
        }
    }
    fn longs(le: bool, tag: u16, values: &[u32]) -> TestEntry {
        TestEntry {
            tag,
            kind: Kind::Long as u16,
            count: values.len() as u32,
            payload: Payload::Raw(values.iter().flat_map(|v| w32(le, *v)).collect()),
        }
    }
    fn rational(le: bool, tag: u16, n: u32, d: u32) -> TestEntry {
        let mut bytes = w32(le, n).to_vec();
        bytes.extend_from_slice(&w32(le, d));
        TestEntry {
            tag,
            kind: Kind::Rational as u16,
            count: 1,
            payload: Payload::Raw(bytes),
        }
    }
    fn refs(tag: u16, kind: Kind, which: Vec<usize>) -> TestEntry {
        TestEntry {
            tag,
            kind: kind as u16,
            count: which.len() as u32,
            payload: Payload::Refs(which),
        }
    }

    /// Assemble a classic TIFF: header, the IFDs laid out in order,
    /// then a heap holding every value too long to sit in an entry.
    fn build(le: bool, ifds: &[TestIfd]) -> Vec<u8> {
        let mut offsets = Vec::new();
        let mut at = 8usize;
        for ifd in ifds {
            offsets.push(at);
            at += 2 + 12 * ifd.entries.len() + 4;
        }
        let heap_start = at;
        let mut heap: Vec<u8> = Vec::new();
        let mut placed: Vec<Vec<(Vec<u8>, Option<u32>)>> = Vec::new();
        for ifd in ifds {
            let mut row = Vec::new();
            for entry in &ifd.entries {
                let bytes = match &entry.payload {
                    Payload::Raw(b) => b.clone(),
                    Payload::Refs(which) => which
                        .iter()
                        .flat_map(|i| w32(le, offsets[*i] as u32))
                        .collect(),
                };
                if bytes.len() <= 4 {
                    row.push((bytes, None));
                } else {
                    let at = (heap_start + heap.len()) as u32;
                    heap.extend_from_slice(&bytes);
                    if heap.len() % 2 == 1 {
                        heap.push(0);
                    }
                    row.push((bytes, Some(at)));
                }
            }
            placed.push(row);
        }

        let mut out = Vec::new();
        out.extend_from_slice(if le { b"II\x2a\x00" } else { b"MM\x00\x2a" });
        out.extend_from_slice(&w32(le, offsets[0] as u32));
        for (i, ifd) in ifds.iter().enumerate() {
            out.extend_from_slice(&w16(le, ifd.entries.len() as u16));
            for (entry, (bytes, at)) in ifd.entries.iter().zip(&placed[i]) {
                out.extend_from_slice(&w16(le, entry.tag));
                out.extend_from_slice(&w16(le, entry.kind));
                out.extend_from_slice(&w32(le, entry.count));
                match at {
                    Some(at) => out.extend_from_slice(&w32(le, *at)),
                    None => {
                        let mut inline = bytes.clone();
                        inline.resize(4, 0);
                        out.extend_from_slice(&inline);
                    }
                }
            }
            let next = match ifd.next {
                Next::End => 0,
                Next::Ifd(i) => offsets[i] as u32,
                Next::Raw(v) => v,
            };
            out.extend_from_slice(&w32(le, next));
        }
        assert_eq!(out.len(), heap_start);
        out.extend_from_slice(&heap);
        out
    }

    /// One BigTIFF IFD: 64-bit counts, an 8-byte value field, and an
    /// 8-byte next pointer.
    fn build_big(le: bool, entries: &[(u16, Kind, u64, Vec<u8>)]) -> Vec<u8> {
        let ifd_at = 16usize;
        let heap_start = ifd_at + 8 + 20 * entries.len() + 8;
        let mut heap: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        out.extend_from_slice(if le { b"II\x2b\x00" } else { b"MM\x00\x2b" });
        out.extend_from_slice(&w16(le, 8));
        out.extend_from_slice(&w16(le, 0));
        out.extend_from_slice(&w64(le, ifd_at as u64));
        out.extend_from_slice(&w64(le, entries.len() as u64));
        for (tag, kind, count, bytes) in entries {
            out.extend_from_slice(&w16(le, *tag));
            out.extend_from_slice(&w16(le, *kind as u16));
            out.extend_from_slice(&w64(le, *count));
            if bytes.len() <= 8 {
                let mut inline = bytes.clone();
                inline.resize(8, 0);
                out.extend_from_slice(&inline);
            } else {
                out.extend_from_slice(&w64(le, (heap_start + heap.len()) as u64));
                heap.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(&w64(le, 0));
        assert_eq!(out.len(), heap_start);
        out.extend_from_slice(&heap);
        out
    }

    #[test]
    fn classic_both_byte_orders() {
        for le in [true, false] {
            let bytes = build(
                le,
                &[ifd(vec![
                    shorts(le, tags::IMAGE_WIDTH, &[4032]),
                    longs(le, tags::IMAGE_LENGTH, &[3024]),
                    ascii(tags::MAKE, "NIKON CORPORATION"),
                    ascii(tags::MODEL, "NIKON D7200"),
                    rational(le, tags::EXPOSURE_TIME, 1, 2000),
                    shorts(le, 0x0102, &[12, 12, 12]),
                ])],
            );
            let tiff = Tiff::parse(&bytes).expect("parses");
            assert_eq!(tiff.little_endian(), le);
            assert!(!tiff.big_tiff());
            assert_eq!(tiff.ifds.len(), 1);
            assert_eq!(
                tiff.make_model(),
                ("NIKON CORPORATION".into(), "NIKON D7200".into())
            );
            assert_eq!(
                tiff.find(tags::IMAGE_WIDTH).and_then(|e| e.u32(0)),
                Some(4032)
            );
            assert_eq!(
                tiff.find(tags::IMAGE_LENGTH).and_then(|e| e.u32(0)),
                Some(3024)
            );
            assert_eq!(
                tiff.find(tags::EXPOSURE_TIME).and_then(|e| e.f64(0)),
                Some(0.0005)
            );
            // Three SHORTs are six bytes: they live on the heap, not
            // in the entry, which is the case inline values get wrong.
            assert_eq!(tiff.find(0x0102).map(|e| e.u32s()), Some(vec![12, 12, 12]));
            // A SHORT is not a string and a string is not a number.
            assert_eq!(tiff.find(tags::IMAGE_WIDTH).and_then(|e| e.str()), None);
            assert_eq!(tiff.find(tags::MAKE).and_then(|e| e.u32(0)), None);
        }
    }

    #[test]
    fn inline_values_are_not_confused_with_offsets() {
        for le in [true, false] {
            // Two SHORTs fit in the entry; three do not. Both must read
            // back the same way.
            let bytes = build(
                le,
                &[ifd(vec![
                    shorts(le, 0x1000, &[1, 2]),
                    shorts(le, 0x1001, &[1, 2, 3]),
                ])],
            );
            let tiff = Tiff::parse(&bytes).unwrap();
            assert_eq!(tiff.find(0x1000).map(|e| e.u32s()), Some(vec![1, 2]));
            assert_eq!(tiff.find(0x1001).map(|e| e.u32s()), Some(vec![1, 2, 3]));
            // The inline one points into the entry itself.
            let inline = tiff.find(0x1000).unwrap();
            assert!(inline.offset > 8 && inline.offset < 8 + 2 + 12 * 2);
        }
    }

    #[test]
    fn sub_ifds_exif_and_gps() {
        let le = true;
        let bytes = build(
            le,
            &[
                // IFD0 points at three children; they are laid out
                // after it so their offsets are known at build time.
                TestIfd {
                    entries: vec![
                        ascii(tags::MAKE, "Canon"),
                        refs(tags::SUB_IFDS, Kind::Long, vec![1, 2]),
                        refs(tags::EXIF_IFD, Kind::Long, vec![3]),
                        refs(tags::GPS_IFD, Kind::Long, vec![4]),
                    ],
                    next: Next::End,
                },
                ifd(vec![
                    shorts(le, tags::PHOTOMETRIC, &[32803]),
                    shorts(le, tags::IMAGE_WIDTH, &[100]),
                ]),
                ifd(vec![shorts(le, tags::IMAGE_WIDTH, &[200])]),
                ifd(vec![shorts(le, tags::ISO, &[400])]),
                ifd(vec![ascii(0x0001, "N")]),
            ],
        );
        let tiff = Tiff::parse(&bytes).unwrap();
        let root = tiff.root();
        assert_eq!(root.sub_ifds.len(), 2);
        assert_eq!(
            root.sub_ifds[0]
                .get(tags::IMAGE_WIDTH)
                .and_then(|e| e.u32(0)),
            Some(100)
        );
        assert_eq!(
            root.sub_ifds[1]
                .get(tags::IMAGE_WIDTH)
                .and_then(|e| e.u32(0)),
            Some(200)
        );
        assert_eq!(
            tiff.exif()
                .and_then(|e| e.get(tags::ISO))
                .and_then(|e| e.u32(0)),
            Some(400)
        );
        assert_eq!(
            root.gps
                .as_ref()
                .and_then(|g| g.get(1))
                .and_then(|e| e.str()),
            Some("N")
        );
        // walk(): self, SubIFDs depth-first, Exif, GPS.
        assert_eq!(tiff.all().len(), 5);
        assert!(tiff.find_ifd(tags::PHOTOMETRIC).is_some());
    }

    #[test]
    fn single_sub_ifd_is_inline() {
        // One SubIFD offset fits in the entry, which is how every DNG
        // with a single raw IFD writes it; type 13 (IFD) is as common
        // as type 4 (LONG).
        let bytes = build(
            true,
            &[
                ifd(vec![refs(tags::SUB_IFDS, Kind::Ifd, vec![1])]),
                ifd(vec![shorts(true, tags::IMAGE_WIDTH, &[7])]),
            ],
        );
        let tiff = Tiff::parse(&bytes).unwrap();
        assert_eq!(tiff.root().sub_ifds.len(), 1);
        assert_eq!(
            tiff.root().sub_ifds[0]
                .get(tags::IMAGE_WIDTH)
                .and_then(|e| e.u32(0)),
            Some(7)
        );
    }

    #[test]
    fn a_loop_in_the_chain_terminates() {
        let mut ifds = vec![
            ifd(vec![ascii(tags::MAKE, "A")]),
            ifd(vec![ascii(tags::MAKE, "B")]),
        ];
        ifds[0].next = Next::Ifd(1);
        ifds[1].next = Next::Ifd(0);
        let bytes = build(true, &ifds);
        let tiff = Tiff::parse(&bytes).unwrap();
        assert_eq!(tiff.ifds.len(), 2);
        assert_eq!(
            tiff.ifds[1].get(tags::MAKE).and_then(|e| e.str()),
            Some("B")
        );
    }

    #[test]
    fn a_self_referential_ifd_terminates() {
        // An IFD whose next pointer is itself, and a SubIFD pointing
        // back at IFD0: both are cycles a fuzzer finds in seconds.
        let mut ifds = vec![ifd(vec![refs(tags::SUB_IFDS, Kind::Long, vec![0])])];
        ifds[0].next = Next::Raw(8);
        let bytes = build(true, &ifds);
        let tiff = Tiff::parse(&bytes).unwrap();
        assert_eq!(tiff.ifds.len(), 1);
        assert!(tiff.root().sub_ifds.is_empty());
    }

    #[test]
    fn values_past_the_end_of_the_file_are_empty() {
        let le = true;
        let mut bytes = build(
            le,
            &[ifd(vec![
                ascii(tags::MAKE, "PENTAX             "),
                shorts(le, tags::IMAGE_WIDTH, &[10]),
            ])],
        );
        // Point Make's value at the far end of nowhere.
        let value_field = 8 + 2 + 8;
        bytes[value_field..value_field + 4].copy_from_slice(&w32(le, 0x7fff_0000));
        let tiff = Tiff::parse(&bytes).unwrap();
        let make = tiff.find(tags::MAKE).expect("the entry is still there");
        assert_eq!(make.str(), Some(""));
        assert_eq!(make.bytes(), Some(&[][..]));
        // Its neighbours are unharmed.
        assert_eq!(
            tiff.find(tags::IMAGE_WIDTH).and_then(|e| e.u32(0)),
            Some(10)
        );
    }

    #[test]
    fn an_absurd_count_does_not_allocate() {
        let le = true;
        let mut bytes = build(le, &[ifd(vec![shorts(le, 0x1000, &[1, 2, 3])])]);
        // count = 4 billion SHORTs, offset 8: the range is not in the
        // file, so the value must come back empty rather than fatal.
        let count_field = 8 + 2 + 4;
        bytes[count_field..count_field + 4].copy_from_slice(&w32(le, 0xffff_fff0));
        let tiff = Tiff::parse(&bytes).unwrap();
        let entry = tiff.find(0x1000).unwrap();
        assert_eq!(entry.count, 0xffff_fff0);
        assert!(entry.u32s().is_empty());
        assert_eq!(entry.u32(0), None);
    }

    #[test]
    fn truncated_ifd_keeps_the_entries_it_has() {
        let le = true;
        let bytes = build(
            le,
            &[ifd(vec![
                shorts(le, 0x1000, &[1]),
                shorts(le, 0x1001, &[2]),
                shorts(le, 0x1002, &[3]),
            ])],
        );
        // Cut the file in the middle of the third entry.
        let cut = 8 + 2 + 12 * 2 + 5;
        let tiff = Tiff::parse(&bytes[..cut]).unwrap();
        assert_eq!(tiff.root().entries.len(), 2);
        assert_eq!(tiff.find(0x1001).and_then(|e| e.u32(0)), Some(2));
    }

    #[test]
    fn big_tiff_both_byte_orders() {
        for le in [true, false] {
            let bytes = build_big(
                le,
                &[
                    (tags::IMAGE_WIDTH, Kind::Long, 1, w32(le, 8192).to_vec()),
                    (tags::MAKE, Kind::Ascii, 8, b"Leica\0\0\0".to_vec()),
                    // A LONG8 value is exactly the size of the field.
                    (
                        tags::STRIP_OFFSETS,
                        Kind::Long8,
                        1,
                        w64(le, 0x1_0000_0000).to_vec(),
                    ),
                    // ... and two of them are not, so they go on the heap.
                    (
                        tags::STRIP_BYTE_COUNTS,
                        Kind::Long8,
                        2,
                        [w64(le, 5), w64(le, 6)].concat(),
                    ),
                ],
            );
            let tiff = Tiff::parse(&bytes).unwrap();
            assert!(tiff.big_tiff());
            assert_eq!(tiff.little_endian(), le);
            assert_eq!(
                tiff.find(tags::IMAGE_WIDTH).and_then(|e| e.u32(0)),
                Some(8192)
            );
            assert_eq!(tiff.find(tags::MAKE).and_then(|e| e.str()), Some("Leica"));
            // Past 4 GB a LONG8 offset must survive as itself.
            assert_eq!(
                tiff.find(tags::STRIP_OFFSETS).and_then(|e| e.u64(0)),
                Some(0x1_0000_0000)
            );
            assert_eq!(
                tiff.find(tags::STRIP_OFFSETS).and_then(|e| e.u32(0)),
                Some(u32::MAX)
            );
            assert_eq!(
                tiff.find(tags::STRIP_BYTE_COUNTS).map(|e| e.u64s()),
                Some(vec![5, 6])
            );
        }
    }

    #[test]
    fn vendor_signatures_are_plain_tiff() {
        for magic in [b"IIRO", b"IIRS", b"IIU\x00"] {
            let mut bytes = build(
                true,
                &[ifd(vec![ascii(tags::MAKE, "OLYMPUS IMAGING CORP.")])],
            );
            bytes[0..4].copy_from_slice(magic);
            let tiff = Tiff::parse(&bytes).expect("vendor magic parses as TIFF");
            assert!(tiff.little_endian());
            assert_eq!(tiff.make_model().0, "OLYMPUS IMAGING CORP.");
        }
        let mut bytes = build(
            false,
            &[ifd(vec![ascii(tags::MAKE, "OLYMPUS OPTICAL CO.,LTD")])],
        );
        bytes[0..4].copy_from_slice(b"MMOR");
        let tiff = Tiff::parse(&bytes).unwrap();
        assert!(!tiff.little_endian());
        assert_eq!(tiff.make_model().0, "OLYMPUS OPTICAL CO.,LTD");
    }

    #[test]
    fn nonsense_headers_are_errors() {
        for bad in [
            &b""[..],
            &b"II"[..],
            &b"II*"[..],
            &b"XX\x2a\x00\x08\x00\x00\x00"[..],
            &b"II\x2a\x00"[..], // header cut before the IFD0 offset
            &[0u8; 64][..],
        ] {
            assert!(Tiff::parse(bad).is_err(), "{bad:02x?} should not parse");
        }
        // A valid header whose IFD0 offset points nowhere.
        let bytes = [b'I', b'I', 0x2a, 0x00, 0xf0, 0xff, 0xff, 0x7f];
        assert!(Tiff::parse(&bytes).is_err());
    }

    #[test]
    fn embedded_tiff_offsets_are_relative_to_the_base() {
        let inner = build(
            true,
            &[ifd(vec![
                ascii(tags::MAKE, "Canon"),
                ascii(tags::MODEL, "Canon EOS R"),
            ])],
        );
        let base = 137;
        let mut outer = vec![0xa5; base];
        outer.extend_from_slice(&inner);
        let tiff = Tiff::parse_embedded(&outer, base).unwrap();
        assert_eq!(tiff.base(), base);
        assert_eq!(tiff.make_model(), ("Canon".into(), "Canon EOS R".into()));
        // The same bytes read from 0 are not a TIFF at all.
        assert!(Tiff::parse(&outer).is_err());
    }

    #[test]
    fn bare_ifd_chains_for_makernotes() {
        let le = true;
        let inner = build(
            le,
            &[ifd(vec![
                shorts(le, 0x0001, &[1]),
                ascii(0x0002, "makernote"),
            ])],
        );
        // parse_at: the IFD lives at a known offset, no header.
        let tiff = Tiff::parse_at(&inner, 8, le).unwrap();
        assert_eq!(
            tiff.root().get(0x0002).and_then(|e| e.str()),
            Some("makernote")
        );

        // parse_at_relative: the same IFD moved bodily into a bigger
        // file, its internal offsets still counted from its own start.
        let base = 64;
        let mut outer = vec![0u8; base];
        outer.extend_from_slice(&inner);
        let tiff = Tiff::parse_at_relative(&outer, base + 8, base, le).unwrap();
        assert_eq!(
            tiff.root().get(0x0002).and_then(|e| e.str()),
            Some("makernote")
        );
        assert_eq!(tiff.root().get(0x0001).and_then(|e| e.u32(0)), Some(1));
        // Without the base it reads whatever happens to be there.
        assert_ne!(
            Tiff::parse_at(&outer, base + 8, le)
                .unwrap()
                .root()
                .get(0x0002)
                .and_then(|e| e.str()),
            Some("makernote")
        );
    }

    #[test]
    fn image_layout_strips_and_tiles() {
        let le = true;
        // Two strips of eight bytes each, in a file long enough to
        // hold them.
        let mut bytes = build(
            le,
            &[ifd(vec![
                shorts(le, tags::IMAGE_WIDTH, &[4]),
                shorts(le, tags::IMAGE_LENGTH, &[4]),
                shorts(le, tags::BITS_PER_SAMPLE, &[16]),
                shorts(le, tags::COMPRESSION, &[1]),
                shorts(le, tags::PHOTOMETRIC, &[32803]),
                shorts(le, tags::ROWS_PER_STRIP, &[2]),
                longs(le, tags::STRIP_OFFSETS, &[200, 208]),
                longs(le, tags::STRIP_BYTE_COUNTS, &[8, 8]),
            ])],
        );
        bytes.resize(216, 0);
        let tiff = Tiff::parse(&bytes).unwrap();
        let layout = ImageLayout::of(&tiff, tiff.root()).unwrap();
        assert_eq!((layout.width, layout.height), (4, 4));
        assert_eq!(layout.bits_per_sample, 16);
        assert_eq!(layout.samples_per_pixel, 1);
        assert_eq!(layout.photometric, 32803);
        assert_eq!(layout.chunks, vec![(200, 8), (208, 8)]);
        assert_eq!(layout.tile, None);
        assert_eq!(layout.rows_per_chunk, 2);

        // The same image as four tiles.
        let mut bytes = build(
            le,
            &[ifd(vec![
                shorts(le, tags::IMAGE_WIDTH, &[4]),
                shorts(le, tags::IMAGE_LENGTH, &[4]),
                shorts(le, tags::TILE_WIDTH, &[2]),
                shorts(le, tags::TILE_LENGTH, &[2]),
                longs(le, tags::TILE_OFFSETS, &[100, 108, 116, 124]),
                longs(le, tags::TILE_BYTE_COUNTS, &[8, 8, 8, 8]),
            ])],
        );
        bytes.resize(132, 0);
        let tiff = Tiff::parse(&bytes).unwrap();
        let layout = ImageLayout::of(&tiff, tiff.root()).unwrap();
        assert_eq!(layout.tile, Some((2, 2)));
        assert_eq!(layout.rows_per_chunk, 2);
        assert_eq!(layout.chunks.len(), 4);
    }

    #[test]
    fn image_layout_rejects_chunks_outside_the_file() {
        let le = true;
        let bytes = build(
            le,
            &[ifd(vec![
                shorts(le, tags::IMAGE_WIDTH, &[4]),
                shorts(le, tags::IMAGE_LENGTH, &[4]),
                longs(le, tags::STRIP_OFFSETS, &[100]),
                longs(le, tags::STRIP_BYTE_COUNTS, &[1 << 20]),
            ])],
        );
        let tiff = Tiff::parse(&bytes).unwrap();
        assert!(matches!(
            ImageLayout::of(&tiff, tiff.root()),
            Err(Error::Corrupt(_))
        ));

        // No dimensions at all is not an image IFD.
        let bytes = build(le, &[ifd(vec![ascii(tags::MAKE, "Sony")])]);
        let tiff = Tiff::parse(&bytes).unwrap();
        assert!(ImageLayout::of(&tiff, tiff.root()).is_err());
    }

    #[test]
    fn image_layout_single_strip_without_byte_counts() {
        // Old ORF and a few converters omit StripByteCounts; the strip
        // then runs to the end of the file.
        let le = true;
        let mut bytes = build(
            le,
            &[ifd(vec![
                shorts(le, tags::IMAGE_WIDTH, &[2]),
                shorts(le, tags::IMAGE_LENGTH, &[2]),
                longs(le, tags::STRIP_OFFSETS, &[64]),
            ])],
        );
        bytes.resize(72, 0);
        let tiff = Tiff::parse(&bytes).unwrap();
        let layout = ImageLayout::of(&tiff, tiff.root()).unwrap();
        assert_eq!(layout.chunks, vec![(64, 8)]);
    }

    // ---------------------------------------------------------------
    // Corpus tests. They need real files: set SCHIST_RAW_CORPUS to a
    // directory of raws (with the `<file>.json` exiftool sidecars the
    // sample fetcher writes) and they compare against exiftool.
    // Without it they skip, silently.
    // ---------------------------------------------------------------

    pub(crate) fn corpus() -> Option<std::path::PathBuf> {
        std::env::var_os("SCHIST_RAW_CORPUS").map(std::path::PathBuf::from)
    }

    /// Every sample in the corpus, ignoring the oracle sidecars beside
    /// them (`<file>.json`, `<file>.tiff`, `<file>.identify.txt`).
    pub(crate) fn samples(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
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
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if name.ends_with(".json") || name.ends_with(".txt") || name.ends_with(".tiff") {
                    continue;
                }
                out.push(path);
            }
        }
        out.sort();
        out
    }

    /// The exiftool sidecar for a sample, as a flat map of its
    /// group-prefixed keys ("EXIF:Make") to rendered values.
    pub(crate) fn sidecar(
        path: &std::path::Path,
    ) -> Option<std::collections::HashMap<String, String>> {
        let mut json = path.as_os_str().to_os_string();
        json.push(".json");
        let text = std::fs::read_to_string(std::path::PathBuf::from(json)).ok()?;
        Some(json_object(&text))
    }

    /// A dependency-light reader for exiftool's `-j` output: the keys
    /// of the first object, with scalars rendered as text and nested
    /// values flattened to their elements. Enough for an oracle, not a
    /// general JSON parser.
    fn json_object(text: &str) -> std::collections::HashMap<String, String> {
        let bytes = text.as_bytes();
        let mut at = 0usize;
        let mut out = std::collections::HashMap::new();
        while at < bytes.len() && bytes[at] != b'{' {
            at += 1;
        }
        if at == bytes.len() {
            return out;
        }
        at += 1;
        loop {
            skip_ws(bytes, &mut at);
            match bytes.get(at) {
                Some(b'}') | None => break,
                Some(b',') => {
                    at += 1;
                    continue;
                }
                Some(b'"') => {}
                _ => break,
            }
            let Some(key) = json_string(bytes, &mut at) else {
                break;
            };
            skip_ws(bytes, &mut at);
            if bytes.get(at) != Some(&b':') {
                break;
            }
            at += 1;
            let Some(value) = json_value(bytes, &mut at) else {
                break;
            };
            out.insert(key, value);
        }
        out
    }

    fn skip_ws(bytes: &[u8], at: &mut usize) {
        while matches!(bytes.get(*at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            *at += 1;
        }
    }

    fn json_string(bytes: &[u8], at: &mut usize) -> Option<String> {
        if bytes.get(*at) != Some(&b'"') {
            return None;
        }
        *at += 1;
        let mut out = String::new();
        loop {
            match *bytes.get(*at)? {
                b'"' => {
                    *at += 1;
                    return Some(out);
                }
                b'\\' => {
                    *at += 1;
                    let escape = *bytes.get(*at)?;
                    *at += 1;
                    out.push(match escape {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'u' => {
                            let hex = std::str::from_utf8(bytes.get(*at..*at + 4)?).ok()?;
                            *at += 4;
                            char::from_u32(u32::from_str_radix(hex, 16).ok()?).unwrap_or('?')
                        }
                        other => other as char,
                    });
                }
                other => {
                    // exiftool's JSON is UTF-8; take bytes as they come
                    // and let lossy handling deal with anything odd.
                    let start = *at;
                    let mut end = *at + 1;
                    while end < bytes.len() && bytes[end] & 0xc0 == 0x80 {
                        end += 1;
                    }
                    if end > start + 1 {
                        out.push_str(&String::from_utf8_lossy(&bytes[start..end]));
                        *at = end;
                    } else {
                        out.push(other as char);
                        *at += 1;
                    }
                }
            }
        }
    }

    fn json_value(bytes: &[u8], at: &mut usize) -> Option<String> {
        skip_ws(bytes, at);
        match *bytes.get(*at)? {
            b'"' => json_string(bytes, at),
            b'{' | b'[' => {
                // Consume the nested value, rendering its scalars
                // separated by spaces (arrays of ISO values, mostly).
                let open = bytes[*at];
                let close = if open == b'{' { b'}' } else { b']' };
                *at += 1;
                let mut parts: Vec<String> = Vec::new();
                loop {
                    skip_ws(bytes, at);
                    match bytes.get(*at) {
                        None => return Some(parts.join(" ")),
                        Some(c) if *c == close => {
                            *at += 1;
                            return Some(parts.join(" "));
                        }
                        Some(b',') | Some(b':') => {
                            *at += 1;
                        }
                        _ => {
                            let part = json_value(bytes, at)?;
                            parts.push(part);
                        }
                    }
                }
            }
            _ => {
                let start = *at;
                while matches!(bytes.get(*at), Some(c) if !matches!(c, b',' | b'}' | b']' | b'\n'))
                {
                    *at += 1;
                }
                Some(
                    String::from_utf8_lossy(&bytes[start..*at])
                        .trim()
                        .to_string(),
                )
            }
        }
    }

    /// The value of the first of `keys` present, matched exactly (bar
    /// case). The group prefix matters: exiftool reports several ISOs
    /// per file — "EXIF:ISO" is the tag, "MakerNotes:ISO" is what the
    /// vendor's own block says (Nikon writes one where the EXIF tag is
    /// absent), and "Composite:ISO" is exiftool's own arithmetic from
    /// the makernote (Canon's 519 against an EXIF 500). Only the tags
    /// this crate's shared TIFF code can see are fair comparisons.
    pub(crate) fn oracle<'a>(
        json: &'a std::collections::HashMap<String, String>,
        keys: &[&str],
    ) -> Option<&'a String> {
        for key in keys {
            for (have, value) in json {
                if have.eq_ignore_ascii_case(key) && !value.is_empty() {
                    return Some(value);
                }
            }
        }
        None
    }

    /// A TIFF-shaped file, by the signatures [`Tiff::parse`] accepts.
    fn is_tiff_shaped(bytes: &[u8]) -> bool {
        matches!(
            bytes.get(0..4),
            Some(b"II\x2a\x00" | b"MM\x00\x2a" | b"II\x2b\x00" | b"MM\x00\x2b")
                | Some(b"IIRO" | b"IIRS" | b"MMOR" | b"IIU\x00")
        )
    }

    #[test]
    fn corpus_tiff_files_parse_and_match_exiftool() {
        let Some(root) = corpus() else { return };
        let mut checked = 0;
        let mut previews = 0;
        let mut problems: Vec<String> = Vec::new();
        for path in samples(&root) {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if !is_tiff_shaped(&bytes) {
                continue;
            }
            let name = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let tiff = match Tiff::parse(&bytes) {
                Ok(tiff) => tiff,
                Err(e) => {
                    problems.push(format!("{name}: does not parse: {e}"));
                    continue;
                }
            };
            checked += 1;
            let Some(json) = sidecar(&path) else { continue };
            let (make, model) = tiff.make_model();
            if let Some(want) = oracle(&json, &["EXIF:Make", "IFD0:Make", "Make"]) {
                // Cameras pad Make with spaces and NULs; exiftool trims.
                if make.trim() != want.trim() {
                    problems.push(format!("{name}: make {make:?}, exiftool {want:?}"));
                }
            }
            if let Some(want) = oracle(&json, &["EXIF:Model", "IFD0:Model", "Model"]) {
                if model.trim() != want.trim() {
                    problems.push(format!("{name}: model {model:?}, exiftool {want:?}"));
                }
            }
            // A JPEG exiftool can reach through the standard EXIF tags
            // must be one largest_jpeg finds too. The makernote-only
            // previews (Olympus, Epson, Nikon's scanner NEFs) are not
            // this function's job — their vendor module digs those out
            // — so only the EXIF group counts here.
            // Panasonic (and the Leica bodies that write RW2/RWL, and
            // the Digilux's ".RAW") keeps both the preview and the ISO
            // in vendor tags of IFD0 — the JPEG's bytes in 0x002E and
            // the ISO in 0x0017 — which exiftool reports under the
            // EXIF group all the same. Neither is a tag the shared
            // helpers are meant to know; `formats::rw2` reads them.
            let panasonic = crate::probe(&bytes) == Some(crate::Format::Rw2);
            let has_jpeg = !panasonic
                && [
                    "EXIF:JpgFromRaw",
                    "EXIF:PreviewImage",
                    "EXIF:ThumbnailImage",
                    "EXIF:OtherImage",
                ]
                .iter()
                .any(|k| oracle(&json, &[k]).is_some());
            let jpeg = crate::formats::common::largest_jpeg(&tiff);
            if let Some(jpeg) = &jpeg {
                // Starting with FFD8 only proves the offset; decoding
                // proves the length as well, which is the half that
                // goes wrong when a tag is read in the wrong order or
                // an embedded TIFF's base is forgotten.
                match image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg) {
                    Ok(image) => {
                        if image.width() == 0 || image.height() == 0 {
                            problems.push(format!("{name}: preview decodes to nothing"));
                        }
                    }
                    Err(e) => problems.push(format!(
                        "{name}: preview ({} bytes) will not decode: {e}",
                        jpeg.len()
                    )),
                }
            }
            match (&jpeg, has_jpeg) {
                (Some(jpeg), _) if !jpeg.starts_with(&[0xff, 0xd8]) => {
                    problems.push(format!("{name}: largest_jpeg is not a JPEG"));
                }
                (None, true) => {
                    problems.push(format!(
                        "{name}: exiftool finds a preview, largest_jpeg does not"
                    ));
                }
                _ => {}
            }
            // And it should be the *largest* one: exiftool prints the
            // size of every image it can extract ("(Binary data
            // 1155191 bytes, ...)"), so the biggest it found through
            // the standard tags is the one to beat.
            if let Some(jpeg) = jpeg.as_ref().filter(|_| !panasonic) {
                let biggest = ["EXIF:JpgFromRaw", "EXIF:PreviewImage", "EXIF:OtherImage"]
                    .iter()
                    .filter_map(|k| oracle(&json, &[k]))
                    .filter_map(|v| v.split_whitespace().nth(2)?.parse::<usize>().ok())
                    .max();
                if let Some(biggest) = biggest {
                    if jpeg.len() < biggest {
                        problems.push(format!(
                            "{name}: preview {} bytes, exiftool has {biggest}",
                            jpeg.len()
                        ));
                    }
                }
            }
            if jpeg.is_some() {
                previews += 1;
            }
            // ISO, the one shooting field every camera records.
            // Only the Exif-group value: older Nikon and Kodak bodies
            // record ISO in the makernote alone, which `metadata`
            // deliberately does not read (exiftool's bare "ISO" is the
            // makernote's).
            if let Some(want) = oracle(&json, &["EXIF:ISO", "IFD0:ISO"]).filter(|_| !panasonic) {
                let want: Option<f32> = want.split_whitespace().next().and_then(|v| v.parse().ok());
                let have = crate::formats::common::metadata(&tiff).iso;
                match (want, have) {
                    (Some(want), Some(have)) if (want - have).abs() > 0.5 => {
                        problems.push(format!("{name}: ISO {have}, exiftool {want}"));
                    }
                    (Some(want), None) => {
                        problems.push(format!(
                            "{name}: exiftool has ISO {want}, metadata() has none"
                        ));
                    }
                    _ => {}
                }
            }
            // Every IFD that says it holds CFA data must resolve to
            // strips or tiles inside the file: that is the path a
            // decoder takes to the sensor samples.
            for ifd in tiff.all() {
                if ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)) != Some(32803) {
                    continue;
                }
                match ImageLayout::of(&tiff, ifd) {
                    Ok(layout) => {
                        if layout.chunks.is_empty() {
                            problems.push(format!("{name}: CFA IFD with no chunks"));
                        }
                        if layout.width == 0 || layout.height == 0 {
                            problems.push(format!(
                                "{name}: CFA IFD is {}x{}",
                                layout.width, layout.height
                            ));
                        }
                        if !matches!(layout.bits_per_sample, 8 | 10 | 12 | 14 | 16) {
                            problems.push(format!(
                                "{name}: CFA IFD at {} bits",
                                layout.bits_per_sample
                            ));
                        }
                    }
                    // One sample lies about its strip: the CF132 back
                    // writes the *uncompressed* size (4096 x 5456 x 2
                    // bytes) into StripByteCounts, half again as much
                    // as the whole file. Refusing it is the contract
                    // working; the Hasselblad module has to resolve
                    // that strip itself.
                    Err(_) if name.ends_with("RAW_HASSELBLAD_IXPRESS_CF132.3FR") => {
                        eprintln!("{name}: StripByteCounts is the uncompressed size, so no layout");
                    }
                    Err(e) => problems.push(format!("{name}: CFA IFD has no usable layout: {e}")),
                }
            }
            // Exposure and aperture, where the file records them.
            for (key, what, have) in [
                (
                    "EXIF:FNumber",
                    "FNumber",
                    crate::formats::common::metadata(&tiff).f_number,
                ),
                (
                    "EXIF:FocalLength",
                    "FocalLength",
                    crate::formats::common::metadata(&tiff).focal_length,
                ),
            ] {
                let Some(want) = oracle(&json, &[key]) else {
                    continue;
                };
                // exiftool prints focal length as "50.0 mm".
                let Some(want) = want
                    .trim_start_matches("f/")
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f32>().ok())
                else {
                    continue;
                };
                // A zero focal length (Samsung EX1) is "not recorded".
                if want == 0.0 {
                    continue;
                }
                match have {
                    // exiftool prints "4.5 mm" for a 445/100 rational,
                    // so allow its rounding to one decimal place.
                    Some(have) if (want - have).abs() > 0.06 => {
                        problems.push(format!("{name}: {what} {have}, exiftool {want}"));
                    }
                    None => problems.push(format!(
                        "{name}: exiftool has {what} {want}, metadata() has none"
                    )),
                    _ => {}
                }
            }
        }
        assert!(checked > 0, "corpus held no TIFF-shaped files");
        assert!(
            problems.is_empty(),
            "{} problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
        eprintln!("corpus: {checked} TIFF-shaped files parsed, {previews} with a preview JPEG");
    }

    /// Samples `crate::probe` gets wrong today, with the reason. The
    /// fix belongs in `lib.rs`, which this worker does not own; the
    /// test reports them and fails on anything *not* on the list.
    const KNOWN_PROBE_GAPS: &[(&str, &str)] = &[
        (
            "Dimage_Z2-RAW_MINOLTA_DIMAGE_Z2.MRW",
            "a headerless sensor dump with no \\0MRM container; only a file-size table could name it",
        ),
        (
            "Nikon/Nikon_COOLSCAN_V_ED-Image21-600pixels.nef",
            "a scanner NEF: RGB (photometric 2) throughout, no CFA data, so probe_tiff \
             refuses it. Arguably correct — there is no sensor mosaic to decode",
        ),
    ];

    #[test]
    fn corpus_probe_recognises_every_sample() {
        let Some(root) = corpus() else { return };
        let mut problems = Vec::new();
        let mut known = Vec::new();
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for path in samples(&root) {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let name = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_uppercase())
                .unwrap_or_default();
            let want = match ext.as_str() {
                "DNG" | "GPR" => Some(crate::Format::Dng),
                "NEF" | "NRW" => Some(crate::Format::Nef),
                "ARW" | "SR2" | "SRF" => Some(crate::Format::Arw),
                "CR2" => Some(crate::Format::Cr2),
                "CRW" => Some(crate::Format::Crw),
                "CR3" => Some(crate::Format::Cr3),
                "RAF" => Some(crate::Format::Raf),
                "ORF" => Some(crate::Format::Orf),
                "RW2" | "RWL" => Some(crate::Format::Rw2),
                "PEF" => Some(crate::Format::Pef),
                "SRW" => Some(crate::Format::Srw),
                "MRW" => Some(crate::Format::Mrw),
                "DCR" | "KDC" => Some(crate::Format::Kodak),
                "ERF" => Some(crate::Format::Erf),
                "MEF" => Some(crate::Format::Mef),
                "IIQ" => Some(crate::Format::Iiq),
                "3FR" | "FFF" => Some(crate::Format::Hasselblad),
                "MOS" => Some(crate::Format::Mos),
                "X3F" => Some(crate::Format::X3f),
                // .TIF and .RAW samples are vendor raws wearing a
                // generic extension; they are checked by hand below.
                _ => None,
            };
            let got = crate::probe(&bytes);
            *counts.entry(format!("{ext} -> {got:?}")).or_default() += 1;
            let Some(want) = want else { continue };
            if got == Some(want) {
                continue;
            }
            // Files exiftool itself rejects are not raws at all: three
            // of the raw.pixls.us ".CRW" samples are CHDK sensor dumps
            // with no container (exiftool: "File format error"), and
            // probe is right to say None.
            let unreadable = sidecar(&path)
                .map(|json| oracle(&json, &["ExifTool:Error"]).is_some())
                .unwrap_or(false);
            if unreadable && got.is_none() {
                known.push(format!(
                    "{name}: not a container at all (exiftool cannot read it either)"
                ));
                continue;
            }
            let lower = name.to_ascii_lowercase();
            match KNOWN_PROBE_GAPS
                .iter()
                .find(|(which, _)| lower.ends_with(&which.to_ascii_lowercase()))
            {
                Some((_, why)) => known.push(format!("{name}: probe says {got:?}; {why}")),
                None => problems.push(format!(
                    "{name}: probe says {got:?}, extension says {want:?}"
                )),
            }
        }
        for (what, n) in &counts {
            eprintln!("probe: {n:3} x {what}");
        }
        for note in &known {
            eprintln!("probe gap: {note}");
        }
        assert!(
            problems.is_empty(),
            "{} problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    #[test]
    fn truncated_corpus_files_never_panic() {
        let Some(root) = corpus() else { return };
        let samples = samples(&root);
        // A spread of containers rather than the whole corpus: eight
        // files x 20 cuts is enough to catch a missing bounds check.
        let mut chosen: Vec<std::path::PathBuf> = Vec::new();
        for want in [
            "NEF", "CR2", "CR3", "ORF", "RW2", "ARW", "DNG", "PEF", "RAF", "3FR",
        ] {
            if let Some(path) = samples.iter().find(|p| {
                p.extension()
                    .map(|e| e.to_string_lossy().to_ascii_uppercase() == want)
                    .unwrap_or(false)
            }) {
                chosen.push(path.clone());
            }
        }
        assert!(!chosen.is_empty(), "corpus held nothing to truncate");
        // A tiny LCG so the cut points are spread but reproducible.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for path in chosen {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            for _ in 0..20 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let cut = (seed >> 11) as usize % bytes.len().max(1);
                let head = &bytes[..cut];
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = crate::probe(head);
                    if let Ok(tiff) = Tiff::parse(head) {
                        for ifd in tiff.all() {
                            let _ = ImageLayout::of(&tiff, ifd);
                            for entry in &ifd.entries {
                                let _ = entry.u32(0);
                                let _ = entry.f64(0);
                                let _ = entry.str();
                                let _ = entry.u32s();
                                let _ = entry.bytes();
                            }
                        }
                        let _ = crate::formats::common::largest_jpeg(&tiff);
                        let _ = crate::formats::common::metadata(&tiff);
                        let _ = crate::formats::common::orientation(&tiff);
                    }
                    let _ = crate::bmff::parse(head);
                }));
                assert!(
                    result.is_ok(),
                    "panic on {} truncated to {cut} bytes",
                    path.display()
                );
            }
        }
    }
}
