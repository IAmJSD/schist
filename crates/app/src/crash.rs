//! Opt-in crash reporting.
//!
//! Off until the user turns it on, and nothing is sent anywhere even
//! then: a crash writes a local report file next to the recovery
//! snapshots and that is all. Update checks, the one thing here that
//! ever spoke to the network, live in [`crate::update`].

use std::path::PathBuf;

/// Per-user state directory: XDG on Unix, local app data on Windows.
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

/// Where crash reports are written.
pub fn crash_dir() -> Option<PathBuf> {
    Some(state_dir()?.join("schist/crashes"))
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
        "schist {}\n\
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
    eprintln!("schist: crash report written to {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::crash_dir;

    #[test]
    fn crash_dir_is_under_the_state_directory() {
        let dir = crash_dir().expect("a state directory exists in test environments");
        assert!(dir.ends_with("schist/crashes"), "{dir:?}");
    }
}
