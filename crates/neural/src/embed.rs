//! Image and text embeddings for the gallery's search: the two
//! MobileCLIP towers ("embed-image", "embed-text") and the CLIP
//! tokenizer the text one needs.
//!
//! The tokenizer is OpenAI CLIP's byte-level BPE, implemented here
//! rather than pulled in as a crate: lowercase, split the way CLIP's
//! pattern splits (contractions, letter runs, single digits, punctuation
//! runs), map each piece's UTF-8 bytes through the GPT-2 byte→unicode
//! table, then merge by rank with `</w>` closing every word. The vocab
//! and merges ride in the binary — half a megabyte each, extracted from
//! the model's own tokenizer.json — so the text tower needs no second
//! download. Desktop only, with the gallery that uses it.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The context length the text tower is fixed to.
const CONTEXT: usize = 77;
const BOS: i64 = 49406;
const EOS: i64 = 49407;

/// Both towers are installed, so search can work.
pub fn ready() -> bool {
    crate::installed("embed-image") && crate::installed("embed-text")
}

/// L2-normalize in place, so similarity is a plain dot product.
fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Embed one frame of interleaved RGB in 0..=1, sized as the
/// "embed-image" spec says (256×256). `None` when the model is not
/// installed.
pub fn embed_image(rgb: &[f32]) -> Option<Vec<f32>> {
    let model = crate::get("embed-image")?;
    let mut v = model
        .run_scores(rgb)
        .map_err(|e| log::warn!("image embedding failed: {e:#}"))
        .ok()?;
    normalize(&mut v);
    Some(v)
}

/// Embed a search query. `None` when the model is not installed.
pub fn embed_text(query: &str) -> Option<Vec<f32>> {
    let model = crate::get("embed-text")?;
    let ids = encode(query);
    let mut v = model
        .run_token_scores(&ids)
        .map_err(|e| log::warn!("text embedding failed: {e:#}"))
        .ok()?;
    normalize(&mut v);
    Some(v)
}

// ----- the CLIP tokenizer -----

struct Bpe {
    vocab: HashMap<String, i64>,
    ranks: HashMap<(String, String), usize>,
    /// GPT-2's byte→unicode map: every byte as a printable char.
    bytes: [char; 256],
}

/// GPT-2's trick: printable bytes map to themselves, the rest to code
/// points above 255, so any byte string becomes a clean unicode string
/// the merge table can be written in.
fn byte_map() -> [char; 256] {
    let mut out = ['\0'; 256];
    let mut extra = 0u32;
    for b in 0..=255u32 {
        let printable =
            (0x21..=0x7E).contains(&b) || (0xA1..=0xAC).contains(&b) || (0xAE..=0xFF).contains(&b);
        out[b as usize] = if printable {
            char::from_u32(b).unwrap()
        } else {
            let c = char::from_u32(256 + extra).unwrap();
            extra += 1;
            c
        };
    }
    out
}

fn bpe() -> &'static Bpe {
    static BPE: OnceLock<Bpe> = OnceLock::new();
    BPE.get_or_init(|| {
        let vocab = include_str!("../assets/clip-bpe/vocab.txt")
            .lines()
            .enumerate()
            .map(|(i, tok)| (tok.to_string(), i as i64))
            .collect();
        let ranks = include_str!("../assets/clip-bpe/merges.txt")
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let (a, b) = line.split_once(' ')?;
                Some(((a.to_string(), b.to_string()), i))
            })
            .collect();
        Bpe {
            vocab,
            ranks,
            bytes: byte_map(),
        }
    })
}

/// CLIP's pre-tokenizer, by hand: after lowercasing, a piece is a
/// contraction ('s 't 're 've 'm 'll 'd), a run of letters, one digit,
/// or a run of anything else that is not whitespace.
fn pieces(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '\'' {
            let mut took = false;
            for tail in ["'s", "'t", "'re", "'ve", "'m", "'ll", "'d"] {
                let t: Vec<char> = tail.chars().collect();
                if chars[i..].starts_with(&t) {
                    out.push(tail.to_string());
                    i += t.len();
                    took = true;
                    break;
                }
            }
            if took {
                continue;
            }
            // A bare apostrophe joins the punctuation run below.
        }
        if c.is_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_alphabetic() {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else if c.is_numeric() {
            out.push(c.to_string());
            i += 1;
        } else {
            let start = i;
            while i < chars.len()
                && !chars[i].is_whitespace()
                && !chars[i].is_alphabetic()
                && !chars[i].is_numeric()
            {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        }
    }
    out
}

/// Merge one pre-tokenized piece down to vocabulary tokens.
fn merge_piece(bpe: &Bpe, piece: &str) -> Vec<i64> {
    // Bytes to the merge alphabet, `</w>` closing the word.
    let mut parts: Vec<String> = piece
        .bytes()
        .map(|b| bpe.bytes[b as usize].to_string())
        .collect();
    if parts.is_empty() {
        return Vec::new();
    }
    let last = parts.len() - 1;
    parts[last].push_str("</w>");
    loop {
        let mut best: Option<(usize, usize)> = None; // (rank, index)
        for i in 0..parts.len().saturating_sub(1) {
            if let Some(&rank) = bpe.ranks.get(&(parts[i].clone(), parts[i + 1].clone())) {
                if best.map(|(r, _)| rank < r).unwrap_or(true) {
                    best = Some((rank, i));
                }
            }
        }
        let Some((_, i)) = best else { break };
        let merged = format!("{}{}", parts[i], parts[i + 1]);
        parts.splice(i..=i + 1, [merged]);
    }
    parts
        .iter()
        .filter_map(|p| bpe.vocab.get(p).copied())
        .collect()
}

/// A query as the padded id row the text tower takes.
pub fn encode(text: &str) -> Vec<i64> {
    let bpe = bpe();
    let mut ids = vec![BOS];
    for piece in pieces(text) {
        ids.extend(merge_piece(bpe, &piece));
        if ids.len() >= CONTEXT - 1 {
            ids.truncate(CONTEXT - 1);
            break;
        }
    }
    ids.push(EOS);
    ids.resize(CONTEXT, 0);
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unpadded ids for a string.
    fn enc(text: &str) -> Vec<i64> {
        let ids = encode(text);
        let end = ids
            .iter()
            .position(|&i| i == EOS)
            .expect("every encoding ends");
        ids[..=end].to_vec()
    }

    #[test]
    fn matches_the_reference_tokenizer() {
        // Ground truth from the model's own tokenizer.json run through
        // the HuggingFace tokenizers library.
        assert_eq!(
            enc("a photo of a cat"),
            [49406, 320, 1125, 539, 320, 2368, 49407]
        );
        assert_eq!(
            enc("Dog on the Beach!"),
            [49406, 1929, 525, 518, 2117, 256, 49407]
        );
        assert_eq!(
            enc("sunset over the sea"),
            [49406, 3424, 962, 518, 2102, 49407]
        );
        assert_eq!(
            enc("grandma's 2 birthday cakes"),
            [49406, 10525, 568, 273, 1166, 5950, 49407]
        );
        assert_eq!(enc("café au lait"), [49406, 15304, 2566, 572, 585, 49407]);
        assert_eq!(enc("HELLO WORLD"), [49406, 3306, 1002, 49407]);
        assert_eq!(
            enc("snow-capped mountains, 4k"),
            [49406, 2583, 268, 24659, 5873, 267, 275, 330, 49407]
        );
        assert_eq!(enc(""), [49406, 49407]);
        assert_eq!(enc("  "), [49406, 49407]);
    }

    #[test]
    fn the_row_is_padded_to_the_context_length() {
        let ids = encode("a photo of a cat");
        assert_eq!(ids.len(), CONTEXT);
        assert_eq!(ids[0], BOS);
        assert_eq!(ids[7..], vec![0; CONTEXT - 7][..]);
    }

    #[test]
    fn normalize_makes_unit_vectors() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
    }
}
