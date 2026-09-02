//! The index snapshot: one file with everything indexing learned, so a
//! relaunch (or a headless server) reads it in one go instead of
//! probing thousands of per-photo caches.

use crate::paths::index_snapshot_path;
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
}

pub const INDEX_MAGIC: &[u8; 8] = b"SCHIDX1\n";

/// Serialize the index rows: magic, a count, then per row the path,
/// mtime, a presence-flags byte and the present fields, all
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
    if take(&mut at, 8)? != INDEX_MAGIC {
        return None;
    }
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
                Some((get_f64(&mut at)?, get_f64(&mut at)?))
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
        rows.push(IndexRow {
            path,
            mtime,
            embed,
            gps,
            taken,
            place,
            flagged,
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
            },
            IndexRow {
                path: PathBuf::from("/p/bare.jpg"),
                mtime: 9,
                embed: None,
                gps: Some(None),
                taken: None,
                place: Some(None),
                flagged: Some(false),
            },
        ];
        let dir = std::env::temp_dir().join(format!("schist-idx-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("index.v1");
        write_index_snapshot_to(&file, &rows).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let back = parse_index_snapshot(&bytes).expect("parses");
        assert_eq!(back.len(), 2);
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
        assert!(parse_index_snapshot(&bytes[..bytes.len() - 3]).is_none());
        assert!(parse_index_snapshot(b"not an index").is_none());
    }
}
