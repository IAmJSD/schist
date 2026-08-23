//! The Affinity container: a tiny versioned filesystem inside one file.
//!
//! Layout (all little-endian):
//!
//! ```text
//! magic 00 FF 4B 41 · u16 version (7–11) · u16 flags · u32 class tag ("Prsn" for documents)
//! "#Inf" block: FAT offset, thumbnail offset, lengths, creation date, revision
//! version > 7: "Prot" tag + u32 protocol revision
//! ...data...
//! FAT chain at FAT offset: "#FAT"/"#FT2"/"#FT3"/"#FT4" blocks, linked by next-offset
//! ```
//!
//! Each FAT block is one *savepoint* (Affinity keeps history on disk): a
//! creation date plus a table of entries `{id, offset, size, compressed
//! size, crc32, compression}`. An entry with flag 0 also carries the file
//! name that introduced the id; later revisions of the same name repeat
//! only the id (flag 1), and flag 2 marks it deleted. The head revision
//! of a name is the entry in the newest FAT that mentions its id.
//!
//! Entry payloads sit at their offset behind a "#Fil" tag, compressed
//! with zlib (Affinity 1) or zstd (Affinity 2), optionally delta-coded
//! ("prediction") before compression, and carry a CRC-32 of the plain
//! bytes. This module reproduces the exact decode pipeline.

use crate::error::{malformed, AffinityError};

pub const MAGIC: [u8; 4] = [0x00, 0xFF, 0x4B, 0x41];

// Container-level tags are stored as literal byte sequences ("#Inf"
// reads as '#','I','n','f'), unlike class/field tags in the object
// graph, which are stored reversed. Compare little-endian reads against
// little-endian packing here.
const fn tag(name: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*name)
}

const TAG_INF: u32 = tag(b"#Inf");
const TAG_PROT: u32 = tag(b"Prot");
const TAG_THMB: u32 = tag(b"Thmb");
const TAG_FIL: u32 = tag(b"#Fil");
const TAG_FAT: u32 = tag(b"#FAT");
const TAG_FT2: u32 = tag(b"#FT2");
const TAG_FT3: u32 = tag(b"#FT3");
const TAG_FT4: u32 = tag(b"#FT4");

/// Bounded little-endian reader over the raw file bytes.
pub(crate) struct Cursor<'a> {
    pub bytes: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, pos: 0 }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], AffinityError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or(AffinityError::Truncated {
                offset: self.pos,
                needed: n,
            })?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, AffinityError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, AffinityError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, AffinityError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, AffinityError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// A tag as its big-endian name value, so it compares against
    /// `tag(b"#FAT")` even though the file stores the bytes reversed.
    pub fn tag(&mut self) -> Result<u32, AffinityError> {
        self.u32()
    }
    pub fn seek(&mut self, pos: u64) -> Result<(), AffinityError> {
        let pos = usize::try_from(pos).map_err(|_| AffinityError::Truncated {
            offset: usize::MAX,
            needed: 0,
        })?;
        if pos > self.bytes.len() {
            return Err(AffinityError::Truncated {
                offset: pos,
                needed: 0,
            });
        }
        self.pos = pos;
        Ok(())
    }
}

/// One revision of one named entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: u32,
    /// 0 = named (first appearance), 1 = revision, 2 = deleted.
    pub flag: u8,
    pub offset: u64,
    pub size: u64,
    pub compressed_size: u64,
    pub crc32: u32,
    /// Normalized compression byte: low 2 bits algorithm (0 stored,
    /// 1 zlib, 2 zstd), bits 6–7 prediction type, bit 5 prediction flag.
    pub compression: u8,
    /// Creation date of the FAT (savepoint) this revision belongs to.
    pub savepoint: u64,
}

/// The parsed container: header facts plus every entry of every savepoint.
pub struct Archive<'a> {
    bytes: &'a [u8],
    pub version: u16,
    pub class_tag: u32,
    pub creation_date: u64,
    thumb_offset: u64,
    /// Name → id, as introduced by flag-0 entries.
    names: rustc_hash::FxHashMap<String, u32>,
    entries: Vec<Entry>,
    /// First-appearance order of names, for `names()`.
    name_order: Vec<String>,
    /// Id → index in `entries` of its newest revision. Documents hold a
    /// tile entry per 64 KiB of pixels, and each is looked up by name;
    /// scanning the entry table per lookup is quadratic in file size.
    heads: rustc_hash::FxHashMap<u32, usize>,
}

pub fn is_affinity(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

impl<'a> Archive<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Archive<'a>, AffinityError> {
        if !is_affinity(bytes) {
            return Err(AffinityError::NotAffinity);
        }
        let mut c = Cursor::new(bytes);
        c.pos = 4;
        let version = c.u16()?;
        let flags = c.u16()?;
        let class_tag = c.tag()?;
        if !(7..=12).contains(&version) {
            return Err(AffinityError::UnsupportedVersion(version));
        }
        if flags & 3 != 0 {
            return Err(malformed(format!("unsupported header flags {flags:#x}")));
        }
        if c.tag()? != TAG_INF {
            return Err(malformed("missing #Inf block"));
        }
        let fat_offset = c.u64()?;
        let thumb_offset = c.u64()?;
        let _length = c.u64()?;
        let _unknown = c.u64()?;
        let creation_date = c.u64()?;
        let _revision = c.u32()?;
        let _num = c.u32()?;
        if version > 7 {
            if c.tag()? != TAG_PROT {
                return Err(malformed("missing Prot block"));
            }
            let _protocol = c.u32()?;
        }

        let mut archive = Archive {
            bytes,
            version,
            class_tag,
            creation_date,
            thumb_offset,
            names: Default::default(),
            entries: Vec::new(),
            name_order: Vec::new(),
            heads: Default::default(),
        };
        archive.parse_fats(&mut c, fat_offset)?;
        for (at, entry) in archive.entries.iter().enumerate() {
            match archive.heads.entry(entry.id) {
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    // `>=` so that of two revisions in equally-dated
                    // savepoints, the later-listed one wins.
                    if entry.savepoint >= archive.entries[*o.get()].savepoint {
                        o.insert(at);
                    }
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(at);
                }
            }
        }
        Ok(archive)
    }

    fn parse_fats(&mut self, c: &mut Cursor<'a>, first: u64) -> Result<(), AffinityError> {
        let mut offset = first;
        let mut seen = 0u32;
        while offset != 0 {
            // A cycle in the chain would loop forever; no real file has
            // anywhere near this many savepoints.
            seen += 1;
            if seen > 10_000 {
                return Err(malformed("FAT chain does not terminate"));
            }
            c.seek(offset)?;
            let fat_tag = c.tag()?;
            if ![TAG_FAT, TAG_FT2, TAG_FT3, TAG_FT4].contains(&fat_tag) {
                return Err(malformed(format!("bad FAT tag {fat_tag:#010x}")));
            }
            let next_offset = c.u64()?;
            let savepoint = c.u64()?;
            let _some_offset = c.u64()?;
            let _some_length = c.u64()?;
            let _unknown5 = c.u64()?;
            let files_count = c.u32()?;
            let _unknown7 = c.u32()?;
            let _unknown8 = c.u32()?;
            let dirs_count = c.u16()?;
            let _unknown10 = c.u8()?;

            for _ in 0..files_count {
                let id = c.u32()?;
                let flag = c.u8()?;
                if flag > 2 {
                    return Err(malformed(format!("bad entry flag {flag}")));
                }
                let mut entry = Entry {
                    id,
                    flag,
                    offset: 0,
                    size: 0,
                    compressed_size: 0,
                    crc32: 0,
                    compression: 0,
                    savepoint,
                };
                if flag != 2 {
                    entry.offset = c.u64()?;
                    entry.size = c.u64()?;
                    entry.compressed_size = c.u64()?;
                    entry.crc32 = c.u32()?;
                    entry.compression = c.u8()?;
                    if fat_tag != TAG_FAT {
                        let _unknown6 = c.u32()?;
                    }
                    if fat_tag == TAG_FT4 {
                        let _unknown = c.u32()?;
                    }
                    // Early FATs store a compression *index*; later ones
                    // store the full byte. Normalize to the full form.
                    if fat_tag == TAG_FAT || fat_tag == TAG_FT2 {
                        entry.compression = match entry.compression {
                            0x01 => 0x01,
                            0x02 => 0x41,
                            0x03 => 0x81,
                            0x04 => 0xc1,
                            _ => 0,
                        };
                    }
                }
                if flag == 0 {
                    let len = c.u16()? as usize;
                    let name = String::from_utf8_lossy(c.take(len)?).into_owned();
                    match self.names.get(&name) {
                        Some(existing) if *existing != id => {
                            return Err(malformed(format!("name {name:?} maps to two ids")));
                        }
                        Some(_) => {}
                        None => {
                            self.name_order.push(name.clone());
                            self.names.insert(name, id);
                        }
                    }
                }
                self.entries.push(entry);
            }

            for _ in 0..dirs_count {
                let len = c.u16()? as usize;
                let some_len = c.u16()?;
                let _files_num = c.u64()?;
                let _name = c.take(len)?;
                if some_len != 0 {
                    return Err(malformed("unexpected directory payload"));
                }
            }

            offset = next_offset;
        }
        Ok(())
    }

    /// The embedded document thumbnail: a PNG the writing app rendered,
    /// so it doubles as ground truth for what the document looks like.
    ///
    /// Layout at the #Inf block's thumbnail offset:
    /// `FF FF FF FF "Thmb" · u32 count · u32 block_size`, then per
    /// thumbnail a small header (`u32 ?, u32 ?, u32 png_size, u8 ?`)
    /// and the PNG bytes.
    pub fn thumbnail(&self) -> Option<&'a [u8]> {
        if self.thumb_offset == 0 {
            return None;
        }
        let mut c = Cursor::new(self.bytes);
        c.seek(self.thumb_offset).ok()?;
        if c.u32().ok()? != 0xFFFF_FFFF || c.tag().ok()? != TAG_THMB {
            return None;
        }
        let _count = c.u32().ok()?;
        let block_size = c.u32().ok()? as usize;
        let block = c.take(block_size).ok()?;
        // Header sizes have not been stable across writers; the PNG
        // signature inside the (size-bounded) block is.
        let sig = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let start = block.windows(sig.len()).position(|w| w == sig)?;
        Some(&block[start..])
    }

    /// Names of every entry ever stored, in first-appearance order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.name_order.iter().map(|n| n.as_str())
    }

    /// The newest revision of `name`, if it exists and is not deleted.
    pub fn head(&self, name: &str) -> Option<&Entry> {
        let id = self.names.get(name)?;
        let entry = &self.entries[*self.heads.get(id)?];
        (entry.flag != 2).then_some(entry)
    }

    /// Decode one entry: decompress, undo prediction, verify the CRC.
    pub fn extract(&self, entry: &Entry) -> Result<Vec<u8>, AffinityError> {
        if entry.size == 0 {
            return Err(malformed("empty entry"));
        }
        let mut c = Cursor::new(self.bytes);
        c.seek(entry.offset)?;
        if c.tag()? != TAG_FIL {
            return Err(malformed("entry does not point at a #Fil block"));
        }

        let algorithm = entry.compression & 3;
        let mut predict_flag = (entry.compression >> 5) & 1;
        let predict_type = match entry.compression & 0xc0 {
            0x40 => {
                predict_flag = 0;
                1
            }
            0x80 => 2,
            0xc0 => 3,
            _ => {
                predict_flag = 0;
                0
            }
        };

        let mut plain = match algorithm {
            1 => {
                let raw = c.take(entry.compressed_size as usize)?;
                miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
                    raw,
                    entry.size as usize + 1,
                )
                .map_err(|e| malformed(format!("zlib: {e}")))?
            }
            2 => {
                let raw = c.take(entry.compressed_size as usize)?;
                let mut decoder = ruzstd::decoding::StreamingDecoder::new(raw)
                    .map_err(|e| malformed(format!("zstd: {e}")))?;
                let mut out = Vec::with_capacity(entry.size as usize);
                std::io::Read::read_to_end(&mut decoder, &mut out)
                    .map_err(|e| malformed(format!("zstd: {e}")))?;
                out
            }
            _ => c.take(entry.size as usize)?.to_vec(),
        };
        if plain.len() as u64 != entry.size {
            return Err(malformed(format!(
                "entry decompressed to {} bytes, expected {}",
                plain.len(),
                entry.size
            )));
        }

        if predict_flag == 0 {
            if predict_type == 1 {
                undo_delta_u8(&mut plain);
            }
            if predict_type == 2 {
                undo_delta_u16(&mut plain);
            }
        } else if predict_type == 2 {
            undo_delta_u8(&mut plain);
            if plain.len() == 0x10000 {
                undo_deinterleave_16(&mut plain);
            }
        }

        let crc = crc32(&plain);
        if crc != entry.crc32 {
            return Err(malformed(format!(
                "CRC mismatch: computed {crc:#010x}, stored {:#010x}",
                entry.crc32
            )));
        }
        Ok(plain)
    }
}

/// Bytes are stored as successive differences; integrate to recover them.
fn undo_delta_u8(data: &mut [u8]) {
    let mut acc = 0u8;
    for b in data {
        acc = acc.wrapping_add(*b);
        *b = acc;
    }
}

/// Same, over little-endian u16s.
fn undo_delta_u16(data: &mut [u8]) {
    let mut acc = 0u16;
    for pair in data.as_chunks_mut::<2>().0.iter_mut() {
        acc = acc.wrapping_add(u16::from_le_bytes([pair[0], pair[1]]));
        pair.copy_from_slice(&acc.to_le_bytes());
    }
}

/// 64 KiB 16-bit tiles are additionally split into low/high byte planes;
/// re-interleave them. (Plane pairs: dst[4k+1]=src[2k], dst[4k+3]=src[2k+1],
/// dst[4k]=src[0x8000+2k], dst[4k+2]=src[0x8000+2k+1].)
fn undo_deinterleave_16(data: &mut [u8]) {
    debug_assert_eq!(data.len(), 0x10000);
    let mut out = vec![0u8; 0x10000];
    for k in 0..0x4000 {
        out[4 * k + 1] = data[2 * k];
        out[4 * k + 3] = data[2 * k + 1];
        out[4 * k] = data[0x8000 + 2 * k];
        out[4 * k + 2] = data[0x8000 + 2 * k + 1];
    }
    data.copy_from_slice(&out);
}

/// Standard IEEE CRC-32 (the zlib polynomial). Every tile of every
/// raster layer is checksummed, so this sits on the import hot path.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        // Known vector: CRC-32 of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn delta_decode_round_trips() {
        let mut data = vec![1u8, 1, 1, 255, 2];
        undo_delta_u8(&mut data);
        assert_eq!(data, vec![1, 2, 3, 2, 4]);
    }
}
