//! Canon CR3: an ISO-BMFF file with the CRX codec inside.
//!
//! A CR3 is a QuickTime-shaped movie of still tracks. `moov` holds a
//! `uuid` box of Canon's own (see [`crate::bmff::UUID_CANON_CRAW`])
//! with the file's metadata, and one `trak` per image:
//!
//! * a full-size preview — a `JPEG` sample entry on bodies up to the
//!   R6 generation, an HEVC one on the R8 and its contemporaries;
//! * a small "SD" raw, a quarter the size, that nothing here wants;
//! * the full "HD" raw, a `CRAW` sample entry whose `CMP1` box says
//!   how the CRX stream is shaped and whose `CDI1`/`IAD1` box gives
//!   the sensor's active area;
//! * `CTMD`, Canon's timed metadata, which is where the CR3 keeps the
//!   `ColorData` record — the white balance, the per-channel black
//!   level and the saturation point all live there, not in the
//!   makernote IFD where a CR2 keeps them.
//!
//! Dual-pixel files add a fifth track holding the second half of the
//! split photosites; it is a whole extra frame and is not decoded.
//!
//! The metadata blocks CMT1..CMT4 are complete little TIFFs: IFD0,
//! the Exif IFD, the Canon makernote IFD and GPS.

use crate::bmff::{self, Box_, UUID_CANON_CRAW, UUID_CANON_PREVIEW};
use crate::formats::common;
use crate::formats::crx;
use crate::tiff::{tags, Tiff};
use crate::{Cfa, Error, Format, Orientation, RawData, RawImage, Rect, Result};

/// Canon's makernote tags this decoder reads, all in CMT3.
mod canon {
    /// SensorInfo: an int16 array whose 1..=2 are the raw frame and
    /// 5..=8 the inclusive borders of the area Canon calls the image.
    pub const SENSOR_INFO: u16 = 0x00E0;
    /// ColorData, in the CTMD track's makernote for a CR3.
    pub const COLOR_DATA: u16 = 0x4001;
}

pub fn decode(bytes: &[u8]) -> Result<RawImage> {
    let file = File::parse(bytes)?;
    let raw = file.raw_track()?;
    let header = file.image_header(
        raw.cmp1
            .clone()
            .ok_or_else(|| Error::Corrupt("CR3 raw track has no CMP1".into()))?,
    )?;

    let sample = raw
        .sample(0)
        .and_then(|(at, len)| bytes.get(at..at.checked_add(len)?))
        .ok_or_else(|| Error::Corrupt("CR3 raw track has no sample in the file".into()))?;
    let data = crx::decode(&header, sample)?;

    let mut image = RawImage::new(
        Format::Cr3,
        header.width,
        header.height,
        1,
        RawData::U16(data),
        // Every CR3 body is RGGB at the origin of the full frame, and
        // the CMP1 layout nibble is 0 to say so on the full-size raw
        // track. (The small SD track uses 1, but nothing decodes it.)
        Cfa::RGGB,
    );
    file.describe(&mut image);
    image.apply_camera_table();
    Ok(image)
}

pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(File::parse(bytes)?.preview())
}

/// A parsed CR3: the box tree plus the pieces of it worth naming.
struct File<'a> {
    bytes: &'a [u8],
    boxes: Vec<Box_>,
    /// The four CMT TIFFs, when present and parseable.
    cmt1: Option<Tiff<'a>>,
    cmt2: Option<Tiff<'a>>,
    cmt3: Option<Tiff<'a>>,
    tracks: Vec<Track>,
}

/// One `trak`: its sample entry and its sample table.
struct Track {
    /// The fourcc of the single `stsd` entry: `CRAW`, `JPEG`, `CTMD`.
    kind: [u8; 4],
    /// The payload of the entry's `CMP1` child, if it has one.
    cmp1: Option<std::ops::Range<usize>>,
    /// The payload of the `IAD1` inside the entry's `CDI1` child.
    iad1: Option<std::ops::Range<usize>>,
    /// Whether the sample entry holds an `HEVC` child, which marks the
    /// R8 generation's preview track as one we cannot hand out.
    hevc: bool,
    /// Absolute file offsets of the samples and their lengths.
    offsets: Vec<usize>,
    sizes: Vec<usize>,
}

impl Track {
    fn sample(&self, i: usize) -> Option<(usize, usize)> {
        Some((*self.offsets.get(i)?, *self.sizes.get(i)?))
    }
}

impl<'a> File<'a> {
    fn parse(bytes: &'a [u8]) -> Result<File<'a>> {
        let boxes = bmff::parse(bytes)?;
        let craw = find_uuid(&boxes, &UUID_CANON_CRAW);
        let cmt = |kind: &[u8; 4]| {
            craw.and_then(|craw| craw.child(kind))
                .and_then(|b| Tiff::parse_embedded(bytes, b.data.start).ok())
        };
        let tracks = boxes
            .iter()
            .filter(|b| &b.kind == b"moov")
            .flat_map(|moov| moov.children.iter())
            .filter(|b| &b.kind == b"trak")
            .filter_map(|trak| track(bytes, trak))
            .collect();
        Ok(File {
            bytes,
            cmt1: cmt(b"CMT1"),
            cmt2: cmt(b"CMT2"),
            cmt3: cmt(b"CMT3"),
            boxes,
            tracks,
        })
    }

    /// The full-size raw track: the largest `CRAW` entry that carries
    /// a `CMP1`. On a dual-pixel file two tracks are that size — the
    /// second holds one half of the split photosites, a separate
    /// image — so the first of them wins.
    fn raw_track(&self) -> Result<&Track> {
        let mut best: Option<(usize, &Track)> = None;
        for track in &self.tracks {
            if &track.kind != b"CRAW" {
                continue;
            }
            let Some(header) = track.cmp1.clone().and_then(|r| self.image_header(r).ok()) else {
                continue;
            };
            let area = header.width * header.height;
            if best.is_none_or(|(best, _)| area > best) {
                best = Some((area, track));
            }
        }
        best.map(|(_, track)| track)
            .ok_or_else(|| Error::Corrupt("CR3 with no CRAW track".into()))
    }

    fn image_header(&self, cmp1: std::ops::Range<usize>) -> Result<crx::ImageHeader> {
        let payload = self
            .bytes
            .get(cmp1)
            .ok_or_else(|| Error::Corrupt("CMP1 outside the file".into()))?;
        crx::ImageHeader::parse(payload)
    }

    /// Fill in everything about the image that is not pixels.
    fn describe(&self, image: &mut RawImage) {
        let (make, model) = self.cmt1.as_ref().map(Tiff::make_model).unwrap_or_default();
        image.set_camera(&make, &model);
        image.orientation = self
            .cmt1
            .as_ref()
            .map(common::orientation)
            .unwrap_or(Orientation::Normal);
        if let Some(cmt2) = &self.cmt2 {
            image.metadata = common::metadata(cmt2);
        }
        // Canon's lens name is a makernote string, not Exif's
        // LensModel, on every body that writes a CR3.
        if image.metadata.lens.is_none() {
            image.metadata.lens = self
                .cmt3
                .as_ref()
                .and_then(|t| t.find(0x0095))
                .and_then(|e| e.str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
        if let Some(crop) = self.crop(image.width, image.height) {
            image.crop = crop;
        }
        self.levels_and_white_balance(image);
        image.preview = self.preview();
    }

    /// The area Canon calls the image: SensorInfo's inclusive borders,
    /// with the raw track's `IAD1` box saying the same thing when the
    /// makernote is missing.
    ///
    /// This is the crop LibRaw reports as "Raw inset". Its own "Image
    /// size" is a different, per-generation rectangle — one pixel
    /// narrower on the R5 and R8, and neither the inset nor the active
    /// area on the EOS R and 90D — so it is not what this follows.
    fn crop(&self, width: usize, height: usize) -> Option<Rect> {
        let borders = self
            .sensor_info()
            .or_else(|| self.iad1_crop())
            .filter(|[l, t, r, b]| r > l && b > t)?;
        let [left, top, right, bottom] = borders.map(|v| v as usize);
        (right < width && bottom < height).then_some(Rect {
            x: left,
            y: top,
            width: right - left + 1,
            height: bottom - top + 1,
        })
    }

    /// SensorInfo's left, top, right, bottom.
    fn sensor_info(&self) -> Option<[u32; 4]> {
        let entry = self.cmt3.as_ref()?.find(canon::SENSOR_INFO)?;
        Some([entry.u32(5)?, entry.u32(6)?, entry.u32(7)?, entry.u32(8)?])
    }

    /// The first rectangle of the raw track's `IAD1`, which carries
    /// the same borders as SensorInfo: after a version word and the
    /// frame size come two flag words and then the crop, as four
    /// big-endian 16-bit inclusive edges.
    fn iad1_crop(&self) -> Option<[u32; 4]> {
        let range = self.raw_track().ok()?.iad1.clone()?;
        let payload = self.bytes.get(range)?;
        let at = |i: usize| -> Option<u32> {
            let b = payload.get(i..i + 2)?;
            Some(u16::from_be_bytes([b[0], b[1]]) as u32)
        };
        Some([at(16)?, at(18)?, at(20)?, at(22)?])
    }

    /// Black level, white level and as-shot white balance, from the
    /// ColorData record in the CTMD track.
    fn levels_and_white_balance(&self, image: &mut RawImage) {
        let Some(color_data) = self.color_data() else {
            return;
        };
        let Some(layout) = ColorDataLayout::of(&color_data) else {
            log::debug!(
                "cr3: ColorData version {:?} is not one this knows; leaving levels to the camera table",
                color_data.first()
            );
            return;
        };
        let at = |i: usize| color_data.get(i).copied().map(f32::from);
        // WB_RGGBLevelsAsShot is four levels in CFA order — red, the
        // two greens, blue — and a multiplier is a level over green,
        // which is the convention `wb_coeffs` and LibRaw's `cam_mul`
        // share.
        if let (Some(r), Some(g1), Some(g2), Some(b)) = (
            at(layout.white_balance),
            at(layout.white_balance + 1),
            at(layout.white_balance + 2),
            at(layout.white_balance + 3),
        ) {
            if g1 > 0.0 && r > 0.0 && b > 0.0 && g2 > 0.0 {
                image.wb_coeffs = [r / g1, 1.0, b / g1, g2 / g1];
            }
        }
        if let Some(white) = at(layout.white_level).filter(|w| *w > 0.0) {
            image.white_level = white;
        }
        let black: Vec<f32> = (0..4).filter_map(|i| at(layout.black_level + i)).collect();
        if let Ok(black) = <[f32; 4]>::try_from(black) {
            if black.iter().all(|b| *b < image.white_level) {
                image.black_levels = black;
            }
        }
    }

    /// The ColorData (0x4001) array out of the CTMD track.
    ///
    /// A CTMD sample is a run of records — `u32 size`, `u16 type`,
    /// `u16` — of which types 7, 8 and 9 hold, after four bytes of
    /// their own, a sequence of blocks: `u32 size`, `u32 tag`, then a
    /// complete little-endian TIFF. The block tagged 0x927C is a Canon
    /// makernote IFD, and ColorData is an entry in it.
    fn color_data(&self) -> Option<Vec<u16>> {
        for track in &self.tracks {
            if &track.kind != b"CTMD" {
                continue;
            }
            for i in 0..track.offsets.len() {
                let (start, len) = track.sample(i)?;
                let sample = self.bytes.get(start..start.checked_add(len)?)?;
                if let Some(found) = color_data_in(self.bytes, start, sample) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// The largest JPEG in the file: the preview track's sample, the
    /// `PRVW` box in the top-level preview uuid, or the thumbnail in
    /// `THMB`. The R8 generation codes the first two as HEVC and there
    /// is no JPEG in the file at all, so this returns `None` there
    /// rather than something a viewer cannot open.
    fn preview(&self) -> Option<Vec<u8>> {
        let mut best: Option<&[u8]> = None;
        let mut consider = |candidate: Option<&'a [u8]>| {
            if let Some(jpeg) = candidate.filter(|j| j.starts_with(&[0xff, 0xd8])) {
                if best.is_none_or(|best| jpeg.len() > best.len()) {
                    best = Some(jpeg);
                }
            }
        };
        for track in &self.tracks {
            // A CRAW entry holding an HEVC box is the R8's preview
            // track: a still HEVC frame, not a JPEG.
            if track.hevc || &track.kind == b"CTMD" || track.cmp1.is_some() {
                continue;
            }
            if let Some((at, len)) = track.sample(0) {
                consider(self.bytes.get(at..at.checked_add(len)?));
            }
        }
        // PRVW: a version word, the preview's size in pixels, a count
        // and the JPEG's length, then the stream — sixteen bytes in
        // front of it. Version 1, on the R8, describes an HEVC frame
        // instead, and the `starts_with` test drops it.
        if let Some(prvw) =
            find_uuid(&self.boxes, &UUID_CANON_PREVIEW).and_then(|b| b.child(b"PRVW"))
        {
            consider(self.bytes.get(prvw.data.start + 16..prvw.data.end));
        }
        // THMB has the same shape and holds the little 160x120 one.
        if let Some(thmb) = find_uuid(&self.boxes, &UUID_CANON_CRAW).and_then(|b| b.child(b"THMB"))
        {
            consider(self.bytes.get(thmb.data.start + 16..thmb.data.end));
        }
        best.map(<[u8]>::to_vec)
    }
}

/// Where the fields this decoder wants sit in a ColorData record.
///
/// Canon rewrites the layout every few generations and marks it with
/// the version in the first int16. The positions below are the ones
/// ExifTool documents for the three layouts that appear in CR3 files,
/// checked against every sample in the corpus; a version outside them
/// is left alone rather than read at the wrong offset.
#[derive(Debug, Clone, Copy)]
struct ColorDataLayout {
    white_balance: usize,
    black_level: usize,
    white_level: usize,
}

impl ColorDataLayout {
    fn of(color_data: &[u16]) -> Option<ColorDataLayout> {
        let layout = match *color_data.first()? {
            // EOS R, RP, M50, 90D, M6 II, M200, 850D.
            17..=19 => ColorDataLayout {
                white_balance: 0x47,
                black_level: 0x149,
                // NormalWhiteLevel is at 0x31c and the saturation
                // point — what LibRaw calls the linearity limit and
                // what a developer must clip at — at 0x31d.
                white_level: 0x31d,
            },
            // 1D X Mark III, R5, R6.
            32..=34 => ColorDataLayout {
                white_balance: 0x55,
                black_level: 0x157,
                white_level: 0x32b,
            },
            // R3, R7, R10, R6 Mark II, R8, R50.
            42..=48 => ColorDataLayout {
                white_balance: 0x69,
                black_level: 0x16b,
                white_level: 0x281,
            },
            _ => return None,
        };
        // Canon's greens are both 1024 in every as-shot record; if
        // they are not, the offset is wrong for this file and the
        // values would be noise.
        let green = |i: usize| color_data.get(i).copied();
        (green(layout.white_balance + 1) == Some(1024)
            && green(layout.white_balance + 2) == Some(1024))
        .then_some(layout)
    }
}

/// The ColorData array inside one CTMD sample, `sample` starting at
/// absolute offset `base` in `bytes`.
fn color_data_in(bytes: &[u8], base: usize, sample: &[u8]) -> Option<Vec<u16>> {
    let u32_at = |at: usize| -> Option<u32> {
        let b = sample.get(at..at + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut at = 0usize;
    while at + 8 <= sample.len() {
        let size = u32_at(at)? as usize;
        let kind = u16::from_le_bytes([*sample.get(at + 4)?, *sample.get(at + 5)?]);
        if size < 8 || at + size > sample.len() {
            return None;
        }
        if matches!(kind, 7..=9) {
            // Four bytes of the record's own, then the blocks.
            let mut block = at + 12;
            while block + 8 <= at + size {
                let block_size = u32_at(block)? as usize;
                let tag = u32_at(block + 4)?;
                if block_size < 8 || block + block_size > at + size {
                    break;
                }
                if tag == tags::MAKER_NOTE as u32 {
                    // The block's TIFF has its own header, and every
                    // offset in it is relative to that header.
                    if let Ok(tiff) = Tiff::parse_embedded(bytes, base + block + 8) {
                        if let Some(entry) = tiff.find(canon::COLOR_DATA) {
                            let values: Vec<u16> = (0..entry.count)
                                .map_while(|i| entry.u32(i).map(|v| v as u16))
                                .collect();
                            if values.len() > 0x100 {
                                return Some(values);
                            }
                        }
                    }
                }
                block += block_size;
            }
        }
        at += size;
    }
    None
}

/// The first `uuid` box anywhere in the tree with this type.
fn find_uuid<'b>(boxes: &'b [Box_], id: &[u8; 16]) -> Option<&'b Box_> {
    for b in boxes {
        if b.uuid.as_ref() == Some(id) {
            return Some(b);
        }
        if let Some(found) = find_uuid(&b.children, id) {
            return Some(found);
        }
    }
    None
}

/// One `trak`, if it has a sample entry and a sample table.
fn track(bytes: &[u8], trak: &Box_) -> Option<Track> {
    let stbl = trak.find_all(b"stbl").into_iter().next()?;
    let entry = stbl.child(b"stsd")?.children.first()?;
    let sizes = sample_sizes(bytes, stbl)?;
    let offsets = chunk_offsets(bytes, stbl)?;
    Some(Track {
        kind: entry.kind,
        cmp1: entry.child(b"CMP1").map(|b| b.data.clone()),
        // CDI1 is a full box: four bytes of version and flags, then
        // the IAD1 box with the sensor's rectangles.
        iad1: entry
            .child(b"CDI1")
            .and_then(|cdi1| child_box(bytes, &cdi1.data, b"IAD1")),
        hevc: entry.child(b"HEVC").is_some(),
        offsets,
        sizes,
    })
}

/// The payload range of a child box inside a range that is not parsed
/// as a container — CDI1's IAD1, which sits after CDI1's own version
/// word.
fn child_box(
    bytes: &[u8],
    parent: &std::ops::Range<usize>,
    kind: &[u8; 4],
) -> Option<std::ops::Range<usize>> {
    let mut at = parent.start + 4;
    while at + 8 <= parent.end {
        let size = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        if size < 8 || at + size > parent.end {
            return None;
        }
        if bytes.get(at + 4..at + 8)? == kind {
            return Some(at + 8..at + size);
        }
        at += size;
    }
    None
}

/// `stsz`: a version word, a sample size that is zero when the sizes
/// are listed one by one, and a count.
fn sample_sizes(bytes: &[u8], stbl: &Box_) -> Option<Vec<usize>> {
    let stsz = stbl.child(b"stsz")?;
    let p = bytes.get(stsz.data.clone())?;
    let u32_at = |at: usize| -> Option<usize> {
        Some(u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?) as usize)
    };
    let uniform = u32_at(4)?;
    let count = u32_at(8)?.min(MAX_SAMPLES);
    if uniform != 0 {
        return Some(vec![uniform; count]);
    }
    (0..count).map(|i| u32_at(12 + 4 * i)).collect()
}

/// `co64` (64-bit) or `stco` (32-bit): the absolute file offset of
/// each chunk. A CR3 track is one sample a chunk, so these are the
/// sample offsets.
fn chunk_offsets(bytes: &[u8], stbl: &Box_) -> Option<Vec<usize>> {
    let (b, wide) = match (stbl.child(b"co64"), stbl.child(b"stco")) {
        (Some(co64), _) => (co64, true),
        (None, Some(stco)) => (stco, false),
        (None, None) => return None,
    };
    let p = bytes.get(b.data.clone())?;
    let count = (u32::from_be_bytes(p.get(4..8)?.try_into().ok()?) as usize).min(MAX_SAMPLES);
    (0..count)
        .map(|i| {
            if wide {
                let at = 8 + 8 * i;
                usize::try_from(u64::from_be_bytes(p.get(at..at + 8)?.try_into().ok()?)).ok()
            } else {
                let at = 8 + 4 * i;
                Some(u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?) as usize)
            }
        })
        .collect()
}

/// A still CR3 track holds one sample; the cap is only so a corrupt
/// count cannot make us allocate.
const MAX_SAMPLES: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiff::tests::{corpus, samples};

    // ---------------------------------------------------------------
    // Hand-built files.
    // ---------------------------------------------------------------

    fn box_(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn uuid_box(id: &[u8; 16], payload: &[u8]) -> Vec<u8> {
        let mut body = id.to_vec();
        body.extend_from_slice(payload);
        box_(b"uuid", &body)
    }

    /// A little-endian TIFF of one IFD with these entries, each
    /// `(tag, kind, count, value-or-offset)`, and the blob its
    /// out-of-line values live in appended after it.
    fn tiff(entries: &[(u16, u16, u32, u32)], tail: &[u8]) -> Vec<u8> {
        let mut out = b"II*\0".to_vec();
        out.extend(8u32.to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        for (tag, kind, count, value) in entries {
            out.extend(tag.to_le_bytes());
            out.extend(kind.to_le_bytes());
            out.extend(count.to_le_bytes());
            out.extend(value.to_le_bytes());
        }
        out.extend(0u32.to_le_bytes());
        out.extend_from_slice(tail);
        out
    }

    /// A JFIF-shaped stream a viewer would accept.
    fn jpeg(padding: usize) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 0x11, 0];
        out.extend(std::iter::repeat_n(0u8, padding));
        out.extend([0xff, 0xd9]);
        out
    }

    /// A sample table for one sample at `offset` of `size` bytes.
    fn sample_table(entry: Vec<u8>, offset: u64, size: u32) -> Vec<u8> {
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(entry);
        let mut stsz = vec![0u8; 8];
        stsz.extend(1u32.to_be_bytes());
        stsz.extend(size.to_be_bytes());
        let mut co64 = vec![0u8; 4];
        co64.extend(1u32.to_be_bytes());
        co64.extend(offset.to_be_bytes());
        box_(
            b"stbl",
            &[
                box_(b"stsd", &stsd),
                box_(b"stsz", &stsz),
                box_(b"co64", &co64),
            ]
            .concat(),
        )
    }

    fn trak(stbl: Vec<u8>) -> Vec<u8> {
        box_(b"trak", &box_(b"mdia", &box_(b"minf", &stbl)))
    }

    /// A CRAW sample entry: 82 bytes of fixed fields, then the boxes.
    fn craw_entry(children: &[u8]) -> Vec<u8> {
        let mut payload = vec![0u8; 82];
        payload.extend_from_slice(children);
        box_(b"CRAW", &payload)
    }

    fn cmp1_box(width: u32, height: u32, levels: u8, header: u32) -> Vec<u8> {
        let mut p = vec![0u8; 52];
        p[0..2].copy_from_slice(&0xff00u16.to_be_bytes());
        p[2..4].copy_from_slice(&0x0030u16.to_be_bytes());
        p[4..6].copy_from_slice(&1u16.to_be_bytes());
        p[8..12].copy_from_slice(&width.to_be_bytes());
        p[12..16].copy_from_slice(&height.to_be_bytes());
        p[16..20].copy_from_slice(&width.to_be_bytes());
        p[20..24].copy_from_slice(&height.to_be_bytes());
        p[24] = 14;
        p[25] = 0x40;
        p[26] = levels;
        p[28..32].copy_from_slice(&header.to_be_bytes());
        box_(b"CMP1", &p)
    }

    /// CDI1 wrapping an IAD1 whose first rectangle is this crop.
    fn cdi1_box(width: u16, height: u16, crop: [u16; 4]) -> Vec<u8> {
        let mut iad1 = vec![0u8; 4];
        iad1.extend(width.to_be_bytes());
        iad1.extend(height.to_be_bytes());
        iad1.extend([0, 1, 0, 0, 0, 1, 0, 0]);
        for edge in crop {
            iad1.extend(edge.to_be_bytes());
        }
        let mut cdi1 = vec![0u8; 4];
        cdi1.extend(box_(b"IAD1", &iad1));
        box_(b"CDI1", &cdi1)
    }

    /// A CR3 with a preview track, a raw track and Canon's metadata
    /// uuid. `mdat` holds the preview JPEG then the raw sample.
    fn synthetic_cr3(raw_sample: &[u8], levels: u8, mdat_header: u32) -> Vec<u8> {
        let preview = jpeg(200);
        // Build the moov first with placeholder offsets, then fix them
        // up once its length is known: mdat follows moov.
        let build = |mdat_at: u64| -> Vec<u8> {
            let cmt1 = tiff(
                &[
                    // Three entries make the directory 50 bytes, so
                    // the strings after it start at 0x32.
                    (tags::MAKE, 2, 6, 0x32),
                    (tags::MODEL, 2, 12, 0x38),
                    (tags::ORIENTATION, 3, 1, 6),
                ],
                b"Canon\0Canon EOS R\0",
            );
            let cmt2 = tiff(&[(tags::ISO, 3, 1, 400)], b"");
            let cmt3 = tiff(&[(canon::SENSOR_INFO, 3, 17, 0x1a)], &{
                let mut v = Vec::new();
                for x in [34u16, 64, 32, 1, 1, 4, 2, 59, 29, 0, 0, 0, 0, 0, 0, 0, 0] {
                    v.extend(x.to_le_bytes());
                }
                v
            });
            let craw_uuid = uuid_box(
                &UUID_CANON_CRAW,
                &[
                    box_(b"CNCV", b"CanonCR3_001/00.09.00/00.00.00"),
                    box_(b"CMT1", &cmt1),
                    box_(b"CMT2", &cmt2),
                    box_(b"CMT3", &cmt3),
                    box_(b"THMB", &[vec![0u8; 16], jpeg(4)].concat()),
                ]
                .concat(),
            );
            let preview_trak = trak(sample_table(
                box_(b"JPEG", &[0u8; 78]),
                mdat_at,
                preview.len() as u32,
            ));
            let raw_trak = trak(sample_table(
                craw_entry(
                    &[
                        cmp1_box(64, 32, levels, mdat_header),
                        cdi1_box(64, 32, [4, 2, 59, 29]),
                    ]
                    .concat(),
                ),
                mdat_at + preview.len() as u64,
                raw_sample.len() as u32,
            ));
            box_(b"moov", &[craw_uuid, preview_trak, raw_trak].concat())
        };
        let ftyp = box_(b"ftyp", b"crx isom");
        let moov = build(0);
        let mdat_at = (ftyp.len() + moov.len() + 8) as u64;
        let moov = build(mdat_at);
        assert_eq!(
            mdat_at,
            (ftyp.len() + moov.len() + 8) as u64,
            "moov length must not shift"
        );
        [
            ftyp,
            moov,
            box_(b"mdat", &[preview.clone(), raw_sample.to_vec()].concat()),
        ]
        .concat()
    }

    #[test]
    fn probe_and_metadata_of_a_synthetic_file() {
        let file = synthetic_cr3(&[0u8; 16], 0, 16);
        assert_eq!(crate::probe(&file), Some(Format::Cr3));
        let parsed = File::parse(&file).unwrap();
        let mut image = RawImage::new(
            Format::Cr3,
            64,
            32,
            1,
            RawData::U16(vec![0; 64 * 32]),
            Cfa::RGGB,
        );
        parsed.describe(&mut image);
        assert_eq!(image.make, "Canon");
        assert_eq!(image.model, "Canon EOS R");
        assert_eq!(image.orientation, Orientation::Rotate90CW);
        assert_eq!(image.metadata.iso, Some(400.0));
        // SensorInfo's borders are inclusive, so 4..=59 is 56 wide.
        assert_eq!(
            image.crop,
            Rect {
                x: 4,
                y: 2,
                width: 56,
                height: 28
            }
        );
        // The track's JPEG is bigger than the one in THMB.
        let preview = image.preview.expect("a preview");
        assert_eq!(preview.len(), jpeg(200).len());
        assert_eq!(super::preview(&file).unwrap(), Some(preview));
    }

    #[test]
    fn the_crop_falls_back_to_iad1_without_a_makernote() {
        let mut file = synthetic_cr3(&[0u8; 16], 0, 16);
        // Break CMT3's fourcc so the makernote cannot be found; IAD1
        // carries the same rectangle.
        let at = file.windows(4).position(|w| w == b"CMT3").unwrap();
        file[at..at + 4].copy_from_slice(b"CMTX");
        let parsed = File::parse(&file).unwrap();
        let mut image = RawImage::new(
            Format::Cr3,
            64,
            32,
            1,
            RawData::U16(vec![0; 64 * 32]),
            Cfa::RGGB,
        );
        parsed.describe(&mut image);
        assert_eq!(image.crop.width, 56);
        assert_eq!(image.crop.height, 28);
    }

    #[test]
    fn an_unsupported_codec_variant_says_so() {
        // Three wavelet levels with a header that describes nothing.
        let file = synthetic_cr3(&[0u8; 16], 3, 0);
        assert!(matches!(
            decode(&file),
            Err(Error::Unsupported(_)) | Err(Error::Corrupt(_))
        ));
        // The preview is still reachable without the sensor data.
        assert!(preview(&file).unwrap().is_some());
    }

    #[test]
    fn truncated_and_garbage_files_are_errors_not_panics() {
        let file = synthetic_cr3(&[0u8; 64], 0, 16);
        for cut in 0..file.len() {
            let head = &file[..cut];
            assert!(
                std::panic::catch_unwind(|| {
                    let _ = decode(head);
                    let _ = preview(head);
                })
                .is_ok(),
                "panic on a {cut}-byte prefix"
            );
        }
        for garbage in [vec![0u8; 64], b"ftypcrx ".to_vec(), vec![0xff; 4096]] {
            assert!(std::panic::catch_unwind(|| {
                let _ = decode(&garbage);
                let _ = preview(&garbage);
            })
            .is_ok());
        }
    }

    // ---------------------------------------------------------------
    // Corpus.
    // ---------------------------------------------------------------

    /// A field of `raw-identify -v -w` output.
    fn identify(path: &std::path::Path) -> Option<String> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".identify.txt");
        std::fs::read_to_string(std::path::PathBuf::from(name)).ok()
    }

    /// The numbers on the line beginning `key`.
    fn numbers(text: &str, key: &str) -> Vec<f64> {
        text.lines()
            .find(|l| l.trim_start().starts_with(key))
            .map(|line| {
                line[line.find(key).unwrap() + key.len()..]
                    .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
                    .filter_map(|t| t.parse::<f64>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `unprocessed_raw -T`'s output beside a sample.
    fn oracle_frame(path: &std::path::Path) -> Option<(usize, usize, Vec<u16>)> {
        let mut name = path.as_os_str().to_os_string();
        name.push(".tiff");
        let image = image::open(std::path::PathBuf::from(name))
            .ok()?
            .into_luma16();
        Some((
            image.width() as usize,
            image.height() as usize,
            image.into_raw(),
        ))
    }

    /// Files whose CRX variant this decoder knows it cannot do yet,
    /// with why. Anything else that fails is a bug.
    ///
    /// Every CR3 is on this list at the moment: the container, the
    /// metadata, the preview and every layer of the CRX headers are
    /// read, but the entropy-coded coefficients are not decoded, so
    /// `decode` reports the codec as unsupported and only `preview`
    /// and the metadata below are exercised against real files.
    fn unsupported_reason(_name: &str) -> Option<&'static str> {
        Some("the CRX entropy coder is not implemented")
    }

    #[test]
    fn corpus_cr3_files_decode_and_match_the_oracle() {
        let Some(root) = corpus() else { return };
        let mut problems: Vec<String> = Vec::new();
        let mut decoded = 0;
        let mut skipped = 0;
        for path in samples(&root) {
            let is_cr3 = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_uppercase() == "CR3")
                .unwrap_or(false);
            if !is_cr3 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let name = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            macro_rules! note {
                ($what:expr) => {
                    problems.push(format!("{}: {}", name, $what))
                };
            }

            if crate::probe(&bytes) != Some(Format::Cr3) {
                note!("probe does not say Cr3");
                continue;
            }
            // The preview is reachable whatever the codec does. The
            // R8 generation writes HEVC for both its previews, so
            // `None` is the right answer there and the identify
            // oracle agrees with a thumbnail size of zero.
            let text = identify(&path).unwrap_or_default();
            let thumb = numbers(&text, "Thumb size:");
            match preview(&bytes) {
                Ok(Some(jpeg)) => {
                    if image::load_from_memory(&jpeg).is_err() {
                        note!("the preview does not decode");
                    }
                    if thumb.first() == Some(&0.0) {
                        note!("a preview where LibRaw found none");
                    }
                }
                Ok(None) => {
                    if thumb.first().is_some_and(|w| *w > 0.0) {
                        note!(format!("no preview, LibRaw found {thumb:?}"));
                    }
                }
                Err(e) => note!(format!("preview: {e}")),
            }

            // Ten cuts through a real file: none may panic, and a
            // prefix that still holds the moov must still probe.
            for i in 0..10 {
                let cut = bytes.len() * (i * 7 + 3) / 71;
                let head = &bytes[..cut];
                let survived = std::panic::catch_unwind(|| {
                    let _ = decode(head);
                    let _ = preview(head);
                    let _ = crate::probe(head);
                });
                if survived.is_err() {
                    note!(format!("panic on a {cut}-byte prefix"));
                }
            }

            let image = match decode(&bytes) {
                Ok(image) => image,
                Err(Error::Unsupported(why)) => {
                    match unsupported_reason(&name) {
                        Some(_) => skipped += 1,
                        None => note!(format!("unexpectedly unsupported: {why}")),
                    }
                    // Metadata is worth checking even when the pixels
                    // are out of reach.
                    let parsed = File::parse(&bytes).unwrap();
                    let mut image =
                        RawImage::new(Format::Cr3, 2, 2, 1, RawData::U16(vec![0; 4]), Cfa::RGGB);
                    let full = numbers(&text, "Full size:");
                    if let [w, h] = full[..] {
                        image.width = w as usize;
                        image.height = h as usize;
                        image.data = RawData::U16(vec![0; image.width * image.height]);
                        image.crop = Rect {
                            x: 0,
                            y: 0,
                            width: image.width,
                            height: image.height,
                        };
                    }
                    parsed.describe(&mut image);
                    check_metadata(&name, &image, &text, &mut problems);
                    continue;
                }
                Err(e) => {
                    note!(format!("decode: {e}"));
                    continue;
                }
            };
            decoded += 1;
            if let Err(e) = image.validate() {
                note!(format!("validate: {e}"));
            }
            check_metadata(&name, &image, &text, &mut problems);
            match oracle_frame(&path) {
                None => note!("no oracle TIFF beside it"),
                Some((width, height, want)) => {
                    if (width, height) != (image.width, image.height) {
                        note!(format!(
                            "{}x{} against the oracle's {width}x{height}",
                            image.width, image.height
                        ));
                        continue;
                    }
                    let RawData::U16(have) = &image.data else {
                        note!("not 16-bit samples");
                        continue;
                    };
                    let mut wrong = Vec::new();
                    let mut count = 0usize;
                    for (i, (a, b)) in have.iter().zip(&want).enumerate() {
                        if a != b {
                            count += 1;
                            if wrong.len() < 6 {
                                wrong.push(format!("[{},{}] {a} != {b}", i % width, i / width));
                            }
                        }
                    }
                    if count > 0 {
                        note!(format!("{count} samples differ: {}", wrong.join(", ")));
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "{} problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
        eprintln!("corpus: {decoded} CR3 files decoded, {skipped} unsupported variants skipped");
    }

    /// Levels, white balance, crop, orientation and CFA against
    /// `raw-identify`.
    fn check_metadata(name: &str, image: &RawImage, text: &str, problems: &mut Vec<String>) {
        let mut note = |what: String| problems.push(format!("{name}: {what}"));
        if !image.make.eq_ignore_ascii_case("Canon") {
            note(format!("make {:?}", image.make));
        }
        let full = numbers(text, "Full size:");
        if let [w, h] = full[..] {
            if (image.width, image.height) != (w as usize, h as usize) {
                note(format!(
                    "frame {}x{}, LibRaw {w}x{h}",
                    image.width, image.height
                ));
            }
        }
        // "Raw inset, width x height: 6720 x 4480 left: 156 top: 58".
        let inset = numbers(text, "Raw inset, width x height:");
        if let [w, h, x, y] = inset[..] {
            let want = Rect {
                x: x as usize,
                y: y as usize,
                width: w as usize,
                height: h as usize,
            };
            if image.crop != want {
                note(format!("crop {:?}, LibRaw's inset {want:?}", image.crop));
            }
        }
        if let [flip] = numbers(text, "Image flip:")[..] {
            let want = match flip as u32 {
                3 => Orientation::Rotate180,
                5 => Orientation::Rotate270CW,
                6 => Orientation::Rotate90CW,
                _ => Orientation::Normal,
            };
            if image.orientation != want {
                note(format!(
                    "orientation {:?}, LibRaw {want:?}",
                    image.orientation
                ));
            }
        }
        if let Some(pattern) = text.lines().find(|l| l.starts_with("Filter pattern:")) {
            if !pattern.contains("RGGB") || image.cfa != Cfa::RGGB {
                note(format!("CFA {:?} against {pattern:?}", image.cfa));
            }
        }
        if let Some(&white) = numbers(text, "Highlight linearity limits:").first() {
            if image.white_level != white as f32 {
                note(format!("white {}, LibRaw {white}", image.white_level));
            }
        }
        // LibRaw only prints cblack for the generations it reads a
        // per-channel black for; where it does, it must agree.
        let cblack = numbers(text, "cblack[0 .. 3]:");
        if cblack.len() == 4 {
            let want: Vec<f32> = cblack.iter().map(|v| *v as f32).collect();
            if image.black_levels.to_vec() != want {
                note(format!("black {:?}, LibRaw {want:?}", image.black_levels));
            }
        } else if image.black_levels == [0.0; 4] {
            note("no black level at all".into());
        }
        // "As shot   2001 1024 1582 1024" is R, G, B, G2 unnormalised.
        let shot = numbers(text, "As shot");
        if let [r, g, b, g2] = shot[..] {
            let want = [
                r as f32 / g as f32,
                1.0,
                b as f32 / g as f32,
                g2 as f32 / g as f32,
            ];
            let off = (0..4).any(|i| (image.wb_coeffs[i] - want[i]).abs() > 1e-4);
            if off {
                note(format!(
                    "white balance {:?}, LibRaw {want:?}",
                    image.wb_coeffs
                ));
            }
        }
    }
}
