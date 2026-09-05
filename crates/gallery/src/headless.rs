//! The gallery without the app: what the stdio MCP server answers with
//! when nothing is running. It reads what the app wrote — the library
//! file, the scan of its folders, the index snapshot — and can write
//! buckets back. It cannot see selection or grouping (those are the
//! window's), and it never indexes: photos the app has not looked at
//! are simply unscored here.

use crate::geo::find_place;
use crate::index::{read_index_snapshot, IndexRow};
use crate::meta::taken_from_unix;
use crate::paths::{thumb_cache_path, thumb_source};
use crate::persist::{BucketFile, LibraryFile};
use crate::scan::{scan_folders, Entry, Section};
use crate::search::{rank, Ranked, SEARCH_KEPT};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Gallery {
    pub file: LibraryFile,
    pub sections: Vec<Section>,
    /// Snapshot rows by path, only those whose mtime the scan confirms.
    pub index: HashMap<PathBuf, IndexRow>,
}

impl Gallery {
    /// Load the library, scan its folders for the given extensions, and
    /// take the index snapshot on board.
    pub fn open(exts: &[String]) -> Gallery {
        let file = LibraryFile::load();
        let sections = scan_folders(&file.folders, exts);
        let current: HashMap<&Path, u64> = sections
            .iter()
            .flat_map(|s| s.entries.iter())
            .map(|e| (e.path.as_path(), e.mtime))
            .collect();
        let index = read_index_snapshot()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| current.get(r.path.as_path()) == Some(&r.mtime))
            .map(|r| (r.path.clone(), r))
            .collect();
        Gallery {
            file,
            sections,
            index,
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.sections.iter().flat_map(|s| s.entries.iter())
    }

    pub fn entry(&self, path: &Path) -> Option<&Entry> {
        self.entries().find(|e| e.path == path)
    }

    /// Capture time as sortable text: the EXIF's when indexed, the
    /// file's clock otherwise.
    pub fn taken_of(&self, entry: &Entry) -> String {
        self.index
            .get(&entry.path)
            .and_then(|r| r.taken.clone())
            .unwrap_or_else(|| taken_from_unix(entry.mtime))
    }

    /// The content filter's verdict, as a word.
    pub fn verdict(&self, path: &Path) -> &'static str {
        match self.index.get(path).and_then(|r| r.flagged) {
            Some(true) => "flagged",
            Some(false) => "clean",
            None => "unscored",
        }
    }

    pub fn entry_json(&self, entry: &Entry) -> Value {
        let row = self.index.get(&entry.path);
        json!({
            "path": entry.path.display().to_string(),
            "taken": self.taken_of(entry),
            "place": row.and_then(|r| r.place.clone()).flatten(),
            "edited": entry.edited,
            "flagged": row.and_then(|r| r.flagged),
        })
    }

    pub fn state_json(&self) -> Value {
        let all: Vec<&Entry> = self.entries().collect();
        let count = |v: &str| all.iter().filter(|e| self.verdict(&e.path) == v).count();
        let embedded = self
            .index
            .values()
            .filter(|r| r.embed.as_ref().is_some_and(|v| !v.is_empty()))
            .count();
        json!({
            "folders": self.file.folders.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
            "photos": all.len(),
            "buckets": self.file.buckets.iter().enumerate().map(|(i, b)| json!({
                "index": i, "name": b.name(), "photos": b.photos().len(),
                "query": b.query(), "area": b.area().map(|(_, name)| name.clone()),
            })).collect::<Vec<_>>(),
            "index": {
                "embedded": embedded, "total": all.len(),
                "search_models_installed": schist_neural::embed::ready(),
                "content_model_installed": crate::scores::nsfw_installed(),
            },
            "content": {"flagged": count("flagged"), "clean": count("clean"), "unscored": count("unscored")},
            "people": self.people_json(),
            "note": "Read from the gallery's files on disk; the app is what indexes. Selection and grouping belong to the app's window.",
        })
    }

    /// The people album: each name with how many photos and faces
    /// carry it, plus how many detected faces nobody has named.
    pub fn people_json(&self) -> Value {
        let people: Vec<Value> = self
            .file
            .people
            .iter()
            .map(|p| json!({"name": p.name, "photos": p.photos().len(), "faces": p.faces.len()}))
            .collect();
        let unnamed = self
            .index
            .values()
            .filter_map(|row| Some((row.path.as_path(), row.faces.as_ref()?)))
            .flat_map(|(path, faces)| faces.iter().map(move |f| (path, f)))
            .filter(|(path, face)| {
                !self.file.people.iter().any(|p| p.tagged(path, &face.rect))
                    && !self
                        .file
                        .ignored_faces
                        .iter()
                        .any(|i| i.photo == *path && i.rect.same_face(&face.rect))
            })
            .count();
        json!({
            "people": people,
            "unnamed_faces": unnamed,
            "detector_installed": schist_neural::installed("face"),
            "recogniser_installed": schist_neural::installed("face-embed"),
        })
    }

    /// One person's photos, in tag order.
    pub fn person_photos(&self, name: &str) -> Option<Vec<Entry>> {
        let person = self
            .file
            .people
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim()))?;
        Some(
            person
                .photos()
                .iter()
                .filter_map(|p| self.entry(p).cloned())
                .collect(),
        )
    }

    /// Photos in scan order — the lot, one folder, or one bucket.
    pub fn list(&self, folder: Option<&str>, bucket: Option<&str>) -> Vec<Entry> {
        if let Some(name) = bucket {
            let Some(b) = self
                .file
                .buckets
                .iter()
                .find(|b| b.name().eq_ignore_ascii_case(name))
            else {
                return Vec::new();
            };
            return b
                .photos()
                .iter()
                .filter_map(|p| self.entry(p).cloned())
                .collect();
        }
        self.sections
            .iter()
            .filter(|s| folder.is_none_or(|f| s.dir.starts_with(f)))
            .flat_map(|s| s.entries.iter().cloned())
            .collect()
    }

    /// The search box's ranking, from the snapshot's embeddings and
    /// positions. Blocking on the text tower. Given a bucket, the
    /// bucket filters first and the query ranks what is left — its
    /// hand-picked photos, which are all this side can see of it (a
    /// smart rule's matches live in the app, recomputed per session).
    /// An unknown bucket name is an error, not a library-wide search.
    pub fn search(
        &self,
        query: &str,
        bucket: Option<&str>,
    ) -> anyhow::Result<(Ranked, Option<String>)> {
        let scope: Option<HashSet<PathBuf>> = match bucket {
            Some(name) => Some(
                self.file
                    .buckets
                    .iter()
                    .find(|b| b.name().eq_ignore_ascii_case(name))
                    .ok_or_else(|| anyhow::anyhow!("no bucket named {name:?}"))?
                    .photos()
                    .iter()
                    .cloned()
                    .collect(),
            ),
            None => None,
        };
        let text = schist_neural::embed::embed_text(query);
        let place = find_place(query);
        let vectors = self
            .index
            .values()
            .filter_map(|r| r.embed.as_ref().map(|v| (&r.path, v.as_slice())));
        let positions = self
            .index
            .values()
            .filter_map(|r| r.gps.flatten().map(|g| (&r.path, g)));
        let mut ranked = rank(
            text.as_deref(),
            place.as_ref(),
            scope.as_ref(),
            vectors,
            positions,
        );
        ranked.truncate(SEARCH_KEPT);
        Ok((ranked, place.map(|p| p.name)))
    }

    /// The cached thumbnail PNG, when the app has rendered one.
    pub fn thumbnail_png(&self, path: &Path) -> Option<Vec<u8>> {
        let entry = self.entry(path)?;
        let cache = thumb_cache_path(&thumb_source(&entry.path, entry.edited), entry.mtime)?;
        std::fs::read(cache).ok()
    }

    /// A new bucket, optionally with a query the app will keep matching.
    pub fn create_bucket(
        &mut self,
        name: &str,
        query: Option<&str>,
        photos: Vec<PathBuf>,
    ) -> anyhow::Result<usize> {
        self.file.buckets.push(BucketFile::Rich {
            name: name.to_string(),
            photos,
            query: query.map(str::to_string),
            area: None,
        });
        self.file.save()?;
        Ok(self.file.buckets.len() - 1)
    }

    pub fn add_to_bucket(&mut self, name: &str, paths: &[PathBuf]) -> anyhow::Result<usize> {
        let bucket = self
            .file
            .buckets
            .iter_mut()
            .find(|b| b.name().eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow::anyhow!("no bucket named {name:?}"))?;
        // A legacy pair becomes the named form on its first write.
        if let BucketFile::Plain(n, p) = bucket {
            *bucket = BucketFile::Rich {
                name: std::mem::take(n),
                photos: std::mem::take(p),
                query: None,
                area: None,
            };
        }
        if let BucketFile::Rich { photos, .. } = bucket {
            for path in paths {
                if !photos.contains(path) {
                    photos.push(path.clone());
                }
            }
        }
        let count = bucket.photos().len();
        self.file.save()?;
        Ok(count)
    }
}
