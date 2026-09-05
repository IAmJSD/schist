//! People: the faces the detector finds in each photo, and the names
//! given to them — Picasa's People album.
//!
//! Two kinds of record. *Detected* faces are what the models found:
//! a box per face and, when the recogniser is installed, a vector that
//! says who it looks like. They are derived — cached beside the
//! thumbnail and carried in the index snapshot — and never edited.
//! *Tagged* faces are the user's: a box in a photo with a person's
//! name on it, persisted in `library.json`. A tag usually sits on a
//! detected box, but a face the detector missed can be drawn by hand,
//! so the two are matched by overlap rather than by identity.
//!
//! Boxes are fractions of the photo, not pixels: the same face is the
//! same box in the 256 px thumbnail, the 1600 px viewer and the
//! original, and an edit that does not crop leaves every tag valid.

use std::path::{Path, PathBuf};

/// A face's box, as fractions of the photo's width and height.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FaceRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Two boxes overlapping by more than this (intersection over union)
/// are the same face: a tag drawn over a detection claims it.
pub const SAME_FACE_IOU: f32 = 0.3;

impl FaceRect {
    /// From pixel coordinates in a `width`x`height` image.
    pub fn from_pixels(x: f32, y: f32, w: f32, h: f32, width: f32, height: f32) -> FaceRect {
        FaceRect {
            x: x / width,
            y: y / height,
            w: w / width,
            h: h / height,
        }
        .clamped()
    }

    /// Kept inside the photo, with a positive size.
    pub fn clamped(self) -> FaceRect {
        let x0 = self.x.clamp(0.0, 1.0);
        let y0 = self.y.clamp(0.0, 1.0);
        let x1 = (self.x + self.w).clamp(0.0, 1.0);
        let y1 = (self.y + self.h).clamp(0.0, 1.0);
        FaceRect {
            x: x0.min(x1),
            y: y0.min(y1),
            w: (x1 - x0).abs(),
            h: (y1 - y0).abs(),
        }
    }

    /// Whether a point (in the same fractions) falls inside.
    pub fn contains(&self, fx: f32, fy: f32) -> bool {
        fx >= self.x && fy >= self.y && fx <= self.x + self.w && fy <= self.y + self.h
    }

    /// Intersection over union with another box, 0 when apart.
    pub fn overlap(&self, other: &FaceRect) -> f32 {
        let x = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let y = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);
        if x <= 0.0 || y <= 0.0 {
            return 0.0;
        }
        let inter = x * y;
        let union = self.w * self.h + other.w * other.h - inter;
        if union <= 0.0 {
            0.0
        } else {
            inter / union
        }
    }

    /// Whether this and `other` are one face, by overlap.
    pub fn same_face(&self, other: &FaceRect) -> bool {
        self.overlap(other) > SAME_FACE_IOU
    }

    /// The square the recogniser and the avatars crop: centred on the
    /// box, its longer side times `grow`, in a `width`x`height` image,
    /// as pixel `(x, y, side)` kept inside the image.
    pub fn crop_square(&self, grow: f32, width: u32, height: u32) -> (u32, u32, u32) {
        let (w, h) = (width as f32, height as f32);
        let side = (self.w * w).max(self.h * h) * grow;
        let side = side.min(w).min(h).max(1.0);
        let cx = (self.x + self.w / 2.0) * w;
        let cy = (self.y + self.h / 2.0) * h;
        let x0 = (cx - side / 2.0).round().clamp(0.0, w - side);
        let y0 = (cy - side / 2.0).round().clamp(0.0, h - side);
        (x0 as u32, y0 as u32, side.round().max(1.0) as u32)
    }

    /// A stable text key, for caches keyed by face: quantised to a
    /// thousandth, so the same box read back from JSON matches.
    pub fn key(&self) -> String {
        format!("{:.3},{:.3},{:.3},{:.3}", self.x, self.y, self.w, self.h)
    }
}

/// One face the detector found, with what the recogniser made of it —
/// `None` when the recogniser was not installed at the time, an empty
/// vector when it was and could not (so the photo is not asked again).
#[derive(Clone, Debug, PartialEq)]
pub struct DetectedFace {
    pub rect: FaceRect,
    pub embed: Option<Vec<f32>>,
}

impl DetectedFace {
    /// The recogniser's vector, when there is a real one.
    pub fn vector(&self) -> Option<&[f32]> {
        self.embed.as_deref().filter(|v| !v.is_empty())
    }

    /// Whether the recogniser has had its turn — a vector, or a
    /// recorded failure.
    pub fn embedded(&self) -> bool {
        self.embed.is_some()
    }
}

/// A face in a photo, as the user's tags refer to it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TaggedFace {
    pub photo: PathBuf,
    pub rect: FaceRect,
    /// Put here by the recogniser rather than by hand. Automatic tags
    /// count as the person everywhere but in their own mean vector —
    /// one wrong guess must not pull the next guess after it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
}

/// A face the user said is *not* this person: the recogniser will not
/// put it back. Kept by name, so the same face may still be offered
/// as somebody else.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeniedFace {
    pub photo: PathBuf,
    pub rect: FaceRect,
    pub name: String,
}

/// A named person and every face tagged as them, as `library.json`
/// holds it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersonFile {
    pub name: String,
    #[serde(default)]
    pub faces: Vec<TaggedFace>,
}

impl PersonFile {
    /// The photos this person appears in, each once, in tag order.
    pub fn photos(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for face in &self.faces {
            if !out.contains(&face.photo) {
                out.push(face.photo.clone());
            }
        }
        out
    }

    pub fn tagged(&self, photo: &Path, rect: &FaceRect) -> bool {
        self.faces
            .iter()
            .any(|f| f.photo == photo && f.rect.same_face(rect))
    }
}

/// The recogniser's grow factor: the detector's box is the face,
/// forehead to chin; the recogniser was trained on crops with a little
/// air round that.
pub const EMBED_GROW: f32 = 1.1;
/// The avatars' grow factor — a portrait, not a passport photo.
pub const AVATAR_GROW: f32 = 1.5;

/// The recogniser's cosine above which an unnamed face is put with the
/// person outright, as an automatic tag. Between [`SUGGEST_COSINE`] and
/// this it is only offered. On the test portraits the same person's
/// weakest pair scored 0.48 — that one becomes a question, the rest
/// are answered.
pub const AUTO_COSINE: f32 = 0.5;

/// The recogniser's cosine above which two faces are suggested to be
/// the same person. SFace's authors publish 0.363 for aligned crops;
/// ours are plain squares round the detector's box, which spreads the
/// scores (on the test portraits the same person scored 0.48–0.79,
/// different people up to 0.44), so the bar sits above the overlap.
/// The user confirms every suggestion, so a miss costs a click.
pub const SUGGEST_COSINE: f32 = 0.45;

/// The unit-length mean of some unit vectors — what a person's faces
/// add up to. `None` when there are none.
pub fn centroid<'a>(vectors: impl Iterator<Item = &'a [f32]>) -> Option<Vec<f32>> {
    let mut sum: Vec<f32> = Vec::new();
    let mut n = 0usize;
    for v in vectors {
        if sum.is_empty() {
            sum = vec![0.0; v.len()];
        }
        if sum.len() != v.len() {
            continue;
        }
        for (s, x) in sum.iter_mut().zip(v) {
            *s += x;
        }
        n += 1;
    }
    if n == 0 {
        return None;
    }
    let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut sum {
            *v /= norm;
        }
    }
    Some(sum)
}

/// The best-matching person for a face: the index and cosine of the
/// centroid nearest `embed`, when it clears [`SUGGEST_COSINE`].
pub fn best_match<'a>(
    embed: &[f32],
    centroids: impl Iterator<Item = (usize, &'a [f32])>,
) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (index, c) in centroids {
        if c.len() != embed.len() {
            continue;
        }
        let cos: f32 = c.iter().zip(embed).map(|(a, b)| a * b).sum();
        if cos >= SUGGEST_COSINE && best.is_none_or(|(_, b)| cos > b) {
            best = Some((index, cos));
        }
    }
    best
}

/// Whether a search query names a person: the query is the name, or
/// one of the name's words (a first name), or the name appears in the
/// query ("astrid at the beach"). Case-insensitive, whole words.
pub fn name_matches_query(name: &str, query: &str) -> bool {
    let name = name.trim().to_lowercase();
    let query = query.trim().to_lowercase();
    if name.is_empty() || query.is_empty() {
        return false;
    }
    if name == query {
        return true;
    }
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    };
    let name_words = words(&name);
    let query_words = words(&query);
    if query_words.is_empty() {
        return false;
    }
    // Every word of the query is a word of the name (one word: a first
    // name; two: the full name in either order).
    if query_words.iter().all(|q| name_words.contains(q)) {
        return true;
    }
    // Or the whole name sits inside the query, as words.
    name_words
        .windows(name_words.len())
        .next()
        .is_some_and(|_| {
            query_words
                .windows(name_words.len())
                .any(|w| w == name_words.as_slice())
        })
}

const FACES_MAGIC: &[u8; 8] = b"SCHFACE1";

/// The detected faces as bytes: magic, a count, then per face the box
/// and its embedding (a zero length when there is none). Little-endian
/// throughout, like the index snapshot.
pub fn encode_faces(faces: &[DetectedFace]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 2 + faces.len() * (16 + 2 + 512));
    out.extend_from_slice(FACES_MAGIC);
    out.extend_from_slice(&(faces.len().min(u16::MAX as usize) as u16).to_le_bytes());
    for face in faces.iter().take(u16::MAX as usize) {
        for v in [face.rect.x, face.rect.y, face.rect.w, face.rect.h] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let embed = face.embed.as_deref().unwrap_or(&[]);
        out.extend_from_slice(&(embed.len() as u16).to_le_bytes());
        for v in embed {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// The inverse of [`encode_faces`]; `None` for foreign or torn bytes.
pub fn decode_faces(bytes: &[u8]) -> Option<Vec<DetectedFace>> {
    let mut at = 0usize;
    let mut faces = Vec::new();
    decode_faces_at(bytes, &mut at, &mut faces, true)?;
    Some(faces)
}

/// Read faces from `bytes` at `*at`, advancing it: the magic when
/// `with_magic`, a count, then the faces. Shared with the index
/// snapshot, which embeds the same shape per row without the magic.
pub fn decode_faces_at(
    bytes: &[u8],
    at: &mut usize,
    out: &mut Vec<DetectedFace>,
    with_magic: bool,
) -> Option<()> {
    let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
        let slice = bytes.get(*at..*at + n)?;
        *at += n;
        Some(slice)
    };
    if with_magic && take(at, 8)? != FACES_MAGIC {
        return None;
    }
    let count = u16::from_le_bytes(take(at, 2)?.try_into().ok()?) as usize;
    let get_f32 =
        |at: &mut usize| -> Option<f32> { Some(f32::from_le_bytes(take(at, 4)?.try_into().ok()?)) };
    for _ in 0..count {
        let rect = FaceRect {
            x: get_f32(at)?,
            y: get_f32(at)?,
            w: get_f32(at)?,
            h: get_f32(at)?,
        };
        let dim = u16::from_le_bytes(take(at, 2)?.try_into().ok()?) as usize;
        let embed = if dim == 0 {
            None
        } else {
            let raw = take(at, dim * 4)?;
            Some(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c))
                    .collect::<Vec<f32>>(),
            )
        };
        out.push(DetectedFace { rect, embed });
    }
    Some(())
}

/// The bytes [`encode_faces`] puts after its magic, for the index row.
pub fn encode_faces_body(faces: &[DetectedFace]) -> Vec<u8> {
    encode_faces(faces)[8..].to_vec()
}

/// The cached detections beside a thumbnail (`.faces`).
pub fn read_faces_cache(cache: &Option<PathBuf>) -> Option<Vec<DetectedFace>> {
    let bytes = std::fs::read(cache.as_ref()?.with_extension("faces")).ok()?;
    decode_faces(&bytes)
}

pub fn write_faces_cache(cache: &Option<PathBuf>, faces: &[DetectedFace]) {
    if let Some(path) = cache {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path.with_extension("faces"), encode_faces(faces));
    }
}

/// Cut the square round a face out of an RGBA image and resample it to
/// `side` pixels: the recogniser's input and the avatars' pixels.
/// Returns straight RGBA, `side * side * 4` bytes.
pub fn face_crop_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    rect: &FaceRect,
    grow: f32,
    side: u32,
) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())?;
    let (x, y, edge) = rect.crop_square(grow, width, height);
    let edge = edge.min(width - x).min(height - y).max(1);
    let crop = image::imageops::crop_imm(&img, x, y, edge, edge).to_image();
    let filter = if edge > side {
        image::imageops::FilterType::Triangle
    } else {
        image::imageops::FilterType::CatmullRom
    };
    Some(image::imageops::resize(&crop, side, side, filter).into_raw())
}

/// The same crop as interleaved RGB in 0..=1, the recogniser's diet.
pub fn face_crop_rgb(
    rgba: &[u8],
    width: u32,
    height: u32,
    rect: &FaceRect,
    grow: f32,
    side: u32,
) -> Option<Vec<f32>> {
    let crop = face_crop_rgba(rgba, width, height, rect, grow, side)?;
    Some(
        crop.as_chunks::<4>()
            .0
            .iter()
            .flat_map(|px| {
                [
                    px[0] as f32 / 255.0,
                    px[1] as f32 / 255.0,
                    px[2] as f32 / 255.0,
                ]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> FaceRect {
        FaceRect { x, y, w, h }
    }

    #[test]
    fn boxes_overlap_by_intersection_over_union() {
        let a = rect(0.1, 0.1, 0.2, 0.2);
        assert!((a.overlap(&a) - 1.0).abs() < 1e-6);
        assert_eq!(a.overlap(&rect(0.5, 0.5, 0.1, 0.1)), 0.0);
        // Half of each: a third of the union.
        assert!((a.overlap(&rect(0.2, 0.1, 0.2, 0.2)) - 1.0 / 3.0).abs() < 1e-6);
        assert!(a.same_face(&rect(0.12, 0.1, 0.2, 0.2)));
        assert!(!a.same_face(&rect(0.25, 0.25, 0.2, 0.2)));
    }

    #[test]
    fn clamping_keeps_a_box_inside_the_photo() {
        let r = rect(-0.1, 0.9, 0.3, 0.3).clamped();
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(close(r.x, 0.0) && close(r.y, 0.9) && close(r.w, 0.2) && close(r.h, 0.1));
        // A box dragged up-and-left still has positive size.
        let r = rect(0.5, 0.5, -0.2, -0.1).clamped();
        assert!((r.x - 0.3).abs() < 1e-6 && (r.w - 0.2).abs() < 1e-6);
        assert!((r.y - 0.4).abs() < 1e-6 && (r.h - 0.1).abs() < 1e-6);
    }

    #[test]
    fn the_crop_square_stays_inside_the_image() {
        // A face at the right edge: the square slides left rather than
        // hanging off the picture.
        // 200 px wide, 100 tall: the square follows the longer side.
        let (x, y, side) = rect(0.8, 0.4, 0.2, 0.2).crop_square(1.5, 1000, 500);
        assert_eq!(side, 300);
        assert_eq!(x + side, 1000);
        assert_eq!(y, 100);
        // A face bigger than the short side: the square is the short side.
        let (_, _, side) = rect(0.0, 0.0, 1.0, 1.0).crop_square(1.1, 1000, 500);
        assert_eq!(side, 500);
    }

    #[test]
    fn faces_round_trip_with_and_without_embeddings() {
        let faces = vec![
            DetectedFace {
                rect: rect(0.1, 0.2, 0.3, 0.4),
                embed: Some(vec![0.5, -0.25, 1.0]),
            },
            DetectedFace {
                rect: rect(0.0, 0.0, 0.0, 0.0),
                embed: None,
            },
        ];
        assert_eq!(decode_faces(&encode_faces(&faces)).unwrap(), faces);
        assert_eq!(
            decode_faces(&encode_faces(&[])).unwrap(),
            Vec::<DetectedFace>::new()
        );
        assert!(decode_faces(b"SCHFACE1\x02\x00").is_none(), "torn");
        assert!(decode_faces(b"nonsense").is_none());
    }

    #[test]
    fn a_centroid_is_the_unit_mean_and_matches_clear_a_threshold() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        let c = centroid([a.as_slice(), b.as_slice()].into_iter()).unwrap();
        assert!(
            (c[0] - c[1]).abs() < 1e-6 && (c[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5
        );
        assert!(centroid(std::iter::empty()).is_none());
        let people = [c.clone(), vec![-1.0, 0.0]];
        let probe = [0.9f32, 0.436]; // near the first
        let found = best_match(
            &probe,
            people.iter().enumerate().map(|(i, v)| (i, v.as_slice())),
        );
        assert_eq!(found.map(|(i, _)| i), Some(0));
        // Straight away from everyone: nothing suggested.
        assert!(best_match(
            &[0.0, -1.0],
            people.iter().enumerate().map(|(i, v)| (i, v.as_slice()))
        )
        .is_none());
    }

    #[test]
    fn a_query_names_a_person_by_first_name_full_name_or_in_passing() {
        assert!(name_matches_query("Astrid Example", "astrid"));
        assert!(name_matches_query("Astrid Example", "Example Astrid"));
        assert!(name_matches_query(
            "Astrid Example",
            "astrid example at the beach"
        ));
        assert!(!name_matches_query("Astrid Example", "beach"));
        assert!(!name_matches_query("Astrid Example", "astridx"));
        assert!(!name_matches_query("Astrid Example", "astrid beach"));
        assert!(!name_matches_query("", "astrid"));
    }

    #[test]
    fn a_person_lists_each_photo_once() {
        let p = PersonFile {
            name: "A".into(),
            faces: vec![
                TaggedFace {
                    photo: "/a.jpg".into(),
                    rect: rect(0.1, 0.1, 0.1, 0.1),
                    auto: false,
                },
                TaggedFace {
                    photo: "/b.jpg".into(),
                    rect: rect(0.1, 0.1, 0.1, 0.1),
                    auto: true,
                },
                TaggedFace {
                    photo: "/a.jpg".into(),
                    rect: rect(0.5, 0.5, 0.1, 0.1),
                    auto: false,
                },
            ],
        };
        assert_eq!(
            p.photos(),
            vec![PathBuf::from("/a.jpg"), PathBuf::from("/b.jpg")]
        );
        assert!(p.tagged(Path::new("/a.jpg"), &rect(0.51, 0.5, 0.1, 0.1)));
        assert!(!p.tagged(Path::new("/c.jpg"), &rect(0.1, 0.1, 0.1, 0.1)));
    }

    #[test]
    fn a_face_crop_is_square_and_sized_as_asked() {
        let (w, h) = (40u32, 20u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        // A red block where the face is.
        for y in 5..15 {
            for x in 10..20 {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = 255;
                rgba[i + 3] = 255;
            }
        }
        let face = rect(10.0 / 40.0, 5.0 / 20.0, 10.0 / 40.0, 10.0 / 20.0);
        let crop = face_crop_rgba(&rgba, w, h, &face, 1.0, 8).unwrap();
        assert_eq!(crop.len(), 8 * 8 * 4);
        // The middle of the crop is the middle of the face: red.
        let mid = ((4 * 8 + 4) * 4) as usize;
        assert_eq!(crop[mid], 255);
        let rgb = face_crop_rgb(&rgba, w, h, &face, 1.0, 8).unwrap();
        assert_eq!(rgb.len(), 8 * 8 * 3);
        assert!((rgb[(4 * 8 + 4) * 3] - 1.0).abs() < 1e-6);
    }
}
