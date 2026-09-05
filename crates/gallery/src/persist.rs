//! `library.json`.

use crate::geo::GeoBounds;
use crate::paths::library_path;
use crate::people::{DeniedFace, PersonFile, TaggedFace};
use std::path::PathBuf;

/// A bucket as `library.json` holds it. Untagged so the shape saved
/// before buckets had rules — a bare `[name, [photos]]` pair — still
/// reads; writes always use the named form.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum BucketFile {
    Rich {
        name: String,
        #[serde(default)]
        photos: Vec<PathBuf>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        area: Option<(GeoBounds, String)>,
    },
    Plain(String, Vec<PathBuf>),
}

impl BucketFile {
    pub fn name(&self) -> &str {
        match self {
            BucketFile::Rich { name, .. } | BucketFile::Plain(name, _) => name,
        }
    }
    pub fn photos(&self) -> &[PathBuf] {
        match self {
            BucketFile::Rich { photos, .. } | BucketFile::Plain(_, photos) => photos,
        }
    }
    pub fn query(&self) -> Option<&str> {
        match self {
            BucketFile::Rich { query, .. } => query.as_deref(),
            BucketFile::Plain(..) => None,
        }
    }
    pub fn area(&self) -> Option<&(GeoBounds, String)> {
        match self {
            BucketFile::Rich { area, .. } => area.as_ref(),
            BucketFile::Plain(..) => None,
        }
    }
}

/// What `library.json` persists: the watched folders, the recents, the
/// grid's preferences, the buckets, and the people — every name given
/// to a face, and every detected face waved away as not one. Everything
/// else — sections, thumbnails, the index, the detections themselves —
/// is derived from the disk.
#[derive(Default, serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct LibraryFile {
    pub folders: Vec<PathBuf>,
    #[serde(default)]
    pub recents: Vec<PathBuf>,
    #[serde(default)]
    pub thumb_px: Option<f32>,
    #[serde(default)]
    pub group_by: Option<String>,
    #[serde(default)]
    pub buckets: Vec<BucketFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub people: Vec<PersonFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_faces: Vec<TaggedFace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_faces: Vec<DeniedFace>,
}

impl LibraryFile {
    /// Read the file, or the defaults when it is missing or unreadable.
    pub fn load() -> LibraryFile {
        library_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = library_path() else {
            anyhow::bail!("no config directory to save the library in");
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_saved_before_they_had_rules_still_read() {
        let legacy: Vec<BucketFile> =
            serde_json::from_str(r#"[["Trip", ["/a.jpg"]]]"#).expect("legacy shape");
        assert!(matches!(&legacy[0], BucketFile::Plain(name, photos)
            if name == "Trip" && photos == &[PathBuf::from("/a.jpg")]));
        let rich: Vec<BucketFile> = serde_json::from_str(
            r#"[{"name": "NYC dogs", "query": "dog",
                 "area": [{"south": 40.0, "west": -75.0, "north": 41.0, "east": -73.0}, "New York City"]}]"#,
        )
        .expect("rich shape");
        assert_eq!(rich[0].name(), "NYC dogs");
        assert_eq!(rich[0].query(), Some("dog"));
        assert!(rich[0]
            .area()
            .is_some_and(|(b, place)| place == "New York City" && b.contains(40.7, -74.0)));
    }
}
