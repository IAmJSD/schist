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
    let advance = |ch: char, prev: Option<char>| -> f32 {
        let m = font.metrics(ch, spec.size);
        let kern = prev
            .and_then(|p| match &gpos {
                Some(g) => g.kern(p, ch),
                None => font.horizontal_kern(p, ch, spec.size),
            })
            .unwrap_or(0.0);
        m.advance_width + kern + spec.tracking
    };

    // Split into wrapped lines of (text, width).
    let mut lines: Vec<(String, f32)> = Vec::new();
    for raw_line in spec.text.split('\n') {
        let mut current = String::new();
        let mut width = 0.0f32;
        let mut prev: Option<char> = None;
        // Wrap on word boundaries; a single over-long word is left to
        // overflow rather than being broken mid-word.
        for word in raw_line.split_inclusive(' ') {
            let mut word_width = 0.0;
            let mut p = prev;
            for ch in word.chars() {
                word_width += advance(ch, p);
                p = Some(ch);
            }
            let wraps = spec
                .wrap_width
                .is_some_and(|w| !current.is_empty() && width + word_width > w);
            if wraps {
                lines.push((std::mem::take(&mut current), width));
                width = 0.0;
                // Re-measure the word with no kerning context: it now
                // starts a line, so there is no preceding glyph.
                let mut p = None;
                word_width = 0.0;
                for ch in word.chars() {
                    word_width += advance(ch, p);
                    p = Some(ch);
                }
            }
            current.push_str(word);
            width += word_width;
            prev = word.chars().last();
        }
        lines.push((current, width));
    }

    let max_width = lines.iter().map(|(_, w)| *w).fold(0.0f32, f32::max);
    let mut placed = Vec::new();
    for (i, (line, width)) in lines.iter().enumerate() {
        let baseline = ascent + i as f32 * line_advance;
        let mut x = match spec.align {
            Align::Left => 0.0,
            Align::Center => (max_width - width) / 2.0,
            Align::Right => max_width - width,
        };
        let mut prev: Option<char> = None;
        for ch in line.chars() {
            if !ch.is_whitespace() {
                placed.push(PlacedGlyph { ch, x, baseline });
            }
            x += advance(ch, prev);
            prev = Some(ch);
        }
    }
    Layout {
        glyphs: placed,
        first_baseline: ascent,
        line_advance,
        layout_width: max_width,
    }
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
        let (metrics, bitmap) = font.rasterize(g.ch, spec.size);
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
}
