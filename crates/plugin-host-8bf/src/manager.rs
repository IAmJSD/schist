//! Photoshop plug-ins as ordinary Schist filters.
//!
//! [`discover_dir`](crate::discover_dir) finds what is installed and
//! [`remote::apply`] runs one; this module is the join between them and
//! [`PluginRegistry`], so a `.8bf` reaches the Filter menu and the MCP
//! server by the same route a first-party filter does.
//!
//! Only plug-ins that can actually run here are registered — a Windows
//! filter on a machine with no Wine is listed with the reason instead,
//! because "install Wine" is a better answer than a menu entry that
//! fails when clicked.
//!
//! Behind the `registry` feature: the helper binary is cross-compiled to
//! every architecture a plug-in might be, and it has no use for the
//! editor's plugin API.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use schist_plugin_api::{FilterPlugin, FilterValues, PluginRegistry};

use crate::host::Image;
use crate::{discover_dir, remote, Found};

/// Whether a person is present to answer a plug-in's dialog.
///
/// A `.8bf` carries its own UI and no parameter list, so this is the
/// whole of its configuration: the app asks for the dialog, and the MCP
/// server — where nobody could dismiss it — runs the plug-in with its
/// own defaults instead. Asking for a dialog with no one there would
/// hang the helper until it timed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interactive {
    Yes,
    No,
}

/// One discovered plug-in, runnable or not, for the manager UI.
#[derive(Debug, Clone)]
pub struct PluginEntry {
    /// The `.8bf` file or `.plugin` bundle, as it sits in the folder.
    pub container: PathBuf,
    pub id: String,
    /// `Category > Name`, as it would read in Photoshop's Filter menu.
    pub name: String,
    /// What the plug-in was built for, in words.
    pub architecture: String,
    pub enabled: bool,
    /// Why it is not runnable here, or `None` if it is. An entry with a
    /// reason is listed but not registered.
    pub blocker: Option<String>,
}

/// Scans the plug-in folders and owns the enable/disable state.
#[derive(Debug, Default)]
pub struct PluginManager {
    pub entries: Vec<PluginEntry>,
    /// Every folder scanned, in order, for the UI to name.
    pub dirs: Vec<PathBuf>,
    disabled: Mutex<Vec<String>>,
}

impl PluginManager {
    /// Where Schist keeps Photoshop plug-ins: XDG config on Unix, local
    /// app data on Windows, matching the rest of the app's state.
    pub fn plugin_dir() -> Option<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var("LOCALAPPDATA")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .map(PathBuf::from)
        } else {
            std::env::var("XDG_CONFIG_HOME")
                .ok()
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var("HOME")
                        .ok()
                        .map(|h| PathBuf::from(h).join(".config"))
                })
        }?;
        Some(base.join("schist/photoshop-plugins"))
    }

    /// Every folder to scan: Schist's own, then anything named in
    /// `SCHIST_8BF_PATH`.
    ///
    /// The environment variable is there because people who own these
    /// plug-ins already have them in a folder — usually one Photoshop or
    /// another host installed — and copying is not the only reasonable
    /// answer. Separated the way the platform separates `PATH`.
    pub fn search_dirs() -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Self::plugin_dir().into_iter().collect();
        if let Some(extra) = std::env::var_os("SCHIST_8BF_PATH") {
            dirs.extend(std::env::split_paths(&extra).filter(|p| !p.as_os_str().is_empty()));
        }
        dirs
    }

    /// Discover every plug-in in `dirs` and register the runnable ones.
    ///
    /// A folder that does not exist is not an error: most installs have
    /// no Photoshop plug-ins at all, and the folder is made on first
    /// install rather than at startup.
    pub fn load_dirs(
        dirs: &[PathBuf],
        registry: &mut PluginRegistry,
        interactive: Interactive,
    ) -> PluginManager {
        let mut manager = PluginManager {
            dirs: dirs.to_vec(),
            ..PluginManager::default()
        };
        let disabled = read_disabled_list(dirs.first());
        for dir in dirs {
            let found = match discover_dir(dir) {
                Ok(found) => found,
                Err(err) => {
                    log::debug!("no Photoshop plug-ins in {dir:?}: {err}");
                    continue;
                }
            };
            for plugin in found {
                manager.add(plugin, registry, interactive, &disabled);
            }
        }
        *manager.disabled.lock().unwrap() = disabled;
        manager
    }

    /// Record one discovered filter, registering it if it can run.
    fn add(
        &mut self,
        found: Found,
        registry: &mut PluginRegistry,
        interactive: Interactive,
        disabled: &[String],
    ) {
        let id = plugin_id(&found, self.entries.len());
        let name = found.menu_name();
        // `blocker` answers "is this a filter this machine can run",
        // and `readiness` adds "is the helper for it actually here" —
        // which is the difference between a plug-in that needs Wine and
        // one that needs a build carrying the right helper.
        let blocker = found
            .blocker()
            .map(|b| b.to_string())
            .or_else(|| remote::readiness(&found).err().map(|e| e.to_string()));
        let enabled = !disabled.contains(&id);
        self.entries.push(PluginEntry {
            container: found.container.clone(),
            id: id.clone(),
            name: name.clone(),
            architecture: found.architecture(),
            enabled,
            blocker: blocker.clone(),
        });
        if let Some(why) = blocker {
            log::info!("Photoshop plug-in {name:?} is listed but not offered: {why}");
            return;
        }
        if !enabled {
            log::info!("Photoshop plug-in {id} is disabled; skipping");
            return;
        }
        log::info!("loaded Photoshop plug-in {id} from {:?}", found.container);
        registry.register_filter(Box::new(EightBfFilter::new(found, id, interactive)));
    }

    /// Toggle a plug-in. Takes effect on the next launch, which the UI
    /// says — a registered filter cannot be unregistered.
    pub fn set_enabled(&mut self, id: &str, enabled: bool, dir: &Path) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.enabled = enabled;
        }
        let mut disabled = self.disabled.lock().unwrap();
        disabled.retain(|d| d != id);
        if !enabled {
            disabled.push(id.to_string());
        }
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join("disabled.txt"), disabled.join("\n"));
    }

    /// Copy a plug-in into the plug-in folder.
    ///
    /// It is inspected first, so a file that holds no filter never lands
    /// in the folder — but a plug-in that merely cannot run *here* is
    /// still installed, because the machine it cannot run on today may
    /// grow the thing it needs tomorrow.
    pub fn install(source: &Path, dir: &Path) -> std::io::Result<PathBuf> {
        let found = if source.is_dir() {
            crate::inspect_bundle(source)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
        } else {
            crate::inspect_file(source)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
        };
        if found.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "holds no filter module",
            ));
        }
        let name = source.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "source has no file name")
        })?;
        std::fs::create_dir_all(dir)?;
        let dest = dir.join(name);
        if source.is_dir() {
            copy_tree(source, &dest)?;
        } else {
            std::fs::copy(source, &dest)?;
        }
        Ok(dest)
    }
}

/// A stable identity for one filter inside one plug-in file.
///
/// The container's file name and the entry point together, because a
/// single `.8bf` routinely holds several filters and the folder may hold
/// two files of the same name from different vendors — `index` breaks
/// the remaining ties so an id is never silently shared.
fn plugin_id(found: &Found, index: usize) -> String {
    let file = found
        .container
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let entry = found
        .entry_point
        .clone()
        .unwrap_or_else(|| index.to_string());
    format!("8bf.{}.{}", slug(&file), slug(&entry))
}

/// Lowercase, with anything that is not alphanumeric folded to `-`, so
/// an id is safe to type on an MCP command line and in a menu action.
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Ids the user has switched off, from the first folder scanned. Only
/// the first: the others may be read-only folders belonging to another
/// application, which Schist has no business writing to.
fn read_disabled_list(dir: Option<&PathBuf>) -> Vec<String> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    std::fs::read_to_string(dir.join("disabled.txt"))
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Copy a `.plugin` bundle, which is a directory.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Adapts a Photoshop filter to the internal [`FilterPlugin`] trait.
struct EightBfFilter {
    found: Found,
    // `FilterPlugin` hands out `&'static str`; a plug-in's metadata is
    // owned and discovered at runtime, so it is leaked once at load
    // time — bounded by the number of plug-ins installed.
    id: &'static str,
    name: &'static str,
    category: &'static str,
    show_dialog: bool,
    /// What went wrong last time, reported through [`FilterPlugin::info`]
    /// because the trait's `apply` has nowhere to return an error.
    last_error: Mutex<Option<String>>,
    /// The plug-in's own settings from its last successful run, replayed
    /// into the next one. This is what makes a plug-in's dialog open on
    /// what you last chose rather than on its defaults, and what makes a
    /// second silent run repeat the first.
    last_parameters: Mutex<Option<Vec<u8>>>,
}

impl EightBfFilter {
    fn new(found: Found, id: String, interactive: Interactive) -> EightBfFilter {
        let name = found.pipl.name().unwrap_or_else(|| found.menu_name());
        // Group under the plug-in's own Photoshop category, so a suite
        // that installs six filters arrives as one submenu rather than
        // six loose entries. Vendors that set none share a fallback.
        let category = found
            .pipl
            .category()
            .unwrap_or_else(|| "Photoshop Plug-ins".to_string());
        EightBfFilter {
            found,
            id: Box::leak(id.into_boxed_str()),
            name: Box::leak(name.into_boxed_str()),
            category: Box::leak(category.into_boxed_str()),
            show_dialog: interactive == Interactive::Yes,
            last_error: Mutex::new(None),
            last_parameters: Mutex::new(None),
        }
    }
}

impl FilterPlugin for EightBfFilter {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn category(&self) -> &'static str {
        self.category
    }

    /// None. A `.8bf` publishes no parameter list — it carries its own
    /// dialog instead, which is what `show_dialog` asks for.
    fn params(&self) -> Vec<schist_plugin_api::FilterParam> {
        Vec::new()
    }

    /// Always: the plug-in is a separate process, and when its own
    /// dialog is showing it is waiting on a person.
    fn runs_out_of_process(&self) -> bool {
        true
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    fn info(&self) -> Option<String> {
        if let Some(err) = self.last_error.lock().unwrap().clone() {
            return Some(format!("Last run failed: {err}"));
        }
        if self.show_dialog {
            None
        } else if self.last_parameters.lock().unwrap().is_some() {
            Some("Repeats the settings from its last run; its dialog needs a window.".into())
        } else {
            Some("Runs with the plug-in's own defaults; its dialog needs a window.".into())
        }
    }

    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, _values: &FilterValues) {
        let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
            return;
        };
        if width == 0 || height == 0 || pixels.len() < width * height * 4 {
            return;
        }
        // A trailing plane is transparency to a plug-in, and an image
        // with none is cheaper to hand over — so only send the alpha
        // plane when there is something in it to send.
        let opaque = pixels.chunks_exact(4).all(|p| p[3] >= 1.0);
        let planes: u16 = if opaque { 3 } else { 4 };
        let mut image = Image::new(w, h, planes);
        for (px, out) in pixels
            .chunks_exact(4)
            .zip(image.data.chunks_exact_mut(planes as usize))
        {
            for (sample, value) in out.iter_mut().zip(px) {
                *sample = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }

        let opts = remote::RemoteOptions {
            show_dialog: self.show_dialog,
            parameters: self.last_parameters.lock().unwrap().clone(),
            ..remote::RemoteOptions::default()
        };
        let run = match remote::apply(&self.found, &mut image, &opts) {
            Ok(run) => run,
            Err(err) => {
                // The document is untouched on every one of these,
                // including a crash — that is what the helper is for.
                log::warn!("Photoshop plug-in {} failed: {err}", self.id);
                *self.last_error.lock().unwrap() = Some(err.to_string());
                return;
            }
        };
        *self.last_error.lock().unwrap() = None;
        // A plug-in that keeps no settings sends nothing back; holding
        // on to the previous block would then replay something the last
        // run never produced.
        *self.last_parameters.lock().unwrap() =
            (!run.parameters.is_empty()).then_some(run.parameters);

        for (px, sample) in pixels
            .chunks_exact_mut(4)
            .zip(image.data.chunks_exact(planes as usize))
        {
            for (value, byte) in px.iter_mut().zip(sample) {
                *value = *byte as f32 / 255.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_safe_to_type() {
        assert_eq!(slug("Alien Skin: Eye Candy!"), "alien-skin-eye-candy");
        assert_eq!(slug("  "), "");
        assert_eq!(slug("PluginMain"), "pluginmain");
    }

    #[test]
    fn the_disabled_list_survives_a_missing_folder() {
        assert!(read_disabled_list(None).is_empty());
        let dir = tempfile::tempdir().unwrap();
        assert!(read_disabled_list(Some(&dir.path().to_path_buf())).is_empty());
        std::fs::write(dir.path().join("disabled.txt"), "8bf.a.b\n\n  8bf.c.d  \n").unwrap();
        assert_eq!(
            read_disabled_list(Some(&dir.path().to_path_buf())),
            vec!["8bf.a.b".to_string(), "8bf.c.d".to_string()]
        );
    }

    #[test]
    fn refusing_a_file_that_holds_no_filter() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("not-a-plugin.8bf");
        std::fs::write(&source, b"this is not a PE file").unwrap();
        let dest = dir.path().join("installed");
        assert!(PluginManager::install(&source, &dest).is_err());
        assert!(!dest.join("not-a-plugin.8bf").exists());
    }
}
