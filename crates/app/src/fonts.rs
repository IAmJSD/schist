//! Finding the fonts a document asks for and fetching the ones we may
//! legally supply.
//!
//! Opening someone else's document usually means opening it without the
//! fonts it was set in. The text engine silently substitutes, which
//! keeps the file readable but changes every glyph width and line break,
//! so the page no longer looks like the page. This module works out what
//! is missing and, on request, fetches it.
//!
//! Two rules keep the fetching honest:
//!
//! * Only families the Google Fonts repository carries under a libre
//!   licence directory (`ofl/`, `apache/`, `ufl/`) are downloaded. That
//!   repository *is* the licence check — we do not guess from a name.
//! * A proprietary family is never fetched. Where a metric-compatible
//!   libre design exists (Arimo for Arial, Tinos for Times New Roman)
//!   we offer that instead and say so, because matching metrics is what
//!   actually restores the layout.

use schist_core::Document;

/// Where the licence check and the font files come from.
const GF_RAW: &str = "https://raw.githubusercontent.com/google/fonts/main";
const GF_CSS: &str = "https://fonts.googleapis.com/css2";

/// A family the document names that this system does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFont {
    /// The family as the document spells it.
    pub family: String,
    /// A metric-compatible libre family to install in its place, when
    /// the named one cannot be given away.
    pub substitute: Option<&'static str>,
}

impl MissingFont {
    /// The family a download would actually install.
    pub fn target(&self) -> &str {
        self.substitute.unwrap_or(&self.family)
    }

    /// One line explaining what the user gets, for the dialog row.
    pub fn detail(&self) -> String {
        match self.substitute {
            Some(sub) => format!("not openly licensed \u{b7} {sub} matches its metrics"),
            None => "missing \u{b7} text is being set in a substitute".to_string(),
        }
    }
}

/// The libre family in the Google Fonts catalogue built to share metrics
/// with a proprietary one.
///
/// These are drop-in replacements by design — same advance widths, so a
/// document laid out in the original keeps its line breaks. The list is
/// deliberately short: a merely similar-looking font is not a substitute
/// and pretending otherwise would silently reflow the page.
fn catalogue_twin(family: &str) -> Option<&'static str> {
    Some(match family.trim().to_ascii_lowercase().as_str() {
        "arial" | "arial mt" | "arialmt" | "helvetica" | "helvetica neue" => "Arimo",
        "times" | "times new roman" | "timesnewromanpsmt" => "Tinos",
        "courier" | "courier new" => "Cousine",
        "calibri" => "Carlito",
        "cambria" => "Caladea",
        "georgia" => "Gelasio",
        _ => return None,
    })
}

/// A family named "Geist (Beta)" is shipped as "Geist"; drop a trailing
/// parenthetical before looking anything up.
fn base_name(family: &str) -> &str {
    match family.find(" (") {
        Some(i) if family.trim_end().ends_with(')') => family[..i].trim_end(),
        _ => family.trim(),
    }
}

fn slug(family: &str) -> String {
    family
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Families the document's text layers name but this system lacks.
///
/// Purely local: no network, so opening a file never reaches out on its
/// own. Only the fetch does, and only when asked.
pub fn missing_in(doc: &Document) -> Vec<MissingFont> {
    let mut out = Vec::new();
    for family in schist_tools_type::families_used(doc) {
        let name = base_name(&family).to_string();
        if name.is_empty() || schist_text_engine::has_family(&name) {
            continue;
        }
        let substitute = catalogue_twin(&name)
            // Already have the twin? Then nothing needs downloading, but
            // the family is still worth reporting as substituted.
            .filter(|t| !schist_text_engine::has_family(t));
        if catalogue_twin(&name).is_some() && substitute.is_none() {
            continue; // a metric-identical face is already installed
        }
        if !out.iter().any(|m: &MissingFont| m.family == name) {
            out.push(MissingFont {
                family: name,
                substitute,
            });
        }
    }
    out
}

/// One downloaded face: the file name to store it under and its bytes.
pub type Face = (String, Vec<u8>);

/// Fetch the regular and bold faces of a family. Blocking — call it off
/// the UI thread.
///
/// This and the update check are the only network requests Schist makes,
/// and both only when the user asks.
pub fn fetch_family(family: &str) -> Result<Vec<Face>, String> {
    let licence =
        licence_dir(family).ok_or_else(|| format!("{family} is not in the open font catalogue"))?;
    log::info!("fetching {family} ({licence}/) from Google Fonts");

    let mut faces = Vec::new();
    for weight in [400, 700] {
        let css = format!("{GF_CSS}?family={}:wght@{weight}", family.replace(' ', "+"));
        let sheet = match get_text(&css) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("{family} weight {weight}: {e}");
                continue;
            }
        };
        let Some(url) = first_font_url(&sheet) else {
            continue;
        };
        // Only the open catalogue path. Google also serves a `/l/font`
        // path for families it licenses but does not give away, and that
        // is exactly what must not be downloaded.
        if !url.starts_with("https://fonts.gstatic.com/s/") {
            log::warn!("{family} weight {weight}: not an open-catalogue URL");
            continue;
        }
        let bytes = get_bytes(&url)?;
        let ext = if url.ends_with(".otf") { "otf" } else { "ttf" };
        faces.push((format!("{}-{weight}.{ext}", slug(family)), bytes));
    }
    if faces.is_empty() {
        return Err(format!("no downloadable faces for {family}"));
    }
    Ok(faces)
}

/// The licence directory google/fonts keeps a family in, if any. Its
/// presence there is the licence check.
fn licence_dir(family: &str) -> Option<&'static str> {
    let slug = slug(family);
    ["ofl", "apache", "ufl"].into_iter().find(|dir| {
        let url = format!("{GF_RAW}/{dir}/{slug}/METADATA.pb");
        ureq::get(&url)
            .header("User-Agent", "schist-font-fetch")
            .call()
            .is_ok_and(|r| r.status() == 200)
    })
}

/// The first `src: url(...)` in a CSS sheet.
fn first_font_url(sheet: &str) -> Option<String> {
    let at = sheet.find("src: url(")? + "src: url(".len();
    let rest = &sheet[at..];
    let end = rest.find(')')?;
    Some(rest[..end].to_string())
}

fn get_text(url: &str) -> Result<String, String> {
    ureq::get(url)
        // Without a browser agent the CSS API answers with the legacy
        // sheet, which points at formats fontdb cannot read.
        .header("User-Agent", "Mozilla/5.0")
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    // A font is a few hundred kilobytes; the cap guards against a
    // redirect to something enormous rather than being a real limit.
    const MAX: u64 = 32 << 20;
    let mut response = ureq::get(url)
        .header("User-Agent", "schist-font-fetch")
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    Read::take(response.body_mut().as_reader(), MAX)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("empty response".into());
    }
    Ok(bytes)
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proprietary_families_map_to_metric_twins() {
        assert_eq!(catalogue_twin("Arial"), Some("Arimo"));
        assert_eq!(catalogue_twin("helvetica neue"), Some("Arimo"));
        assert_eq!(catalogue_twin("Times New Roman"), Some("Tinos"));
        // A libre family has no twin: it is fetched under its own name.
        assert_eq!(catalogue_twin("Geist"), None);
        assert_eq!(catalogue_twin("Montserrat"), None);
    }

    #[test]
    fn a_trailing_parenthetical_is_not_part_of_the_name() {
        assert_eq!(base_name("Geist (Beta)"), "Geist");
        assert_eq!(base_name("Geist"), "Geist");
        // Parentheses mid-name are left alone.
        assert_eq!(base_name("Foo (Bar) Sans"), "Foo (Bar) Sans");
    }

    #[test]
    fn slugs_match_the_catalogue_directory_names() {
        assert_eq!(slug("Noto Sans"), "notosans");
        assert_eq!(slug("Geist"), "geist");
    }

    #[test]
    fn a_download_targets_the_twin_when_there_is_one() {
        let arial = MissingFont {
            family: "Arial".into(),
            substitute: Some("Arimo"),
        };
        assert_eq!(arial.target(), "Arimo");
        assert!(arial.detail().contains("Arimo"));

        let geist = MissingFont {
            family: "Geist".into(),
            substitute: None,
        };
        assert_eq!(geist.target(), "Geist");
    }

    #[test]
    fn font_urls_are_read_out_of_a_css_sheet() {
        let sheet = "@font-face {\n  font-weight: 400;\n  \
                     src: url(https://fonts.gstatic.com/s/geist/v5/abc.ttf) format('truetype');\n}";
        assert_eq!(
            first_font_url(sheet).as_deref(),
            Some("https://fonts.gstatic.com/s/geist/v5/abc.ttf")
        );
        assert_eq!(first_font_url("no source here"), None);
    }
}
