//! The index snapshot: one file with everything indexing learned, so a
//! relaunch (or a headless server) reads it in one go instead of
//! probing thousands of per-photo caches.

use crate::paths::index_snapshot_path;
use crate::people::{decode_faces_at, encode_faces_body, DetectedFace};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One photo's index entry as the snapshot file stores it. The outer
/// Option per field means "was this ever computed" — `gps: Some(None)`
/// is a probed photo with no position, worth remembering so it is not
/// probed again.
#[derive(Clone, Debug)]
pub struct IndexRow {
    pub path: PathBuf,
    pub mtime: u64,
    pub embed: Option<Arc<Vec<f32>>>,
    pub gps: Option<Option<(f64, f64)>>,
    pub taken: Option<String>,
    pub place: Option<Option<String>>,
    pub flagged: Option<bool>,
    /// The faces the detector found — `Some(empty)` for a photo it
    /// looked at and found none in — each with the recogniser's
    /// vector when that model was installed at the time.
    pub faces: Option<Vec<DetectedFace>>,
}

impl IndexRow {
    /// Whether nothing was ever learned about the photo.
    pub fn is_empty(&self) -> bool {
        self.embed.is_none()
            && self.gps.is_none()
            && self.taken.is_none()
            && self.place.is_none()
            && self.flagged.is_none()
            && self.faces.is_none()
    }
}

/// The first format: one presence-flags byte per row.
pub const INDEX_MAGIC_V1: &[u8; 8] = b"SCHIDX1\n";
/// The second: a further flags byte per row, for the faces. Written
/// always; both are read.
pub const INDEX_MAGIC: &[u8; 8] = b"SCHIDX2\n";

/// Serialize the index rows: magic, a count, then per row the path,
/// mtime, two presence-flags bytes and the present fields, all
/// little-endian. Hand-rolled because 10 MB of f32s deserves neither
/// JSON nor a new dependency. Non-UTF-8 paths are skipped — they
/// cannot round-trip through this file, and re-indexing them is only
/// what happens today.
pub fn write_index_snapshot(rows: &[IndexRow]) -> anyhow::Result<()> {
    let Some(path) = index_snapshot_path() else {
        return Ok(());
    };
    write_index_snapshot_to(&path, rows)
}

pub fn write_index_snapshot_to(path: &Path, rows: &[IndexRow]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut out: Vec<u8> = Vec::with_capacity(rows.len() * 2200 + 16);
    out.extend_from_slice(INDEX_MAGIC);
    let counted: Vec<&IndexRow> = rows.iter().filter(|r| r.path.to_str().is_some()).collect();
    out.extend_from_slice(&(counted.len() as u32).to_le_bytes());
    let put_str = |out: &mut Vec<u8>, s: &str| {
        out.extend_from_slice(&(s.len() as u16).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    };
    for row in counted {
        put_str(&mut out, row.path.to_str().expect("filtered above"));
        out.extend_from_slice(&row.mtime.to_le_bytes());
        let mut flags = 0u8;
        if row.embed.is_some() {
            flags |= 1;
        }
        if let Some(gps) = row.gps {
            flags |= 2;
            if gps.is_some() {
                flags |= 4;
            }
        }
        if row.taken.is_some() {
            flags |= 8;
        }
        if let Some(place) = &row.place {
            flags |= 16;
            if place.is_some() {
                flags |= 32;
            }
        }
        if let Some(flagged) = row.flagged {
            flags |= 64;
            if flagged {
                flags |= 128;
            }
        }
        out.push(flags);
        let mut flags2 = 0u8;
        if row.faces.is_some() {
            flags2 |= 1;
        }
        out.push(flags2);
        if let Some(embed) = &row.embed {
            out.extend_from_slice(&(embed.len() as u16).to_le_bytes());
            for v in embed.iter() {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        if let Some(Some((lat, lon))) = row.gps {
            out.extend_from_slice(&lat.to_le_bytes());
            out.extend_from_slice(&lon.to_le_bytes());
        }
        if let Some(taken) = &row.taken {
            put_str(&mut out, taken);
        }
        if let Some(Some(place)) = &row.place {
            put_str(&mut out, place);
        }
        if let Some(faces) = &row.faces {
            out.extend_from_slice(&encode_faces_body(faces));
        }
    }
    // Atomically: a crash mid-write must not leave a torn file.
    let tmp = path.with_extension("v1.tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the snapshot back; `None` for a missing, foreign or torn file
/// — every failure just means indexing from the per-photo caches.
pub fn read_index_snapshot() -> Option<Vec<IndexRow>> {
    let bytes = std::fs::read(index_snapshot_path()?).ok()?;
    parse_index_snapshot(&bytes)
}

pub fn parse_index_snapshot(bytes: &[u8]) -> Option<Vec<IndexRow>> {
    let mut at = 0usize;
    let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
        let slice = bytes.get(*at..*at + n)?;
        *at += n;
        Some(slice)
    };
    let version = match take(&mut at, 8)? {
        m if m == INDEX_MAGIC_V1 => 1,
        m if m == INDEX_MAGIC => 2,
        _ => return None,
    };
    let count = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
    let get_str = |at: &mut usize| -> Option<String> {
        let len = u16::from_le_bytes(take(at, 2)?.try_into().ok()?) as usize;
        String::from_utf8(take(at, len)?.to_vec()).ok()
    };
    let get_f64 =
        |at: &mut usize| -> Option<f64> { Some(f64::from_le_bytes(take(at, 8)?.try_into().ok()?)) };
    let mut rows = Vec::with_capacity(count.min(65536));
    for _ in 0..count {
        let path = PathBuf::from(get_str(&mut at)?);
        let mtime = u64::from_le_bytes(take(&mut at, 8)?.try_into().ok()?);
        let flags = take(&mut at, 1)?[0];
        let flags2 = if version >= 2 {
            take(&mut at, 1)?[0]
        } else {
            0
        };
        let embed = if flags & 1 != 0 {
            let dim = u16::from_le_bytes(take(&mut at, 2)?.try_into().ok()?) as usize;
            let raw = take(&mut at, dim * 4)?;
            Some(Arc::new(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c))
                    .collect::<Vec<f32>>(),
            ))
        } else {
            None
        };
        let gps = if flags & 2 != 0 {
            Some(if flags & 4 != 0 {
                let (lat, lon) = (get_f64(&mut at)?, get_f64(&mut at)?);
                crate::meta::valid_gps_position(lat, lon).then_some((lat, lon))
            } else {
                None
            })
        } else {
            None
        };
        let taken = if flags & 8 != 0 {
            Some(get_str(&mut at)?)
        } else {
            None
        };
        let place = if flags & 16 != 0 {
            Some(if flags & 32 != 0 {
                Some(get_str(&mut at)?)
            } else {
                None
            })
        } else {
            None
        };
        let flagged = (flags & 64 != 0).then_some(flags & 128 != 0);
        let faces = if flags2 & 1 != 0 {
            let mut faces = Vec::new();
            decode_faces_at(bytes, &mut at, &mut faces, false)?;
            Some(faces)
        } else {
            None
        };
        rows.push(IndexRow {
            path,
            mtime,
            embed,
            gps,
            taken,
            place,
            flagged,
            faces,
        });
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_snapshot_round_trips_every_field_shape() {
        let rows = vec![
            IndexRow {
                path: PathBuf::from("/p/full.jpg"),
                mtime: 7,
                embed: Some(Arc::new(vec![0.25f32, -1.0, 3.5])),
                gps: Some(Some((40.7, -74.0))),
                taken: Some("2026-09-01 12:00:00".into()),
                place: Some(Some("New York City".into())),
                flagged: Some(true),
                faces: Some(vec![
                    DetectedFace {
                        rect: crate::people::FaceRect {
                            x: 0.1,
                            y: 0.2,
                            w: 0.3,
                            h: 0.4,
                        },
                        embed: Some(vec![0.5, -0.5]),
                    },
                    DetectedFace {
                        rect: crate::people::FaceRect {
                            x: 0.6,
                            y: 0.2,
                            w: 0.1,
                            h: 0.1,
                        },
                        embed: None,
                    },
                ]),
            },
            IndexRow {
                path: PathBuf::from("/p/bare.jpg"),
                mtime: 9,
                embed: None,
                gps: Some(None),
                taken: None,
                place: Some(None),
                flagged: Some(false),
                faces: Some(Vec::new()),
            },
            IndexRow {
                path: PathBuf::from("/p/unlooked.jpg"),
                mtime: 11,
                embed: None,
                gps: Some(None),
                taken: None,
                place: None,
                flagged: None,
                faces: None,
            },
            IndexRow {
                path: PathBuf::from("/p/zero.jpg"),
                mtime: 10,
                embed: None,
                gps: Some(Some((0.0, -0.0))),
                taken: Some("2026-09-02 12:00:00".into()),
                place: Some(None),
                flagged: Some(false),
                faces: None,
            },
        ];
        let dir = std::env::temp_dir().join(format!("schist-idx-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("index.v1");
        write_index_snapshot_to(&file, &rows).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let back = parse_index_snapshot(&bytes).expect("parses");
        assert_eq!(back.len(), 4);
        assert_eq!(back[0].path, rows[0].path);
        assert_eq!(back[0].mtime, 7);
        assert_eq!(back[0].embed.as_deref(), Some(&vec![0.25f32, -1.0, 3.5]));
        assert_eq!(back[0].gps, Some(Some((40.7, -74.0))));
        assert_eq!(back[0].taken.as_deref(), Some("2026-09-01 12:00:00"));
        assert_eq!(back[0].place, Some(Some("New York City".into())));
        assert_eq!(back[0].flagged, Some(true));
        assert_eq!(back[1].gps, Some(None));
        assert_eq!(back[1].place, Some(None));
        assert_eq!(back[1].flagged, Some(false));
        assert_eq!(back[1].embed, None);
        // Still probed, so the loader will not keep re-indexing this photo.
        assert_eq!(back[3].gps, Some(None));
        assert_eq!(back[3].taken.as_deref(), Some("2026-09-02 12:00:00"));
        assert_eq!(back[0].faces, rows[0].faces);
        assert_eq!(back[1].faces, Some(Vec::new()));
        assert_eq!(back[2].faces, None);
        assert_eq!(back[3].faces, None);
        assert!(parse_index_snapshot(&bytes[..bytes.len() - 3]).is_none());
        assert!(parse_index_snapshot(b"not an index").is_none());
    }

    #[test]
    fn a_first_format_snapshot_still_reads() {
        // Hand-assembled v1 bytes: one row, position only.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(INDEX_MAGIC_V1);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let path = "/p/old.jpg";
        bytes.extend_from_slice(&(path.len() as u16).to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.extend_from_slice(&5u64.to_le_bytes());
        bytes.push(2 | 4); // gps probed and present
        bytes.extend_from_slice(&51.5f64.to_le_bytes());
        bytes.extend_from_slice(&(-0.1f64).to_le_bytes());
        let rows = parse_index_snapshot(&bytes).expect("v1 parses");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].gps, Some(Some((51.5, -0.1))));
        assert_eq!(rows[0].faces, None);
        assert!(!rows[0].is_empty());
    }
}
