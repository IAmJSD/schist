//! One module per container. Each exposes
//!
//! ```text
//! pub fn decode(bytes: &[u8]) -> Result<RawImage>;
//! pub fn preview(bytes: &[u8]) -> Result<Option<Vec<u8>>>;
//! ```
//!
//! `decode` fills a [`crate::RawImage`] completely — data, CFA, levels,
//! white balance, crop, orientation, make/model via `set_camera`,
//! preview, metadata — and calls `apply_camera_table` last. `preview`
//! finds the largest embedded JPEG without decoding sensor data.

pub mod arw;
pub mod cr2;
pub mod cr3;
pub mod crw;
pub mod crx;
pub mod dng;
pub mod erf;
pub mod hasselblad;
pub mod iiq;
pub mod kodak;
pub mod mef;
pub mod mos;
pub mod mrw;
pub mod nef;
pub mod orf;
pub mod pef;
pub mod raf;
pub mod raf_compressed;
pub mod rw2;
pub mod srw;
pub mod vc5;
pub mod x3f;

/// Shared helpers for the TIFF-shaped formats.
pub mod common {
    use crate::tiff::{tags, Tiff};
    use crate::{Metadata, Orientation};

    /// The largest JPEG referenced by JPEGInterchangeFormat/Length or
    /// stored as a JPEG-compressed (6/7) strip in any IFD.
    ///
    /// Vendors disagree about where the preview goes: Canon and Nikon
    /// point at it with JPEGInterchangeFormat (0x0201) and its length
    /// (0x0202), Sony and Pentax store it as the single strip of an
    /// IFD whose Compression says JPEG, Olympus marks the preview IFD
    /// only with NewSubfileType 1. All three are checked, in every IFD
    /// of the file, and the biggest JPEG wins — that is the full-size
    /// preview rather than the 160x120 thumbnail.
    pub fn largest_jpeg(tiff: &Tiff<'_>) -> Option<Vec<u8>> {
        let bytes = tiff.bytes();
        let base = tiff.base();
        let mut best: Option<(usize, usize)> = None;
        let mut consider = |offset: u64, len: u64| {
            let (Ok(offset), Ok(len)) = (usize::try_from(offset), usize::try_from(len)) else {
                return;
            };
            let Some(start) = offset.checked_add(base) else {
                return;
            };
            let Some(end) = start.checked_add(len) else {
                return;
            };
            // Only a real, showable JPEG counts: several formats leave
            // a stale 0x0201 pointing into the raw data, and the raw
            // data itself is frequently a lossless-JPEG stream that
            // begins with the same two bytes.
            let Some(stream) = bytes.get(start..end) else {
                return;
            };
            if !stream.starts_with(&[0xff, 0xd8]) || !is_displayable_jpeg(stream) {
                return;
            }
            if best.is_none_or(|(_, best)| len > best) {
                best = Some((start, len));
            }
        };
        for ifd in tiff.all() {
            if let (Some(offset), Some(len)) = (
                ifd.get(tags::JPEG_INTERCHANGE_FORMAT)
                    .and_then(|e| e.u64(0)),
                ifd.get(tags::JPEG_INTERCHANGE_FORMAT_LENGTH)
                    .and_then(|e| e.u64(0)),
            ) {
                consider(offset, len);
            }
            let compression = ifd
                .get(tags::COMPRESSION)
                .and_then(|e| e.u32(0))
                .unwrap_or(0);
            let preview_ifd = ifd.get(tags::NEW_SUBFILE_TYPE).and_then(|e| e.u32(0)) == Some(1);
            // An IFD that says it holds sensor samples holds sensor
            // samples, however JPEG-shaped they are: 32803 is CFA,
            // 34892 LinearRaw.
            if matches!(
                ifd.get(tags::PHOTOMETRIC).and_then(|e| e.u32(0)),
                Some(32803 | 34892)
            ) {
                continue;
            }
            // 6 is TIFF 6.0's old-style JPEG, 7 the "new" one; both
            // hold a complete JFIF stream when there is one strip.
            if compression == 6 || compression == 7 || preview_ifd {
                let offsets = ifd
                    .get(tags::STRIP_OFFSETS)
                    .map(|e| e.u64s())
                    .unwrap_or_default();
                let lengths = ifd
                    .get(tags::STRIP_BYTE_COUNTS)
                    .map(|e| e.u64s())
                    .unwrap_or_default();
                // A JPEG split over several strips is not a JPEG we can
                // hand out whole, and nobody stores previews that way.
                if let ([offset], [len]) = (&offsets[..], &lengths[..]) {
                    consider(*offset, *len);
                }
            }
        }
        best.and_then(|(start, len)| bytes.get(start..start + len).map(|s| s.to_vec()))
    }

    /// Whether a JPEG stream is one a viewer could show.
    ///
    /// Raw data is very often *also* a JPEG: Canon's CR2 keeps the
    /// sensor in a lossless (SOF3) stream, as do lossless DNG,
    /// Hasselblad and Pentax, and all of them start with the same
    /// FFD8. A preview is a picture — baseline, extended sequential or
    /// progressive — so the first frame header decides it. The scan
    /// walks marker segments only, never entropy-coded data, and stops
    /// at the first SOF or at anything malformed.
    fn is_displayable_jpeg(stream: &[u8]) -> bool {
        let mut at = 2;
        while at + 4 <= stream.len() {
            if stream[at] != 0xff {
                return false;
            }
            let marker = stream[at + 1];
            match marker {
                // Padding, and the standalone markers with no length.
                0xff => {
                    at += 1;
                    continue;
                }
                0x01 | 0xd0..=0xd8 => {
                    at += 2;
                    continue;
                }
                // SOF0/1/2 and their arithmetic-coded twins SOF9/10.
                0xc0 | 0xc1 | 0xc2 | 0xc9 | 0xca => return true,
                // SOF3 and the rest of the lossless and hierarchical
                // family: raw data, not a picture.
                0xc3 | 0xc5..=0xc7 | 0xcb | 0xcd..=0xcf => return false,
                // A scan before any frame header is not a JPEG we
                // understand well enough to hand out.
                0xda | 0xd9 => return false,
                _ => {}
            }
            let len = u16::from_be_bytes([stream[at + 2], stream[at + 3]]) as usize;
            if len < 2 {
                return false;
            }
            at += 2 + len;
        }
        false
    }

    /// ISO, exposure, aperture, focal length, lens and date from the
    /// Exif IFD (and IFD0's DateTime as a fallback).
    pub fn metadata(tiff: &Tiff<'_>) -> Metadata {
        let exif = tiff.exif();
        // The Exif IFD is where these belong, but a few raws (Panasonic
        // RW2, some Olympus) put the same tags straight in IFD0 and
        // have no Exif IFD at all, so fall back to a search of the
        // whole file before giving up on a tag.
        let entry = |tag: u16| exif.and_then(|ifd| ifd.get(tag)).or_else(|| tiff.find(tag));
        let number = |tag: u16| entry(tag).and_then(|e| e.f64(0)).map(|v| v as f32);
        let text = |tag: u16| {
            entry(tag)
                .and_then(|e| e.str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        Metadata {
            iso: number(tags::ISO),
            exposure_time: number(tags::EXPOSURE_TIME),
            f_number: number(tags::F_NUMBER),
            focal_length: number(tags::FOCAL_LENGTH),
            lens: text(tags::LENS_MODEL),
            // DateTimeOriginal is when the shutter fired; IFD0's
            // DateTime is when the file was written, which for a raw
            // straight out of a camera is the same moment.
            date_time: text(tags::DATE_TIME_ORIGINAL).or_else(|| {
                tiff.root()
                    .get(tags::DATE_TIME)
                    .and_then(|e| e.str())
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
            }),
        }
    }

    /// IFD0's Orientation tag.
    pub fn orientation(tiff: &Tiff<'_>) -> Orientation {
        tiff.root()
            .get(tags::ORIENTATION)
            .and_then(|e| e.u32(0))
            .map(Orientation::from_exif)
            .unwrap_or_default()
    }
}
