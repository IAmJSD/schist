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
//! sensor uncompressed, ten bits a sample, and those decode here.
//!
//! Every EOS CRW and every later PowerShot compresses, and the three
//! Huffman table sets its decoder picks between are *not* in the file.
//! Record `0x1835` gives only the index of the set to use; the tables
//! themselves appear in no specification and in no published format
//! write-up, only in the source of the copyleft decoders this crate
//! may not read. They cannot be guessed and may not be copied, so a
//! compressed image is [`Error::Unsupported`] and the message says
//! why rather than pretending the file is broken.
//!
//! [`preview`] still works on those files: the camera's own full-size
//! JPEG is a record like any other, and it is the whole picture.
//!
//! Clean-room: written from the public CIFF 1.0 specification and
//! third-party descriptions of it, ExifTool's tag documentation, and
//! measurement of the sample files named in this module's tests.

use crate::bits::{BitPump, BitPumpMsb};
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
    if ciff.find(tag::DECODER_TABLE).is_some() {
        return Err(Error::Unsupported(format!(
            "crw: {model}'s image is Canon's compressed CIFF data; its Huffman \
             tables are published nowhere this clean-room decoder may read"
        )));
    }

    let sensor = ciff.sensor_info();
    let info = ciff.image_info();
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
    let cfa = uncompressed_cfa(&model).ok_or_else(|| {
        Error::Unsupported(format!(
            "crw: uncompressed CIFF from {model}: this decoder does not know its filter array"
        ))
    })?;

    let data = unpack10(ciff.slice(image)?, width * height);
    let mut raw = RawImage::new(Format::Crw, width, height, 1, RawData::U16(data), cfa);
    raw.set_camera(&make, &model);
    // Ten bits a sample, and no record says the sensor saturates
    // earlier.
    raw.white_level = 1023.0;
    if let Some(sensor) = sensor {
        if let Some(crop) = sensor.crop(width, height) {
            raw.crop = crop;
        }
    }
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

    /// Compressed CIFF is refused on purpose: see this module's header.
    /// Everything the corpus holds but the Pro70 is compressed.
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

    #[test]
    fn corpus_decodes_what_it_can_and_refuses_the_rest() {
        let Some(dir) = corpus() else { return };
        let mut files = Vec::new();
        crw_files(&dir, &mut files);
        files.sort();
        let (mut uncompressed, mut decoded) = (0, 0);
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
            // An image record with no decoder-table index beside it is
            // one this module is meant to decode, so a refusal on one
            // of those would be a bug rather than the documented gap.
            let ciff = Ciff::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            if ciff.find(tag::DECODER_TABLE).is_none() && ciff.find(tag::IMAGE_DATA).is_some() {
                uncompressed += 1;
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
                        why.contains("compressed") || why.contains("filter array"),
                        "{name}: {why}"
                    );
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
        assert_eq!(
            decoded, uncompressed,
            "every uncompressed CIFF in the corpus should decode"
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
