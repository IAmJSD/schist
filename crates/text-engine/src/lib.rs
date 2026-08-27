//! Text layout and rasterization for text layers.
//!
//! Scope: system font discovery, a single font/size/colour per layer,
//! left-to-right line layout with kerning, word wrapping and alignment,
//! rasterized to an 8-bit coverage mask. Complex shaping (ligature
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
        }
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
    // Both fallback caches too: installing a CJK font is exactly when a
    // remembered "nothing covers this" stops being true, and that is the
    // flow this exists for.
    if let Ok(mut cache) = fallback_cache().lock() {
        cache.clear();
    }
    if let Ok(mut cache) = uncovered_cache().lock() {
        cache.clear();
    }
    if let Ok(mut names) = family_name_cache().write() {
        *names = leak_family_names();
    }
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

/// A face that actually has the glyph for `ch`.
///
/// `load_font` resolves one face for the whole spec and every glyph is
/// rasterized from it. fontdue maps a codepoint the face does not cover
/// to glyph 0, so `.notdef` -- the tofu box -- is drawn. Typing Japanese
/// or an emoji gave a row of empty rectangles even with Noto CJK and
/// Noto Color Emoji installed.
///
/// Returns `None` when the primary face already covers `ch`, so the
/// common path allocates and locks nothing.
fn fallback_for(ch: char, primary: &LoadedFace, bold: bool, italic: bool) -> Option<LoadedFace> {
    // Whitespace has no ink; asking for a face for it would pick one at
    // random and drag its metrics in.
    if ch.is_whitespace() || primary.font.has_glyph(ch) {
        return None;
    }
    let key = (fallback_key(ch), bold, italic);
    // A bucket is a hint about where to look first, not a promise that
    // one face serves the whole range -- and bucket 0 is a catch-all for
    // every script without an entry. Taking a hit unverified meant one
    // symbols-only face cached for an arrow rendered Cyrillic as that
    // face's tofu, and a cached miss for a single Private Use codepoint
    // blacked the whole bucket out for the session.
    if let Some(Some(face)) = fallback_cache().lock().ok()?.get(&key) {
        if face.font.has_glyph(ch) {
            return Some(face.clone());
        }
    }
    // Misses are remembered per character, for the same reason.
    if let Ok(c) = uncovered_cache().lock() {
        if c.contains(&(ch, bold, italic)) {
            return None;
        }
    }
    // Named preferences first -- they are what a person would pick, and
    // they avoid a scan in the cases that actually come up.
    let mut found = None;
    for name in preferred_families(ch) {
        if let Some(face) = load_font(name, bold, italic) {
            if face.font.has_glyph(ch) {
                found = Some(face);
                break;
            }
        }
    }
    // Otherwise ask every installed face, once per script bucket.
    if found.is_none() {
        let ids: Vec<fontdb::ID> = db().faces().map(|f| f.id).collect();
        for id in ids {
            let covers = db()
                .with_face_data(id, |data, index| {
                    ttf_parser::Face::parse(data, index)
                        .ok()
                        .and_then(|f| f.glyph_index(ch))
                        .is_some()
                })
                .unwrap_or(false);
            if !covers {
                continue;
            }
            let family = db()
                .face(id)
                .and_then(|f| f.families.first().map(|(n, _)| n.clone()));
            if let Some(face) = family.and_then(|n| load_font(&n, bold, italic)) {
                if face.font.has_glyph(ch) {
                    found = Some(face);
                    break;
                }
            }
        }
    }
    if found.is_none() {
        log::debug!("text-engine: no installed face covers U+{:04X}", ch as u32);
    }
    match &found {
        Some(_) => {
            if let Ok(mut c) = fallback_cache().lock() {
                c.insert(key, found.clone());
            }
        }
        None => {
            if let Ok(mut c) = uncovered_cache().lock() {
                c.insert((ch, bold, italic));
            }
        }
    }
    found
}

/// Which script bucket a character falls in, so the fallback is resolved
/// once per script rather than once per character.
fn fallback_key(ch: char) -> u32 {
    let c = ch as u32;
    match c {
        0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE00..=0xFE0F => 1, // emoji and symbols
        0x3000..=0x30FF | 0x31F0..=0x31FF | 0xFF00..=0xFFEF => 2,   // kana and CJK punctuation
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF => 3,   // han
        0xAC00..=0xD7AF | 0x1100..=0x11FF => 4,                     // hangul
        0x0590..=0x05FF => 5,                                       // hebrew
        0x0600..=0x06FF | 0x0750..=0x077F => 6,                     // arabic
        0x0900..=0x097F => 7,                                       // devanagari
        0x0E00..=0x0E7F => 8,                                       // thai
        _ => 0,
    }
}

/// Families worth trying for a character before scanning everything.
fn preferred_families(ch: char) -> &'static [&'static str] {
    match fallback_key(ch) {
        // Emoji land here for their *metrics*: fontdue rasterizes
        // outlines, and the colour emoji faces are bitmap (CBDT/sbix) or
        // COLR, so a matched face advances correctly but draws nothing.
        // Better than a row of tofu at the wrong widths, and the place to
        // start from when colour glyphs get a renderer.
        1 => &["Noto Color Emoji", "Apple Color Emoji", "Segoe UI Emoji"],
        2 | 3 => &[
            "Noto Sans CJK JP",
            "Noto Sans CJK SC",
            "Noto Sans JP",
            "Source Han Sans",
            "Hiragino Sans",
            "Yu Gothic",
        ],
        4 => &["Noto Sans CJK KR", "Noto Sans KR", "Malgun Gothic"],
        5 => &["Noto Sans Hebrew", "David"],
        6 => &["Noto Sans Arabic", "Geeza Pro"],
        7 => &["Noto Sans Devanagari", "Mangal"],
        8 => &["Noto Sans Thai", "Thonburi"],
        _ => &[],
    }
}

type FallbackKey = (u32, bool, bool);

fn fallback_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<FallbackKey, Option<LoadedFace>>> {
    static CACHE: OnceLock<
        std::sync::Mutex<std::collections::HashMap<FallbackKey, Option<LoadedFace>>>,
    > = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Characters no installed face covers, so the whole-database scan runs
/// once each rather than once per layout.
fn uncovered_cache() -> &'static std::sync::Mutex<std::collections::HashSet<(char, bool, bool)>> {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashSet<(char, bool, bool)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
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
}

/// Break `spec.text` into lines (honouring explicit newlines and wrapping),
/// then place glyphs along each baseline.
/// Laid-out glyphs, the widest line, the first baseline and the
/// baseline-to-baseline step.
struct Layout {
    glyphs: Vec<PlacedGlyph>,
    first_baseline: f32,
    line_advance: f32,
    layout_width: f32,
    lines: Vec<LineSpan>,
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

fn layout(spec: &TextSpec, face: &LoadedFace) -> Layout {
    let font = &face.font;
    let metrics = font.horizontal_line_metrics(spec.size);
    let (ascent, line_gap) = metrics
        .map(|m| (m.ascent, m.new_line_size))
        .unwrap_or((spec.size * 0.8, spec.size * 1.2));
    let line_advance = line_gap * spec.line_height.max(0.1);

    // A face with GPOS kerning speaks through it alone; the legacy
    // `kern` table is only consulted when there is no GPOS to read.
    let gpos = GposKern::new(&face.data, face.index, spec.size);
    // A tab is a stop, not a glyph. Four spaces wide, as every editor
    // and word processor has settled on. The Type tool turns a Tab
    // keypress into four spaces, but text arriving from a PSD's `PsTx`
    // block can contain a real `\t`, and asking the font for the tab
    // glyph's advance gave it a single ~11 px step -- so imported tabbed
    // text did not line up in any column.
    let tab_stop = (font.metrics(' ', spec.size).advance_width * 4.0).max(1.0);
    // `pen` is the position along the line, measured from the line's own
    // start, which is what a tab stop is relative to.
    let advance = |ch: char, prev: Option<char>, pen: f32| -> f32 {
        if ch == '\t' {
            return ((pen / tab_stop).floor() + 1.0) * tab_stop - pen;
        }
        // A glyph the chosen face does not cover is drawn from a
        // fallback, so its advance has to come from there too or the
        // text would be laid out against the .notdef box's width.
        if let Some(fb) = fallback_for(ch, face, spec.bold, spec.italic) {
            return fb.font.metrics(ch, spec.size).advance_width + spec.tracking;
        }
        let m = font.metrics(ch, spec.size);
        let kern = prev
            .and_then(|p| match &gpos {
                Some(g) => g.kern(p, ch),
                None => font.horizontal_kern(p, ch, spec.size),
            })
            .unwrap_or(0.0);
        m.advance_width + kern + spec.tracking
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
        let mut prev: Option<char> = None;
        let mut start = line_start;
        let mut at = line_start;
        // Wrap on word boundaries; a single over-long word is left to
        // overflow rather than being broken mid-word.
        for word in raw_line.split_inclusive(' ') {
            let mut word_width = 0.0;
            let mut p = prev;
            for ch in word.chars() {
                word_width += advance(ch, p, width + word_width);
                p = Some(ch);
            }
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
                let mut p = None;
                word_width = 0.0;
                for ch in word.chars() {
                    word_width += advance(ch, p, word_width);
                    p = Some(ch);
                }
            }
            current.push_str(word);
            width += word_width;
            at += word.len();
            prev = word.chars().last();
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

    // Alignment measures the *visible* line. `split_inclusive(' ')` keeps
    // each word's trailing space, so a line ending in one measured wider
    // than its ink and right- and centre-aligned text hung short of the
    // edge by exactly that space.
    //
    // Walking the pen rather than subtracting a trailing run also gets
    // tabs right: a tab's width depends on where it starts, so it cannot
    // be measured in isolation.
    let visible = |line: &str| -> f32 {
        let mut pen = 0.0f32;
        let mut ink_end = 0.0f32;
        let mut prev: Option<char> = None;
        for ch in line.chars() {
            pen += advance(ch, prev, pen);
            if !ch.is_whitespace() {
                ink_end = pen;
            }
            prev = Some(ch);
        }
        ink_end
    };
    let widths: Vec<f32> = lines.iter().map(|l| visible(&l.text)).collect();
    // Wrapped text aligns to its own box, not to whichever line happens to
    // be longest.
    let max_width = spec
        .wrap_width
        .unwrap_or_else(|| widths.iter().copied().fold(0.0f32, f32::max));
    let mut placed = Vec::new();
    let mut spans = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        // The span keeps the full width -- the caret and the selection
        // highlight run to the end of the text, trailing space included.
        let width = widths[i];
        let baseline = ascent + i as f32 * line_advance;
        let start_x = match spec.align {
            Align::Left => 0.0,
            Align::Center => (max_width - width) / 2.0,
            Align::Right => max_width - width,
        };
        spans.push(LineSpan {
            start: line.start,
            end: line.end,
            x: start_x,
            width: line.width,
            top: i as f32 * line_advance,
            height: line_advance,
        });
        let mut prev: Option<char> = None;
        // The pen, not a running x: a tab's width depends on where in the
        // line it starts, so it cannot be measured in isolation.
        let mut pen = 0.0f32;
        for ch in line.text.chars() {
            if !ch.is_whitespace() {
                placed.push(PlacedGlyph {
                    ch,
                    x: start_x + pen,
                    baseline,
                });
            }
            pen += advance(ch, prev, pen);
            prev = Some(ch);
        }
    }
    Layout {
        glyphs: placed,
        first_baseline: ascent,
        line_advance,
        layout_width: max_width,
        lines: spans,
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
    let prefix = spec.text.get(span.start..upto).unwrap_or("");
    Some(Caret {
        x: span.x + measure(spec, &face, prefix),
        top: span.top,
        height: if span.height > 0.0 {
            span.height
        } else {
            spec.size
        },
    })
}

/// Advance width of `text` laid out with `spec`'s font and tracking.
///
/// Used for caret placement, so it has to agree with `layout`'s advance
/// exactly, kerning included.
fn measure(spec: &TextSpec, face: &LoadedFace, text: &str) -> f32 {
    let font = &face.font;
    let gpos = GposKern::new(&face.data, face.index, spec.size);
    let mut width = 0.0;
    let mut prev: Option<char> = None;
    for ch in text.chars() {
        let m = font.metrics(ch, spec.size);
        let kern = prev
            .and_then(|p| match &gpos {
                Some(g) => g.kern(p, ch),
                None => font.horizontal_kern(p, ch, spec.size),
            })
            .unwrap_or(0.0);
        width += m.advance_width + kern + spec.tracking;
        prev = Some(ch);
    }
    width
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
    let font = &face.font;
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
        let fb = fallback_for(g.ch, &face, spec.bold, spec.italic);
        let glyph_font = fb.as_ref().map_or(font, |f| &f.font);
        let (metrics, bitmap) = glyph_font.rasterize(g.ch, spec.size);
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

    /// Whether this machine can actually draw `ch` at all.
    ///
    /// A face with no coverage still rasterizes `.notdef`, and that box
    /// has ink, so "the raster is empty" never became true on a runner
    /// with no CJK font -- the fallback tests failed there instead of
    /// skipping. Ask the resolver the question directly.
    fn resolvable(ch: char) -> bool {
        let spec = spec("");
        let Some(primary) = load_font(&spec.family, spec.bold, spec.italic) else {
            return false;
        };
        primary.font.has_glyph(ch) || fallback_for(ch, &primary, spec.bold, spec.italic).is_some()
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

    /// Right edge of the rendered ink.
    fn right_edge(r: &TextRaster) -> i32 {
        r.bounds.right
    }

    #[test]
    fn a_trailing_space_does_not_shift_aligned_text() {
        // `split_inclusive(' ')` keeps each word's trailing space, and the
        // alignment offset was computed from that padded width, so a line
        // ending in a space hung short of the edge by exactly one space.
        let render = |text: &str| {
            rasterize(&TextSpec {
                text: text.into(),
                size: 32.0,
                align: Align::Right,
                ..Default::default()
            })
            .expect("rasterized")
        };
        let without = render("mmmm\nX");
        let with = render("mmmm\nX ");
        assert_eq!(
            right_edge(&without),
            right_edge(&with),
            "a trailing space must not move the ink"
        );
    }

    #[test]
    fn wrapped_text_aligns_to_its_box_not_its_longest_line() {
        // `max_width` was the widest *rendered* line, so right-aligned
        // paragraph text sat inside its own frame by however much the
        // longest line fell short of the wrap width.
        let spec = TextSpec {
            text: "aaa bbb ccc ddd eee fff".into(),
            size: 24.0,
            align: Align::Right,
            wrap_width: Some(400.0),
            ..Default::default()
        };
        let r = rasterize(&spec).expect("rasterized");
        assert_eq!(
            r.layout_width, 400.0,
            "the alignment box is the wrap width, not the longest line"
        );
    }

    /// A tab is a stop, not a glyph.
    ///
    /// `advance` had no `'\t'` case, so it asked the font for the tab
    /// glyph's advance and got a single ~11 px step. The Type tool turns
    /// a Tab keypress into four spaces, but text arriving from a PSD's
    /// `PsTx` block can hold a real tab, and tabbed text imported that
    /// way lined up in no column at all.
    #[test]
    fn a_tab_aligns_prefixes_that_share_a_stop() {
        // Full stops are narrow enough that one, two and three of them
        // all sit inside the first stop, which is where a tab is
        // supposed to hide the difference.
        let one = rasterize(&spec(".\tX")).expect("font loads");
        let two = rasterize(&spec("..\tX")).expect("font loads");
        let three = rasterize(&spec("...\tX")).expect("font loads");
        assert!(
            (one.layout_width - two.layout_width).abs() < 0.5
                && (one.layout_width - three.layout_width).abs() < 0.5,
            "the tab did not align the column: {} / {} / {}",
            one.layout_width,
            two.layout_width,
            three.layout_width
        );
        // Without the tab they differ, so the test is measuring the tab
        // rather than a font that renders every prefix the same width.
        let plain_one = rasterize(&spec(".X")).expect("font loads");
        let plain_three = rasterize(&spec("...X")).expect("font loads");
        assert!(plain_three.layout_width > plain_one.layout_width + 1.0);
    }

    /// A prefix past the first stop reaches the next one, so the stops
    /// are a grid rather than a fixed pad.
    #[test]
    fn a_long_prefix_reaches_the_next_stop() {
        let short = rasterize(&spec(".\tX")).expect("font loads");
        let long = rasterize(&spec("MMMMMM\tX")).expect("font loads");
        assert!(
            long.layout_width > short.layout_width + 1.0,
            "{} did not clear the first stop ({})",
            long.layout_width,
            short.layout_width
        );
    }

    /// Stops are measured from each line's own start, so a wide line does
    /// not drag the line below it out of column.
    #[test]
    fn tab_stops_are_per_line() {
        let alone = rasterize(&spec(".\tX")).expect("font loads");
        let under = rasterize(&spec("MMMMMM\tX\n.\tX")).expect("font loads");
        // The second line still sits on the first stop, so the block is
        // exactly as wide as its widest line -- the long one.
        let long = rasterize(&spec("MMMMMM\tX")).expect("font loads");
        assert!(
            (under.layout_width - long.layout_width).abs() < 0.5,
            "the short line drifted: {} vs {}",
            under.layout_width,
            long.layout_width
        );
        assert!(long.layout_width > alone.layout_width);
    }

    /// Every glyph was rasterized from one face, and fontdue maps a
    /// codepoint that face does not cover to glyph 0 — the .notdef box.
    /// The tell is identical ink per glyph across unrelated characters:
    /// the same empty rectangle repeated.
    #[test]
    fn characters_outside_the_chosen_face_are_not_all_the_same_box() {
        // A machine with no CJK face installed has nothing to fall back
        // to, and drawing tofu is then the honest answer.
        if !resolvable('\u{3053}') || !resolvable('\u{4e2d}') {
            return;
        }
        let hiragana = rasterize(&spec("\u{3053}\u{3093}\u{306b}")).expect("font loads");
        let han = rasterize(&spec("\u{4e2d}\u{6587}\u{5b57}")).expect("font loads");
        let per_glyph = |r: &TextRaster| ink(r) as f32 / 3.0;
        let (a, b) = (per_glyph(&hiragana), per_glyph(&han));
        assert!(
            (a - b).abs() > 1.0,
            "hiragana and han have identical ink per glyph ({a} vs {b}), \
             which is the .notdef box repeated"
        );
    }

    /// The cache is keyed by script bucket, and bucket 0 is a catch-all:
    /// the face found for one of its characters need not cover the next,
    /// and a character nothing covers must not answer for the rest.
    #[test]
    fn one_bucket_zero_character_does_not_answer_for_another() {
        let spec = spec("");
        let Some(primary) = load_font(&spec.family, spec.bold, spec.italic) else {
            return;
        };
        // Cyrillic, an arrow and a Private Use codepoint all land in
        // bucket 0.
        let cyrillic = '\u{0416}';
        let before = fallback_for(cyrillic, &primary, false, false);
        if let Some(face) = &before {
            assert!(face.font.has_glyph(cyrillic), "a face that cannot draw it");
        }
        // Nothing covers this, so it caches a miss.
        let pua = fallback_for('\u{E000}', &primary, false, false);
        assert!(pua.is_none() || pua.unwrap().font.has_glyph('\u{E000}'));
        // Which must not have blanked the bucket.
        let after = fallback_for(cyrillic, &primary, false, false);
        assert_eq!(
            before.is_some(),
            after.is_some(),
            "a cached miss for one character swallowed another"
        );
        // Nor may a face cached for the arrow answer for Cyrillic.
        let arrow = '\u{2192}';
        if let Some(face) = fallback_for(arrow, &primary, false, false) {
            assert!(face.font.has_glyph(arrow));
        }
        if let Some(face) = fallback_for(cyrillic, &primary, false, false) {
            assert!(
                face.font.has_glyph(cyrillic),
                "the arrow's face was handed back for Cyrillic"
            );
        }
    }

    /// And the fallback's advance is used, so the layout is not measured
    /// against the box it is not drawing.
    #[test]
    fn a_fallback_glyph_advances_by_its_own_width() {
        if !resolvable('\u{4e2d}') {
            return;
        }
        let narrow = rasterize(&spec("AA")).expect("font loads");
        let Some(cjk) = rasterize(&spec("\u{4e2d}\u{6587}")) else {
            return;
        };
        // Han glyphs are full-width; two of them are wider than two
        // latin capitals at the same size.
        assert!(
            cjk.layout_width > narrow.layout_width,
            "{} should exceed {}",
            cjk.layout_width,
            narrow.layout_width
        );
    }

    /// Characters the chosen face does cover keep using it, so nothing
    /// about ordinary latin text changes.
    #[test]
    fn covered_characters_do_not_go_through_the_fallback() {
        let face = load_font(&default_family(), false, false).expect("a default face");
        for ch in "Hello, world! 123".chars() {
            assert!(
                fallback_for(ch, &face, false, false).is_none(),
                "{ch:?} should not need a fallback"
            );
        }
    }
}
