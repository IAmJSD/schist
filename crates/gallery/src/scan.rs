//! Walking the watched folders.

use crate::paths::{backing_psd, mtime_secs, thumb_source};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Folder scanning stops here rather than following a loop of symlinks
/// (or someone's home directory) forever.
pub const SCAN_MAX_DEPTH: usize = 6;
pub const SCAN_MAX_FILES: usize = 5000;

/// One scanned directory and the images in it, a section of the grid.
#[derive(Clone, Debug)]
pub struct Section {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
}

/// One image in the gallery.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    /// Modification seconds of whichever file the thumbnail renders from,
    /// part of the disk-cache key.
    pub mtime: u64,
    /// A PSD sidecar exists: the thumbnail shows the edit, and the cell
    /// wears a badge.
    pub edited: bool,
}

/// Walk the watched folders and group every decodable image by
/// directory. `exts` are lowercase extensions without the dot.
/// Blocking, so callers run it on a background thread.
pub fn scan_folders(roots: &[PathBuf], exts: &[String]) -> Vec<Section> {
    let mut by_dir: BTreeMap<PathBuf, Vec<Entry>> = BTreeMap::new();
    let mut budget = SCAN_MAX_FILES;
    for root in roots {
        walk(root, 0, exts, &mut by_dir, &mut budget);
    }
    by_dir
        .into_iter()
        .map(|(dir, mut entries)| {
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            Section { dir, entries }
        })
        .collect()
}

fn walk(
    dir: &Path,
    depth: usize,
    exts: &[String],
    out: &mut BTreeMap<PathBuf, Vec<Entry>>,
    budget: &mut usize,
) {
    if depth > SCAN_MAX_DEPTH || *budget == 0 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        if *budget == 0 {
            return;
        }
        let path = item.path();
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'));
        // Dot-directories include the `.schist` sidecars, which must not
        // list as photos of their own.
        if hidden {
            continue;
        }
        if path.is_dir() {
            walk(&path, depth + 1, exts, out, budget);
            continue;
        }
        let known = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| exts.iter().any(|x| x == &e));
        if !known {
            continue;
        }
        let edited = backing_psd(&path).is_some_and(|p| p.exists());
        let mtime = mtime_secs(&thumb_source(&path, edited));
        *budget -= 1;
        out.entry(dir.to_path_buf()).or_default().push(Entry {
            path,
            mtime,
            edited,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanning_skips_hidden_directories_and_unknown_files() {
        let root = std::env::temp_dir().join(format!("schist-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("album/.schist")).unwrap();
        std::fs::write(root.join("album/one.png"), b"x").unwrap();
        std::fs::write(root.join("album/two.txt"), b"x").unwrap();
        // A sidecar PSD must not list as a photo of its own.
        std::fs::write(root.join("album/.schist/one.png.psd"), b"x").unwrap();
        let sections = scan_folders(
            std::slice::from_ref(&root),
            &["png".to_string(), "psd".to_string()],
        );
        let all: Vec<_> = sections
            .iter()
            .flat_map(|s| s.entries.iter().map(|e| e.path.clone()))
            .collect();
        assert_eq!(all, vec![root.join("album/one.png")]);
        // And the photo with a sidecar knows it has been edited.
        assert!(sections[0].entries[0].edited);
        let _ = std::fs::remove_dir_all(&root);
    }
}
