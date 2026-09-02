//! Ranking photos against a query: what the search box, the smart
//! buckets and the headless server all do the same way.

use crate::geo::{geo_affinity, GeoMatch};
use std::path::PathBuf;

/// The most results a search shows, and the similarity below which a
/// result is the model shrugging rather than matching (cosines from the
/// MobileCLIP pair sit around 0.2–0.3 for a real match).
pub const SEARCH_KEPT: usize = 200;
pub const SEARCH_FLOOR: f32 = 0.15;
/// What being squarely *in* the named place adds to a photo's score —
/// bigger than any cosine gap, so location dominates the ordering when
/// the query names one, without exiling good semantic matches.
pub const GEO_BOOST: f32 = 0.35;

/// Two readings of a query, blended: what the photos look like (the
/// text tower's unit vector against each photo's) and, when it names
/// somewhere, where they were taken. Everything above the floor, best
/// first — callers truncate to what they can show.
pub fn rank<'a>(
    text: Option<&[f32]>,
    place: Option<&GeoMatch>,
    vectors: impl IntoIterator<Item = (&'a PathBuf, &'a [f32])>,
    positions: impl IntoIterator<Item = (&'a PathBuf, (f64, f64))>,
) -> Vec<(PathBuf, f32)> {
    if text.is_none() && place.is_none() {
        return Vec::new();
    }
    let mut scored: std::collections::HashMap<&PathBuf, f32> = std::collections::HashMap::new();
    if let Some(text) = text {
        for (path, v) in vectors {
            scored.insert(path, v.iter().zip(text.iter()).map(|(a, b)| a * b).sum());
        }
    }
    if let Some(place) = place {
        for (path, (lat, lon)) in positions {
            let affinity = geo_affinity(place, lat, lon);
            if affinity > 0.0 {
                *scored.entry(path).or_insert(0.0) += GEO_BOOST * affinity;
            }
        }
    }
    let floor = if text.is_some() {
        SEARCH_FLOOR
    } else {
        // Location-only: being near the place is the whole score.
        GEO_BOOST * 0.3
    };
    let mut ranked: Vec<(PathBuf, f32)> = scored
        .into_iter()
        .filter(|(_, s)| *s >= floor)
        .map(|(p, s)| (p.clone(), s))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked
}
