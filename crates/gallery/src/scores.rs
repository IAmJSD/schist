//! The content filter's verdicts and the per-photo caches beside the
//! thumbnails.

use std::path::PathBuf;

/// The two signals the flag rule reads out of the model's five softmax
/// classes: porn+hentai together, and "sexy" alone.
#[derive(Clone, Copy, Debug)]
pub struct ExplicitScore {
    pub explicit: f32,
    pub sexy: f32,
}

/// Whether a photo counts as explicit. The nsfwjs guidance, learned
/// again the hard way: flag on the porn and hentai classes, and only on
/// a near-certain "sexy" — that class fires on bare shoulders and
/// swimwear, and summing it in flagged most of a real camera roll.
pub fn is_explicit(score: ExplicitScore) -> bool {
    score.explicit >= 0.5 || score.sexy >= 0.9
}

/// Whether the Content (NSFW Filter) model is installed.
pub fn nsfw_installed() -> bool {
    schist_neural::installed("nsfw")
}

/// The cached content scores beside a thumbnail (`.score2`).
pub fn read_score_cache(cache: &Option<PathBuf>) -> Option<ExplicitScore> {
    let text = cache
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p.with_extension("score2")).ok())?;
    let mut parts = text
        .split_whitespace()
        .filter_map(|v| v.parse::<f32>().ok());
    match (parts.next(), parts.next()) {
        (Some(explicit), Some(sexy)) => Some(ExplicitScore { explicit, sexy }),
        _ => None,
    }
}

/// The cached search embedding beside a thumbnail (`.embed`), little-
/// endian f32s.
pub fn read_embed_cache(cache: &Option<PathBuf>) -> Option<Vec<f32>> {
    let bytes = cache
        .as_ref()
        .and_then(|p| std::fs::read(p.with_extension("embed")).ok())?;
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_photos_of_people_are_not_flagged() {
        // The first formula summed "sexy" into the verdict, and on a
        // real camera roll — people, beaches, shoulders — it flagged
        // nearly everything. Only near-certain "sexy" counts alone.
        assert!(!is_explicit(ExplicitScore {
            explicit: 0.1,
            sexy: 0.6
        }));
        assert!(is_explicit(ExplicitScore {
            explicit: 0.1,
            sexy: 0.95
        }));
        assert!(is_explicit(ExplicitScore {
            explicit: 0.55,
            sexy: 0.0
        }));
    }
}
