//! Text layout and rasterization for text layers.
//!
//! Scope: system font discovery, one colour per layer with the family,
//! style and size free to change from character to character (see
//! [`StyleRun`]), left-to-right line layout with kerning, word wrapping
//! and alignment, rasterized to an 8-bit coverage mask. Complex shaping (ligature
//! substitution, bidi, vertical scripts) is out of scope for v1 — those
//! need a full shaper, which is why parley/swash is the
//! eventual home for this crate.

use schist_core::IntRect;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

/// Horizontal alignment of wrapped lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    pub fn display_name(self) -> &'static str {
        match self {
            Align::Left => "Left",
            Align::Center => "Center",
            Align::Right => "Right",
        }
    }
}

/// Everything needed to lay a text layer out. Kept serializable-simple so a
/// text layer can be re-rendered whenever its content changes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TextSpec {
    pub text: String,
    pub family: String,
    /// Ask the font database for the bold face of `family`. Defaulted so
    /// that text layers written before this existed still load.
    #[serde(default)]
    pub bold: bool,
    /// Ask for the italic face.
    #[serde(default)]
    pub italic: bool,
    /// Size in pixels (em size).
    pub size: f32,
    pub align: Align,
    /// Extra spacing between lines, as a multiple of the font's default.
    pub line_height: f32,
    /// Extra spacing between characters, in pixels.
    pub tracking: f32,
    /// Wrap width in pixels; `None` means never wrap.
    pub wrap_width: Option<f32>,
    /// Stretches of `text` set differently from the rest, by byte range.
    /// Sorted and non-overlapping; whatever they leave uncovered is set
    /// in the layer's own `family`/`bold`/`italic`/`size`. Empty for
    /// text in one font, which is also what files written before runs
    /// existed load as.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<StyleRun>,
}

impl Default for TextSpec {
    fn default() -> Self {
        TextSpec {
            text: String::new(),
            family: default_family(),
            bold: false,
            italic: false,
            size: 48.0,
            align: Align::Left,
            line_height: 1.0,
            tracking: 0.0,
            wrap_width: None,
            runs: Vec::new(),
        }
    }
}

/// A range of characters set in something other than the layer's own
/// font: each field that is `Some` overrides the layer's, the rest
/// inherit. Byte offsets into `TextSpec::text`.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct StyleRun {
    pub start: usize,
    pub end: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
}

impl StyleRun {
    /// True when this run changes nothing, so it can be dropped.
    pub fn is_plain(&self) -> bool {
        self.family.is_none() && self.bold.is_none() && self.italic.is_none() && self.size.is_none()
    }

    /// The overrides alone, without the range: what an edit applies.
    pub fn overrides(&self) -> StyleRun {
        StyleRun {
            start: 0,
            end: 0,
            ..self.clone()
        }
    }

    /// Lay `over`'s overrides on top of this run's.
    pub fn merge(&mut self, over: &StyleRun) {
        if over.family.is_some() {
            self.family = over.family.clone();
        }
        if over.bold.is_some() {
            self.bold = over.bold;
        }
        if over.italic.is_some() {
            self.italic = over.italic;
        }
        if over.size.is_some() {
            self.size = over.size;
        }
    }

    /// Whether the two runs would set a character the same way.
    fn same_style(&self, other: &StyleRun) -> bool {
        self.family == other.family
            && self.bold == other.bold
            && self.italic == other.italic
            && self.size == other.size
    }
}

/// The font one character is set in, once the layer's own settings and
/// any run covering it have been reconciled.
#[derive(Debug, Clone, PartialEq)]
pub struct CharStyle {
    pub family: String,
    pub bold: bool,
    pub italic: bool,
    pub size: f32,
}

impl CharStyle {
    /// The style as an override that would reproduce it in full.
    pub fn as_run(&self) -> StyleRun {
        StyleRun {
            start: 0,
            end: 0,
            family: Some(self.family.clone()),
            bold: Some(self.bold),
            italic: Some(self.italic),
            size: Some(self.size),
        }
    }
}

impl TextSpec {
    /// The layer's own font, which uncovered text is set in.
    pub fn base_style(&self) -> CharStyle {
        CharStyle {
            family: self.family.clone(),
            bold: self.bold,
            italic: self.italic,
            size: self.size,
        }
    }

    /// The font the character at `byte` is set in.
    pub fn style_at(&self, byte: usize) -> CharStyle {
        let mut style = self.base_style();
        if let Some(run) = self.runs.iter().find(|r| r.start <= byte && byte < r.end) {
            if let Some(f) = &run.family {
                style.family = f.clone();
            }
            if let Some(b) = run.bold {
                style.bold = b;
            }
            if let Some(i) = run.italic {
                style.italic = i;
            }
            if let Some(s) = run.size {
                style.size = s;
            }
        }
        style
    }

    /// Every family the text is set in: the layer's own first, then the
    /// runs', without repeats.
    pub fn families(&self) -> Vec<&str> {
        let mut out = vec![self.family.as_str()];
        for run in &self.runs {
            if let Some(f) = &run.family {
                if !out.contains(&f.as_str()) {
                    out.push(f);
                }
            }
        }
        out
    }

    /// Set `range` in `over`'s overrides, splitting whatever runs it
    /// cuts through. A range spanning the whole text moves the setting
    /// onto the layer itself and lifts it from every run, so text set
    /// in one font stays described as such.
    pub fn apply_style(&mut self, range: std::ops::Range<usize>, over: &StyleRun) {
        let len = self.text.len();
        let range = range.start.min(len)..range.end.min(len);
        if over.is_plain() {
            return;
        }
        if range.start == 0 && range.end == len {
            if let Some(f) = &over.family {
                self.family = f.clone();
                self.runs.iter_mut().for_each(|r| r.family = None);
            }
            if let Some(b) = over.bold {
                self.bold = b;
                self.runs.iter_mut().for_each(|r| r.bold = None);
            }
            if let Some(i) = over.italic {
                self.italic = i;
                self.runs.iter_mut().for_each(|r| r.italic = None);
            }
            if let Some(s) = over.size {
                self.size = s;
                self.runs.iter_mut().for_each(|r| r.size = None);
            }
            self.normalize_runs();
            return;
        }
        if range.is_empty() {
            return;
        }
        // Cut the existing runs at the range's edges, so the ones inside
        // can take the override while the parts outside keep theirs.
        let mut runs = Vec::with_capacity(self.runs.len() + 2);
        for run in self.runs.drain(..) {
            if run.end <= range.start || run.start >= range.end {
                runs.push(run);
                continue;
            }
            if run.start < range.start {
                runs.push(StyleRun {
                    end: range.start,
                    ..run.clone()
                });
            }
            let mut inside = StyleRun {
                start: run.start.max(range.start),
                end: run.end.min(range.end),
                ..run.clone()
            };
            inside.merge(over);
            runs.push(inside);
            if run.end > range.end {
                runs.push(StyleRun {
                    start: range.end,
                    ..run
                });
            }
        }
        // Whatever the range covers that no run did gets a fresh run.
        let mut at = range.start;
        let mut covered: Vec<(usize, usize)> = runs
            .iter()
            .filter(|r| r.start >= range.start && r.end <= range.end)
            .map(|r| (r.start, r.end))
            .collect();
        covered.sort_unstable();
        for (s, e) in covered {
            if s > at {
                runs.push(StyleRun {
                    start: at,
                    end: s,
                    ..over.overrides()
                });
            }
            at = at.max(e);
        }
        if at < range.end {
            runs.push(StyleRun {
                start: at,
                end: range.end,
                ..over.overrides()
            });
        }
        self.runs = runs;
        self.normalize_runs();
    }

    /// Keep the runs in step with `text` after `range` was replaced by
    /// `inserted` bytes.
    ///
    /// An edge before the edit stays, one after it shifts, one inside
    /// the replaced span collapses to its start. Text put down at the
    /// end of a run joins it, the way typing after a bold word stays
    /// bold; text put down at a run's start goes before it; text
    /// replacing a selection takes the style of what it replaced.
    pub fn splice_runs(&mut self, range: std::ops::Range<usize>, inserted: usize) {
        let removed = range.end.saturating_sub(range.start);
        let map = |at: usize| -> usize {
            if at < range.start {
                at
            } else if at >= range.end {
                at - removed + inserted
            } else {
                range.start
            }
        };
        for run in &mut self.runs {
            let (s, e) = (run.start, run.end);
            run.start = map(s);
            run.end = map(e);
            // The run holding the first replaced character takes the
            // replacement, however much of the run the selection took.
            if s <= range.start && range.start < e {
                run.end = run.end.max(range.start + inserted);
            }
        }
        self.normalize_runs();
    }

    /// Sort the runs, drop the empty and the plain, and join neighbours
    /// that say the same thing.
    pub fn normalize_runs(&mut self) {
        let len = self.text.len();
        for run in &mut self.runs {
            run.start = run.start.min(len);
            run.end = run.end.min(len);
        }
        self.runs.retain(|r| r.start < r.end && !r.is_plain());
        self.runs.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<StyleRun> = Vec::with_capacity(self.runs.len());
        for run in self.runs.drain(..) {
            if let Some(last) = merged.last_mut() {
                if last.end == run.start && last.same_style(&run) {
                    last.end = run.end;
                    continue;
                }
                // Overlaps only arise from corrupt input; the later run
                // starts where the earlier one ends.
                if run.start < last.end {
                    let mut run = run;
                    run.start = last.end;
                    if run.start < run.end {
                        merged.push(run);
                    }
                    continue;
                }
            }
            merged.push(run);
        }
        self.runs = merged;
    }
}

/// A rasterized text run: an 8-bit coverage mask and where it sits relative
/// to the text origin.
#[derive(Debug, Clone)]
pub struct TextRaster {
    /// Bounds relative to the layout origin (may start negative: glyphs sit
    /// above the baseline).
    pub bounds: IntRect,
    /// `bounds.width() * bounds.height()` coverage bytes.
    pub coverage: Vec<u8>,
    /// Baseline of the first line, in the same space as `bounds`. With
    /// `bounds.top` this gives the block's cap height, which is what
    /// page geometry recorded by other apps tends to be measured from.
    pub first_baseline: f32,
    /// Baseline-to-baseline distance actually used, so a caller that
    /// must hit a recorded block height can solve for `line_height`.
    pub line_advance: f32,
    /// The widest line's advance (pen) width — sum of advances rather
    /// than ink extent, so it includes both side bearings. Layout boxes
    /// recorded by other apps measure this, not the ink. The pen box
    /// spans `0..layout_width` in the same space as `bounds`.
    pub layout_width: f32,
    /// The face's capital height at the requested size, when the face
    /// declares one — the distance a flat-topped capital rises above the
    /// baseline, which is less than the ink top of an ascender.
    pub cap_height: Option<f32>,
}

impl TextRaster {
    pub fn is_empty(&self) -> bool {
        self.bounds.is_empty() || self.coverage.iter().all(|&c| c == 0)
    }
}

/// The process-wide font database, behind a lock rather than a
/// `OnceLock` because installing a font has to take effect at once: a
/// document that asked for a family we just fetched should set in it
/// now, not after a restart.
fn font_db() -> &'static RwLock<Arc<fontdb::Database>> {
    static DB: OnceLock<RwLock<Arc<fontdb::Database>>> = OnceLock::new();
    DB.get_or_init(|| RwLock::new(Arc::new(scan_fonts())))
}

/// A snapshot of the database. Callers hold an `Arc` so a concurrent
/// [`refresh`] swapping in a new scan cannot pull it out from under them.
fn db() -> Arc<fontdb::Database> {
    let cell = font_db();
    match cell.read() {
        Ok(g) => Arc::clone(&g),
        Err(poisoned) => Arc::clone(&poisoned.into_inner()),
    }
}

/// Re-scan the font directories and drop every cached face.
///
/// Call after installing a font. Names previously returned by
/// [`family_names`] stay valid: the list is rebuilt and re-leaked rather
/// than mutated, so a caller still holding the old slice keeps reading
/// good memory.
pub fn refresh() {
    let scanned = Arc::new(scan_fonts());
    match font_db().write() {
        Ok(mut g) => *g = scanned,
        Err(poisoned) => *poisoned.into_inner() = scanned,
    }
    if let Ok(mut cache) = font_cache().lock() {
        cache.clear();
    }
    if let Ok(mut names) = family_name_cache().write() {
        *names = leak_family_names();
    }
}

/// Font files handed over at startup on the web, where there are no font
/// directories to scan: the loading page fetches them and the app calls
/// [`add_font_data`] before anything shapes text.
#[cfg(target_arch = "wasm32")]
fn web_faces() -> &'static std::sync::Mutex<Vec<Vec<u8>>> {
    static FACES: OnceLock<std::sync::Mutex<Vec<Vec<u8>>>> = OnceLock::new();
    FACES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Register a font from raw file bytes and make it usable at once.
#[cfg(target_arch = "wasm32")]
pub fn add_font_data(bytes: Vec<u8>) {
    if let Ok(mut faces) = web_faces().lock() {
        faces.push(bytes);
    }
    refresh();
}

/// Where fonts fetched by the app are installed, alongside whatever the
/// platform already provides.
pub fn font_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Fonts"));
    #[cfg(target_os = "windows")]
    let base =
        std::env::var_os("LOCALAPPDATA").map(|a| PathBuf::from(a).join("Microsoft/Windows/Fonts"));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("fonts"));
    base
}

/// Write one font face into [`font_dir`] and make it usable at once.
///
/// `file_name` is trusted only as far as its last component; anything
/// that looks like a path is rejected rather than escaped, since these
/// names come from a remote catalogue.
pub fn install_face(file_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let stem = std::path::Path::new(file_name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .ok_or_else(|| format!("unusable font file name {file_name:?}"))?;
    if !stem
        .rsplit('.')
        .next()
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc"))
    {
        return Err(format!("{stem:?} is not a font file"));
    }
    // Parse it before it lands in a directory the whole system scans:
    // a catalogue that hands us an HTML error page should fail here, not
    // pollute every font list on the machine.
    let mut probe = fontdb::Database::new();
    probe.load_font_data(bytes.to_vec());
    if probe.is_empty() {
        return Err("not a usable font file".into());
    }
    let dir = font_dir().ok_or_else(|| "no user font directory on this platform".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(stem);
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Parsed faces, keyed by the whole request: the bold face of a family
/// is a different file from its regular one.
type FaceKey = (String, bool, bool);

/// A loaded face: the parsed font plus its raw file bytes, kept because
/// fontdue reads only the legacy `kern` table and modern faces store
/// their kerning as GPOS pair adjustments, which layout reads itself.
#[derive(Clone)]
struct LoadedFace {
    font: Arc<fontdue::Font>,
    data: Arc<Vec<u8>>,
    index: u32,
    /// OS/2 `sCapHeight` as a fraction of the em, when declared.
    cap_ratio: Option<f32>,
}

fn font_cache() -> &'static std::sync::Mutex<std::collections::HashMap<FaceKey, Option<LoadedFace>>>
{
    static CACHE: OnceLock<
        std::sync::Mutex<std::collections::HashMap<FaceKey, Option<LoadedFace>>>,
    > = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn scan_fonts() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    if let Some(dir) = font_dir() {
        db.load_fonts_dir(dir);
    }
    // Font Book is sandboxed on modern macOS, so a font "installed" by
    // double-clicking lands in its container rather than ~/Library/Fonts.
    // Every CoreText app sees it; a directory scan does not, which is why
    // user-installed fonts were missing from the font menu.
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        db.load_fonts_dir(
            PathBuf::from(home).join("Library/Containers/com.apple.FontBook/Data/Library/Fonts"),
        );
    }
    #[cfg(target_arch = "wasm32")]
    if let Ok(faces) = web_faces().lock() {
        for bytes in faces.iter() {
            db.load_font_data(bytes.clone());
        }
    }
    // Point the generic families at something that actually exists, so
    // an unknown family name resolves through `Family::SansSerif`
    // instead of failing the query outright.
    let pick = |db: &fontdb::Database, candidates: &[&str]| -> Option<String> {
        candidates
            .iter()
            .find(|c| db.faces().any(|f| f.families.iter().any(|(n, _)| n == *c)))
            .map(|c| c.to_string())
            .or_else(|| {
                db.faces()
                    .next()
                    .and_then(|f| f.families.first().map(|(n, _)| n.clone()))
            })
    };
    if let Some(name) = pick(
        &db,
        &[
            "DejaVu Sans",
            "Noto Sans",
            "Liberation Sans",
            "Arial",
            "Helvetica",
        ],
    ) {
        db.set_sans_serif_family(name);
    }
    if let Some(name) = pick(
        &db,
        &[
            "DejaVu Serif",
            "Noto Serif",
            "Liberation Serif",
            "Times New Roman",
        ],
    ) {
        db.set_serif_family(name);
    }
    if let Some(name) = pick(
        &db,
        &[
            "DejaVu Sans Mono",
            "Noto Sans Mono",
            "Liberation Mono",
            "Courier New",
        ],
    ) {
        db.set_monospace_family(name);
    }
    log::debug!("text-engine: {} font faces", db.len());
    db
}

/// Families available on this system, sorted and de-duplicated.
pub fn families() -> Vec<String> {
    let mut names: Vec<String> = db()
        .faces()
        .filter_map(|f| f.families.first().map(|(name, _)| name.clone()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// True when this exact family is installed — not a substitute for it.
///
/// [`rasterize`] never fails on an unknown family (the query falls
/// through to the generic sans), so this is the only way to tell that a
/// document asked for something we do not have.
pub fn has_family(name: &str) -> bool {
    let name = name.trim();
    db().faces()
        .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
}

fn leak_family_names() -> &'static [&'static str] {
    let names: Vec<&'static str> = families()
        .into_iter()
        .map(|n| &*Box::leak(n.into_boxed_str()))
        .collect();
    Box::leak(names.into_boxed_slice())
}

fn family_name_cache() -> &'static RwLock<&'static [&'static str]> {
    static NAMES: OnceLock<RwLock<&'static [&'static str]>> = OnceLock::new();
    NAMES.get_or_init(|| RwLock::new(leak_family_names()))
}

/// The installed families as a fixed list, for controls that need one.
///
/// The options bar asks for this on every frame it draws, so the list is
/// built once and leaked rather than re-collected; [`refresh`] rebuilds
/// it after an install.
pub fn family_names() -> &'static [&'static str] {
    match family_name_cache().read() {
        Ok(g) => *g,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// A reasonable default family: whatever the database resolved as its
/// sans-serif alias.
pub fn default_family() -> String {
    db().family_name(&fontdb::Family::SansSerif).to_string()
}

/// Metric-compatible stand-ins for a family this system lacks, best
/// match first.
///
/// A document was laid out against real advance widths, so substituting
/// Arial with a Helvetica clone reproduces its line breaks and its
/// measured text extents; falling straight through to the generic sans
/// (often DejaVu, which is appreciably wider) does not. Only families
/// designed to share metrics belong here — this is not a lookalike
/// table.
pub fn substitutes(family: &str) -> &'static [&'static str] {
    match family.trim().to_ascii_lowercase().as_str() {
        "arial" | "arial mt" | "helvetica" | "helvetica neue" | "swiss 721" => &[
            "Liberation Sans",
            "Arimo",
            "Nimbus Sans",
            "Helvetica",
            "Arial",
        ],
        "times" | "times new roman" | "timesnewromanpsmt" => &[
            "Liberation Serif",
            "Tinos",
            "Nimbus Roman",
            "Times New Roman",
        ],
        "courier" | "courier new" => &[
            "Liberation Mono",
            "Cousine",
            "Nimbus Mono PS",
            "Courier New",
        ],
        "georgia" => &["Gelasio", "Tinos"],
        "verdana" | "tahoma" => &["DejaVu Sans", "Bitstream Vera Sans"],
        "calibri" => &["Carlito", "Liberation Sans"],
        "cambria" => &["Caladea", "Liberation Serif"],
        _ => &[],
    }
}

/// The best metric-compatible stand-in for a family, whether or not it
/// is installed yet — what to offer someone whose document names a font
/// they cannot legally be given.
///
/// Returns `None` for a family with no such twin; that one has to be
/// found in a font catalogue or not at all.
pub fn nearest_substitute(family: &str) -> Option<&'static str> {
    substitutes(family).first().copied()
}

/// Load and cache a parsed font by family name.
fn load_font(family: &str, bold: bool, italic: bool) -> Option<LoadedFace> {
    let cache = font_cache();
    let key = (family.to_string(), bold, italic);
    if let Some(hit) = cache.lock().ok()?.get(&key) {
        return hit.clone();
    }

    // Asked-for family first, then its metric equivalents, then the
    // generic sans as a last resort.
    let mut families = vec![fontdb::Family::Name(family)];
    families.extend(substitutes(family).iter().map(|n| fontdb::Family::Name(n)));
    families.push(fontdb::Family::SansSerif);
    let query = fontdb::Query {
        families: &families,
        weight: if bold {
            fontdb::Weight::BOLD
        } else {
            fontdb::Weight::NORMAL
        },
        style: if italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
        ..Default::default()
    };
    let font = db().query(&query).and_then(|id| {
        db().with_face_data(id, |data, index| {
            fontdue::Font::from_bytes(
                data,
                fontdue::FontSettings {
                    collection_index: index,
                    ..Default::default()
                },
            )
            .ok()
            .map(|font| LoadedFace {
                font: Arc::new(font),
                data: Arc::new(data.to_vec()),
                index,
                cap_ratio: ttf_parser::Face::parse(data, index).ok().and_then(|f| {
                    let cap = f.capital_height().filter(|&c| c > 0)? as f32;
                    Some(cap / f.units_per_em() as f32)
                }),
            })
        })
        .flatten()
    });
    if font.is_none() {
        log::warn!("text-engine: no usable font for {family:?}");
    }
    if let Ok(mut c) = cache.lock() {
        c.insert(key, font.clone());
    }
    font
}

/// The GPOS `kern`-feature pair adjustments of a face, resolved once per
/// layout. fontdue reads only the legacy `kern` table; most modern faces
/// keep their kerning here instead, and Affinity applies it, so matching
/// its line widths requires it.
struct GposKern<'a> {
    face: ttf_parser::Face<'a>,
    subtables: Vec<ttf_parser::gpos::PairAdjustment<'a>>,
    /// Pixels per font unit at the requested size.
    scale: f32,
}

impl<'a> GposKern<'a> {
    fn new(data: &'a [u8], index: u32, size: f32) -> Option<Self> {
        let face = ttf_parser::Face::parse(data, index).ok()?;
        let gpos = face.tables().gpos?;
        let mut lookup_indices: Vec<u16> = Vec::new();
        for feature in gpos.features {
            if feature.tag == ttf_parser::Tag::from_bytes(b"kern") {
                for i in feature.lookup_indices {
                    if !lookup_indices.contains(&i) {
                        lookup_indices.push(i);
                    }
                }
            }
        }
        let mut subtables = Vec::new();
        for i in lookup_indices {
            let Some(lookup) = gpos.lookups.get(i) else {
                continue;
            };
            for j in 0..lookup.subtables.len() {
                if let Some(ttf_parser::gpos::PositioningSubtable::Pair(pair)) =
                    lookup
                        .subtables
                        .get::<ttf_parser::gpos::PositioningSubtable>(j)
                {
                    subtables.push(pair);
                }
            }
        }
        if subtables.is_empty() {
            return None;
        }
        let upem = face.units_per_em();
        Some(Self {
            scale: size / upem as f32,
            face,
            subtables,
        })
    }

    /// Advance adjustment for the adjacent pair `(prev, next)`, in px.
    /// The first subtable that covers the pair speaks for the face.
    fn kern(&self, prev: char, next: char) -> Option<f32> {
        use ttf_parser::gpos::PairAdjustment;
        let a = self.face.glyph_index(prev)?;
        let b = self.face.glyph_index(next)?;
        for st in &self.subtables {
            match st {
                PairAdjustment::Format1 { coverage, sets } => {
                    if let Some(idx) = coverage.get(a) {
                        if let Some((first, _)) = sets.get(idx).and_then(|s| s.get(b)) {
                            return Some(first.x_advance as f32 * self.scale);
                        }
                    }
                }
                PairAdjustment::Format2 {
                    coverage,
                    classes,
                    matrix,
                } => {
                    if coverage.contains(a) {
                        let pair = (classes.0.get(a), classes.1.get(b));
                        if let Some((first, _)) = matrix.get(pair) {
                            return Some(first.x_advance as f32 * self.scale);
                        }
                    }
                }
            }
        }
        None
    }
}

/// One laid-out glyph, positioned relative to the layout origin.
#[derive(Debug, Clone, Copy)]
struct PlacedGlyph {
    ch: char,
    x: f32,
    baseline: f32,
    /// Index into the layout's faces: which font, at which size.
    face: usize,
}

/// Where a character's pen starts, for caret placement.
#[derive(Debug, Clone, Copy)]
struct CharPos {
    byte: usize,
    x: f32,
}

/// Laid-out glyphs, the widest line, the first baseline and the
/// baseline-to-baseline step.
struct Layout {
    glyphs: Vec<PlacedGlyph>,
    first_baseline: f32,
    line_advance: f32,
    layout_width: f32,
    lines: Vec<LineSpan>,
    chars: Vec<CharPos>,
}

/// The faces a spec sets its text in, one per distinct family, style
/// and size, and which of them each byte of the text uses. Index 0 is
/// the layer's own font.
struct Faces {
    faces: Vec<(LoadedFace, f32)>,
    by_byte: Vec<usize>,
}

impl Faces {
    fn resolve(spec: &TextSpec, base: &LoadedFace) -> Faces {
        let base_style = spec.base_style();
        let mut styles = vec![base_style.clone()];
        let mut faces = vec![(base.clone(), spec.size)];
        let mut by_byte = vec![0usize; spec.text.len()];
        for run in &spec.runs {
            let (s, e) = (run.start.min(spec.text.len()), run.end.min(spec.text.len()));
            if s >= e {
                continue;
            }
            let style = spec.style_at(s);
            if style == base_style {
                continue;
            }
            let ix = match styles.iter().position(|k| *k == style) {
                Some(ix) => ix,
                None => {
                    // A family that cannot be loaded falls back to the
                    // layer's own face, at the run's size.
                    let face = load_font(&style.family, style.bold, style.italic)
                        .unwrap_or_else(|| base.clone());
                    styles.push(style.clone());
                    faces.push((face, style.size));
                    faces.len() - 1
                }
            };
            for b in &mut by_byte[s..e] {
                *b = ix;
            }
        }
        Faces { faces, by_byte }
    }

    fn at(&self, byte: usize) -> usize {
        self.by_byte.get(byte).copied().unwrap_or(0)
    }

    /// Ascent and natural line step of face `ix`.
    fn line_metrics(&self, ix: usize) -> (f32, f32) {
        let (face, size) = &self.faces[ix];
        face.font
            .horizontal_line_metrics(*size)
            .map(|m| (m.ascent, m.new_line_size))
            .unwrap_or((size * 0.8, size * 1.2))
    }
}

/// One laid-out line, and the byte range of `TextSpec::text` it covers.
///
/// Wrapping means a line does not always correspond to a source line, so
/// the range is what lets a caret offset be mapped onto the page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineSpan {
    /// Byte offset of the line's first character in `TextSpec::text`.
    pub start: usize,
    /// Byte offset one past the line's last character.
    pub end: usize,
    /// x of the line's first glyph, after alignment.
    pub x: f32,
    /// Advance width of the line.
    pub width: f32,
    /// y of the line's top, relative to the raster origin.
    pub top: f32,
    /// Baseline-to-baseline step, i.e. this line's height.
    pub height: f32,
}

/// Where a caret sits, relative to the text raster's origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caret {
    pub x: f32,
    pub top: f32,
    pub height: f32,
}

fn layout(spec: &TextSpec, base: &LoadedFace) -> Layout {
    let faces = Faces::resolve(spec, base);
    // A face with GPOS kerning speaks through it alone; the legacy
    // `kern` table is only consulted when there is no GPOS to read.
    // Kerning is looked up per face, and only between neighbours set in
    // the same face at the same size: a pair that straddles a font
    // change has no table to consult.
    let kerns: Vec<Option<GposKern>> = faces
        .faces
        .iter()
        .map(|(face, size)| GposKern::new(&face.data, face.index, *size))
        .collect();
    let advance = |ch: char, prev: Option<(char, usize)>, ix: usize| -> f32 {
        let (face, size) = &faces.faces[ix];
        let m = face.font.metrics(ch, *size);
        let kern = prev
            .filter(|(_, pix)| *pix == ix)
            .and_then(|(p, _)| match &kerns[ix] {
                Some(g) => g.kern(p, ch),
                None => face.font.horizontal_kern(p, ch, *size),
            })
            .unwrap_or(0.0);
        m.advance_width + kern + spec.tracking
    };
    // Advance of `word` starting at byte `from`, after `prev`.
    let measure_word = |word: &str, from: usize, mut prev: Option<(char, usize)>| -> f32 {
        let mut width = 0.0;
        for (i, ch) in word.char_indices() {
            let ix = faces.at(from + i);
            width += advance(ch, prev, ix);
            prev = Some((ch, ix));
        }
        width
    };

    // Split into wrapped lines, carrying each line's byte range in
    // `spec.text` so a caret offset can be mapped onto the page. The
    // caret and the glyphs must come from the same pass: measuring them
    // separately is what let the overlay drift away from the ink.
    struct Line {
        text: String,
        width: f32,
        start: usize,
        end: usize,
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut line_start = 0usize;
    for raw_line in spec.text.split('\n') {
        let mut current = String::new();
        let mut width = 0.0f32;
        let mut prev: Option<(char, usize)> = None;
        let mut start = line_start;
        let mut at = line_start;
        // Wrap on word boundaries; a single over-long word is left to
        // overflow rather than being broken mid-word.
        for word in raw_line.split_inclusive(' ') {
            let mut word_width = measure_word(word, at, prev);
            let wraps = spec
                .wrap_width
                .is_some_and(|w| !current.is_empty() && width + word_width > w);
            if wraps {
                lines.push(Line {
                    text: std::mem::take(&mut current),
                    width,
                    start,
                    end: at,
                });
                start = at;
                width = 0.0;
                // Re-measure the word with no kerning context: it now
                // starts a line, so there is no preceding glyph.
                word_width = measure_word(word, at, None);
            }
            current.push_str(word);
            width += word_width;
            prev = word
                .char_indices()
                .last()
                .map(|(i, ch)| (ch, faces.at(at + i)));
            at += word.len();
        }
        lines.push(Line {
            text: current,
            width,
            start,
            end: at,
        });
        // Step past this source line and the newline that ended it.
        line_start = at + 1;
    }

    let max_width = lines.iter().map(|l| l.width).fold(0.0f32, f32::max);
    let mut placed = Vec::new();
    let mut chars = Vec::new();
    let mut spans = Vec::with_capacity(lines.len());
    let mut first_baseline = 0.0;
    let mut first_advance = 0.0;
    let mut top = 0.0f32;
    for (i, line) in lines.iter().enumerate() {
        // A line is as tall as the tallest face on it, and an empty line
        // as tall as the layer's own font.
        let mut used: Vec<usize> = line
            .text
            .char_indices()
            .map(|(k, _)| faces.at(line.start + k))
            .collect();
        used.sort_unstable();
        used.dedup();
        if used.is_empty() {
            used.push(0);
        }
        let (ascent, line_gap) = used
            .iter()
            .map(|&ix| faces.line_metrics(ix))
            .fold((0.0f32, 0.0f32), |(a, g), (fa, fg)| (a.max(fa), g.max(fg)));
        let line_advance = line_gap * spec.line_height.max(0.1);
        let baseline = top + ascent;
        if i == 0 {
            first_baseline = ascent;
            first_advance = line_advance;
        }
        let start_x = match spec.align {
            Align::Left => 0.0,
            Align::Center => (max_width - line.width) / 2.0,
            Align::Right => max_width - line.width,
        };
        spans.push(LineSpan {
            start: line.start,
            end: line.end,
            x: start_x,
            width: line.width,
            top,
            height: line_advance,
        });
        let mut x = start_x;
        let mut prev: Option<(char, usize)> = None;
        for (k, ch) in line.text.char_indices() {
            let byte = line.start + k;
            let ix = faces.at(byte);
            chars.push(CharPos { byte, x });
            if !ch.is_whitespace() {
                placed.push(PlacedGlyph {
                    ch,
                    x,
                    baseline,
                    face: ix,
                });
            }
            x += advance(ch, prev, ix);
            prev = Some((ch, ix));
        }
        top += line_advance;
    }
    Layout {
        glyphs: placed,
        first_baseline,
        line_advance: first_advance,
        layout_width: max_width,
        lines: spans,
        chars,
    }
}

/// The laid-out lines of `spec`, with the byte range of `spec.text` each
/// one covers.
///
/// Returns an empty vec when no font can be loaded.
pub fn line_spans(spec: &TextSpec) -> Vec<LineSpan> {
    let Some(face) = load_font(&spec.family, spec.bold, spec.italic) else {
        return Vec::new();
    };
    layout(spec, &face).lines
}

/// Where a caret sitting at `byte` in `spec.text` lands, relative to the
/// raster's top-left origin.
///
/// `byte` is clamped into range and snapped to a char boundary, so a
/// caller that has lost track of the text cannot panic the layout.
pub fn caret_at(spec: &TextSpec, byte: usize) -> Option<Caret> {
    let face = load_font(&spec.family, spec.bold, spec.italic)?;
    let laid = layout(spec, &face);
    let byte = clamp_to_boundary(&spec.text, byte);

    // The last line whose range starts at or before `byte`: with an
    // explicit newline the offset sits in two ranges (the end of one and
    // the start of the next), and a caret after a newline belongs on the
    // new line.
    let span = laid
        .lines
        .iter()
        .rev()
        .find(|l| l.start <= byte)
        .copied()
        .or_else(|| laid.lines.first().copied())
        .unwrap_or(LineSpan {
            start: 0,
            end: 0,
            x: 0.0,
            width: 0.0,
            top: 0.0,
            height: laid.line_advance,
        });

    let upto = byte.clamp(span.start, span.end);
    // The pen position of the character the caret sits before, or the
    // line's end after its last one. Same pass as the glyphs, so a
    // caret between two fonts lands exactly where the ink changes.
    let x = if upto >= span.end {
        span.x + span.width
    } else {
        laid.chars
            .iter()
            .find(|c| c.byte == upto)
            .map(|c| c.x)
            .unwrap_or(span.x + span.width)
    };
    Some(Caret {
        x,
        top: span.top,
        height: if span.height > 0.0 {
            span.height
        } else {
            spec.size
        },
    })
}

/// Nearest char boundary at or below `byte`, clamped to the string.
fn clamp_to_boundary(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Lay out and rasterize `spec` into a coverage mask.
///
/// Returns `None` when no font could be loaded; an empty string yields an
/// empty raster rather than an error.
pub fn rasterize(spec: &TextSpec) -> Option<TextRaster> {
    let face = load_font(&spec.family, spec.bold, spec.italic)?;
    if spec.text.is_empty() || spec.size <= 0.0 {
        return Some(TextRaster {
            bounds: IntRect::EMPTY,
            coverage: Vec::new(),
            first_baseline: 0.0,
            line_advance: 0.0,
            layout_width: 0.0,
            cap_height: face.cap_ratio.map(|r| r * spec.size),
        });
    }
    let faces = Faces::resolve(spec, &face);
    let Layout {
        glyphs: placed,
        first_baseline,
        line_advance,
        layout_width,
        ..
    } = layout(spec, &face);
    if placed.is_empty() {
        return Some(TextRaster {
            bounds: IntRect::EMPTY,
            coverage: Vec::new(),
            first_baseline: 0.0,
            line_advance: 0.0,
            layout_width: 0.0,
            cap_height: face.cap_ratio.map(|r| r * spec.size),
        });
    }

    // Rasterize once to find the union of glyph boxes...
    let mut rasterized = Vec::with_capacity(placed.len());
    let mut bounds = IntRect::EMPTY;
    for g in &placed {
        let (font, size) = &faces.faces[g.face];
        let (metrics, bitmap) = font.font.rasterize(g.ch, *size);
        if metrics.width == 0 || metrics.height == 0 {
            continue;
        }
        let left = (g.x + metrics.xmin as f32).floor() as i32;
        let top = (g.baseline - metrics.height as f32 - metrics.ymin as f32).floor() as i32;
        let rect = IntRect::from_xywh(left, top, metrics.width as u32, metrics.height as u32);
        bounds = bounds.union(&rect);
        rasterized.push((rect, bitmap));
    }
    if bounds.is_empty() {
        return Some(TextRaster {
            bounds: IntRect::EMPTY,
            coverage: Vec::new(),
            first_baseline: 0.0,
            line_advance: 0.0,
            layout_width: 0.0,
            cap_height: face.cap_ratio.map(|r| r * spec.size),
        });
    }

    // ...then blit them into one mask, taking the max where glyphs overlap.
    let w = bounds.width() as usize;
    let h = bounds.height() as usize;
    let mut coverage = vec![0u8; w * h];
    for (rect, bitmap) in rasterized {
        for gy in 0..rect.height() {
            for gx in 0..rect.width() {
                let v = bitmap[(gy * rect.width() + gx) as usize];
                if v == 0 {
                    continue;
                }
                let x = (rect.left + gx - bounds.left) as usize;
                let y = (rect.top + gy - bounds.top) as usize;
                let slot = &mut coverage[y * w + x];
                *slot = (*slot).max(v);
            }
        }
    }
    Some(TextRaster {
        bounds,
        coverage,
        first_baseline,
        line_advance,
        layout_width,
        cap_height: face.cap_ratio.map(|r| r * spec.size),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(text: &str) -> TextSpec {
        TextSpec {
            text: text.into(),
            size: 32.0,
            ..Default::default()
        }
    }

    fn ink(r: &TextRaster) -> usize {
        r.coverage.iter().filter(|&&v| v > 0).count()
    }

    fn run(start: usize, end: usize) -> StyleRun {
        StyleRun {
            start,
            end,
            ..Default::default()
        }
    }

    #[test]
    fn a_run_sets_part_of_the_text_in_another_size() {
        let plain = spec("AB");
        let mut mixed = spec("AB");
        mixed.runs.push(StyleRun {
            size: Some(96.0),
            ..run(1, 2)
        });
        let small = rasterize(&plain).unwrap();
        let big_b = rasterize(&mixed).unwrap();
        assert!(big_b.bounds.width() > small.bounds.width() + 10);
        assert!(big_b.bounds.height() > small.bounds.height() + 10);
        // The A is untouched, so the caret between the two letters has
        // not moved; the caret after the B has, by a lot.
        let between = caret_at(&mixed, 1).unwrap().x;
        assert!((between - caret_at(&plain, 1).unwrap().x).abs() < 0.01);
        assert!(caret_at(&mixed, 2).unwrap().x > caret_at(&plain, 2).unwrap().x + 20.0);
        // Both lines of a two-line layout are placed, and the tall B's
        // line is taller than a plain one.
        let mut two = spec("AB\nA");
        two.runs.push(StyleRun {
            size: Some(96.0),
            ..run(1, 2)
        });
        let spans = line_spans(&two);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].height > spans[1].height + 10.0);
        assert!((spans[1].top - spans[0].height).abs() < 0.01);
    }

    #[test]
    fn old_specs_load_without_runs_and_new_ones_keep_them() {
        let old = r#"{"text":"Hi","family":"X","size":12.0,"align":"Left","line_height":1.0,"tracking":0.0,"wrap_width":null}"#;
        let spec: TextSpec = serde_json::from_str(old).unwrap();
        assert!(spec.runs.is_empty());
        let mut with = spec.clone();
        with.runs.push(StyleRun {
            family: Some("Y".into()),
            bold: Some(true),
            ..run(0, 1)
        });
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains("\"runs\""));
        assert!(
            !json.contains("italic\":null"),
            "unset overrides stay out: {json}"
        );
        assert_eq!(serde_json::from_str::<TextSpec>(&json).unwrap(), with);
        // A plain spec writes no runs key at all, so the block is
        // byte-for-byte what it was before runs existed.
        assert!(!serde_json::to_string(&spec).unwrap().contains("runs"));
    }

    #[test]
    fn styling_a_range_splits_the_runs_it_cuts_through() {
        let mut s = spec("hello world");
        s.family = "Base".into();
        s.apply_style(
            0..5,
            &StyleRun {
                bold: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            s.runs,
            vec![StyleRun {
                bold: Some(true),
                ..run(0, 5)
            }]
        );
        // A family over "llo wo" cuts the bold run and covers the gap.
        s.apply_style(
            2..8,
            &StyleRun {
                family: Some("Other".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            s.runs,
            vec![
                StyleRun {
                    bold: Some(true),
                    ..run(0, 2)
                },
                StyleRun {
                    bold: Some(true),
                    family: Some("Other".into()),
                    ..run(2, 5)
                },
                StyleRun {
                    family: Some("Other".into()),
                    ..run(5, 8)
                },
            ]
        );
        assert_eq!(s.style_at(3).family, "Other");
        assert!(s.style_at(3).bold);
        assert_eq!(s.style_at(9).family, "Base");
        assert_eq!(s.families(), vec!["Base", "Other"]);
        // The whole text moves the setting onto the layer and lifts it
        // from the runs; the bold stays where it was.
        s.apply_style(
            0..s.text.len(),
            &StyleRun {
                family: Some("All".into()),
                ..Default::default()
            },
        );
        assert_eq!(s.family, "All");
        assert_eq!(
            s.runs,
            vec![StyleRun {
                bold: Some(true),
                ..run(0, 5)
            }]
        );
        assert_eq!(s.families(), vec!["All"]);
    }

    #[test]
    fn editing_the_text_keeps_the_runs_in_step() {
        let mut s = spec("ab cd");
        s.runs.push(StyleRun {
            bold: Some(true),
            ..run(3, 5)
        });
        // Typing after the bold word stays bold.
        s.text.push('e');
        s.splice_runs(5..5, 1);
        assert_eq!(s.runs[0].end, 6);
        // Typing before it goes in plain and pushes it along.
        s.text.insert_str(3, "X");
        s.splice_runs(3..3, 1);
        assert_eq!((s.runs[0].start, s.runs[0].end), (4, 7));
        // Typing inside it grows it.
        s.text.insert_str(5, "Y");
        s.splice_runs(5..5, 1);
        assert_eq!((s.runs[0].start, s.runs[0].end), (4, 8));
        // Deleting across its start shortens it from the front.
        s.text.replace_range(2..6, "");
        s.splice_runs(2..6, 0);
        assert_eq!((s.runs[0].start, s.runs[0].end), (2, 4));
        // Deleting it entirely drops it.
        s.text.replace_range(2..4, "");
        s.splice_runs(2..4, 0);
        assert!(s.runs.is_empty());
        // Typing over a selection that starts inside a run keeps its style.
        let mut s = spec("abcdef");
        s.runs.push(StyleRun {
            italic: Some(true),
            ..run(2, 4)
        });
        s.text.replace_range(3..5, "XYZ");
        s.splice_runs(3..5, 3);
        assert_eq!((s.runs[0].start, s.runs[0].end), (2, 6));
    }

    #[test]
    fn system_fonts_are_available() {
        assert!(!families().is_empty(), "no system fonts found");
        assert!(!default_family().is_empty());
    }

    #[test]
    fn renders_glyph_coverage() {
        let r = rasterize(&spec("Hi")).expect("font loads");
        assert!(!r.is_empty(), "expected ink");
        assert!(r.bounds.width() > 10, "bounds {:?}", r.bounds);
        assert!(r.bounds.height() > 10);
        assert_eq!(
            r.coverage.len(),
            (r.bounds.width() * r.bounds.height()) as usize
        );
    }

    #[test]
    fn empty_text_is_empty_not_an_error() {
        let r = rasterize(&spec("")).expect("font loads");
        assert!(r.is_empty());
        assert!(r.bounds.is_empty());
    }

    #[test]
    fn whitespace_only_produces_no_ink() {
        let r = rasterize(&spec("   ")).expect("font loads");
        assert!(r.is_empty());
    }

    #[test]
    fn larger_size_makes_larger_output() {
        let small = rasterize(&TextSpec {
            size: 16.0,
            ..spec("Ag")
        })
        .unwrap();
        let large = rasterize(&TextSpec {
            size: 64.0,
            ..spec("Ag")
        })
        .unwrap();
        assert!(
            large.bounds.width() > small.bounds.width() * 2,
            "{} vs {}",
            large.bounds.width(),
            small.bounds.width()
        );
    }

    #[test]
    fn newlines_stack_lines_vertically() {
        let one = rasterize(&spec("A")).unwrap();
        let two = rasterize(&spec("A\nA")).unwrap();
        assert!(
            two.bounds.height() > one.bounds.height() + 10,
            "two lines should be taller: {} vs {}",
            two.bounds.height(),
            one.bounds.height()
        );
        assert!(two.bounds.width() <= one.bounds.width() + 2);
    }

    #[test]
    fn wrapping_narrows_and_heightens() {
        let unwrapped = rasterize(&spec("hello world hello world")).unwrap();
        let wrapped = rasterize(&TextSpec {
            wrap_width: Some(120.0),
            ..spec("hello world hello world")
        })
        .unwrap();
        assert!(wrapped.bounds.width() < unwrapped.bounds.width());
        assert!(wrapped.bounds.height() > unwrapped.bounds.height());
        // Wrapping must not drop glyphs.
        assert!(ink(&wrapped) as f32 > ink(&unwrapped) as f32 * 0.9);
    }

    #[test]
    fn tracking_widens_without_changing_height() {
        let plain = rasterize(&spec("iiii")).unwrap();
        let tracked = rasterize(&TextSpec {
            tracking: 6.0,
            ..spec("iiii")
        })
        .unwrap();
        assert!(tracked.bounds.width() > plain.bounds.width() + 12);
        assert_eq!(tracked.bounds.height(), plain.bounds.height());
    }

    #[test]
    fn alignment_shifts_short_lines() {
        let left = rasterize(&TextSpec {
            align: Align::Left,
            ..spec("mmmmmmm\ni")
        })
        .unwrap();
        let right = rasterize(&TextSpec {
            align: Align::Right,
            ..spec("mmmmmmm\ni")
        })
        .unwrap();
        // Same overall box, but the short line's ink moves to the far side.
        let column_ink = |r: &TextRaster, from: f32, to: f32| {
            let w = r.bounds.width() as usize;
            let x0 = (w as f32 * from) as usize;
            let x1 = (w as f32 * to) as usize;
            let mut n = 0;
            for y in (r.bounds.height() / 2) as usize..r.bounds.height() as usize {
                for x in x0..x1.min(w) {
                    if r.coverage[y * w + x] > 0 {
                        n += 1;
                    }
                }
            }
            n
        };
        assert!(column_ink(&left, 0.0, 0.2) > column_ink(&left, 0.8, 1.0));
        assert!(column_ink(&right, 0.8, 1.0) > column_ink(&right, 0.0, 0.2));
    }

    #[test]
    fn unknown_family_falls_back_instead_of_failing() {
        let r = rasterize(&TextSpec {
            family: "No Such Font 12345".into(),
            ..spec("A")
        });
        assert!(r.is_some(), "should fall back to a system sans");
        assert!(!r.unwrap().is_empty());
    }
    #[test]
    fn caret_advances_along_a_line() {
        let s = spec("abc");
        let a = caret_at(&s, 0).unwrap();
        let b = caret_at(&s, 1).unwrap();
        let c = caret_at(&s, 3).unwrap();
        assert!(a.x < b.x && b.x < c.x, "{a:?} {b:?} {c:?}");
        // All on the first line.
        assert_eq!(a.top, 0.0);
        assert_eq!(c.top, 0.0);
    }

    #[test]
    fn caret_steps_down_by_the_real_line_advance() {
        let s = spec("ab\ncd");
        let first = caret_at(&s, 0).unwrap();
        let second = caret_at(&s, 3).unwrap(); // just after the newline
        assert!(second.top > first.top, "second line must sit lower");
        // The step is the engine's own line advance, which is what the
        // old overlay got wrong by assuming size * line_height.
        let spans = line_spans(&s);
        assert_eq!(spans.len(), 2);
        assert!((second.top - first.top - spans[0].height).abs() < 0.01);
    }

    #[test]
    fn caret_after_a_newline_starts_the_next_line() {
        let s = spec("ab\ncd");
        let after_newline = caret_at(&s, 3).unwrap();
        let line_start = line_spans(&s)[1];
        assert!((after_newline.x - line_start.x).abs() < 0.01);
    }

    #[test]
    fn line_spans_cover_the_source_text() {
        let s = spec("ab\ncde\nf");
        let spans = line_spans(&s);
        assert_eq!(spans.len(), 3);
        assert_eq!((spans[0].start, spans[0].end), (0, 2));
        assert_eq!((spans[1].start, spans[1].end), (3, 6));
        assert_eq!((spans[2].start, spans[2].end), (7, 8));
    }

    #[test]
    fn a_trailing_newline_gets_its_own_line() {
        // `str::lines` drops this, which is why the old caret stayed put
        // when you pressed Enter at the end of the text.
        let s = spec("ab\n");
        assert_eq!(line_spans(&s).len(), 2);
        let end = caret_at(&s, 3).unwrap();
        assert!(end.top > 0.0, "caret must move to the new empty line");
    }

    #[test]
    fn an_out_of_range_or_mid_char_offset_does_not_panic() {
        let s = spec("héllo");
        assert!(caret_at(&s, 999).is_some());
        // Byte 2 is inside the two-byte 'é'.
        assert!(caret_at(&s, 2).is_some());
    }
}
