//! Text layout and rasterization for text layers (M7).
//!
//! Scope: system font discovery, a single font/size/colour per layer,
//! left-to-right line layout with kerning, word wrapping and alignment,
//! rasterized to an 8-bit coverage mask. Complex shaping (ligature
//! substitution, bidi, vertical scripts) is out of scope for v1 — those
//! need a full shaper, which is why `PLAN.md` §2 named parley/swash as the
//! eventual home for this crate.

use schist_core::IntRect;
use std::sync::{Arc, OnceLock};

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
}

impl TextRaster {
    pub fn is_empty(&self) -> bool {
        self.bounds.is_empty() || self.coverage.iter().all(|&c| c == 0)
    }
}

/// Lazily-scanned system font database.
fn db() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
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
        log::debug!("text-engine: {} system font faces", db.len());
        db
    })
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

/// The installed families as a fixed list, for controls that need one.
///
/// The set cannot change while the process runs, so it is built once and
/// leaked rather than re-collected -- the options bar asks for this on
/// every frame it draws.
pub fn family_names() -> &'static [&'static str] {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        families()
            .into_iter()
            .map(|n| &*Box::leak(n.into_boxed_str()))
            .collect()
    })
}

/// A reasonable default family: whatever the database resolved as its
/// sans-serif alias.
pub fn default_family() -> String {
    db().family_name(&fontdb::Family::SansSerif).to_string()
}

/// Load and cache a parsed font by family name.
fn load_font(family: &str, bold: bool, italic: bool) -> Option<Arc<fontdue::Font>> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    // Keyed by the whole request: the bold face of a family is a different
    // file from its regular one.
    type Key = (String, bool, bool);
    static CACHE: OnceLock<Mutex<HashMap<Key, Option<Arc<fontdue::Font>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (family.to_string(), bold, italic);
    if let Some(hit) = cache.lock().ok()?.get(&key) {
        return hit.clone();
    }

    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family), fontdb::Family::SansSerif],
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
            .map(Arc::new)
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

/// One laid-out glyph, positioned relative to the layout origin.
#[derive(Debug, Clone, Copy)]
struct PlacedGlyph {
    ch: char,
    x: f32,
    baseline: f32,
}

/// Break `spec.text` into lines (honouring explicit newlines and wrapping),
/// then place glyphs along each baseline.
fn layout(spec: &TextSpec, font: &fontdue::Font) -> (Vec<PlacedGlyph>, f32) {
    let metrics = font.horizontal_line_metrics(spec.size);
    let (ascent, line_gap) = metrics
        .map(|m| (m.ascent, m.new_line_size))
        .unwrap_or((spec.size * 0.8, spec.size * 1.2));
    let line_advance = line_gap * spec.line_height.max(0.1);

    let advance = |ch: char, prev: Option<char>| -> f32 {
        let m = font.metrics(ch, spec.size);
        let kern = prev
            .and_then(|p| font.horizontal_kern(p, ch, spec.size))
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
    (placed, max_width)
}

/// Lay out and rasterize `spec` into a coverage mask.
///
/// Returns `None` when no font could be loaded; an empty string yields an
/// empty raster rather than an error.
pub fn rasterize(spec: &TextSpec) -> Option<TextRaster> {
    let font = load_font(&spec.family, spec.bold, spec.italic)?;
    if spec.text.is_empty() || spec.size <= 0.0 {
        return Some(TextRaster {
            bounds: IntRect::EMPTY,
            coverage: Vec::new(),
        });
    }
    let (placed, _) = layout(spec, &font);
    if placed.is_empty() {
        return Some(TextRaster {
            bounds: IntRect::EMPTY,
            coverage: Vec::new(),
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
    Some(TextRaster { bounds, coverage })
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
