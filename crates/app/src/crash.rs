//! Opt-in crash reporting.
//!
//! Off until the user turns it on: a crash writes a local report file next
//! to the recovery snapshots, and uploading that report to Sentry is a
//! second, separate opt-in — writing a file to this machine and sending one
//! to us are not the same decision. Update checks, the other thing here that
//! speaks to the network, live in [`crate::update`].
//!
//! The upload has a second lock on it. It needs a DSN, and a DSN is baked
//! in at build time from `SCHIST_SENTRY_DSN`, which only the release
//! workflow sets. A build from source has none, so the preference does not
//! even appear, and `sentry::init` is never reached.

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

/// This build's home for uploaded crashes, or `None` if it has not got one.
///
/// A runtime `SCHIST_SENTRY_DSN` beats the compiled-in value, which is how
/// the upload path gets exercised against a scratch project without cutting
/// a release. Cargo re-runs the compile when the build-time variable
/// changes: rustc records `option_env!` reads in its dep-info.
fn dsn() -> Option<String> {
    std::env::var("SCHIST_SENTRY_DSN")
        .ok()
        .or_else(|| option_env!("SCHIST_SENTRY_DSN").map(str::to_owned))
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
}

/// Whether this build could upload a crash at all.
///
/// The Preferences checkbox is hidden when this is false — offering to send
/// reports from a build that has nowhere to send them would be a lie.
pub fn reporting_available() -> bool {
    dsn().is_some()
}

/// A live Sentry client. Holding it is what keeps uploads working; dropping
/// it flushes whatever is still queued, so `main` keeps it until it exits.
pub struct Reporter(#[allow(dead_code)] sentry::ClientInitGuard);

/// Start uploading crashes, if the user asked for it and this build can.
///
/// Returns `None` when either is untrue, and in that case nothing is
/// initialised — no client, no transport thread, no network.
///
/// Call this *before* [`install_handler`]: both chain themselves in front of
/// whichever panic hook they find, so calling them in this order leaves the
/// local report being written first and the upload attempted second — a
/// network that is down or slow then cannot cost the user the report on
/// disk.
pub fn start_reporting(enabled: bool) -> Option<Reporter> {
    if !enabled {
        return None;
    }
    let dsn = dsn()?;
    // Checked here rather than left to `ClientOptions::dsn`, which panics on
    // anything it cannot parse. A crash reporter that takes the editor down
    // over its own configuration would be worse than no crash reporter.
    if dsn.parse::<sentry::types::Dsn>().is_err() {
        log::warn!("SCHIST_SENTRY_DSN is not a DSN; crash uploads stay off");
        return None;
    }
    // Not the version alone: Sentry matches an event to its uploaded debug
    // files by debug id, but groups and charts it by release, and
    // "schist@0.6.0" is the name the release workflow registers.
    let release = format!("schist@{}", crate::update::current_version());
    // Which build of that release. The four we publish differ in what is
    // even reachable — the GPU compositor, the .8bf helpers — so a crash is
    // much easier to place with this than without.
    let dist = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let home = home_dir().and_then(|h| h.to_str().map(str::to_owned));

    let guard = sentry::init(
        sentry::ClientOptions::new()
            .dsn(&dsn)
            .release(release)
            // `sentry-contexts` fills this with the machine's hostname when
            // it is left unset, and a hostname is very often a person's
            // name. Setting it stops that lookup happening at all.
            .server_name("redacted")
            .send_default_pii(false)
            // Schist records no breadcrumbs, and this makes sure a
            // dependency cannot start doing so on its behalf.
            .max_breadcrumbs(0)
            .before_send(move |mut event| {
                if let Some(home) = home.as_deref() {
                    redact_paths(&mut event, home);
                }
                event.dist = Some(dist.clone().into());
                Some(event)
            }),
    );
    // A DSN that parsed but was rejected leaves a disabled client behind;
    // there is nothing to hold on to in that case.
    if !guard.is_enabled() {
        return None;
    }
    // The SDK hands the event to a background thread and counts on its guard
    // being dropped to flush the queue. A crash in a GUI often never gets
    // that far — GPUI unwinds through platform callbacks, and an unwind
    // across one of those aborts the process outright — so the flush is
    // forced from inside the hook instead, where there is still certainly a
    // process to do it in. Two seconds, the same as the guard would wait.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        if let Some(client) = sentry::Hub::current().client() {
            client.flush(Some(std::time::Duration::from_secs(2)));
        }
    }));
    Some(Reporter(guard))
}

/// This user's home directory, by the same variables [`state_dir`] uses.
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// Replace the user's home directory with `~` throughout an event.
///
/// A panic message routinely quotes the path it choked on, and for an image
/// editor that path is a document someone opened — `/home/astrid/clients/…`
/// names both them and their work. The rest of the event is safe as it
/// stands: frame filenames are the *build* machine's source paths, and the
/// module list is addresses and debug ids.
fn redact_paths(event: &mut sentry::protocol::Event<'static>, home: &str) {
    // "/" and "" would rewrite every path in the event into nonsense.
    if home.len() < 2 {
        return;
    }
    if let Some(message) = event.message.as_mut() {
        *message = redact(message, home);
    }
    for exception in &mut event.exception.values {
        if let Some(value) = exception.value.as_mut() {
            *value = redact(value, home);
        }
    }
}

/// `redact_paths` for one string.
fn redact(text: &str, home: &str) -> String {
    // Backslashes as well as forward, because a Windows panic message may
    // quote either: `Path::display` gives `C:\Users\a`, but a path built
    // from a `file://` URL keeps the slashes it arrived with.
    if cfg!(windows) && home.contains('\\') {
        let slashed = home.replace('\\', "/");
        return text.replace(home, "~").replace(&slashed, "~");
    }
    text.replace(home, "~")
}

#[cfg(test)]
mod tests {
    use super::{crash_dir, redact, redact_paths, reporting_available, start_reporting};

    #[test]
    fn home_paths_are_redacted_out_of_reports() {
        assert_eq!(
            redact(
                "failed to open /home/astrid/clients/acme.psd",
                "/home/astrid"
            ),
            "failed to open ~/clients/acme.psd"
        );
        // Every occurrence, not just the first: a message often names both
        // the source and the destination of an operation.
        assert_eq!(
            redact("/home/astrid/a.psd -> /home/astrid/b.psd", "/home/astrid"),
            "~/a.psd -> ~/b.psd"
        );
        // A message that never mentions the home directory is untouched.
        assert_eq!(
            redact("index out of bounds", "/home/astrid"),
            "index out of bounds"
        );
    }

    #[test]
    fn a_degenerate_home_redacts_nothing() {
        let mut event = sentry::protocol::Event {
            message: Some("/usr/share/schist".into()),
            ..Default::default()
        };
        redact_paths(&mut event, "/");
        assert_eq!(event.message.as_deref(), Some("/usr/share/schist"));
    }

    #[test]
    fn redaction_reaches_the_exception_value() {
        let mut event = sentry::protocol::Event {
            exception: vec![sentry::protocol::Exception {
                ty: "panic".into(),
                value: Some("no such file: /home/astrid/a.psd".into()),
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        };
        redact_paths(&mut event, "/home/astrid");
        assert_eq!(
            event.exception.values[0].value.as_deref(),
            Some("no such file: ~/a.psd")
        );
    }

    #[test]
    fn a_build_without_a_dsn_never_starts_a_client() {
        // The test binary is compiled without SCHIST_SENTRY_DSN, so this
        // stands in for every build but the official releases: asking for
        // uploads has to be a no-op rather than a half-configured client.
        if option_env!("SCHIST_SENTRY_DSN").is_none() && std::env::var("SCHIST_SENTRY_DSN").is_err()
        {
            assert!(!reporting_available());
            assert!(start_reporting(true).is_none());
        }
    }

    #[test]
    fn uploads_stay_off_when_the_preference_is_off() {
        assert!(start_reporting(false).is_none());
    }

    #[test]
    fn crash_dir_is_under_the_state_directory() {
        let dir = crash_dir().expect("a state directory exists in test environments");
        assert!(dir.ends_with("schist/crashes"), "{dir:?}");
    }
}
