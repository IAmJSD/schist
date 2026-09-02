//! Ranking photos against a query: what the search box, the smart
//! buckets and the headless server all do the same way.

use crate::geo::{geo_affinity, GeoMatch};
use std::collections::HashSet;
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

/// Photos with their scores, best first.
pub type Ranked = Vec<(PathBuf, f32)>;

/// Two readings of a query, blended: what the photos look like (the
/// text tower's unit vector against each photo's) and, when it names
/// somewhere, where they were taken. Everything above the floor, best
/// first — callers truncate to what they can show.
///
/// `scope` confines the search to some photos — a bucket's contents,
/// when the search is made while viewing one — so the bucket filters
/// first and the query ranks what is left. `None` searches the lot.
pub fn rank<'a>(
    text: Option<&[f32]>,
    place: Option<&GeoMatch>,
    scope: Option<&HashSet<PathBuf>>,
    vectors: impl IntoIterator<Item = (&'a PathBuf, &'a [f32])>,
    positions: impl IntoIterator<Item = (&'a PathBuf, (f64, f64))>,
) -> Ranked {
    if text.is_none() && place.is_none() {
        return Vec::new();
    }
    let in_scope = |path: &PathBuf| scope.is_none_or(|s| s.contains(path));
    let mut scored: std::collections::HashMap<&PathBuf, f32> = std::collections::HashMap::new();
    if let Some(text) = text {
        for (path, v) in vectors {
            if !in_scope(path) {
                continue;
            }
            scored.insert(path, v.iter().zip(text.iter()).map(|(a, b)| a * b).sum());
        }
    }
    if let Some(place) = place {
        for (path, (lat, lon)) in positions {
            if !in_scope(path) {
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn photos() -> Vec<(PathBuf, Vec<f32>)> {
        vec![
            (PathBuf::from("/p/best.jpg"), vec![1.0, 0.0]),
            (PathBuf::from("/p/good.jpg"), vec![0.8, 0.6]),
            (PathBuf::from("/p/miss.jpg"), vec![0.0, 1.0]),
        ]
    }

    fn ranked(scope: Option<&HashSet<PathBuf>>) -> Vec<PathBuf> {
        let photos = photos();
        rank(
            Some(&[1.0, 0.0]),
            None,
            scope,
            photos.iter().map(|(p, v)| (p, v.as_slice())),
            std::iter::empty(),
        )
        .into_iter()
        .map(|(p, _)| p)
        .collect()
    }

    #[test]
    fn a_scope_filters_before_the_query_ranks() {
        // Unscoped: everything above the floor, best first.
        assert_eq!(
            ranked(None),
            vec![PathBuf::from("/p/best.jpg"), PathBuf::from("/p/good.jpg")]
        );
        // Scoped to a bucket: the bucket filters first, then the
        // query ranks what is left — the best photo in the library
        // stays out because it is not in the bucket, and the bucket's
        // own non-match still fails the floor.
        let scope: HashSet<PathBuf> =
            [PathBuf::from("/p/good.jpg"), PathBuf::from("/p/miss.jpg")].into();
        assert_eq!(ranked(Some(&scope)), vec![PathBuf::from("/p/good.jpg")]);
        // An empty bucket searches nothing rather than everything.
        assert!(ranked(Some(&HashSet::new())).is_empty());
    }
}
