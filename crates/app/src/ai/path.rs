//! The PATH the agent CLIs are looked up on.
//!
//! A GUI launch inherits launchd's (or the desktop session's) minimal
//! PATH, never the user's shell configuration, so a `claude` or `codex`
//! living in ~/.local/bin, /opt/homebrew/bin or an npm prefix looks
//! missing while working fine from a terminal. The fix is to ask the
//! user's login shell what its PATH is — but that is a whole shell
//! startup, rc files and all, and on a real dotfile setup it costs the
//! better part of a second. Nothing about opening a window should wait
//! for it, so the shell is asked on a thread and the answer collected
//! whenever it turns up: by the picker when it repaints ([`current`]),
//! and by a worker thread that is actually about to spawn a CLI
//! ([`resolved`], the only caller that waits).
//!
//! The answer is deliberately *not* published with `set_var`. By the time
//! it lands the app is many threads deep in GPUI, and editing the process
//! environment underneath them is unsound; it is handed to each spawn
//! explicitly instead — an absolute path for the CLI itself, a `PATH`
//! override for the children it goes on to spawn.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};

/// How long the shell gets to answer before the app gives up on it.
const SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The longest [`resolved`] will wait — the shell's own budget plus a
/// margin, so a probe that was never started (or a thread that died
/// between spawning and publishing) cannot wedge a worker forever.
const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

#[derive(Default)]
struct Probe {
    /// `None` while the shell is still being asked; `Some(None)` once it
    /// has been asked and had nothing to add.
    answer: Mutex<Option<Option<OsString>>>,
    answered: Condvar,
}

fn probe() -> &'static Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    PROBE.get_or_init(Probe::default)
}

fn publish(path: Option<OsString>) {
    let probe = probe();
    if let Ok(mut slot) = probe.answer.lock() {
        *slot = Some(path);
    }
    probe.answered.notify_all();
}

/// Whatever PATH this process was launched with.
fn inherited() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

/// Start asking the login shell for its PATH. Returns immediately; call
/// it once, early in `main`.
pub fn start() {
    #[cfg(unix)]
    {
        // A terminal launch already carries the user's PATH; don't spend
        // a shell startup on it.
        if std::env::var_os("TERM").is_none() {
            let spawned = std::thread::Builder::new()
                .name("shell-path".into())
                .spawn(|| publish(ask_login_shell()));
            if spawned.is_ok() {
                return;
            }
        }
    }
    publish(None);
}

/// Ask the user's login shell what its PATH is. `None` when it could not
/// be asked, took too long, or said nothing useful.
#[cfg(unix)]
fn ask_login_shell() -> Option<OsString> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    // Interactive + login so both the profile and the rc file
    // contribute — installers append PATH edits to either. Those files
    // may print banners, so the value is fenced with markers; and the
    // inner printf runs under `sh` because fish would expand its own
    // "$PATH" as a space-joined list, while the exported environment is
    // colon-joined for every shell.
    const MARK: &str = "__SCHIST_PATH__";
    const PRINTER: &str = "sh -c 'printf \"__SCHIST_PATH__%s__SCHIST_PATH__\" \"$PATH\"'";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("shell-path-run".into())
        .spawn({
            let shell = shell.clone();
            move || {
                let out = std::process::Command::new(shell)
                    .args(["-l", "-i", "-c", PRINTER])
                    .stdin(std::process::Stdio::null())
                    .output();
                let _ = tx.send(out);
            }
        })
        .ok()?;
    // A shell rc that hangs must not hold an agent up for ever: give it a
    // few seconds and settle for the plain PATH otherwise. (On timeout
    // the probe thread is abandoned; it exits whenever the shell does.)
    let out = match rx.recv_timeout(SHELL_TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            log::warn!("asking {shell} for PATH failed: {e}");
            return None;
        }
        Err(_) => {
            log::warn!("asking {shell} for PATH timed out; installed CLIs may look missing");
            return None;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.split(MARK).nth(1) {
        Some(path) if !path.is_empty() => Some(OsString::from(path)),
        _ => {
            log::warn!("{shell} printed no PATH; installed CLIs may look missing");
            None
        }
    }
}

/// Whether the probe has answered. False means [`current`] is still the
/// launch PATH and may yet improve.
pub fn ready() -> bool {
    probe().answer.lock().map(|s| s.is_some()).unwrap_or(true)
}

/// The PATH to look agent CLIs up on, as far as it is known *now*: the
/// login shell's once the probe has answered, this process's own until
/// then. Never blocks — this is the one the UI thread asks.
pub fn current() -> OsString {
    match probe().answer.lock() {
        Ok(slot) => match slot.as_ref() {
            Some(Some(path)) => path.clone(),
            _ => inherited(),
        },
        Err(_) => inherited(),
    }
}

/// The same, but waits for the probe to answer (it gives up on its own,
/// so the wait is bounded). For worker threads about to spawn a CLI —
/// never for the UI thread.
pub fn resolved() -> OsString {
    let probe = probe();
    let Ok(slot) = probe.answer.lock() else {
        return inherited();
    };
    let waited = probe
        .answered
        .wait_timeout_while(slot, WAIT_TIMEOUT, |slot| slot.is_none());
    match waited {
        Ok((slot, _)) => match slot.as_ref() {
            Some(Some(path)) => path.clone(),
            _ => inherited(),
        },
        Err(_) => inherited(),
    }
}

/// The environment an agent CLI is spawned with, on top of the app's own:
/// the PATH it was found on, so the children *it* spawns see what a
/// terminal would.
pub fn child_env() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([(
        "PATH".to_string(),
        resolved().to_string_lossy().into_owned(),
    )])
}

/// Where `binary` lives on `path`, by the same lookup a shell does.
pub fn lookup(binary: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|dir| {
        let file = dir.join(binary);
        #[cfg(windows)]
        let file = file.with_extension("exe");
        file.is_file().then_some(file)
    })
}

#[cfg(test)]
mod tests {
    use super::lookup;

    #[test]
    fn a_binary_is_found_on_the_path_it_is_given() {
        let dir = std::env::temp_dir().join(format!("schist-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join(if cfg!(windows) { "agent.exe" } else { "agent" });
        std::fs::write(&bin, b"").unwrap();
        let path = std::env::join_paths([std::path::Path::new("/nowhere"), dir.as_path()]).unwrap();
        assert_eq!(lookup("agent", &path), Some(bin));
        // Not on the path at all, and a directory is not a command.
        assert_eq!(lookup("missing", &path), None);
        let only_dirs = std::env::join_paths([dir.parent().unwrap()]).unwrap();
        assert_eq!(
            lookup(dir.file_name().unwrap().to_str().unwrap(), &only_dirs),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
