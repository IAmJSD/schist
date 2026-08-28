//! Plug-in helpers carried inside the executable, unpacked on first use.
//!
//! A helper is a binary for another architecture, so it cannot simply be
//! part of the same build. The alternative to carrying it is shipping it
//! loose beside the app, which works but multiplies the things an
//! installer has to place and a signature has to cover. Carrying it
//! means one file ships and the helpers appear when a plug-in is
//! actually run — which for most users is never.
//!
//! What lands here is decided at build time by `build.rs`; with nothing
//! bundled every function below reports that honestly and the host falls
//! back to looking beside the executable.

use std::io;
use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/bundled.rs"));

/// The helpers this build carries, by file name.
pub fn names() -> impl Iterator<Item = &'static str> {
    BUNDLED.iter().map(|(n, _)| *n)
}

/// Whether this build carries any helper at all.
pub fn is_empty() -> bool {
    BUNDLED.is_empty()
}

fn bytes(name: &str) -> Option<&'static [u8]> {
    BUNDLED.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

/// Per-user cache directory, following the same rules as the app's own
/// state directory: XDG on Unix, local app data on Windows.
fn cache_root() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .map(PathBuf::from)
    } else {
        std::env::var("XDG_CACHE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".cache"))
            })
    }
}

/// Where this build's helpers unpack to. Keyed by the bundle's hash, so
/// an upgrade never reuses the previous version's binaries.
pub fn unpack_dir() -> Option<PathBuf> {
    Some(cache_root()?.join("schist/8bf-helpers").join(BUNDLE_ID))
}

/// Unpack `name` if it is carried, and return the directory holding it.
///
/// `Ok(None)` means this build carries no such helper, which is not an
/// error: it is the ordinary state of a build that ships its helpers
/// loose instead.
pub fn extract(name: &str) -> io::Result<Option<PathBuf>> {
    let Some(bytes) = bytes(name) else {
        return Ok(None);
    };
    let Some(dir) = unpack_dir() else {
        return Ok(None);
    };
    let path = dir.join(name);

    // Already unpacked. The directory name pins the contents, so a file
    // of the right length in the right directory is the right file.
    if has_len(&path, bytes.len()) {
        return Ok(Some(dir));
    }

    std::fs::create_dir_all(&dir)?;
    write_executable(&path, bytes)?;
    Ok(Some(dir))
}

fn has_len(path: &Path, len: usize) -> bool {
    matches!(path.metadata(), Ok(m) if m.len() == len as u64)
}

/// Write `bytes` to `path` and make it runnable.
///
/// Through a temporary file and a rename, because two Schist windows may
/// unpack the same helper at the same moment: a rename is atomic, so the
/// worst case is that one of them does redundant work, rather than
/// either seeing a half-written binary.
fn write_executable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    set_executable(&tmp)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Windows refuses to replace a file that is currently
            // running, which here means another process got there first
            // and the file is already correct.
            let _ = std::fs::remove_file(&tmp);
            if has_len(path, bytes.len()) {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    // Windows runs a file by extension, not by a permission bit.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_with_no_helpers_says_so_rather_than_failing() {
        // The ordinary `cargo build` case: nothing bundled, and asking
        // for a helper is answered with "not carried", not an error.
        if is_empty() {
            assert!(names().next().is_none());
            assert_eq!(extract("schist-8bf-helper-x86_64.exe").unwrap(), None);
        }
    }

    #[test]
    fn every_carried_helper_unpacks_and_is_runnable() {
        for name in names() {
            let dir = extract(name).unwrap().expect("a carried helper unpacks");
            let path = dir.join(name);
            assert!(path.is_file(), "{} should exist", path.display());
            assert_eq!(
                path.metadata().unwrap().len() as usize,
                bytes(name).unwrap().len(),
                "unpacked {name} should be whole"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = path.metadata().unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "{name} should be executable");
            }
            // Twice is a no-op, not a rewrite: this is the path every
            // run after the first takes.
            assert_eq!(extract(name).unwrap(), Some(dir));
        }
    }

    #[test]
    fn the_bundle_id_names_the_contents() {
        // Empty or not, it has to be a usable directory component.
        assert!(!BUNDLE_ID.is_empty());
        assert!(BUNDLE_ID.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
