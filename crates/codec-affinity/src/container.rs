//! The Affinity container, write direction: the inverse of
//! [`crate::archive`].
//!
//! Writes a version-12 (unified ".af") container the way Affinity 3.x
//! lays one out, byte conventions transcribed from real files:
//!
//! - header (`00 FF 4B 41` · v12 · flags 0 · "Prsn" reversed), `#Inf`
//!   block, `Prot` + protocol 12;
//! - the first entry's `#Fil` block directly after the header, every
//!   later block preceded by an `FF FF FF FF` sentinel;
//! - one `#FT4` FAT savepoint listing every entry (flag 0, named);
//! - a `Thmb` block holding one PNG, referenced from `#Inf`.
//!
//! Entries are zstd-compressed (what every version-12 writer uses),
//! falling back to stored when compression does not help. The `#Inf`
//! "length" field is the sum of compressed sizes, `num` the next free
//! entry id — both as observed across the fixture corpus.

use crate::archive::crc32;

/// One named payload to be written.
pub struct EntryData {
    pub name: String,
    pub plain: Vec<u8>,
}

/// Serialize a complete container.
///
/// `entries` keep their order in the FAT (the first becomes the block
/// directly after the header — put "doc.dat" first for likeness to real
/// files). `thumbnail_png` is the app-rendered preview; `creation_date`
/// is seconds since the Unix epoch.
pub fn write_container(
    entries: &[EntryData],
    thumbnail_png: Option<&[u8]>,
    creation_date: u64,
) -> Vec<u8> {
    struct Packed {
        offset: u64,
        compressed: Vec<u8>,
        crc: u32,
        compression: u8,
    }

    // Compress up front (in parallel — tile entries dominate and are
    // independent) so offsets can be laid out in one pass.
    use rayon::prelude::*;
    let mut packed: Vec<Packed> = entries
        .par_iter()
        .map(|e| {
            let z = ruzstd::encoding::compress_to_vec(
                e.plain.as_slice(),
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            let (compressed, compression) = if z.len() < e.plain.len() {
                (z, 0x02) // zstd, no prediction
            } else {
                (e.plain.clone(), 0x00) // stored
            };
            Packed {
                offset: 0,
                crc: crc32(&e.plain),
                compressed,
                compression,
            }
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&crate::archive::MAGIC);
    out.extend_from_slice(&12u16.to_le_bytes()); // container version
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&crate::graph::tag(b"Prsn").to_le_bytes()); // "nsrP" on disk
    out.extend_from_slice(b"#Inf");
    let inf_at = out.len();
    // Placeholders: fat_offset, thumb_offset, length, unknown,
    // creation date, revision, num.
    out.extend_from_slice(&[0u8; 8 * 5]);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32 + 1).to_le_bytes());
    out.extend_from_slice(b"Prot");
    out.extend_from_slice(&12u32.to_le_bytes()); // protocol revision

    // Data blocks. The first sits directly after the header; every
    // later block gets the FF FF FF FF sentinel prefix.
    for (i, p) in packed.iter_mut().enumerate() {
        if i > 0 {
            out.extend_from_slice(&[0xFF; 4]);
        }
        p.offset = out.len() as u64;
        out.extend_from_slice(b"#Fil");
        out.extend_from_slice(&p.compressed);
    }

    // The FAT: one #FT4 savepoint.
    out.extend_from_slice(&[0xFF; 4]);
    let fat_offset = out.len() as u64;
    out.extend_from_slice(b"#FT4");
    let total_compressed: u64 = packed.iter().map(|p| p.compressed.len() as u64).sum();
    let fat_head_at = out.len();
    out.extend_from_slice(&0u64.to_le_bytes()); // next FAT offset
    out.extend_from_slice(&creation_date.to_le_bytes()); // savepoint
    out.extend_from_slice(&[0u8; 8]); // thumb offset (patched below)
    out.extend_from_slice(&total_compressed.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // table size (patched below)
    let table_size_at = out.len() - 4;

    // Directories: every "x/" prefix in use, with its file count.
    let mut dirs: Vec<(String, u64)> = Vec::new();
    for e in entries {
        if let Some(slash) = e.name.find('/') {
            let prefix = &e.name[..slash + 1];
            match dirs.iter_mut().find(|(d, _)| d == prefix) {
                Some((_, n)) => *n += 1,
                None => dirs.push((prefix.to_string(), 1)),
            }
        }
    }
    out.extend_from_slice(&(dirs.len() as u16).to_le_bytes());
    out.push(0);

    let table_start = out.len();
    for (i, (e, p)) in entries.iter().zip(&packed).enumerate() {
        out.extend_from_slice(&(i as u32 + 1).to_le_bytes()); // id
        out.push(0); // flag 0: named
        out.extend_from_slice(&p.offset.to_le_bytes());
        out.extend_from_slice(&(e.plain.len() as u64).to_le_bytes());
        out.extend_from_slice(&(p.compressed.len() as u64).to_le_bytes());
        out.extend_from_slice(&p.crc.to_le_bytes());
        out.push(p.compression);
        out.extend_from_slice(&32u32.to_le_bytes()); // #FT2+ extra: constant 32
                                                     // The #FT4 extra is a CRC-32 of the *compressed* payload —
                                                     // Affinity verifies it and reports the file corrupted when it
                                                     // is wrong (decoded by matching real files' values; for stored
                                                     // entries it coincides with the plain CRC).
        out.extend_from_slice(&crc32(&p.compressed).to_le_bytes());
        out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        out.extend_from_slice(e.name.as_bytes());
    }
    for (name, files) in &dirs {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&files.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    let table_size = (out.len() - table_start) as u32;
    out[table_size_at..table_size_at + 4].copy_from_slice(&table_size.to_le_bytes());

    // The thumbnail block. Its offset points at the FF FF FF FF
    // sentinel, which here doubles as the block's own lead-in.
    let mut thumb_offset = 0u64;
    if let Some(png) = thumbnail_png {
        thumb_offset = out.len() as u64;
        out.extend_from_slice(&[0xFF; 4]);
        out.extend_from_slice(b"Thmb");
        out.extend_from_slice(&1u32.to_le_bytes()); // count
        out.extend_from_slice(&(13 + png.len() as u32).to_le_bytes()); // block size
        out.extend_from_slice(&29u32.to_le_bytes()); // header constant, as observed
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.push(1);
        out.extend_from_slice(png);
    }

    // Patch #Inf and the FAT's mirror of (thumb offset, length).
    out[inf_at..inf_at + 8].copy_from_slice(&fat_offset.to_le_bytes());
    out[inf_at + 8..inf_at + 16].copy_from_slice(&thumb_offset.to_le_bytes());
    out[inf_at + 16..inf_at + 24].copy_from_slice(&total_compressed.to_le_bytes());
    out[inf_at + 32..inf_at + 40].copy_from_slice(&creation_date.to_le_bytes());
    out[fat_head_at + 16..fat_head_at + 24].copy_from_slice(&thumb_offset.to_le_bytes());

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    #[test]
    fn container_round_trips_through_the_reader() {
        let entries = vec![
            EntryData {
                name: "doc.dat".into(),
                plain: b"not really a graph, but bytes all the same".to_vec(),
            },
            EntryData {
                name: "d/1".into(),
                plain: vec![7u8; 0x10000],
            },
            EntryData {
                name: "c/1".into(),
                plain: (0..=255u8).collect(),
            },
        ];
        let png = b"\x89PNG\r\n\x1a\nfake";
        let bytes = write_container(&entries, Some(png), 1_787_500_000);

        let archive = Archive::parse(&bytes).expect("written container parses");
        assert_eq!(archive.version, 12);
        assert_eq!(
            archive.names().collect::<Vec<_>>(),
            vec!["doc.dat", "d/1", "c/1"]
        );
        for e in &entries {
            let head = archive.head(&e.name).expect("entry has a head");
            assert_eq!(archive.extract(head).unwrap(), e.plain, "{}", e.name);
        }
        assert_eq!(archive.thumbnail(), Some(&png[..]));
    }
}
