//! Opt-in crash reporting and update checks.
//!
//! Both are off until the user turns them on, and neither sends anything
//! anywhere by default: a crash writes a local report file, and the update
//! check is an explicit HTTPS request the user asks for. That keeps the
//! privacy story simple — an image editor has no business phoning home
//! unasked.

use std::path::PathBuf;

/// Where crash reports are written.
pub fn crash_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("photoslop/crashes"))
}

/// Install a panic hook that records a report next to the recovery
/// snapshots, so a crash leaves both the work and the diagnosis behind.
///
/// `enabled` comes from preferences; when false the default hook runs and
/// nothing is written.
pub fn install_handler(enabled: bool) {
    if !enabled {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_report(info);
        previous(info);
    }));
}

fn write_report(info: &std::panic::PanicHookInfo<'_>) {
    let Some(dir) = crash_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into());
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown location".into());
    let report = format!(
        "photoslop {}\n\
         platform: {} {}\n\
         location: {}\n\
         message: {}\n\
         \n\
         backtrace:\n{}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        location,
        message,
        std::backtrace::Backtrace::force_capture()
    );
    // The file name is the process id: a session can only crash once, and
    // it lines up with the recovery snapshot from the same run.
    let path = dir.join(format!("crash-{}.txt", std::process::id()));
    let _ = std::fs::write(&path, report);
    eprintln!("photoslop: crash report written to {}", path.display());
}

/// A release published upstream.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub html_url: String,
}

/// Compare two `x.y.z` version strings. Returns true when `candidate` is
/// newer than `current`; anything unparseable compares as "not newer" so a
/// malformed tag never nags the user.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    fn parts(v: &str) -> Option<(u32, u32, u32)> {
        let v = v.trim().trim_start_matches('v');
        let mut it = v.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().ok()?;
        // Trailing pre-release suffixes ("1.2.3-beta") are ignored.
        let patch = it
            .next()
            .unwrap_or("0")
            .split(['-', '+'])
            .next()?
            .parse()
            .ok()?;
        Some((major, minor, patch))
    }
    match (parts(current), parts(candidate)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

/// This build's version.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Where releases are published.
const RELEASES_API: &str = "https://api.github.com/repos/IAmJSD/photoslop/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/IAmJSD/photoslop/releases";

/// The outcome of an update check.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    UpToDate,
    Available { version: String, url: String },
    Failed(String),
}

/// Ask GitHub for the latest release. Blocking — call it off the UI thread.
///
/// This is the only network request Photoslop ever makes, and only when the
/// user explicitly asks for it.
pub fn check_for_update() -> UpdateStatus {
    let response = ureq::get(RELEASES_API)
        .header("User-Agent", "photoslop-update-check")
        .header("Accept", "application/vnd.github+json")
        .call();
    let release: Release = match response {
        Ok(mut r) => match r.body_mut().read_json() {
            Ok(v) => v,
            Err(err) => return UpdateStatus::Failed(format!("unreadable response: {err}")),
        },
        Err(err) => return UpdateStatus::Failed(format!("{err}")),
    };
    if is_newer(current_version(), &release.tag_name) {
        UpdateStatus::Available {
            version: release.tag_name.trim_start_matches('v').to_string(),
            url: if release.html_url.is_empty() {
                RELEASES_PAGE.to_string()
            } else {
                release.html_url
            },
        }
    } else {
        UpdateStatus::UpToDate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("1.9.0", "1.10.0"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        // Pre-release suffixes compare on the numeric part.
        assert!(is_newer("0.1.0", "0.1.1-beta"));
    }

    #[test]
    fn malformed_versions_never_claim_an_update() {
        assert!(!is_newer("0.1.0", "not-a-version"));
        assert!(!is_newer("", "1.0.0"));
        assert!(!is_newer("0.1.0", ""));
    }

    #[test]
    fn release_json_parses() {
        let json = r#"{"tag_name":"v0.3.0","html_url":"https://example.com/r"}"#;
        let release: Release = serde_json::from_str(json).unwrap();
        assert_eq!(release.tag_name, "v0.3.0");
        assert!(is_newer("0.2.0", &release.tag_name));
    }

    #[test]
    fn crash_dir_is_under_the_state_directory() {
        let dir = crash_dir().expect("HOME is set in tests");
        assert!(dir.ends_with("photoslop/crashes"), "{dir:?}");
    }
}
