//! Autosaved crash-recovery snapshots.

use super::*;

impl Workspace {
    // ----- crash recovery -----

    /// Directory holding autosaved recovery snapshots.
    pub fn recovery_dir() -> Option<PathBuf> {
        Some(crate::crash::state_dir()?.join("schist/recovery"))
    }

    /// One snapshot file per open document, so every dirty tab survives a
    /// crash, not just the frontmost one.
    pub(super) fn recovery_path(&self, id: schist_core::DocumentId) -> Option<PathBuf> {
        Some(Self::recovery_dir()?.join(format!("session-{}-{}.psd", std::process::id(), id.0)))
    }

    /// Write a recovery snapshot for every document with unsaved changes.
    /// Returns true when at least one snapshot was written.
    pub fn autosave(&mut self) -> bool {
        let dirty: Vec<&Document> = self
            .doc
            .iter()
            .chain(self.background_tabs.iter().map(|t| &t.doc))
            .filter(|d| d.dirty)
            .collect();
        if dirty.is_empty() {
            return false;
        }
        let Some(dir) = Self::recovery_dir() else {
            return false;
        };
        if let Err(err) = std::fs::create_dir_all(&dir) {
            log::warn!("autosave: cannot create {dir:?}: {err}");
            return false;
        }
        let mut wrote = false;
        for doc in dirty {
            let Some(path) = self.recovery_path(doc.id) else {
                continue;
            };
            match self.write_doc_to(doc, &path) {
                Ok(()) => {
                    log::debug!("autosaved recovery snapshot to {path:?}");
                    wrote = true;
                }
                Err(err) => log::warn!("autosave failed: {err:#}"),
            }
        }
        wrote
    }

    pub(super) fn clear_recovery(&self) {
        if let Some(doc) = &self.doc {
            self.remove_recovery_for(doc.id);
        }
    }

    pub(super) fn remove_recovery_for(&self, id: schist_core::DocumentId) {
        if let Some(path) = self.recovery_path(id) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Every recovery snapshot left behind by a previous run, newest
    /// first. Snapshots this process owns are skipped.
    pub fn pending_recoveries() -> Vec<PathBuf> {
        let Some(dir) = Self::recovery_dir() else {
            return Vec::new();
        };
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let found = read
            .flatten()
            .filter_map(|entry| {
                let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
                Some((modified, entry.path()))
            })
            .collect();
        Self::rank_snapshots(found, std::process::id())
    }

    /// Pick the snapshots belonging to previous runs, newest first.
    ///
    /// Split out from the directory walk so the filtering and ordering can
    /// be tested without a filesystem.
    pub(super) fn rank_snapshots(
        mut found: Vec<(std::time::SystemTime, PathBuf)>,
        own_pid: u32,
    ) -> Vec<PathBuf> {
        let own_prefix = format!("session-{own_pid}-");
        found.retain(|(_, path)| {
            if path.extension().and_then(|e| e.to_str()) != Some("psd") {
                return false;
            }
            !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&own_prefix))
        });
        // Newest first, so the most recent work is the front tab.
        found.sort_by_key(|a| std::cmp::Reverse(a.0));
        found.into_iter().map(|(_, p)| p).collect()
    }

    /// Load every pending snapshot, one tab each.
    ///
    /// `autosave` writes one snapshot per dirty document, so recovering
    /// only the newest silently dropped the rest and left their files on
    /// disk to be offered one per launch afterwards.
    pub fn recover_all(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let codecs = self.registry.shared_codecs();
        let mut recovered = 0usize;
        for path in paths {
            let Ok(mut doc) = decode_file(&codecs, &path) else {
                log::warn!("could not read recovery snapshot {path:?}");
                continue;
            };
            // No real path: it must be saved somewhere deliberate.
            doc.path = None;
            doc.dirty = true;
            doc.title = format!("{} (recovered)", doc.title);
            self.open_in_tab(doc, recovered == 0);
            // The load path prompts for fonts the document names but the
            // system lacks; a recovered document deserves the same offer.
            self.offer_missing_fonts(cx);
            let _ = std::fs::remove_file(&path);
            recovered += 1;
        }
        if recovered > 0 {
            self.status = if recovered == 1 {
                "Recovered unsaved work from a previous session".into()
            } else {
                format!("Recovered {recovered} documents from a previous session").into()
            };
            cx.notify();
        }
    }
}
