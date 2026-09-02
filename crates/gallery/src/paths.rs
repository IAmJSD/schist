//! Where the gallery keeps things.

use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};

/// Longest edge of a rendered thumbnail. Cells scale the image down from
/// here, so one render serves every position of the size slider — and
/// it is part of the disk-cache key, so a change re-renders the lot.
pub const THUMB_EDGE: u32 = 256;

/// The per-user state directory (`~/.local/state` on Unix, LOCALAPPDATA
/// on Windows): caches, the recovery files, the update stamp.
pub fn state_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .map(PathBuf::from)
    } else {
        std::env::var("XDG_STATE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local/state"))
            })
    }
}

/// `library.json`: the watched folders, buckets and recents.
pub fn library_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("schist/library.json"))
}

/// Where the index snapshot lives between runs.
pub fn index_snapshot_path() -> Option<PathBuf> {
    Some(state_dir()?.join("schist/index.v1"))
}

/// The PSD sidecar an edit of `original` saves into.
pub fn backing_psd(original: &Path) -> Option<PathBuf> {
    let dir = original.parent()?;
    let name = original.file_name()?.to_string_lossy();
    Some(dir.join(".schist").join(format!("{name}.psd")))
}

/// What a thumbnail renders from: the sidecar once one exists, so the
/// gallery shows the edit, as Picasa does.
pub fn thumb_source(original: &Path, edited: bool) -> PathBuf {
    if edited {
        if let Some(psd) = backing_psd(original) {
            return psd;
        }
    }
    original.to_path_buf()
}

/// Modification time in seconds since the epoch; zero when unreadable.
pub fn mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where rendered thumbnails are cached between runs, keyed by source
/// path, mtime and render size — a re-edited photo gets a fresh entry
/// and the stale one ages out with the directory. The score, embedding
/// and metadata caches sit beside it under the same stem.
pub fn thumb_cache_path(source: &Path, mtime: u64) -> Option<PathBuf> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    mtime.hash(&mut hasher);
    THUMB_EDGE.hash(&mut hasher);
    let dir = state_dir()?.join("schist/thumbs");
    Some(dir.join(format!("{:016x}.png", hasher.finish())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidecar_lives_in_a_hidden_directory_beside_the_photo() {
        // The sidecar carries the extension of the original in its name,
        // so `a.jpg` and `a.png` in one folder never share an edit.
        assert_eq!(
            backing_psd(Path::new("/photos/trip/a.jpg")),
            Some(PathBuf::from("/photos/trip/.schist/a.jpg.psd"))
        );
        assert_eq!(
            backing_psd(Path::new("/photos/trip/a.png")),
            Some(PathBuf::from("/photos/trip/.schist/a.png.psd"))
        );
    }

    #[test]
    fn thumb_cache_keys_change_with_the_file() {
        let a = thumb_cache_path(Path::new("/p/a.jpg"), 1);
        let b = thumb_cache_path(Path::new("/p/a.jpg"), 2);
        let c = thumb_cache_path(Path::new("/p/b.jpg"), 1);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, thumb_cache_path(Path::new("/p/a.jpg"), 1));
    }
}
