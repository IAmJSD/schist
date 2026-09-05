//! The gallery on the AI panel's MCP server.
//!
//! The registry's catalog publishes the *document* tools; these are the
//! gallery's, hand-declared because the gallery is not a plugin. They
//! act on the live `Library` the way the sidebar does — the user sees
//! every effect — and refer to photos by path, which is what the tools
//! return and what a file on disk is called.

use super::library::{Entry, GroupBy};
use super::*;
use serde_json::{json, Value};

/// One tool definition in MCP's shape.
fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
    })
}

/// The published gallery tools.
pub(super) fn tool_defs() -> Vec<Value> {
    let paths = json!({"type": "array", "items": {"type": "string"}, "description": "Photo paths, as other gallery tools return them."});
    vec![
        def(
            "gallery_state",
            "Describe the photo gallery: watched folders, photo count, how the grid is grouped and \
             the groups, the selection, the buckets (with their smart rules), the current search, \
             the map filter, and how far the search index has got.",
            json!({}),
            &[],
        ),
        def(
            "gallery_list",
            "List photos in display order — the whole grid, one group (by its title, e.g. \
             \"September 2026\" or \"New York City\"), or one bucket (by name). Each row: path, \
             when taken, place, whether it has an edit, whether it is selected.",
            json!({
                "group": {"type": "string", "description": "A group title from gallery_state."},
                "bucket": {"type": "string", "description": "A bucket name from gallery_state."},
                "offset": {"type": "integer", "minimum": 0, "default": 0},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50},
            }),
            &[],
        ),
        def(
            "gallery_search",
            "Search photos by what is in them (\"dog on a beach\"), by where they were taken \
             (\"taken in nyc\"), or both, ranked best first. The gallery's own search box \
             shows the same results. While a bucket is being viewed (or one is named here, \
             which views it), the bucket filters first and the query ranks its photos. \
             Content search needs the Search models installed; places work from EXIF alone.",
            json!({
                "query": {"type": "string"},
                "bucket": {"type": "string", "description": "Search within this bucket (by \
                           name) only; the gallery switches to viewing it."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20},
            }),
            &["query"],
        ),
        def(
            "gallery_thumbnail",
            "Look at one photo: its gallery thumbnail (up to 256 px) as an image.",
            json!({"path": {"type": "string"}}),
            &["path"],
        ),
        def(
            "gallery_select",
            "Set the gallery's selection to these photos (the last is the lead, which Enter \
             opens). An empty list clears it.",
            json!({"paths": paths}),
            &["paths"],
        ),
        def(
            "gallery_bucket_create",
            "Create a bucket, optionally smart: a query and/or a place name (matched against \
             the gazetteer, e.g. \"Tokyo\") keeps it filling itself as photos are indexed.",
            json!({
                "name": {"type": "string"},
                "query": {"type": "string", "description": "Optional content/place search the bucket keeps matching."},
                "paths": paths,
            }),
            &["name"],
        ),
        def(
            "gallery_bucket_add",
            "Add photos to an existing bucket by name.",
            json!({"bucket": {"type": "string"}, "paths": paths}),
            &["bucket", "paths"],
        ),
        def(
            "gallery_group_by",
            "Regroup the grid by date, folder or place.",
            json!({"by": {"type": "string", "enum": ["date", "folder", "place"]}}),
            &["by"],
        ),
        def(
            "gallery_content_filter",
            "The content (NSFW) filter: read its state, or switch it. On, photos the Content \
             model flags as explicit are kept out of the grid; the reply says whether the model \
             is installed and how many photos are flagged, clean or not yet scored. Switching it \
             on needs the model, installed under Gallery \u{25b8} Manage Models\u{2026}. \
             Scoring happens in the background as photos are indexed.",
            json!({
                "enabled": {"type": "boolean", "description": "Set the filter; omit to just read it."},
            }),
            &[],
        ),
        def(
            "gallery_flagged",
            "List photos by the content filter's verdict — flagged, clean, or unscored — in \
             display order, whatever the filter switch is set to. Folder and map filters apply; \
             a bucket may be given instead.",
            json!({
                "verdict": {"type": "string", "enum": ["flagged", "clean", "unscored"], "default": "flagged"},
                "offset": {"type": "integer", "minimum": 0, "default": 0},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50},
            }),
            &[],
        ),
        def(
            "gallery_people",
            "The People album: every named person with their photo and face counts, how many \
             detected faces are still unnamed, and whether the face models are installed. Given \
             a name, lists that person's photos instead (and shows them in the grid). Faces are \
             named in the app: View a photo, click a face, type a name.",
            json!({
                "person": {"type": "string", "description": "A person's name, for their photos."},
                "offset": {"type": "integer", "minimum": 0, "default": 0},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50},
            }),
            &[],
        ),
        def(
            "gallery_open",
            "Open a photo in the editor (its edit sidecar if it has one). The document tools \
             then apply to it; the gallery is Cmd/Ctrl+Shift+G away.",
            json!({"path": {"type": "string"}}),
            &["path"],
        ),
    ]
}

fn text(value: Value) -> Vec<Value> {
    vec![json!({"type": "text", "text": value.to_string()})]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
}

fn paths_arg(args: &Value, key: &str) -> Vec<PathBuf> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Standard base64, for the one tool that returns pixels. Twenty lines
/// beats a dependency for a thumbnail.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

impl Workspace {
    /// Run one `gallery_*` tool. Everything here is what a click in the
    /// sidebar would do, so the user watches it happen.
    pub(super) fn gallery_tool(
        &mut self,
        name: &str,
        args: &Value,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Vec<Value>> {
        let lib = &self.library;
        match name {
            "gallery_state" => Ok(text(lib.state_json())),
            "gallery_list" => {
                let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .clamp(1, 500) as usize;
                let entries = lib.list_entries(str_arg(args, "group"), str_arg(args, "bucket"));
                let rows: Vec<Value> = entries
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|e| lib.entry_json(e))
                    .collect();
                Ok(text(
                    json!({"total": entries.len(), "offset": offset, "photos": rows}),
                ))
            }
            "gallery_search" => {
                let query = str_arg(args, "query")
                    .ok_or_else(|| anyhow::anyhow!("query is required"))?
                    .to_string();
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20)
                    .clamp(1, 200) as usize;
                if let Some(name) = str_arg(args, "bucket") {
                    let index = self
                        .library
                        .buckets
                        .iter()
                        .position(|b| b.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| anyhow::anyhow!("no bucket named {name:?}"))?;
                    self.library.bucket_filter = Some(index);
                    self.library.folder_filter = None;
                }
                let ranked = self.gallery_search_now(&query, cx);
                let bucket = self
                    .library
                    .search_scoped
                    .and_then(|i| self.library.buckets.get(i))
                    .map(|b| b.name.clone());
                let all: Vec<&Entry> = self
                    .library
                    .sections
                    .iter()
                    .flat_map(|s| s.entries.iter())
                    .collect();
                let rows: Vec<Value> = ranked
                    .iter()
                    .take(limit)
                    .filter_map(|(p, score)| {
                        let e = all.iter().find(|e| &e.path == p)?;
                        let mut row = self.library.entry_json(e);
                        row["score"] = json!((score * 1000.0).round() / 1000.0);
                        Some(row)
                    })
                    .collect();
                Ok(text(json!({
                    "query": query,
                    "bucket": bucket,
                    "place": self.library.search_place,
                    "matches": ranked.len(),
                    "photos": rows,
                })))
            }
            "gallery_thumbnail" => {
                let path = PathBuf::from(
                    str_arg(args, "path").ok_or_else(|| anyhow::anyhow!("path is required"))?,
                );
                let png = lib.thumb_png(&path).ok_or_else(|| {
                    anyhow::anyhow!(
                        "no thumbnail yet for {} — it renders once the photo has been on screen",
                        path.display()
                    )
                })?;
                Ok(vec![
                    json!({"type": "image", "data": base64(&png), "mimeType": "image/png"}),
                ])
            }
            "gallery_select" => {
                let paths = paths_arg(args, "paths");
                self.library.selected.clear();
                for path in paths {
                    self.library.toggle_selected(path);
                }
                cx.notify();
                Ok(text(json!({"selected": self.library.selected.len()})))
            }
            "gallery_bucket_create" => {
                let name = str_arg(args, "name")
                    .ok_or_else(|| anyhow::anyhow!("name is required"))?
                    .to_string();
                let index = self.library.add_bucket(name.clone());
                if let Some(query) = str_arg(args, "query") {
                    self.library.configure_bucket(
                        index,
                        name.clone(),
                        Some(query.to_string()),
                        None,
                    );
                }
                let paths = paths_arg(args, "paths");
                if !paths.is_empty() {
                    self.library.add_to_bucket(index, &paths);
                }
                cx.notify();
                Ok(text(json!({"bucket": name, "index": index})))
            }
            "gallery_bucket_add" => {
                let name =
                    str_arg(args, "bucket").ok_or_else(|| anyhow::anyhow!("bucket is required"))?;
                let index = self
                    .library
                    .buckets
                    .iter()
                    .position(|b| b.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| anyhow::anyhow!("no bucket named {name:?}"))?;
                let paths = paths_arg(args, "paths");
                self.library.add_to_bucket(index, &paths);
                cx.notify();
                Ok(text(json!({
                    "bucket": self.library.buckets[index].name,
                    "photos": self.library.buckets[index].contents().len(),
                })))
            }
            "gallery_people" => match str_arg(args, "person") {
                Some(name) => {
                    let index = self
                        .library
                        .people
                        .iter()
                        .position(|p| p.name.eq_ignore_ascii_case(name.trim()))
                        .ok_or_else(|| anyhow::anyhow!("nobody named {name:?}"))?;
                    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let limit = args
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(50)
                        .clamp(1, 500) as usize;
                    self.show_person(Some(super::library::PersonFilter::Person(index)), cx);
                    let entries = self.library.person_photos(index);
                    let rows: Vec<Value> = entries
                        .iter()
                        .skip(offset)
                        .take(limit)
                        .map(|e| self.library.entry_json(e))
                        .collect();
                    Ok(text(json!({
                        "person": self.library.people[index].name,
                        "total": entries.len(),
                        "offset": offset,
                        "photos": rows,
                    })))
                }
                None => Ok(text(self.library.people_json())),
            },
            "gallery_group_by" => {
                let by = str_arg(args, "by").ok_or_else(|| anyhow::anyhow!("by is required"))?;
                let group = GroupBy::from_key(by)
                    .ok_or_else(|| anyhow::anyhow!("by must be date, folder or place"))?;
                self.set_gallery_group(group, cx);
                Ok(text(json!({"group_by": by})))
            }
            "gallery_content_filter" => {
                if let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) {
                    if enabled && !schist_neural::installed("nsfw") {
                        anyhow::bail!(
                            "the Content (NSFW Filter) model is not installed; it is fetched \
                             under Gallery \u{25b8} Manage Models\u{2026} (17 MB, MIT)"
                        );
                    }
                    self.view.gallery_hide_nsfw = enabled;
                    self.save_view_options();
                    cx.notify();
                }
                let enabled = self.view.gallery_hide_nsfw;
                Ok(text(self.library.content_filter_json(enabled)))
            }
            "gallery_flagged" => {
                let verdict = str_arg(args, "verdict").unwrap_or("flagged");
                if !["flagged", "clean", "unscored"].contains(&verdict) {
                    anyhow::bail!("verdict must be flagged, clean or unscored");
                }
                let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .clamp(1, 500) as usize;
                let entries = lib.entries_by_verdict(verdict);
                let rows: Vec<Value> = entries
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|e| lib.entry_json(e))
                    .collect();
                Ok(text(json!({
                    "verdict": verdict,
                    "total": entries.len(),
                    "offset": offset,
                    "filter_enabled": self.view.gallery_hide_nsfw,
                    "model_installed": schist_neural::installed("nsfw"),
                    "photos": rows,
                })))
            }
            "gallery_open" => {
                let path = PathBuf::from(
                    str_arg(args, "path").ok_or_else(|| anyhow::anyhow!("path is required"))?,
                );
                if !path.exists() {
                    anyhow::bail!("{} does not exist", path.display());
                }
                self.open_from_gallery(path.clone(), cx);
                Ok(text(json!({"opened": path.display().to_string()})))
            }
            other => anyhow::bail!("unknown gallery tool {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gallery_tools_are_well_formed_and_uniquely_named() {
        let defs = tool_defs();
        let mut names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate tool names");
        for def in &defs {
            let name = def["name"].as_str().unwrap();
            assert!(name.starts_with("gallery_"), "{name} is not namespaced");
            assert!(!def["description"].as_str().unwrap().is_empty());
            let schema = &def["inputSchema"];
            assert_eq!(schema["type"], "object");
            // Every required argument is a declared property.
            let props = schema["properties"].as_object().unwrap();
            for required in schema["required"].as_array().unwrap() {
                let key = required.as_str().unwrap();
                assert!(props.contains_key(key), "{name} requires undeclared {key}");
            }
        }
    }

    #[test]
    fn the_thumbnail_base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0x89, b'P', b'N', b'G']), "iVBORw==");
    }
}
