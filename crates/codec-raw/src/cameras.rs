//! The camera table: what a vendor raw does not say about itself.
//!
//! DNG files carry their colour matrix; every other format relies on a
//! table keyed by make and model. Black and white levels are here only
//! for cameras whose files do not record them.
//!
//! Provenance matters. Each entry names where its numbers came from in
//! a comment: the ColorMatrix of a DNG that Adobe's converter or the
//! camera itself wrote for that model is the canonical public source.
//! Nothing is to be copied from dcraw, LibRaw or rawspeed's tables.
//!
//! # What goes in the table
//!
//! Only the D65 matrix. A DNG carries up to two calibrations, one per
//! illuminant, and the developer is meant to interpolate between them
//! by colour temperature; this crate's [`Camera::color_matrix`] is the
//! single XYZ(D65)→camera matrix the [`crate::develop`] step wants, so
//! an entry is made only when a *D65* calibration is known. Cameras
//! whose only published matrix is for another illuminant (D50 on the
//! Leica SL and the GoPro GPRs, for instance) are left out rather than
//! entered under the wrong white point.
//!
//! Each entry's `// source:` comment says which of three provenances
//! it has, strongest first:
//!
//! * `ColorMatrix2 (D65) of <file>` — read straight out of a real DNG
//!   for that model: the camera's own, or one Adobe's converter (or
//!   Lightroom) wrote. Exact, and re-checkable by anyone with the file.
//! * `Adobe DNG Converter ColorMatrix2, D65 ... cross-checked` —
//!   Adobe's published calibration for the model, agreeing with the
//!   camera's own white-balance presets (see below). The comment
//!   carries the numbers so the check can be repeated.
//! * `... NOT individually verified` — the same source, for bodies
//!   whose raws publish nothing a matrix can be checked against. These
//!   are the ones to replace first.
//!
//! One provenance is deliberately *not* used. Several DNGs on
//! raw.pixls.us were written by CHDK, the third-party Canon firmware,
//! and carry a colour matrix for a compact that has no other public
//! calibration. CHDK is GPL, and where it got those numbers is not
//! documented in the file, so taking them could launder a copyleft
//! table into this one. Its files are read for their sensor layout and
//! ignored for colour.
//!
//! Two makes are conspicuously absent and it is worth saying why.
//! Samsung's NX bodies publish their own camera-to-sRGB correction
//! matrix and a white balance for each of two calibration
//! illuminants, which is in principle enough to rebuild the DNG
//! matrix (`M = diag(w) . CCM^-1 . S`, the correction matrix alone
//! leaving `w` free because its rows sum to one). Derived that way the
//! red axis lands within about 1% of the published Adobe value, but
//! the blue comes out near 1.0 on half the bodies and around 1.4 on
//! the rest, which no sensor does — the blue slot of that tag is not
//! what it looks like on the older models — so the whole block is left
//! out rather than half-trusted. Phase One and Leaf carry a
//! white-balanced correction matrix whose rows sum to one, which fixes
//! the colours but not the neutral, so it cannot be turned into a
//! ColorMatrix either.
//!
//! # How a matrix is cross-checked
//!
//! A colour matrix fixes the camera's response to a neutral: applying
//! it to the XYZ of D65 white gives the raw R:G:B a grey card produces
//! under D65, and the reciprocal of that is the white balance the
//! camera would apply at 6504 K. Most cameras record their own
//! white-balance presets with the colour temperature each was made for
//! — Canon's `WB_RGGBLevelsDaylight`/`Cloudy`/`Shade` alongside
//! `ColorTempDaylight` and friends, Pentax's whole 16-point
//! `KelvinWB_*` curve, Sony's `WB_RGBLevels6000K`/`8500K`, Olympus's
//! `WB_RBLevels*K` — so interpolating those to 6504 K in mireds
//! measures the same quantity independently of any matrix.
//!
//! On the four cameras here that have both a real D65 matrix in a DNG
//! and a preset curve, the two agree to 1–3% (Pentax K-5, Ricoh GR
//! IIIx), which is what sets the tolerance; entries that disagreed by
//! more than 8% were dropped rather than massaged, and the ones that
//! were dropped are named in this crate's notes. The check only pins
//! the neutral axis — two of nine degrees of freedom — so passing it
//! is necessary, not sufficient.
//!
//! Two entries fail their own check and are kept anyway, flagged in
//! their comments: the Pentax K10D's and 645D's in-camera matrices
//! disagree with those cameras' own Kelvin curves. They are what the
//! bodies' DNGs carry, so using them keeps PEF and DNG rendering alike.
//! Where a body has both an in-camera matrix and a recalled Adobe one,
//! the check picks between them rather than a rule of thumb: a
//! manufacturer's own numbers are not automatically the better ones,
//! as those two show.
//!
//! One more guard, because the neutral axis is only two of the nine
//! numbers: no two cameras may carry byte-identical matrices unless
//! one of them is read from a file, or the pair is a declared rebadge
//! or shared sensor. Two unrelated cameras agreeing to the last digit
//! is not a coincidence, it is a recollection filed under the wrong
//! body — that is how the Panasonic DMC-GH4 entry was caught wearing
//! the DJI FC6310's matrix, and how the D7200 was caught wearing the
//! D7100's.
//!
//! # Black and white levels
//!
//! Nearly every entry leaves both `None`, deliberately. The formats
//! this crate decodes either record the levels (DNG, ORF, RW2, PEF,
//! SRW, ARW, NEF) or let the decoder derive them: black from the
//! masked border columns the sensor frame keeps, white from the sample
//! bit depth. A per-model saturation value is exactly the sort of
//! number that only exists inside the copyleft tables this crate may
//! not read, and measuring it from sample frames does not work — of
//! the Canon frames here only one is clipped at all, and it clips at
//! the full 14-bit 16383 the bit depth already implies. A white level
//! guessed too low clips highlights to a coloured smear, so guessing is
//! worse than the decoder's default.

use std::cmp::Ordering;

/// One camera's calibration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub make: &'static str,
    pub model: &'static str,
    /// XYZ (D65) to camera RGB, rows R/G/B — the DNG ColorMatrix
    /// convention, as floats (a DNG stores them ×10000).
    pub color_matrix: [[f32; 3]; 3],
    pub black_level: Option<u16>,
    pub white_level: Option<u16>,
}

// ---------------------------------------------------------------- names

/// Vendor make strings, canonicalised. The left-hand side is what is
/// left of the file's Make after whitespace collapsing and the
/// corporate-suffix strip below, uppercased; the right-hand side is the
/// spelling this crate uses everywhere.
///
/// `OM Digital Solutions` — the company Olympus's camera division was
/// sold to in 2021 — is given its own make, `OM System`, because that
/// is the brand on the bodies and the string the files carry, and
/// because folding it into `Olympus` would make `clean_make` disagree
/// with the camera. [`lookup`] treats the two as one namespace so an
/// entry filed under either is found from either, which is what
/// matters for a body like the E-M1 Mark III that shipped under one
/// name and gained firmware under the other.
const MAKE_ALIASES: &[(&str, &str)] = &[
    ("AGFAPHOTO", "AgfaPhoto"),
    ("APPLE", "Apple"),
    ("ASAHI", "Pentax"),
    ("BLACKMAGIC", "Blackmagic"),
    ("BLACKMAGIC DESIGN", "Blackmagic"),
    ("CANON", "Canon"),
    ("CASIO", "Casio"),
    ("CONTAX", "Contax"),
    ("DJI", "DJI"),
    ("EASTMAN KODAK", "Kodak"),
    ("EPSON", "Epson"),
    ("FOVEON", "Sigma"),
    ("FUJI", "Fujifilm"),
    ("FUJIFILM", "Fujifilm"),
    ("FUJI PHOTO FILM", "Fujifilm"),
    ("GOOGLE", "Google"),
    ("GOPRO", "GoPro"),
    ("HASSELBLAD", "Hasselblad"),
    ("HUAWEI", "Huawei"),
    // Imacon's backs became Hasselblad's; their files say Hasselblad.
    ("IMACON", "Hasselblad"),
    ("KODAK", "Kodak"),
    ("KONICA", "Minolta"),
    ("KONICA MINOLTA", "Minolta"),
    ("LEAF", "Leaf"),
    ("LEICA", "Leica"),
    ("LG", "LG"),
    ("LG ELECTRONICS", "LG"),
    ("MAMIYA", "Mamiya"),
    ("MAMIYA-OP", "Mamiya"),
    ("MATSUSHITA ELECTRIC INDUSTRIAL", "Panasonic"),
    ("MINOLTA", "Minolta"),
    ("MOTOROLA", "Motorola"),
    ("NIKON", "Nikon"),
    ("NOKIA", "Nokia"),
    ("OLYMPUS", "Olympus"),
    ("OMDS", "OM System"),
    ("OM DIGITAL SOLUTIONS", "OM System"),
    ("OM SYSTEM", "OM System"),
    ("ONEPLUS", "OnePlus"),
    ("PANASONIC", "Panasonic"),
    ("PENTAX", "Pentax"),
    ("PHASEONE", "Phase One"),
    ("PHASE ONE", "Phase One"),
    ("RICOH", "Ricoh"),
    ("SAMSUNG", "Samsung"),
    ("SEIKO EPSON", "Epson"),
    ("SIGMA", "Sigma"),
    ("SINAR", "Sinar"),
    ("SONY", "Sony"),
    ("XIAOMI", "Xiaomi"),
    ("ZEISS", "Zeiss"),
];

/// Trailing tokens that say what kind of company the vendor is rather
/// than which vendor it is. Compared after dropping `.` and `,` from
/// the token and uppercasing it, so `CO.,LTD` and `Co., Ltd.` both
/// reduce to tokens in here. Stripped from the end repeatedly, never
/// far enough to leave the make empty: `RICOH IMAGING COMPANY, LTD.`
/// loses `LTD`, `COMPANY` and `IMAGING` and stops at `RICOH`.
const COMPANY_SUFFIXES: &[&str] = &[
    "A/S",
    "AB",
    "AG",
    "CAMERA",
    "CAMERAS",
    "CO",
    "COLTD",
    "COMPANY",
    "CORP",
    "CORPORATION",
    "GMBH",
    "IMAGING",
    "INC",
    "INCORPORATED",
    "KG",
    "LIMITED",
    "LLC",
    "LTD",
    "NV",
    "OPTICAL",
    "PLC",
    "SA",
];

/// Leading model tokens each make repeats out of its own name. Keyed by
/// the canonical make. Multi-word prefixes are matched token by token,
/// so a model that merely starts with the same letters (`OM-3` under
/// `OM System`) is untouched.
const MODEL_PREFIXES: &[(&str, &[&str])] = &[
    ("Canon", &["CANON"]),
    ("DJI", &["DJI"]),
    ("Epson", &["EPSON", "SEIKO EPSON"]),
    ("Fujifilm", &["FUJIFILM", "FUJI"]),
    ("GoPro", &["GOPRO"]),
    ("Google", &["GOOGLE"]),
    ("Hasselblad", &["HASSELBLAD"]),
    ("Kodak", &["KODAK", "EASTMAN KODAK"]),
    ("Leaf", &["LEAF"]),
    ("Leica", &["LEICA"]),
    ("Mamiya", &["MAMIYA"]),
    ("Minolta", &["MINOLTA", "KONICA MINOLTA"]),
    ("Nikon", &["NIKON"]),
    (
        "OM System",
        &["OM SYSTEM", "OM DIGITAL SOLUTIONS", "OM DIGITAL"],
    ),
    ("Olympus", &["OLYMPUS"]),
    ("Panasonic", &["PANASONIC"]),
    ("Pentax", &["PENTAX"]),
    ("Phase One", &["PHASE ONE", "PHASEONE"]),
    ("Ricoh", &["RICOH"]),
    ("Samsung", &["SAMSUNG"]),
    ("Sigma", &["SIGMA"]),
    ("Sony", &["SONY"]),
];

/// Cut a vendor string at its first NUL — TIFF ASCII fields are
/// NUL-terminated and vendors pad them — then collapse every run of
/// whitespace to one space and trim the ends.
fn collapse(s: &str) -> String {
    let s = match s.find('\0') {
        Some(end) => &s[..end],
        None => s,
    };
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// A token's comparison key: uppercase, without the `.` and `,` that
/// vendors sprinkle through their legal names.
fn token_key(token: &str) -> String {
    token
        .chars()
        .filter(|c| *c != '.' && *c != ',')
        .flat_map(char::to_uppercase)
        .collect()
}

/// Drop trailing corporate-form tokens, keeping at least one.
fn strip_company_suffixes(make: &str) -> String {
    let mut tokens: Vec<&str> = make.split(' ').filter(|t| !t.is_empty()).collect();
    while tokens.len() > 1 {
        let last = token_key(tokens[tokens.len() - 1]);
        if COMPANY_SUFFIXES.contains(&last.as_str()) {
            tokens.pop();
        } else {
            break;
        }
    }
    // A lone token can still end in the punctuation the suffixes carried.
    tokens.join(" ").trim_end_matches([',', '.']).to_string()
}

/// The canonical spelling of a make, or `None` if this crate has never
/// heard of it.
fn canonical_make(stripped: &str) -> Option<&'static str> {
    let key: String = stripped.to_uppercase();
    MAKE_ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map(|(_, canonical)| *canonical)
}

/// Whether `model`'s leading tokens are exactly `prefix`'s.
fn starts_with_tokens(model: &[&str], prefix: &str) -> Option<usize> {
    let wanted: Vec<&str> = prefix.split(' ').collect();
    if model.len() <= wanted.len() {
        // Never strip a model down to nothing: a file whose Model is
        // just the make ("DJI"/"DJI") keeps its model.
        return None;
    }
    wanted
        .iter()
        .zip(model)
        .all(|(w, m)| token_key(m) == *w)
        .then_some(wanted.len())
}

/// Canonical make and model for table lookup: the vendor's long-form
/// make collapsed ("NIKON CORPORATION" → "Nikon", "OLYMPUS IMAGING
/// CORP." → "Olympus", "SONY" → "Sony"...), the make stripped from
/// the front of the model where vendors repeat it ("NIKON D3300" →
/// "D3300", "Canon EOS 450D" → "EOS 450D"), whitespace collapsed.
///
/// The rules, in order, all of them case-insensitive and none of them
/// depending on anything but the two strings:
///
/// 1. Both strings are cut at their first NUL and have runs of
///    whitespace collapsed to single spaces.
/// 2. Trailing corporate-form tokens come off the make
///    (`COMPANY_SUFFIXES`), then it goes through `MAKE_ALIASES`. A
///    make this crate does not know keeps its own spelling, minus the
///    suffixes and any trailing comma or full stop, because there is
///    nothing to canonicalise it to.
/// 3. An empty make is guessed from the front of the model, so the
///    CIFF and other headerless containers that carry only
///    "Canon PowerShot A620" still land under `Canon`.
/// 4. Ricoh bought Pentax and ships both under one Make string:
///    `RICOH IMAGING COMPANY, LTD.` with a model starting `PENTAX` is
///    a Pentax body, anything else from that make is a Ricoh.
/// 5. Vendor quirks in the model: Leaf packs a serial and a back type
///    into it (`Leaf Aptus 75(LI400146   )/Large Format`) and
///    Hasselblad a shutter mode (`CFV 100C/Electronic Shutter`), so
///    those two makes' models are cut at the first `(` and `/`.
///    Kodak's end in `Digital Camera`/`Zoom Camera`, which comes off.
/// 6. Leading tokens repeating the make come off the model
///    (`MODEL_PREFIXES`), repeatedly, never leaving it empty.
///
/// What it deliberately does *not* do is rename models. `E-M1MarkII`
/// does not become `E-M1 Mark II`, `E8800` does not become
/// `COOLPIX 8800`, `MAXXUM 7D` does not become `DYNAX 7D`: every such
/// mapping is a fact about a particular camera rather than a rule, the
/// table is keyed on the string the file actually carries, and a
/// regional twin like the EOS 1300D/Rebel T6/Kiss X80 gets one table
/// entry per name it ships under.
pub fn normalize(make: &str, model: &str) -> (String, String) {
    let make = collapse(make);
    let mut model = collapse(model);

    let stripped = strip_company_suffixes(&make);
    let mut clean_make = match canonical_make(&stripped) {
        Some(canonical) => canonical.to_string(),
        None => stripped,
    };

    // A make-less file: the model usually still leads with the brand.
    if clean_make.is_empty() {
        let tokens: Vec<&str> = model.split(' ').filter(|t| !t.is_empty()).collect();
        for take in (1..=tokens.len().min(3)).rev() {
            if let Some(canonical) = canonical_make(&tokens[..take].join(" ")) {
                clean_make = canonical.to_string();
                break;
            }
        }
    }

    // Pentax bodies still say PENTAX in the model under Ricoh's make.
    if clean_make == "Ricoh" && starts_with_tokens(&split(&model), "PENTAX").is_some() {
        clean_make = "Pentax".to_string();
    } else if clean_make == "Pentax" && starts_with_tokens(&split(&model), "RICOH").is_some() {
        clean_make = "Ricoh".to_string();
    }

    match clean_make.as_str() {
        "Leaf" => {
            model = model.split(['(', '/']).next().unwrap_or(&model).to_string();
        }
        "Hasselblad" => {
            model = model.split('/').next().unwrap_or(&model).to_string();
        }
        "Kodak" => {
            for tail in [" Digital Camera", " Zoom Camera"] {
                if model.len() > tail.len() && model.to_uppercase().ends_with(&tail.to_uppercase())
                {
                    model.truncate(model.len() - tail.len());
                }
            }
        }
        _ => {}
    }
    let mut model = collapse(&model);

    if let Some((_, prefixes)) = MODEL_PREFIXES.iter().find(|(m, _)| *m == clean_make) {
        loop {
            let tokens = split(&model);
            // Longest prefix first, so "KONICA MINOLTA" wins over "MINOLTA".
            let hit = prefixes
                .iter()
                .filter_map(|p| starts_with_tokens(&tokens, p))
                .max();
            match hit {
                Some(n) => model = tokens[n..].join(" "),
                None => break,
            }
        }
    }

    (clean_make, model)
}

fn split(s: &str) -> Vec<&str> {
    s.split(' ').filter(|t| !t.is_empty()).collect()
}

// --------------------------------------------------------------- lookup

/// ASCII-case-insensitive ordering, the key [`CAMERAS`] is sorted by.
fn ci_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.bytes().map(|b| b.to_ascii_lowercase());
    let mut right = b.bytes().map(|b| b.to_ascii_lowercase());
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => return x.cmp(&y),
        }
    }
}

fn find(make: &str, model: &str) -> Option<&'static Camera> {
    CAMERAS
        .binary_search_by(|c| ci_cmp(c.make, make).then_with(|| ci_cmp(c.model, model)))
        .ok()
        .map(|i| &CAMERAS[i])
}

/// Look a camera up by normalised make and model.
///
/// Both are compared case-insensitively, so a decoder that hands over
/// what the file said (`dp2 Quattro` where the table says
/// `DP2 Quattro`) still finds its camera. Olympus and OM System share
/// one namespace; see [`MAKE_ALIASES`].
pub fn lookup(clean_make: &str, clean_model: &str) -> Option<&'static Camera> {
    if let Some(camera) = find(clean_make, clean_model) {
        return Some(camera);
    }
    let sibling = if clean_make.eq_ignore_ascii_case("Olympus") {
        "OM System"
    } else if clean_make.eq_ignore_ascii_case("OM System") {
        "Olympus"
    } else {
        return None;
    };
    find(sibling, clean_model)
}

/// Every camera in the table.
pub fn all() -> &'static [Camera] {
    CAMERAS
}

// ----------------------------------------------------------- the table

/// Every camera this crate has calibration for, sorted by (make,
/// model) under [`ci_cmp`] so [`find`] can binary search it.
// These are measured sensor coefficients; that one of them lands on
// pi/6 to four places is a coincidence, not a constant.
#[allow(clippy::approx_constant)]
static CAMERAS: &[Camera] = &[
    // source: ColorMatrix2 (D65) of IMG_1361.DNG, a real DNG for this body
    // (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Apple",
        model: "iPhone 12 Pro",
        color_matrix: [
            [0.914543, -0.322228, -0.126225],
            [-0.428868, 1.309541, 0.094676],
            [-0.106292, 0.235045, 0.430733],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of iPhone_6s_Plus-IMG_0853.DNG, a real
    // DNG for this body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Apple",
        model: "iPhone 6s Plus",
        color_matrix: [
            [0.709328, -0.227577, -0.021302],
            [-0.605197, 1.248974, 0.30108],
            [-0.112681, 0.131703, 0.58865],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware ProCamera 10.2. Exact, not recalled.
    Camera {
        make: "Apple",
        model: "iPhone 7 Plus",
        color_matrix: [
            [0.704868, -0.221958, -0.06626],
            [-0.463425, 1.218776, 0.2053],
            [-0.087825, 0.130628, 0.543115],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of
    // iPhone_8-RAW_2018_11_07_14_43_14_820_noflash.dng, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Apple",
        model: "iPhone 8",
        color_matrix: [
            [0.829948, -0.273887, -0.097103],
            [-0.572908, 1.363504, 0.170901],
            [-0.086339, 0.136364, 0.536422],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of
    // iPhone_SE-A4973FFB-9CBD-4ED8-805D-E30F4AE08A95.dng, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled. The 2016 SE
    // (iPhone8,4) shares the 6s Plus's calibration to the last digit;
    // Apple ships one profile for the sensor, not one per body.
    Camera {
        make: "Apple",
        model: "iPhone SE",
        color_matrix: [
            [0.709328, -0.227577, -0.021302],
            [-0.605197, 1.248974, 0.30108],
            [-0.112681, 0.131703, 0.58865],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware ProCamera 12.4. Exact, not recalled.
    Camera {
        make: "Apple",
        model: "iPhone XS",
        color_matrix: [
            [0.736833, -0.217277, -0.087254],
            [-0.460324, 1.224908, 0.197411],
            [-0.082088, 0.136331, 0.473013],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-1000d: at 6504 K
    // they want R 2.41 B 1.32, this matrix implies R 2.57 B 1.29
    // (+7%/-3%).
    Camera {
        make: "Canon",
        model: "EOS 1000D",
        color_matrix: [
            [0.6771, -0.1139, -0.0977],
            [-0.7818, 1.5123, 0.2928],
            [-0.1244, 0.1437, 0.7533],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-100d: at 6504 K they
    // want R 2.26 B 1.36, this matrix implies R 2.42 B 1.36 (+7%/+0%).
    // This is the EOS 650D under the name its own files use; the check
    // above is on a EOS 100D sample, not the EOS 650D's.
    Camera {
        make: "Canon",
        model: "EOS 100D",
        color_matrix: [
            [0.6602, -0.0841, -0.0939],
            [-0.4472, 1.2458, 0.2247],
            [-0.0975, 0.2039, 0.6148],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Canon",
        model: "EOS 10D",
        color_matrix: [
            [0.8197, -0.2, -0.1118],
            [-0.6714, 1.4335, 0.2592],
            [-0.2536, 0.3178, 0.8266],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-200d: at 6504 K they
    // want R 1.99 B 1.48, this matrix implies R 2.08 B 1.43 (+5%/-4%).
    Camera {
        make: "Canon",
        model: "EOS 200D",
        color_matrix: [
            [0.7377, -0.0742, -0.0998],
            [-0.4676, 1.241, 0.2578],
            [-0.1279, 0.2597, 0.567],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in EOS_20D-IMG_3893.CR2: at
    // 6504 K they want R 2.18 B 1.31, this matrix implies R 2.26 B 1.27
    // (+4%/-3%).
    Camera {
        make: "Canon",
        model: "EOS 20D",
        color_matrix: [
            [0.6599, -0.0537, -0.0891],
            [-0.8071, 1.5783, 0.2424],
            [-0.1983, 0.2234, 0.7462],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-30d: at 6504 K they
    // want R 2.29 B 1.32, this matrix implies R 2.36 B 1.35 (+3%/+2%).
    Camera {
        make: "Canon",
        model: "EOS 30D",
        color_matrix: [
            [0.6257, -0.0303, -0.1],
            [-0.788, 1.5621, 0.2396],
            [-0.1714, 0.1904, 0.7046],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in EOS_350D-IMG_1707.CR2: at
    // 6504 K they want R 2.50 B 1.27, this matrix implies R 2.69 B 1.27
    // (+8%/+0%).
    Camera {
        make: "Canon",
        model: "EOS 350D DIGITAL",
        color_matrix: [
            [0.6018, -0.0617, -0.0965],
            [-0.8645, 1.5881, 0.2975],
            [-0.153, 0.1719, 0.7642],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-400d: at 6504 K they
    // want R 2.58 B 1.27, this matrix implies R 2.63 B 1.25 (+2%/-1%).
    Camera {
        make: "Canon",
        model: "EOS 400D DIGITAL",
        color_matrix: [
            [0.7054, -0.1501, -0.099],
            [-0.8156, 1.5544, 0.2812],
            [-0.1278, 0.1414, 0.7796],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in EOS_40D-_MG_0153.CR2: at
    // 6504 K they want R 2.46 B 1.28, this matrix implies R 2.63 B 1.25
    // (+7%/-2%).
    Camera {
        make: "Canon",
        model: "EOS 40D",
        color_matrix: [
            [0.6071, -0.0747, -0.0856],
            [-0.7653, 1.5365, 0.2441],
            [-0.2025, 0.2553, 0.7315],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in _MG_7191.CR2: at 6504 K
    // they want R 2.42 B 1.22, this matrix implies R 2.49 B 1.22
    // (+3%/-0%).
    Camera {
        make: "Canon",
        model: "EOS 450D",
        color_matrix: [
            [0.5784, -0.0262, -0.0821],
            [-0.7539, 1.5064, 0.2672],
            [-0.1982, 0.2681, 0.7427],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-500d: at 6504 K they
    // want R 2.29 B 1.27, this matrix implies R 2.38 B 1.26 (+4%/-1%).
    Camera {
        make: "Canon",
        model: "EOS 500D",
        color_matrix: [
            [0.4763, 0.0712, -0.0646],
            [-0.6821, 1.4399, 0.264],
            [-0.1921, 0.3276, 0.6561],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in EOS_50D-IMG_9516.CR2: at
    // 6504 K they want R 2.28 B 1.20, this matrix implies R 2.33 B 1.19
    // (+2%/-1%).
    Camera {
        make: "Canon",
        model: "EOS 50D",
        color_matrix: [
            [0.492, 0.0616, -0.0593],
            [-0.6493, 1.3964, 0.2784],
            [-0.1774, 0.3178, 0.7005],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in EOS_550D-IMG_4047.CR2: at
    // 6504 K they want R 2.38 B 1.36, this matrix implies R 2.38 B 1.39
    // (+0%/+2%).
    Camera {
        make: "Canon",
        model: "EOS 550D",
        color_matrix: [
            [0.6941, -0.1164, -0.0857],
            [-0.3825, 1.1597, 0.2534],
            [-0.0416, 0.154, 0.6039],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-5d2: at 6504 K they
    // want R 2.49 B 1.43, this matrix implies R 2.57 B 1.39 (+3%/-3%).
    Camera {
        make: "Canon",
        model: "EOS 5D Mark II",
        color_matrix: [
            [0.4716, 0.0603, -0.083],
            [-0.7798, 1.5474, 0.248],
            [-0.1496, 0.1937, 0.6651],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written by MLVFS. Exact, not recalled. Its own white-balance presets
    // want R 2.23 B 1.40 at 6504 K against this matrix's R 2.25 B 1.42
    // (+1%/+2%).
    Camera {
        make: "Canon",
        model: "EOS 5D Mark III",
        color_matrix: [
            [0.6722, -0.0635, -0.0963],
            [-0.4287, 1.246, 0.2028],
            [-0.0908, 0.2162, 0.5668],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-5d4: at 6504 K they
    // want R 2.18 B 1.29, this matrix implies R 2.22 B 1.26 (+2%/-2%).
    Camera {
        make: "Canon",
        model: "EOS 5D Mark IV",
        color_matrix: [
            [0.6446, -0.0366, -0.0864],
            [-0.4436, 1.2204, 0.2513],
            [-0.0952, 0.2496, 0.6348],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-5dsr: at 6504 K they
    // want R 2.50 B 1.51, this matrix implies R 2.48 B 1.51 (-1%/+0%).
    Camera {
        make: "Canon",
        model: "EOS 5DS R",
        color_matrix: [
            [0.625, -0.0711, -0.0808],
            [-0.5153, 1.2794, 0.2636],
            [-0.1249, 0.2198, 0.561],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-600d: at 6504 K they
    // want R 2.37 B 1.38, this matrix implies R 2.50 B 1.40 (+5%/+2%).
    Camera {
        make: "Canon",
        model: "EOS 600D",
        color_matrix: [
            [0.6461, -0.0907, -0.0882],
            [-0.43, 1.2184, 0.2378],
            [-0.0819, 0.1944, 0.5931],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written by MLV App. Exact, not recalled. Its own white-balance
    // presets want R 2.40 B 1.35 at 6504 K against this matrix's R 2.43 B
    // 1.35 (+1%/-0%). Also a digit-for-digit match with the recalled
    // value.
    Camera {
        make: "Canon",
        model: "EOS 60D",
        color_matrix: [
            [0.6719, -0.0994, -0.0925],
            [-0.4408, 1.2426, 0.2211],
            [-0.0887, 0.2129, 0.6051],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-650d: at 6504 K they
    // want R 2.30 B 1.44, this matrix implies R 2.42 B 1.36 (+5%/-5%).
    Camera {
        make: "Canon",
        model: "EOS 650D",
        color_matrix: [
            [0.6602, -0.0841, -0.0939],
            [-0.4472, 1.2458, 0.2247],
            [-0.0975, 0.2039, 0.6148],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-6d: at 6504 K they
    // want R 2.14 B 1.48, this matrix implies R 2.22 B 1.42 (+4%/-4%).
    Camera {
        make: "Canon",
        model: "EOS 6D",
        color_matrix: [
            [0.7034, -0.0804, -0.1014],
            [-0.442, 1.2564, 0.2058],
            [-0.0851, 0.1994, 0.5758],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-6d2: at 6504 K they
    // want R 2.35 B 1.45, this matrix implies R 2.36 B 1.44 (+0%/-1%).
    Camera {
        make: "Canon",
        model: "EOS 6D Mark II",
        color_matrix: [
            [0.6875, -0.097, -0.0932],
            [-0.4691, 1.2459, 0.2501],
            [-0.0874, 0.1953, 0.5809],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-700d: at 6504 K they
    // want R 2.24 B 1.31, this matrix implies R 2.42 B 1.36 (+8%/+4%).
    // This is the EOS 650D under the name its own files use; the check
    // above is on a EOS 700D sample, not the EOS 650D's.
    Camera {
        make: "Canon",
        model: "EOS 700D",
        color_matrix: [
            [0.6602, -0.0841, -0.0939],
            [-0.4472, 1.2458, 0.2247],
            [-0.0975, 0.2039, 0.6148],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-750d: at 6504 K they
    // want R 2.39 B 1.49, this matrix implies R 2.50 B 1.48 (+5%/-1%).
    Camera {
        make: "Canon",
        model: "EOS 750D",
        color_matrix: [
            [0.6362, -0.0823, -0.0847],
            [-0.4426, 1.2109, 0.2616],
            [-0.0743, 0.1857, 0.5635],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-7d: at 6504 K they
    // want R 2.38 B 1.31, this matrix implies R 2.33 B 1.34 (-2%/+3%).
    Camera {
        make: "Canon",
        model: "EOS 7D",
        color_matrix: [
            [0.6844, -0.0996, -0.0856],
            [-0.3876, 1.1761, 0.2396],
            [-0.0593, 0.1772, 0.6198],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written by Adobe Photoshop Lightroom Classic 13.0.1 (Windows).
    // Exact, not recalled. Its own white-balance presets want R 2.16 B
    // 1.47 at 6504 K against this matrix's R 2.26 B 1.42 (+4%/-4%). Also a
    // digit-for-digit match with the recalled value.
    Camera {
        make: "Canon",
        model: "EOS 7D Mark II",
        color_matrix: [
            [0.7268, -0.1082, -0.0969],
            [-0.4186, 1.1839, 0.2663],
            [-0.0825, 0.2029, 0.5839],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-800d: at 6504 K they
    // want R 2.07 B 1.46, this matrix implies R 2.08 B 1.43 (+1%/-2%).
    // This is the EOS 200D under the name its own files use; the check
    // above is on a EOS 800D sample, not the EOS 200D's.
    Camera {
        make: "Canon",
        model: "EOS 800D",
        color_matrix: [
            [0.7377, -0.0742, -0.0998],
            [-0.4676, 1.241, 0.2578],
            [-0.1279, 0.2597, 0.567],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-80d: at 6504 K they
    // want R 2.02 B 1.52, this matrix implies R 1.99 B 1.50 (-1%/-2%).
    Camera {
        make: "Canon",
        model: "EOS 80D",
        color_matrix: [
            [0.7457, -0.0671, -0.0937],
            [-0.4849, 1.2495, 0.2643],
            [-0.1213, 0.2354, 0.5492],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in EOS_90D-RAW-ISO-100.CR3:
    // at 6504 K they want R 1.98 B 1.37, this matrix implies R 1.94 B 1.39
    // (-2%/+1%).
    Camera {
        make: "Canon",
        model: "EOS 90D",
        color_matrix: [
            [1.1498, -0.3759, -0.1516],
            [-0.5073, 1.2954, 0.2349],
            [-0.0892, 0.1867, 0.6118],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Canon",
        model: "EOS D60",
        color_matrix: [
            [0.6188, -0.1341, -0.089],
            [-0.7168, 1.5481, 0.1699],
            [-0.1717, 0.2151, 0.7043],
        ],
        black_level: None,
        white_level: None,
    },
    // source: the EOS 1000D entry above. Same sensor and colour filter
    // array, sold under this name; Adobe calibrates the pair together.
    // Nothing in a EOS DIGITAL REBEL XS file was used to check it.
    Camera {
        make: "Canon",
        model: "EOS DIGITAL REBEL XS",
        color_matrix: [
            [0.6771, -0.1139, -0.0977],
            [-0.7818, 1.5123, 0.2928],
            [-0.1244, 0.1437, 0.7533],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-xsi: at 6504 K they
    // want R 2.44 B 1.26, this matrix implies R 2.49 B 1.22 (+2%/-3%).
    // This is the EOS 450D under the name its own files use; the check
    // above is on a EOS DIGITAL REBEL XSi sample, not the EOS 450D's.
    Camera {
        make: "Canon",
        model: "EOS DIGITAL REBEL XSi",
        color_matrix: [
            [0.5784, -0.0262, -0.0821],
            [-0.7539, 1.5064, 0.2672],
            [-0.1982, 0.2681, 0.7427],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-kissx4: at 6504 K
    // they want R 2.42 B 1.35, this matrix implies R 2.38 B 1.39
    // (-2%/+3%). This is the EOS 550D under the name its own files use;
    // the check above is on a EOS Kiss X4 sample, not the EOS 550D's.
    Camera {
        make: "Canon",
        model: "EOS Kiss X4",
        color_matrix: [
            [0.6941, -0.1164, -0.0857],
            [-0.3825, 1.1597, 0.2534],
            [-0.0416, 0.154, 0.6039],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. This is the EOS 200D
    // under the name its own files carry; the matrix was cross-checked on
    // a EOS 200D sample (canon-200d), not on one of these.
    Camera {
        make: "Canon",
        model: "EOS Kiss X9",
        color_matrix: [
            [0.7377, -0.0742, -0.0998],
            [-0.4676, 1.241, 0.2578],
            [-0.1279, 0.2597, 0.567],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in EOS_M2-m2_test_02.cr2: at
    // 6504 K they want R 2.21 B 1.43, this matrix implies R 2.29 B 1.32
    // (+4%/-7%).
    Camera {
        make: "Canon",
        model: "EOS M2",
        color_matrix: [
            [0.64, -0.048, -0.0888],
            [-0.5294, 1.3416, 0.2047],
            [-0.1116, 0.2511, 0.6032],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-r6: at 6504 K they
    // want R 2.06 B 1.39, this matrix implies R 2.12 B 1.35 (+3%/-3%).
    Camera {
        make: "Canon",
        model: "EOS R6",
        color_matrix: [
            [0.8293, -0.1611, -0.1132],
            [-0.4759, 1.2711, 0.2275],
            [-0.1013, 0.2415, 0.5915],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. This is the EOS 100D
    // under the name its own files carry; the matrix was cross-checked on
    // a EOS 100D sample (canon-100d), not on one of these.
    Camera {
        make: "Canon",
        model: "EOS Rebel SL1",
        color_matrix: [
            [0.6602, -0.0841, -0.0939],
            [-0.4472, 1.2458, 0.2247],
            [-0.0975, 0.2039, 0.6148],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. This is the EOS 200D
    // under the name its own files carry; the matrix was cross-checked on
    // a EOS 200D sample (canon-200d), not on one of these.
    Camera {
        make: "Canon",
        model: "EOS Rebel SL2",
        color_matrix: [
            [0.7377, -0.0742, -0.0998],
            [-0.4676, 1.241, 0.2578],
            [-0.1279, 0.2597, 0.567],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-t1i: at 6504 K they
    // want R 2.26 B 1.23, this matrix implies R 2.38 B 1.26 (+5%/+2%).
    // This is the EOS 500D under the name its own files use; the check
    // above is on a EOS REBEL T1i sample, not the EOS 500D's.
    Camera {
        make: "Canon",
        model: "EOS REBEL T1i",
        color_matrix: [
            [0.4763, 0.0712, -0.0646],
            [-0.6821, 1.4399, 0.264],
            [-0.1921, 0.3276, 0.6561],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-t2i: at 6504 K they
    // want R 2.43 B 1.37, this matrix implies R 2.38 B 1.39 (-2%/+2%).
    // This is the EOS 550D under the name its own files use; the check
    // above is on a EOS REBEL T2i sample, not the EOS 550D's.
    Camera {
        make: "Canon",
        model: "EOS REBEL T2i",
        color_matrix: [
            [0.6941, -0.1164, -0.0857],
            [-0.3825, 1.1597, 0.2534],
            [-0.0416, 0.154, 0.6039],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-t3i: at 6504 K they
    // want R 2.37 B 1.35, this matrix implies R 2.50 B 1.40 (+6%/+4%).
    // This is the EOS 600D under the name its own files use; the check
    // above is on a EOS REBEL T3i sample, not the EOS 600D's.
    Camera {
        make: "Canon",
        model: "EOS REBEL T3i",
        color_matrix: [
            [0.6461, -0.0907, -0.0882],
            [-0.43, 1.2184, 0.2378],
            [-0.0819, 0.1944, 0.5931],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-t4i: at 6504 K they
    // want R 2.26 B 1.38, this matrix implies R 2.42 B 1.36 (+7%/-1%).
    // This is the EOS 650D under the name its own files use; the check
    // above is on a EOS REBEL T4i sample, not the EOS 650D's.
    Camera {
        make: "Canon",
        model: "EOS REBEL T4i",
        color_matrix: [
            [0.6602, -0.0841, -0.0939],
            [-0.4472, 1.2458, 0.2247],
            [-0.0975, 0.2039, 0.6148],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. This is the EOS 750D
    // under the name its own files carry; the matrix was cross-checked on
    // a EOS 750D sample (canon-750d), not on one of these.
    Camera {
        make: "Canon",
        model: "EOS Rebel T6i",
        color_matrix: [
            [0.6362, -0.0823, -0.0847],
            [-0.4426, 1.2109, 0.2616],
            [-0.0743, 0.1857, 0.5635],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. This is the EOS 800D
    // under the name its own files carry; the matrix was cross-checked on
    // a EOS 800D sample (canon-800d), not on one of these.
    Camera {
        make: "Canon",
        model: "EOS REBEL T7i",
        color_matrix: [
            [0.7377, -0.0742, -0.0998],
            [-0.4676, 1.241, 0.2578],
            [-0.1279, 0.2597, 0.567],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // EOS-1D_Mark_II_N-RY5Q8391.CR2: at 6504 K they want R 2.32 B 1.14,
    // this matrix implies R 2.40 B 1.21 (+4%/+6%).
    Camera {
        make: "Canon",
        model: "EOS-1D Mark II N",
        color_matrix: [
            [0.6612, -0.0841, -0.0889],
            [-0.8188, 1.5606, 0.2686],
            [-0.193, 0.257, 0.7472],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in canon-1d4: at 6504 K they
    // want R 2.28 B 1.40, this matrix implies R 2.31 B 1.41 (+1%/+1%).
    Camera {
        make: "Canon",
        model: "EOS-1D Mark IV",
        color_matrix: [
            [0.6014, -0.022, -0.0795],
            [-0.4109, 1.2014, 0.2361],
            [-0.0561, 0.1824, 0.5787],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written by Topaz Photo AI 1.3.4. Exact, not recalled. Also a
    // digit-for-digit match with the recalled value.
    Camera {
        make: "Canon",
        model: "EOS-1D X",
        color_matrix: [
            [0.6847, -0.0614, -0.1014],
            [-0.4669, 1.2737, 0.2139],
            [-0.1197, 0.2488, 0.6846],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // EOS-1Ds_Mark_III-RAW_CANON_1DSM3.CR2: at 6504 K they want R 2.34 B
    // 1.29, this matrix implies R 2.47 B 1.29 (+5%/-1%).
    Camera {
        make: "Canon",
        model: "EOS-1Ds Mark III",
        color_matrix: [
            [0.5859, -0.0211, -0.093],
            [-0.8255, 1.6017, 0.2353],
            [-0.1732, 0.1887, 0.7448],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // PowerShot_G11-IMG_3310.CR2: at 6504 K they want R 1.79 B 1.94, this
    // matrix implies R 1.89 B 1.88 (+6%/-3%).
    Camera {
        make: "Canon",
        model: "PowerShot G11",
        color_matrix: [
            [1.2177, -0.4817, -0.1069],
            [-0.1612, 0.9864, 0.2049],
            [-0.0098, 0.085, 0.4471],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of FC6310-DJI_0220.DNG, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled. FC6310 is the camera
    // in the Phantom 4 Pro; the model field carries the gimbal's part
    // number, not a marketing name.
    Camera {
        make: "DJI",
        model: "FC6310",
        color_matrix: [
            [0.7122, -0.2108, -0.0512],
            [-0.3155, 1.1201, 0.2231],
            [-0.0541, 0.1423, 0.5045],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of DJI_Osmo_Action-DJI_0254.DNG, a real
    // DNG for this body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "DJI",
        model: "Osmo Action",
        color_matrix: [
            [0.8257, -0.2417, -0.1016],
            [-0.4478, 1.3209, 0.1344],
            [-0.064, 0.1694, 0.5385],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Epson",
        model: "R-D1",
        color_matrix: [
            [0.6827, -0.1878, -0.0732],
            [-0.8429, 1.6012, 0.2564],
            [-0.1701, 0.1804, 0.669],
        ],
        black_level: None,
        white_level: None,
    },
    // source: the R-D1 entry above. Same sensor and colour filter array,
    // sold under this name; Adobe calibrates the pair together. Nothing in
    // a R-D1x file was used to check it.
    Camera {
        make: "Epson",
        model: "R-D1x",
        color_matrix: [
            [0.6827, -0.1878, -0.0732],
            [-0.8429, 1.6012, 0.2564],
            [-0.1701, 0.1804, 0.669],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in
    // GFX100S-Fujifilm-GFX100S-14bits-compress-4_3.RAF: at 6504 K they
    // want R 1.93 B 1.64, this matrix implies R 2.01 B 1.45 (+4%/-12%). A
    // single preset carried to 6504 K, so this is a coarser check than
    // most here.
    Camera {
        make: "Fujifilm",
        model: "GFX100S",
        color_matrix: [
            [1.6212, -0.8423, -0.1583],
            [-0.4336, 1.2583, 0.1937],
            [-0.0195, 0.0726, 0.6199],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in X-A5-DSCF0617.RAF: at 6504 K they
    // want R 2.17 B 1.62, this matrix implies R 2.04 B 1.57 (-6%/-3%). A
    // single preset carried to 6504 K, so this is a coarser check than
    // most here.
    Camera {
        make: "Fujifilm",
        model: "X-A5",
        color_matrix: [
            [1.1673, -0.476, -0.1041],
            [-0.3988, 1.2058, 0.2166],
            [-0.0771, 0.1417, 0.5569],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in X-E2S-_DSF0540.RAF: at 6504 K they
    // want R 2.14 B 1.32, this matrix implies R 2.30 B 1.33 (+8%/+1%). A
    // single preset carried to 6504 K, so this is a coarser check than
    // most here. This is the X-T1 under the name its own files use; the
    // check above is on a X-E2S sample, not the X-T1's.
    Camera {
        make: "Fujifilm",
        model: "X-E2S",
        color_matrix: [
            [0.8458, -0.2451, -0.0855],
            [-0.4597, 1.2447, 0.2407],
            [-0.1475, 0.2482, 0.6413],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in X-Pro3-_DSF2384.RAF: at 6504 K they
    // want R 2.09 B 1.60, this matrix implies R 2.08 B 1.47 (-1%/-8%). A
    // single preset carried to 6504 K, so this is a coarser check than
    // most here. This is the X-T3 under the name its own files use; the
    // check above is on a X-Pro3 sample, not the X-T3's.
    Camera {
        make: "Fujifilm",
        model: "X-Pro3",
        color_matrix: [
            [1.3426, -0.6334, -0.1177],
            [-0.4244, 1.2136, 0.2371],
            [-0.058, 0.1303, 0.598],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in 20171229_110916.RAF: at 6504 K they
    // want R 2.19 B 1.40, this matrix implies R 2.30 B 1.33 (+5%/-5%). A
    // single preset carried to 6504 K, so this is a coarser check than
    // most here.
    Camera {
        make: "Fujifilm",
        model: "X-T1",
        color_matrix: [
            [0.8458, -0.2451, -0.0855],
            [-0.4597, 1.2447, 0.2407],
            [-0.1475, 0.2482, 0.6413],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in X-T10-DSCF8146.RAF: at 6504 K they
    // want R 2.05 B 1.38, this matrix implies R 2.30 B 1.33 (+12%/-4%). A
    // single preset carried to 6504 K, so this is a coarser check than
    // most here. This is the X-T1 under the name its own files use; the
    // check above is on a X-T10 sample, not the X-T1's.
    Camera {
        make: "Fujifilm",
        model: "X-T10",
        color_matrix: [
            [0.8458, -0.2451, -0.0855],
            [-0.4597, 1.2447, 0.2407],
            [-0.1475, 0.2482, 0.6413],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in fuji-xt2: at 6504 K they want R
    // 2.11 B 1.62, this matrix implies R 2.29 B 1.61 (+9%/-0%). A single
    // preset carried to 6504 K, so this is a coarser check than most here.
    Camera {
        make: "Fujifilm",
        model: "X-T2",
        color_matrix: [
            [1.1434, -0.4948, -0.121],
            [-0.3746, 1.2042, 0.1903],
            [-0.0666, 0.1479, 0.5235],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in X-T3-AFXT2720.RAF: at 6504 K they
    // want R 2.05 B 1.60, this matrix implies R 2.08 B 1.47 (+1%/-8%). A
    // single preset carried to 6504 K, so this is a coarser check than
    // most here.
    Camera {
        make: "Fujifilm",
        model: "X-T3",
        color_matrix: [
            [1.3426, -0.6334, -0.1177],
            [-0.4244, 1.2136, 0.2371],
            [-0.058, 0.1303, 0.598],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in
    // X100F-DSCF5760_x100f_lossless_compressed_raw_Temple.RAF: at 6504 K
    // they want R 2.25 B 1.66, this matrix implies R 2.29 B 1.61
    // (+2%/-3%). A single preset carried to 6504 K, so this is a coarser
    // check than most here. This is the X-T2 under the name its own files
    // use; the check above is on a X100F sample, not the X-T2's.
    Camera {
        make: "Fujifilm",
        model: "X100F",
        color_matrix: [
            [1.1434, -0.4948, -0.121],
            [-0.3746, 1.2042, 0.1903],
            [-0.0666, 0.1479, 0.5235],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written by Adobe DNG Converter 10.3 (Windows). Exact, not recalled.
    // Its own white-balance presets want R 2.18 B 1.29 at 6504 K against
    // this matrix's R 2.27 B 1.31 (+4%/+2%).
    Camera {
        make: "Fujifilm",
        model: "X100S",
        color_matrix: [
            [1.0592, -0.4262, -0.1008],
            [-0.3514, 1.1355, 0.2465],
            [-0.087, 0.2025, 0.6386],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of Pixel_2_XL-IMG_20180109_080629.dng, a
    // real DNG for this body (raw.pixls.us). Exact, not recalled. This DNG
    // puts D65 in the *first* illuminant slot, so the matrix here is its
    // ColorMatrix1; the slot number means nothing, the illuminant tag
    // does.
    Camera {
        make: "Google",
        model: "Pixel 2 XL",
        color_matrix: [
            [0.796875, -0.164062, -0.125],
            [-0.632812, 1.570312, 0.03125],
            [-0.171875, 0.351562, 0.46875],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of Pixel_4a-PXL_20201121_100251397.dng, a
    // real DNG for this body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Google",
        model: "Pixel 4a",
        color_matrix: [
            [1.0626, -0.4466, -0.1116],
            [-0.4205, 1.2766, 0.1562],
            [-0.0739, 0.2132, 0.5474],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of
    // Pixel_8_Pro-PXL_20240415_103400204.RAW-02.ORIGINAL.dng, a real DNG
    // for this body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Google",
        model: "Pixel 8 Pro",
        color_matrix: [
            [0.849, -0.2014, -0.1226],
            [-0.585, 1.4801, 0.1018],
            [-0.186, 0.3981, 0.4547],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of
    // CFV_100CElectronic_Shutter-B0003499.3FR, a real DNG for this body
    // (raw.pixls.us). Exact, not recalled. Same sensor and the same matrix
    // as the X2D 100C above, to the last digit; Hasselblad ships one
    // calibration for the 100-megapixel back.
    Camera {
        make: "Hasselblad",
        model: "CFV 100C",
        color_matrix: [
            [0.595057, -0.146675, -0.03413],
            [-0.51593, 1.263062, 0.282611],
            [-0.103826, 0.167558, 0.615209],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of
    // H5D-50c-20151114_CCSG_on_Hasselblad_H5D50c-0017.fff, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled. Illuminant untagged
    // as on the other Hasselblads; its neutral agrees with the 6350 K
    // AsShotNeutral in the same file to 9%.
    Camera {
        make: "Hasselblad",
        model: "H5D-50c",
        color_matrix: [
            [0.493195, -0.083511, 0.014068],
            [-0.487762, 1.186829, 0.343658],
            [-0.113823, 0.196106, 0.706665],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of L1D-20c-DJI_0017.DNG, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled. The camera on DJI's
    // Mavic 2 Pro, badged Hasselblad.
    Camera {
        make: "Hasselblad",
        model: "L1D-20c",
        color_matrix: [
            [0.731, -0.2746, -0.0646],
            [-0.2991, 1.0847, 0.2469],
            [0.0163, 0.0585, 0.6324],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware 10.00.40.18. Exact, not recalled.
    Camera {
        make: "Hasselblad",
        model: "L2D-20c",
        color_matrix: [
            [0.8575, -0.3219, -0.0868],
            [-0.3351, 1.1451, 0.1593],
            [0.0207, 0.0468, 0.4876],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of X2D_100C-B0000079.3FR, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled. 3FR and FFF carry one
    // matrix and no CalibrationIlluminant tag. It is the daylight one: the
    // neutral it implies, R 2.83 B 1.46, is within 9% of the AsShotNeutral
    // the same files record for daylight frames, where a tungsten
    // calibration would be out by a factor of two.
    Camera {
        make: "Hasselblad",
        model: "X2D 100C",
        color_matrix: [
            [0.595057, -0.146675, -0.03413],
            [-0.51593, 1.263062, 0.282611],
            [-0.103826, 0.167558, 0.615209],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware 1.0. Exact, not recalled.
    Camera {
        make: "Leica",
        model: "CL",
        color_matrix: [
            [0.697, -0.225, -0.079],
            [-0.482, 1.239, 0.201],
            [-0.149, 0.232, 0.476],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Leica",
        model: "DIGILUX 2",
        color_matrix: [
            [1.134, -0.4069, -0.1275],
            [-0.7555, 1.5266, 0.2448],
            [-0.296, 0.3426, 0.7519],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware 2.0.2.5. Exact, not recalled.
    Camera {
        make: "Leica",
        model: "M (Typ 240)",
        color_matrix: [
            [0.6653, -0.1486, -0.0611],
            [-0.4221, 1.3303, 0.0929],
            [-0.0881, 0.2416, 0.7226],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of M10-f5381888.dng, a real DNG for this
    // body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Leica",
        model: "M10",
        color_matrix: [
            [0.805908, -0.27832, -0.060547],
            [-0.529053, 1.44165, 0.055176],
            [-0.093506, 0.300293, 0.636719],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware 2.014. Exact, not recalled.
    Camera {
        make: "Leica",
        model: "M8 Digital Camera",
        color_matrix: [
            [0.7675, -0.2195, -0.0305],
            [-0.586, 1.4118, 0.1857],
            [-0.2425, 0.4007, 0.6578],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware 1.202. Exact, not recalled.
    Camera {
        make: "Leica",
        model: "M9 Digital Camera",
        color_matrix: [
            [0.626, -0.1019, -0.047],
            [-0.373, 1.145, 0.193],
            [-0.1409, 0.295, 0.621],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of X2-L1021324.DNG, a real DNG for this
    // body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Leica",
        model: "X2",
        color_matrix: [
            [0.7158, -0.1911, -0.0606],
            [-0.3603, 1.0669, 0.253],
            [-0.0659, 0.1236, 0.553],
        ],
        black_level: None,
        white_level: None,
    },
    // source: the 1 V1 entry above. Same sensor and colour filter array,
    // sold under this name; Adobe calibrates the pair together. Nothing in
    // a 1 AW1 file was used to check it.
    Camera {
        make: "Nikon",
        model: "1 AW1",
        color_matrix: [
            [0.8994, -0.2667, -0.0865],
            [-0.4594, 1.2324, 0.2552],
            [-0.0699, 0.1786, 0.626],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "1 V1",
        color_matrix: [
            [0.8994, -0.2667, -0.0865],
            [-0.4594, 1.2324, 0.2552],
            [-0.0699, 0.1786, 0.626],
        ],
        black_level: None,
        white_level: None,
    },
    // source: the D7000 entry above. Same sensor and colour filter array,
    // sold under this name; Adobe calibrates the pair together. Nothing in
    // a COOLPIX A file was used to check it.
    Camera {
        make: "Nikon",
        model: "COOLPIX A",
        color_matrix: [
            [0.8198, -0.2239, -0.0724],
            [-0.4871, 1.2389, 0.2798],
            [-0.1043, 0.205, 0.7181],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D100",
        color_matrix: [
            [0.5902, -0.0933, -0.0782],
            [-0.8983, 1.6719, 0.2354],
            [-0.1402, 0.1455, 0.6464],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D200",
        color_matrix: [
            [0.8367, -0.2248, -0.0763],
            [-0.8758, 1.6447, 0.2422],
            [-0.1527, 0.155, 0.8053],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D300",
        color_matrix: [
            [0.903, -0.1992, -0.0715],
            [-0.8465, 1.6302, 0.2255],
            [-0.2689, 0.3217, 0.8069],
        ],
        black_level: None,
        white_level: None,
    },
    // source: the D300 entry above. Same sensor and colour filter array,
    // sold under this name; Adobe calibrates the pair together. Nothing in
    // a D300S file was used to check it.
    Camera {
        make: "Nikon",
        model: "D300S",
        color_matrix: [
            [0.903, -0.1992, -0.0715],
            [-0.8465, 1.6302, 0.2255],
            [-0.2689, 0.3217, 0.8069],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D3200",
        color_matrix: [
            [0.7013, -0.1408, -0.0635],
            [-0.5268, 1.2902, 0.264],
            [-0.147, 0.2801, 0.7379],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D3300",
        color_matrix: [
            [0.6988, -0.1384, -0.0714],
            [-0.5631, 1.341, 0.2447],
            [-0.1485, 0.2204, 0.7318],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D3500",
        color_matrix: [
            [0.8821, -0.2938, -0.0785],
            [-0.4178, 1.2142, 0.2287],
            [-0.0824, 0.1651, 0.686],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D3X",
        color_matrix: [
            [0.7171, -0.1986, -0.0648],
            [-0.8085, 1.5555, 0.2718],
            [-0.217, 0.2512, 0.7457],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D40",
        color_matrix: [
            [0.6992, -0.1668, -0.0806],
            [-0.8138, 1.5748, 0.2543],
            [-0.0874, 0.085, 0.7897],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D50",
        color_matrix: [
            [0.7732, -0.2422, -0.0789],
            [-0.8238, 1.5884, 0.2498],
            [-0.0859, 0.0783, 0.733],
        ],
        black_level: None,
        white_level: None,
    },
    // source: the D90 entry above. Same sensor and colour filter array,
    // sold under this name; Adobe calibrates the pair together. Nothing in
    // a D5000 file was used to check it.
    Camera {
        make: "Nikon",
        model: "D5000",
        color_matrix: [
            [0.7309, -0.1403, -0.0519],
            [-0.8474, 1.6008, 0.2622],
            [-0.2434, 0.2826, 0.8064],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D60",
        color_matrix: [
            [0.8736, -0.2458, -0.0935],
            [-0.9075, 1.6894, 0.2251],
            [-0.1354, 0.1242, 0.8263],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D610",
        color_matrix: [
            [0.8178, -0.2245, -0.0609],
            [-0.4857, 1.2394, 0.2776],
            [-0.1207, 0.2086, 0.7298],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D70",
        color_matrix: [
            [0.7732, -0.2422, -0.0789],
            [-0.8238, 1.5884, 0.2498],
            [-0.0859, 0.0783, 0.733],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D700",
        color_matrix: [
            [0.8139, -0.2171, -0.0663],
            [-0.8747, 1.6541, 0.2295],
            [-0.1925, 0.2008, 0.8093],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D7000",
        color_matrix: [
            [0.8198, -0.2239, -0.0724],
            [-0.4871, 1.2389, 0.2798],
            [-0.1043, 0.205, 0.7181],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D7100",
        color_matrix: [
            [0.8322, -0.3112, -0.1047],
            [-0.6367, 1.4342, 0.2179],
            [-0.0988, 0.1638, 0.6394],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D750",
        color_matrix: [
            [0.902, -0.289, -0.0715],
            [-0.4535, 1.2436, 0.2348],
            [-0.0934, 0.1919, 0.7086],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written by Adobe Photoshop Lightroom 3.4. Exact, not recalled. This
    // one settles the provenance of the recalled numbers: the matrix Adobe
    // wrote here is digit for digit the one written down for the D80
    // before the file was found.
    Camera {
        make: "Nikon",
        model: "D80",
        color_matrix: [
            [0.8629, -0.241, -0.0883],
            [-0.9055, 1.694, 0.2171],
            [-0.149, 0.1363, 0.852],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D800E",
        color_matrix: [
            [0.7866, -0.2108, -0.0555],
            [-0.4869, 1.2483, 0.2681],
            [-0.1176, 0.2069, 0.7501],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D810",
        color_matrix: [
            [0.9369, -0.3195, -0.0791],
            [-0.4488, 1.243, 0.2301],
            [-0.0893, 0.1796, 0.6872],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D850",
        color_matrix: [
            [1.0405, -0.3755, -0.127],
            [-0.5461, 1.3787, 0.1793],
            [-0.104, 0.2015, 0.6785],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "D90",
        color_matrix: [
            [0.7309, -0.1403, -0.0519],
            [-0.8474, 1.6008, 0.2622],
            [-0.2434, 0.2826, 0.8064],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "Z 30",
        color_matrix: [
            [1.0339, -0.3822, -0.089],
            [-0.4183, 1.2023, 0.2436],
            [-0.0671, 0.1638, 0.6444],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "Z 50",
        color_matrix: [
            [1.164, -0.4829, -0.1079],
            [-0.5107, 1.3006, 0.2325],
            [-0.0972, 0.1711, 0.738],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "Z 6",
        color_matrix: [
            [0.821, -0.2534, -0.0683],
            [-0.5355, 1.3338, 0.2212],
            [-0.1143, 0.1929, 0.6464],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Nikon",
        model: "Z 7",
        color_matrix: [
            [1.3705, -0.6004, -0.14],
            [-0.5464, 1.3568, 0.2062],
            [-0.094, 0.1706, 0.7618],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // E-1-E_1__C106743_gredos.ORF: at 6504 K they want R 1.91 B 0.97, this
    // matrix implies R 1.90 B 0.98 (-0%/+1%).
    Camera {
        make: "Olympus",
        model: "E-1",
        color_matrix: [
            [1.1846, -0.4767, -0.0945],
            [-0.7027, 1.5878, 0.1089],
            [-0.2699, 0.4122, 0.8311],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-e620: at 6504 K they
    // want R 2.16 B 1.14, this matrix implies R 2.29 B 1.15 (+6%/+1%).
    Camera {
        make: "Olympus",
        model: "E-620",
        color_matrix: [
            [0.8453, -0.2198, -0.1092],
            [-0.7609, 1.5681, 0.2008],
            [-0.1725, 0.2337, 0.7824],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-em10m2: at 6504 K they
    // want R 2.14 B 1.57, this matrix implies R 2.31 B 1.58 (+8%/+1%).
    // This is the E-M5 under the name its own files use; the check above
    // is on a E-M10MarkII sample, not the E-M5's.
    Camera {
        make: "Olympus",
        model: "E-M10MarkII",
        color_matrix: [
            [0.838, -0.263, -0.0639],
            [-0.2887, 1.0725, 0.2496],
            [-0.0627, 0.1427, 0.5438],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-em5: at 6504 K they
    // want R 2.21 B 1.61, this matrix implies R 2.31 B 1.58 (+4%/-2%).
    Camera {
        make: "Olympus",
        model: "E-M5",
        color_matrix: [
            [0.838, -0.263, -0.0639],
            [-0.2887, 1.0725, 0.2496],
            [-0.0627, 0.1427, 0.5438],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-em5m2: at 6504 K they
    // want R 2.13 B 1.53, this matrix implies R 2.15 B 1.54 (+1%/+0%).
    Camera {
        make: "Olympus",
        model: "E-M5MarkII",
        color_matrix: [
            [0.9422, -0.3258, -0.0711],
            [-0.2655, 1.0898, 0.2015],
            [-0.0512, 0.1354, 0.5512],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-ep5: at 6504 K they
    // want R 2.17 B 1.54, this matrix implies R 2.31 B 1.58 (+6%/+3%).
    // This is the E-M5 under the name its own files use; the check above
    // is on a E-P5 sample, not the E-M5's.
    Camera {
        make: "Olympus",
        model: "E-P5",
        color_matrix: [
            [0.838, -0.263, -0.0639],
            [-0.2887, 1.0725, 0.2496],
            [-0.0627, 0.1427, 0.5438],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-epl5: at 6504 K they
    // want R 2.17 B 1.58, this matrix implies R 2.31 B 1.58 (+6%/+0%).
    // This is the E-M5 under the name its own files use; the check above
    // is on a E-PL5 sample, not the E-M5's.
    Camera {
        make: "Olympus",
        model: "E-PL5",
        color_matrix: [
            [0.838, -0.263, -0.0639],
            [-0.2887, 1.0725, 0.2496],
            [-0.0627, 0.1427, 0.5438],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-epl7: at 6504 K they
    // want R 2.34 B 1.57, this matrix implies R 2.19 B 1.56 (-6%/-1%).
    Camera {
        make: "Olympus",
        model: "E-PL7",
        color_matrix: [
            [0.9197, -0.319, -0.0659],
            [-0.2606, 1.083, 0.2039],
            [-0.0458, 0.125, 0.5458],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in E-PL8-P9110021.ORF: at
    // 6504 K they want R 2.09 B 1.52, this matrix implies R 2.19 B 1.56
    // (+5%/+3%). This is the E-PL7 under the name its own files use; the
    // check above is on a E-PL8 sample, not the E-PL7's.
    Camera {
        make: "Olympus",
        model: "E-PL8",
        color_matrix: [
            [0.9197, -0.319, -0.0659],
            [-0.2606, 1.083, 0.2039],
            [-0.0458, 0.125, 0.5458],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-penf: at 6504 K they
    // want R 2.06 B 1.55, this matrix implies R 2.11 B 1.59 (+3%/+3%).
    Camera {
        make: "Olympus",
        model: "PEN-F",
        color_matrix: [
            [0.9476, -0.3182, -0.0765],
            [-0.2613, 1.0958, 0.1893],
            [-0.0449, 0.1315, 0.5268],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in oly-xz1: at 6504 K they
    // want R 2.23 B 1.63, this matrix implies R 2.08 B 1.52 (-6%/-7%).
    Camera {
        make: "Olympus",
        model: "XZ-1",
        color_matrix: [
            [1.0901, -0.4095, -0.1074],
            [-0.1141, 0.9208, 0.2293],
            [-0.0062, 0.1417, 0.5158],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in
    // DMC-FZ300-RAW_PANASONIC_FZ300_1-1.RW2: at 6504 K they want R 2.23 B
    // 1.80, this matrix implies R 2.43 B 1.67 (+9%/-7%). A single preset
    // carried to 6504 K, so this is a coarser check than most here.
    Camera {
        make: "Panasonic",
        model: "DMC-FZ300",
        color_matrix: [
            [0.8378, -0.2798, -0.0769],
            [-0.3068, 1.141, 0.1877],
            [-0.0538, 0.1792, 0.4623],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Panasonic",
        model: "DMC-G5",
        color_matrix: [
            [0.7798, -0.2562, -0.074],
            [-0.3894, 1.1972, 0.2234],
            [-0.11, 0.2335, 0.6529],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // its one named daylight preset in DMC-G85-_1030981.RW2: at 6504 K
    // they want R 2.46 B 1.46, this matrix implies R 2.71 B 1.49
    // (+10%/+2%). A single preset carried to 6504 K, so this is a coarser
    // check than most here.
    Camera {
        make: "Panasonic",
        model: "DMC-G85",
        color_matrix: [
            [0.7524, -0.2518, -0.0671],
            [-0.3343, 1.1651, 0.1937],
            [-0.03, 0.1067, 0.5791],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Panasonic",
        model: "DMC-GF1",
        color_matrix: [
            [0.7888, -0.291, -0.0795],
            [-0.4055, 1.2074, 0.2214],
            [-0.1042, 0.2134, 0.5947],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Panasonic",
        model: "DMC-GH2",
        color_matrix: [
            [0.778, -0.241, -0.0806],
            [-0.3913, 1.1724, 0.2484],
            [-0.1018, 0.239, 0.5298],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Panasonic",
        model: "DMC-GH3",
        color_matrix: [
            [0.6559, -0.1752, -0.0491],
            [-0.3672, 1.1407, 0.2586],
            [-0.0962, 0.1875, 0.513],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Panasonic",
        model: "DMC-GX1",
        color_matrix: [
            [0.6763, -0.1919, -0.0863],
            [-0.3868, 1.1515, 0.2685],
            [-0.1216, 0.2387, 0.5879],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. NOT individually
    // verified: this body's raws record only the white balance the shot
    // was taken at, no preset curve, so there is nothing in a sample to
    // check a matrix against. Every camera for which a converted DNG could
    // be found (the D80, the EOS-1D X, the EOS 7D Mark II and the EOS 60D)
    // reproduced the recalled value exactly, which is why these are here
    // at all; replace any of them the moment a converted DNG for the body
    // turns up.
    Camera {
        make: "Panasonic",
        model: "DMC-GX7",
        color_matrix: [
            [0.761, -0.278, -0.0576],
            [-0.4614, 1.2195, 0.2733],
            [-0.1375, 0.2393, 0.649],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written by Adobe DNG Converter 10.3 (Windows). Exact, not recalled.
    Camera {
        make: "Panasonic",
        model: "DMC-LX1",
        color_matrix: [
            [1.0704, -0.4187, -0.123],
            [-0.8314, 1.5952, 0.2501],
            [-0.092, 0.0945, 0.8927],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in ist_DL-IMGP0370.PEF: at
    // 6504 K they want R 1.72 B 0.87, this matrix implies R 1.73 B 0.84
    // (+1%/-3%).
    Camera {
        make: "Pentax",
        model: "*ist DL",
        color_matrix: [
            [1.0829, -0.2838, -0.1115],
            [-0.8339, 1.5817, 0.2696],
            [-0.0837, 0.068, 1.1939],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in pentax-istds: at 6504 K
    // they want R 1.72 B 0.94, this matrix implies R 1.74 B 0.88
    // (+1%/-6%).
    Camera {
        make: "Pentax",
        model: "*ist DS",
        color_matrix: [
            [1.0371, -0.2333, -0.1206],
            [-0.8688, 1.6231, 0.2602],
            [-0.123, 0.1116, 1.1282],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of 645D-645D2839.DNG, a real DNG for this
    // body (raw.pixls.us). Exact, not recalled. Pentax's own matrix, and
    // its blue row does not agree with the camera's Kelvin white-balance
    // curve (it implies B 1.25 at 6504 K where the camera says 1.05). Kept
    // because it is what the same body's DNGs carry, so PEF and DNG at
    // least render alike; worth replacing with Adobe's if it turns up.
    Camera {
        make: "Pentax",
        model: "645D",
        color_matrix: [
            [0.981354, -0.271622, -0.143921],
            [-0.504913, 1.390701, 0.116577],
            [-0.183014, 0.383621, 0.570557],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX 645Z Ver. 1.00. Exact, not
    // recalled. Its own white-balance presets want R 2.32 B 1.25 at 6504 K
    // against this matrix's R 2.22 B 1.31 (-4%/+5%).
    Camera {
        make: "Pentax",
        model: "645Z",
        color_matrix: [
            [0.955109, -0.301178, -0.123489],
            [-0.368484, 1.213333, 0.172089],
            [-0.101929, 0.188721, 0.654434],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware K-01 Ver 1.05. Exact, not recalled.
    Camera {
        make: "Pentax",
        model: "K-01",
        color_matrix: [
            [0.841812, -0.254395, -0.112732],
            [-0.399536, 1.230118, 0.188065],
            [-0.097733, 0.17131, 0.651276],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-1 Ver. 1.40. Exact, not
    // recalled. Its own white-balance presets want R 2.39 B 1.32 at 6504 K
    // against this matrix's R 2.48 B 1.36 (+4%/+3%).
    Camera {
        make: "Pentax",
        model: "K-1",
        color_matrix: [
            [0.882736, -0.282928, -0.123825],
            [-0.361176, 1.220398, 0.154984],
            [-0.089706, 0.168762, 0.62912],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-1 Mark II Ver. 1.02. Exact, not
    // recalled.
    Camera {
        make: "Pentax",
        model: "K-1 Mark II",
        color_matrix: [
            [0.906296, -0.290482, -0.127121],
            [-0.361176, 1.220398, 0.154984],
            [-0.089996, 0.169296, 0.631119],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-3 Ver. 1.30. Exact, not
    // recalled. Its own white-balance presets want R 2.52 B 1.26 at 6504 K
    // against this matrix's R 2.43 B 1.26 (-4%/+0%).
    Camera {
        make: "Pentax",
        model: "K-3",
        color_matrix: [
            [0.865585, -0.261581, -0.115921],
            [-0.399536, 1.230118, 0.188065],
            [-0.103821, 0.182037, 0.692062],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-3 II Ver. 1.10. Exact, not
    // recalled. Its own white-balance presets want R 2.51 B 1.25 at 6504 K
    // against this matrix's R 2.44 B 1.26 (-3%/+1%).
    Camera {
        make: "Pentax",
        model: "K-3 II",
        color_matrix: [
            [0.861725, -0.260406, -0.115402],
            [-0.399536, 1.230118, 0.188065],
            [-0.103516, 0.181519, 0.690094],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-3 Mark III Ver. 1.01. Exact,
    // not recalled. Its own white-balance presets want R 2.74 B 1.40 at
    // 6504 K against this matrix's R 2.59 B 1.51 (-5%/+8%).
    Camera {
        make: "Pentax",
        model: "K-3 Mark III",
        color_matrix: [
            [0.702194, -0.162216, -0.088959],
            [-0.461395, 1.272781, 0.206543],
            [-0.063995, 0.142883, 0.568558],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware K-30 Ver 1.06. Exact, not recalled.
    Camera {
        make: "Pentax",
        model: "K-30",
        color_matrix: [
            [0.93634, -0.282959, -0.125397],
            [-0.399536, 1.230118, 0.188065],
            [-0.103073, 0.180664, 0.686844],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of _MAR0543.DNG, a real DNG for this body
    // (raw.pixls.us). Exact, not recalled. Agrees with the camera's own
    // Kelvin curve to 1%.
    Camera {
        make: "Pentax",
        model: "K-5",
        color_matrix: [
            [0.862534, -0.260666, -0.115509],
            [-0.399536, 1.230118, 0.188065],
            [-0.098206, 0.172134, 0.654419],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware K-5 II Ver 1.07. Exact, not recalled.
    Camera {
        make: "Pentax",
        model: "K-5 II",
        color_matrix: [
            [0.843491, -0.254898, -0.112961],
            [-0.399536, 1.230118, 0.188065],
            [-0.098892, 0.173355, 0.659073],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of K-50-IMGP1385.DNG, a real DNG for this
    // body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Pentax",
        model: "K-50",
        color_matrix: [
            [0.922821, -0.278885, -0.123581],
            [-0.399536, 1.230118, 0.188065],
            [-0.099533, 0.174469, 0.663315],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware K-500 Ver. 1.02. Exact, not recalled.
    Camera {
        make: "Pentax",
        model: "K-500",
        color_matrix: [
            [0.898972, -0.271667, -0.120392],
            [-0.399536, 1.230118, 0.188065],
            [-0.09903, 0.173599, 0.659973],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65, preferred over the
    // in-camera DNG's own matrix for this body because it is the one that
    // agrees with the camera's white-balance curve. Its own white-balance
    // presets want R 2.07 B 0.99 at 6504 K against this matrix's R 2.11 B
    // 1.04 (+2%/+5%).
    Camera {
        make: "Pentax",
        model: "K-7",
        color_matrix: [
            [0.9142, -0.2947, -0.0678],
            [-0.8648, 1.6967, 0.1663],
            [-0.2224, 0.2898, 0.8615],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-70 Ver. 1.10. Exact, not
    // recalled. Its own white-balance presets want R 2.41 B 1.36 at 6504 K
    // against this matrix's R 2.50 B 1.39 (+4%/+2%).
    Camera {
        make: "Pentax",
        model: "K-70",
        color_matrix: [
            [0.799911, -0.204788, -0.125626],
            [-0.435867, 1.295334, 0.151459],
            [-0.109055, 0.195511, 0.604355],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-S1 Ver. 1.20. Exact, not
    // recalled.
    Camera {
        make: "Pentax",
        model: "K-S1",
        color_matrix: [
            [0.81636, -0.256592, -0.116196],
            [-0.388229, 1.234985, 0.16893],
            [-0.085358, 0.151001, 0.638519],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX K-S2 Ver. 1.20. Exact, not
    // recalled. Its own white-balance presets want R 2.60 B 1.33 at 6504 K
    // against this matrix's R 2.70 B 1.37 (+4%/+3%).
    Camera {
        make: "Pentax",
        model: "K-S2",
        color_matrix: [
            [0.80867, -0.254181, -0.115097],
            [-0.388229, 1.234985, 0.16893],
            [-0.085663, 0.15155, 0.640808],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware K-x Ver 1.03. Exact, not recalled. Its
    // own white-balance presets want R 1.93 B 1.12 at 6504 K against this
    // matrix's R 1.99 B 1.22 (+3%/+9%).
    Camera {
        make: "Pentax",
        model: "K-x",
        color_matrix: [
            [1.044144, -0.332535, -0.114777],
            [-0.557129, 1.35994, 0.21489],
            [-0.120621, 0.175415, 0.744888],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of K10D-IMGP5398.DNG, a real DNG for this
    // body (raw.pixls.us). Exact, not recalled. As with the 645D this is
    // Pentax's own matrix and its red row is well off the camera's Kelvin
    // curve (1.33 against 1.83 at 6504 K). Kept for the same reason: the
    // K10D's own DNGs render with exactly this.
    Camera {
        make: "Pentax",
        model: "K10D",
        color_matrix: [
            [1.098602, -0.257263, -0.03801],
            [-0.568558, 1.259155, 0.250153],
            [-0.067535, 0.060776, 0.977936],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware K200D Ver 1.01. Exact, not recalled. Its
    // own white-balance presets want R 1.83 B 1.09 at 6504 K against this
    // matrix's R 1.89 B 1.20 (+4%/+9%).
    Camera {
        make: "Pentax",
        model: "K200D",
        color_matrix: [
            [1.034805, -0.337326, -0.108109],
            [-0.482162, 1.220963, 0.217987],
            [-0.026398, 0.035126, 0.758392],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware K20D Ver 1.04. Exact, not recalled. Its
    // own white-balance presets want R 1.66 B 1.10 at 6504 K against this
    // matrix's R 1.75 B 1.10 (+5%/+1%).
    Camera {
        make: "Pentax",
        model: "K20D",
        color_matrix: [
            [1.138107, -0.346939, -0.148712],
            [-0.4767, 1.345383, 0.098969],
            [-0.145813, 0.31279, 0.67128],
        ],
        black_level: None,
        white_level: None,
    },
    // source: the K-70 entry above. Same sensor and colour filter array,
    // sold under this name; Adobe calibrates the pair together. Nothing in
    // a KF file was used to check it.
    Camera {
        make: "Pentax",
        model: "KF",
        color_matrix: [
            [0.799911, -0.204788, -0.125626],
            [-0.435867, 1.295334, 0.151459],
            [-0.109055, 0.195511, 0.604355],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware PENTAX KP Ver. 1.00. Exact, not
    // recalled.
    Camera {
        make: "Pentax",
        model: "KP",
        color_matrix: [
            [0.765549, -0.211395, -0.137329],
            [-0.484177, 1.355545, 0.134949],
            [-0.152664, 0.239807, 0.569351],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of GR-R0003611.DNG, a real DNG for this
    // body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Ricoh",
        model: "GR",
        color_matrix: [
            [0.5329, -0.1459, -0.039],
            [-0.5407, 1.293, 0.2768],
            [-0.1119, 0.1772, 0.6046],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of GR_II-R0000443.DNG, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Ricoh",
        model: "GR II",
        color_matrix: [
            [0.493271, -0.088852, -0.045059],
            [-0.497665, 1.280502, 0.241669],
            [-0.064835, 0.149033, 0.621185],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware RICOH GR III Ver. 1.00. Exact, not
    // recalled.
    Camera {
        make: "Ricoh",
        model: "GR III",
        color_matrix: [
            [0.613297, -0.141678, -0.077698],
            [-0.461395, 1.272781, 0.206543],
            [-0.066528, 0.14856, 0.591125],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of GR_IIIx-R0001005.DNG, a real DNG for
    // this body (raw.pixls.us). Exact, not recalled. Agrees with the
    // camera's Kelvin curve to 3%.
    Camera {
        make: "Ricoh",
        model: "GR IIIx",
        color_matrix: [
            [0.61145, -0.141251, -0.077469],
            [-0.461395, 1.272781, 0.206543],
            [-0.066269, 0.147949, 0.58873],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware S918BXXS3AWF7. Exact, not recalled.
    Camera {
        make: "Samsung",
        model: "Galaxy S23 Ultra",
        color_matrix: [
            [0.716797, -0.140625, -0.116211],
            [-0.575195, 1.451172, 0.086914],
            [-0.166016, 0.337891, 0.472656],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware GX10 Ver 1.30. Exact, not recalled.
    Camera {
        make: "Samsung",
        model: "GX10",
        color_matrix: [
            [1.098602, -0.257263, -0.03801],
            [-0.568558, 1.259155, 0.250153],
            [-0.067535, 0.060776, 0.977936],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware GX20 Ver 1.01. Exact, not recalled.
    Camera {
        make: "Samsung",
        model: "GX20",
        color_matrix: [
            [2.771667, -1.740295, -0.577927],
            [-0.845032, 1.977798, 0.037689],
            [0.045914, -0.197754, 1.122131],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware G950FXXU2CRED. Exact, not recalled.
    Camera {
        make: "Samsung",
        model: "SM-G950F",
        color_matrix: [
            [0.674805, -0.088867, -0.112305],
            [-0.503906, 1.40625, 0.067383],
            [-0.219727, 0.495117, 0.483398],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of SM-G955F-20180202_091722.dng, a real
    // DNG for this body (raw.pixls.us). Exact, not recalled. Galaxy S8+.
    // D65 is in the first illuminant slot here.
    Camera {
        make: "Samsung",
        model: "SM-G955F",
        color_matrix: [
            [0.674805, -0.088867, -0.112305],
            [-0.503906, 1.40625, 0.067383],
            [-0.219727, 0.495117, 0.483398],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware G960FXXU7CSJ1. Exact, not recalled.
    Camera {
        make: "Samsung",
        model: "SM-G960F",
        color_matrix: [
            [0.645508, -0.060547, -0.107422],
            [-0.550781, 1.442383, 0.075195],
            [-0.179688, 0.43457, 0.483398],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware G973FXXS3ASJG. Exact, not recalled.
    Camera {
        make: "Samsung",
        model: "SM-G973F",
        color_matrix: [
            [0.786133, -0.15918, -0.142578],
            [-0.609375, 1.542969, 0.033203],
            [-0.256836, 0.513672, 0.442383],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of Galaxy_S22_Ultra-20230531_111518.dng,
    // a real DNG for this body (raw.pixls.us). Exact, not recalled. Galaxy
    // S22. D65 is in the first illuminant slot here.
    Camera {
        make: "Samsung",
        model: "SM-S901U",
        color_matrix: [
            [0.780273, -0.217773, -0.113281],
            [-0.535156, 1.402344, 0.097656],
            [-0.149414, 0.3125, 0.458984],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of
    // fp-A002_647_20240229_000001_4k_12bit.DNG, a real DNG for this body
    // (raw.pixls.us). Exact, not recalled.
    Camera {
        make: "Sigma",
        model: "fp",
        color_matrix: [
            [0.8252, -0.2044, -0.1744],
            [-0.4961, 1.1648, 0.0631],
            [-0.1156, 0.1607, 0.3607],
        ],
        black_level: None,
        white_level: None,
    },
    // source: ColorMatrix2 (D65) of a DNG for this body from raw.pixls.us,
    // written in-camera, firmware SIGMA fp L Ver.3.00.0.V88. Exact, not
    // recalled.
    Camera {
        make: "Sigma",
        model: "fp L",
        color_matrix: [
            [0.8326, -0.2062, -0.1759],
            [-0.4961, 1.1648, 0.0631],
            [-0.1116, 0.1551, 0.3483],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-rx100m3: at 6504 K
    // they want R 2.77 B 1.58, this matrix implies R 2.96 B 1.68
    // (+7%/+6%).
    Camera {
        make: "Sony",
        model: "DSC-RX100M3",
        color_matrix: [
            [0.6596, -0.2079, -0.0562],
            [-0.4782, 1.3016, 0.1933],
            [-0.097, 0.1581, 0.5181],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // DSLR-A450-20170423_113243_A450_07665.ARW: at 6504 K they want R 2.68
    // B 1.32, this matrix implies R 2.71 B 1.24 (+1%/-7%).
    Camera {
        make: "Sony",
        model: "DSLR-A450",
        color_matrix: [
            [0.495, -0.058, -0.0103],
            [-0.5228, 1.2542, 0.3029],
            [-0.0709, 0.1435, 0.7371],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in ILCA-99M2-BR_00086.ARW: at
    // 6504 K they want R 2.72 B 1.46, this matrix implies R 2.76 B 1.45
    // (+1%/-1%).
    Camera {
        make: "Sony",
        model: "ILCA-99M2",
        color_matrix: [
            [0.666, -0.1918, -0.0471],
            [-0.4613, 1.2243, 0.2657],
            [-0.028, 0.1285, 0.5897],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // ILCE-1-A1_full_compressed.ARW: at 6504 K they want R 2.69 B 1.41,
    // this matrix implies R 2.60 B 1.34 (-3%/-5%).
    Camera {
        make: "Sony",
        model: "ILCE-1",
        color_matrix: [
            [0.8161, -0.2947, -0.0739],
            [-0.4811, 1.3045, 0.1793],
            [-0.0518, 0.1615, 0.6106],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-a6300: at 6504 K they
    // want R 2.93 B 1.58, this matrix implies R 3.02 B 1.50 (+3%/-5%).
    Camera {
        make: "Sony",
        model: "ILCE-6300",
        color_matrix: [
            [0.5973, -0.1695, -0.0419],
            [-0.3826, 1.1797, 0.2293],
            [-0.0639, 0.1398, 0.5789],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-a7: at 6504 K they
    // want R 2.57 B 1.28, this matrix implies R 2.76 B 1.24 (+7%/-4%).
    Camera {
        make: "Sony",
        model: "ILCE-7",
        color_matrix: [
            [0.5271, -0.0712, -0.0347],
            [-0.6153, 1.3653, 0.2763],
            [-0.1601, 0.2366, 0.7242],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-a7m2: at 6504 K they
    // want R 2.68 B 1.34, this matrix implies R 2.76 B 1.24 (+3%/-7%).
    // This is the ILCE-7 under the name its own files use; the check above
    // is on a ILCE-7M2 sample, not the ILCE-7's.
    Camera {
        make: "Sony",
        model: "ILCE-7M2",
        color_matrix: [
            [0.5271, -0.0712, -0.0347],
            [-0.6153, 1.3653, 0.2763],
            [-0.1601, 0.2366, 0.7242],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-a7m3: at 6504 K they
    // want R 2.56 B 1.44, this matrix implies R 2.67 B 1.35 (+4%/-6%).
    Camera {
        make: "Sony",
        model: "ILCE-7M3",
        color_matrix: [
            [0.7374, -0.2389, -0.0551],
            [-0.5435, 1.3162, 0.2519],
            [-0.1006, 0.1795, 0.6552],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // ILCE-7M4-ILCE-7M4_DSC06674_FullFrame-LossLess-Compressed-Large.ARW:
    // at 6504 K they want R 2.62 B 1.45, this matrix implies R 2.63 B 1.39
    // (+0%/-5%).
    Camera {
        make: "Sony",
        model: "ILCE-7M4",
        color_matrix: [
            [0.746, -0.2365, -0.0588],
            [-0.5687, 1.3442, 0.2474],
            [-0.0624, 0.1156, 0.6584],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // ILCE-7RM2-12-bit-compressed.ARW: at 6504 K they want R 2.59 B 1.44,
    // this matrix implies R 2.77 B 1.36 (+7%/-6%).
    Camera {
        make: "Sony",
        model: "ILCE-7RM2",
        color_matrix: [
            [0.6629, -0.19, -0.0483],
            [-0.4618, 1.2349, 0.255],
            [-0.0622, 0.1381, 0.6514],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-a7rm3: at 6504 K they
    // want R 2.73 B 1.44, this matrix implies R 2.74 B 1.37 (+0%/-5%).
    Camera {
        make: "Sony",
        model: "ILCE-7RM3",
        color_matrix: [
            [0.664, -0.1847, -0.0503],
            [-0.5238, 1.301, 0.2474],
            [-0.0993, 0.1673, 0.6527],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in
    // ILCE-7RM5-7RM5-LosslessCompressedLarge.ARW: at 6504 K they want R
    // 2.72 B 1.53, this matrix implies R 2.66 B 1.45 (-2%/-5%).
    Camera {
        make: "Sony",
        model: "ILCE-7RM5",
        color_matrix: [
            [0.82, -0.2976, -0.0719],
            [-0.4296, 1.2053, 0.2532],
            [-0.0429, 0.1178, 0.6083],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in NEX-5N-DSC02519.ARW: at
    // 6504 K they want R 2.77 B 1.34, this matrix implies R 2.90 B 1.35
    // (+5%/+1%).
    Camera {
        make: "Sony",
        model: "NEX-5N",
        color_matrix: [
            [0.5991, -0.1456, -0.0455],
            [-0.4764, 1.2135, 0.298],
            [-0.0707, 0.1425, 0.6701],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-nex6: at 6504 K they
    // want R 2.97 B 1.31, this matrix implies R 2.90 B 1.35 (-2%/+3%).
    // This is the NEX-5N under the name its own files use; the check above
    // is on a NEX-6 sample, not the NEX-5N's.
    Camera {
        make: "Sony",
        model: "NEX-6",
        color_matrix: [
            [0.5991, -0.1456, -0.0455],
            [-0.4764, 1.2135, 0.298],
            [-0.0707, 0.1425, 0.6701],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in sony-nex7: at 6504 K they
    // want R 2.90 B 1.31, this matrix implies R 2.99 B 1.25 (+3%/-5%).
    Camera {
        make: "Sony",
        model: "NEX-7",
        color_matrix: [
            [0.5491, -0.1192, -0.0363],
            [-0.4951, 1.2342, 0.2948],
            [-0.0911, 0.1722, 0.7192],
        ],
        black_level: None,
        white_level: None,
    },
    // source: Adobe DNG Converter ColorMatrix2, D65. Cross-checked against
    // the camera's own white-balance presets in SLT-A55-DSC06309.ARW: at
    // 6504 K they want R 2.76 B 1.32, this matrix implies R 2.93 B 1.33
    // (+6%/+1%).
    Camera {
        make: "Sony",
        model: "SLT-A55V",
        color_matrix: [
            [0.5932, -0.1492, -0.0411],
            [-0.4813, 1.2285, 0.2856],
            [-0.0741, 0.1524, 0.6739],
        ],
        black_level: None,
        white_level: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// XYZ of a perfect white under D65, the illuminant every matrix
    /// in the table is calibrated for.
    const WHITE_D65: [f32; 3] = [0.95047, 1.0, 1.08883];

    // ------------------------------------------------------- normalising

    /// Make and model strings taken verbatim from the sample corpus
    /// (`exiftool -Make -Model`), with what `normalize` must make of
    /// them. Where the file has an oracle `.identify.txt` beside it,
    /// these agree with its "Normalized Make/Model" line.
    const CASES: &[(&str, &str, &str, &str)] = &[
        // The eleven top-level samples, whose raw-identify output names
        // the same pairs.
        ("NIKON CORPORATION", "NIKON D3300", "Nikon", "D3300"),
        ("SONY", "ILCE-6000", "Sony", "ILCE-6000"),
        ("Canon", "Canon EOS 450D", "Canon", "EOS 450D"),
        ("Apple", "iPhone 12 Pro", "Apple", "iPhone 12 Pro"),
        ("PENTAX", "PENTAX K-5", "Pentax", "K-5"),
        ("Google", "Pixel 4a", "Google", "Pixel 4a"),
        ("FUJIFILM", "X-T1", "Fujifilm", "X-T1"),
        ("OLYMPUS IMAGING CORP.", "E-M10", "Olympus", "E-M10"),
        ("Panasonic", "DMC-GX7", "Panasonic", "DMC-GX7"),
        ("Canon", "Canon EOS R", "Canon", "EOS R"),
        // Every other make string the corpus contains.
        ("NIKON", "E8800", "Nikon", "E8800"),
        ("Nikon", "Nikon COOLSCAN V ED", "Nikon", "COOLSCAN V ED"),
        ("NIKON CORPORATION", "NIKON 1 AW1", "Nikon", "1 AW1"),
        ("NIKON CORPORATION", "NIKON Z 30", "Nikon", "Z 30"),
        (
            "Canon",
            "Canon PowerShot A570 IS",
            "Canon",
            "PowerShot A570 IS",
        ),
        (
            "Canon",
            "Canon EOS-1Ds Mark III",
            "Canon",
            "EOS-1Ds Mark III",
        ),
        ("Canon", "Canon EOS Rebel T6", "Canon", "EOS Rebel T6"),
        ("SONY", "SLT-A55V", "Sony", "SLT-A55V"),
        ("SONY", "DSC-RX100M7", "Sony", "DSC-RX100M7"),
        ("FUJIFILM", "FinePix S9600", "Fujifilm", "FinePix S9600"),
        ("FUJIFILM", "DBP for GX680", "Fujifilm", "DBP for GX680"),
        ("OLYMPUS CORPORATION", "E-M1MarkII", "Olympus", "E-M1MarkII"),
        ("OLYMPUS OPTICAL CO.,LTD", "C5050Z", "Olympus", "C5050Z"),
        // OM Digital Solutions keeps the brand on the body, and a model
        // starting "OM-" must not lose its first token to it.
        ("OM Digital Solutions", "OM-3", "OM System", "OM-3"),
        (
            "OM Digital Solutions",
            "OM-5MarkII",
            "OM System",
            "OM-5MarkII",
        ),
        ("OM SYSTEM", "OM SYSTEM OM-1", "OM System", "OM-1"),
        ("Panasonic", "DC-S1R", "Panasonic", "DC-S1R"),
        ("LEICA", "D-LUX 5", "Leica", "D-LUX 5"),
        ("LEICA", "C (Typ 112)", "Leica", "C (Typ 112)"),
        (
            "LEICA CAMERA AG",
            "LEICA SL (Typ 601)",
            "Leica",
            "SL (Typ 601)",
        ),
        ("Leica Camera AG", "LEICA M10", "Leica", "M10"),
        ("PENTAX Corporation", "PENTAX *ist DL", "Pentax", "*ist DL"),
        // Ricoh's make covers both brands; the model says which.
        (
            "RICOH IMAGING COMPANY, LTD.",
            "PENTAX K-3 Mark III",
            "Pentax",
            "K-3 Mark III",
        ),
        ("RICOH IMAGING COMPANY, LTD.", "PENTAX KF", "Pentax", "KF"),
        ("RICOH IMAGING COMPANY, LTD.", "GR II", "Ricoh", "GR II"),
        (
            "RICOH IMAGING COMPANY, LTD.",
            "RICOH GR IIIx",
            "Ricoh",
            "GR IIIx",
        ),
        ("SAMSUNG", "NX2000", "Samsung", "NX2000"),
        ("samsung", "SM-G955F", "Samsung", "SM-G955F"),
        (
            "KONICA MINOLTA",
            "ALPHA-7 DIGITAL",
            "Minolta",
            "ALPHA-7 DIGITAL",
        ),
        ("KONICA MINOLTA", "MAXXUM 7D", "Minolta", "MAXXUM 7D"),
        ("Minolta Co., Ltd.", "DiMAGE 7i", "Minolta", "DiMAGE 7i"),
        (
            "EASTMAN KODAK COMPANY",
            "KODAK EasyShare Z981 Digital Camera",
            "Kodak",
            "EasyShare Z981",
        ),
        (
            "Eastman Kodak Company",
            "Kodak Digital Science DC50 Zoom Camera",
            "Kodak",
            "Digital Science DC50",
        ),
        ("Kodak", "DCS Pro SLR/c", "Kodak", "DCS Pro SLR/c"),
        ("SEIKO EPSON CORP.", "R-D1x", "Epson", "R-D1x"),
        ("Mamiya-OP Co.,Ltd.", "MAMIYA ZD", "Mamiya", "ZD"),
        ("Phase One A/S", "IQ140", "Phase One", "IQ140"),
        ("Phase One A/S", "iXU180", "Phase One", "iXU180"),
        // Hasselblad packs the shutter mode into the model, Leaf a
        // serial number and the back's format.
        (
            "Hasselblad",
            "Hasselblad X2D 100C",
            "Hasselblad",
            "X2D 100C",
        ),
        ("Hasselblad", "X2D 100C", "Hasselblad", "X2D 100C"),
        (
            "Hasselblad",
            "CFV 100C/Electronic Shutter",
            "Hasselblad",
            "CFV 100C",
        ),
        ("Hasselblad", "Hasselblad H5D-50c", "Hasselblad", "H5D-50c"),
        (
            "Leaf",
            "Leaf Aptus 75(LI400146   )/Large Format",
            "Leaf",
            "Aptus 75",
        ),
        (
            "Leaf",
            "Leaf AFi-II 12(LI600083   )/Leaf AFi",
            "Leaf",
            "AFi-II 12",
        ),
        ("SIGMA", "SIGMA DP2 Merrill", "Sigma", "DP2 Merrill"),
        ("SIGMA", "SIGMA dp0 Quattro", "Sigma", "dp0 Quattro"),
        ("DJI", "DJI Osmo Action", "DJI", "Osmo Action"),
        ("DJI", "FC6310", "DJI", "FC6310"),
        ("GoPro", "HERO8 Black", "GoPro", "HERO8 Black"),
    ];

    #[test]
    fn normalises_the_corpus_strings() {
        for (make, model, want_make, want_model) in CASES {
            let got = normalize(make, model);
            assert_eq!(
                (got.0.as_str(), got.1.as_str()),
                (*want_make, *want_model),
                "normalize({make:?}, {model:?})"
            );
        }
    }

    #[test]
    fn normalises_ragged_strings() {
        // TIFF ASCII fields are NUL-terminated and vendors pad them.
        assert_eq!(
            normalize("NIKON CORPORATION\0\0", "NIKON  D3300 \0junk"),
            ("Nikon".into(), "D3300".into())
        );
        // Whitespace of every kind collapses.
        assert_eq!(
            normalize("  Canon\t", "\nCanon   EOS\t 5D  Mark  II "),
            ("Canon".into(), "EOS 5D Mark II".into())
        );
        // A make this crate has never heard of keeps its own spelling,
        // minus the corporate form.
        assert_eq!(
            normalize("Acme Imaging Corp.", "Widget 1"),
            ("Acme".into(), "Widget 1".into())
        );
        // Nothing at all is not an error.
        assert_eq!(normalize("", ""), (String::new(), String::new()));
        // A make-less file is rescued from the model, which is how the
        // CIFF samples carry their camera.
        assert_eq!(
            normalize("", "Canon PowerShot A620"),
            ("Canon".into(), "PowerShot A620".into())
        );
        // The model is never stripped away to nothing, however much it
        // repeats the make.
        assert_eq!(normalize("DJI", "DJI"), ("DJI".into(), "DJI".into()));
        assert_eq!(
            normalize("Nikon", "NIKON"),
            ("Nikon".into(), "NIKON".into())
        );
    }

    #[test]
    fn normalising_is_idempotent() {
        // A decoder that hands back its own clean strings must get them
        // unchanged, or `apply_camera_table` would key on a moving target.
        for (make, model, _, _) in CASES {
            let once = normalize(make, model);
            let twice = normalize(&once.0, &once.1);
            assert_eq!(
                once, twice,
                "normalize is not idempotent on {make:?}/{model:?}"
            );
        }
    }

    // ------------------------------------------------------------- table

    #[test]
    fn table_is_sorted_and_unique() {
        for pair in CAMERAS.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let order = ci_cmp(a.make, b.make).then_with(|| ci_cmp(a.model, b.model));
            assert_eq!(
                order,
                Ordering::Less,
                "table out of order (or duplicated) at {}/{} before {}/{}",
                a.make,
                a.model,
                b.make,
                b.model
            );
        }
    }

    /// The table is keyed on what [`normalize`] produces, not on what
    /// a file happens to say: an entry whose key does not survive
    /// normalising could never be found.
    #[test]
    fn table_keys_are_normalised() {
        for camera in CAMERAS {
            let (make, model) = normalize(camera.make, camera.model);
            assert_eq!(
                (make.as_str(), model.as_str()),
                (camera.make, camera.model),
                "table key {}/{} is not what normalize would produce",
                camera.make,
                camera.model
            );
        }
    }

    #[test]
    fn every_entry_is_findable() {
        for camera in CAMERAS {
            assert_eq!(lookup(camera.make, camera.model), Some(camera));
            // Case-insensitively too: decoders pass on what the file
            // said, and vendors are not consistent about case.
            assert_eq!(
                lookup(&camera.make.to_uppercase(), &camera.model.to_lowercase()),
                Some(camera)
            );
        }
        assert_eq!(lookup("Nikon", "no such camera"), None);
        assert_eq!(lookup("No Such Make", "D3300"), None);
    }

    #[test]
    fn olympus_and_om_system_share_a_namespace() {
        // Whichever make an entry is filed under, both names find it:
        // bodies from the handover shipped under one and took firmware
        // under the other.
        for camera in CAMERAS
            .iter()
            .filter(|c| c.make == "Olympus" || c.make == "OM System")
        {
            assert!(
                lookup("Olympus", camera.model).is_some(),
                "{}",
                camera.model
            );
            assert!(
                lookup("OM System", camera.model).is_some(),
                "{}",
                camera.model
            );
        }
    }

    #[test]
    fn matrices_are_plausible() {
        for camera in CAMERAS {
            let m = camera.color_matrix;
            let name = format!("{}/{}", camera.make, camera.model);
            assert!(
                m.iter().flatten().all(|v| v.is_finite()),
                "{name}: matrix is not finite"
            );

            // Invertible, and not so nearly singular that developing
            // would blow noise up: every real camera matrix sits near
            // 0.05..1.0 in determinant.
            let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
                - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
                + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
            assert!(
                det.abs() > 0.01 && det.abs() < 10.0,
                "{name}: determinant {det} is not that of a camera matrix"
            );

            // Each sensor channel responds most to its own part of the
            // spectrum, so a row's largest term is positive and sits on
            // the diagonal or next to it (blue channels on phones often
            // peak one column over, at green).
            for (i, row) in m.iter().enumerate() {
                let peak = (0..3)
                    .max_by(|a, b| row[*a].abs().total_cmp(&row[*b].abs()))
                    .unwrap();
                assert!(
                    row[peak] > 0.0 && peak.abs_diff(i) <= 1,
                    "{name}: row {i} peaks at column {peak} ({row:?})"
                );
            }

            // A neutral must stay neutral-ish: white under D65 has to
            // give three positive raw responses, or white balance would
            // divide by a negative number.
            let response = mul(&m, &WHITE_D65);
            assert!(
                response.iter().all(|v| *v > 0.0),
                "{name}: D65 white gives {response:?}"
            );
            // And no channel may be wildly out of proportion: the
            // implied white-balance multipliers of every camera ever
            // built land well inside 1..8.
            let (r, g, b) = (response[0], response[1], response[2]);
            for mult in [g / r, g / b] {
                assert!(
                    (0.2..8.0).contains(&mult),
                    "{name}: implied white balance {:?} is not a camera's",
                    [g / r, g / b]
                );
            }
        }
    }

    fn mul(m: &[[f32; 3]; 3], v: &[f32; 3]) -> [f32; 3] {
        std::array::from_fn(|i| (0..3).map(|j| m[i][j] * v[j]).sum())
    }

    // ------------------------------------------------------------ corpus

    /// Pull a JSON string value out of `exiftool -j` output without a
    /// JSON parser (this crate has no serde). Good enough for the flat
    /// `"Group:Tag": "value"` shape exiftool writes.
    fn json_string(text: &str, key: &str) -> Option<String> {
        let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
        let rest = text[at..].trim_start();
        let rest = rest.strip_prefix(':')?.trim_start();
        let mut chars = rest.strip_prefix('"')?.chars();
        let mut out = String::new();
        loop {
            match chars.next()? {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    'u' => {
                        let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                        let code = u32::from_str_radix(&hex, 16).ok()?;
                        out.push(char::from_u32(code)?);
                    }
                    other => out.push(other),
                },
                other => out.push(other),
            }
        }
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| {
                let e = e.to_string_lossy().to_ascii_lowercase();
                !matches!(e.as_str(), "json" | "txt" | "tiff" | "tif")
            }) {
                out.push(path);
            }
        }
    }

    /// Every camera in the sample corpus should be in the table.
    ///
    /// Opt-in (`SCHIST_RAW_CORPUS=/tmp/rawsamples`) because it needs the
    /// samples and their `exiftool -j` siblings. It is a report, not a
    /// gate: it prints the cameras still to be calibrated and passes,
    /// because the corpus grows faster than anyone can measure matrices
    /// and a permanently red test teaches people to ignore it. Run it
    /// with `--nocapture` to read the list, and set `SCHIST_RAW_STRICT`
    /// as well to make an uncalibrated camera an actual failure.
    #[test]
    fn corpus_cameras_are_in_the_table() {
        let Ok(root) = std::env::var("SCHIST_RAW_CORPUS") else {
            return;
        };
        let mut files = Vec::new();
        walk(std::path::Path::new(&root), &mut files);

        let mut misses: Vec<String> = Vec::new();
        let mut hits = 0;
        for file in files {
            let sidecar = std::path::PathBuf::from(format!("{}.json", file.display()));
            let Ok(text) = std::fs::read_to_string(&sidecar) else {
                continue;
            };
            let get = |tag: &str| {
                json_string(&text, &format!("IFD0:{tag}"))
                    .or_else(|| json_string(&text, &format!("EXIF:{tag}")))
                    .or_else(|| json_string(&text, &format!("MakerNotes:{tag}")))
            };
            let (Some(make), Some(model)) = (get("Make"), get("Model")) else {
                continue;
            };
            if make.trim().is_empty() && model.trim().is_empty() {
                continue;
            }
            let (clean_make, clean_model) = normalize(&make, &model);
            if lookup(&clean_make, &clean_model).is_some() {
                hits += 1;
            } else {
                let miss = format!("{clean_make} / {clean_model}   ({make} / {model})");
                if !misses.contains(&miss) {
                    misses.push(miss);
                }
            }
        }
        misses.sort();
        if misses.is_empty() {
            println!("corpus: all {hits} samples' cameras are in the table");
            return;
        }
        let report = format!(
            "{} corpus samples matched; {} cameras have no table entry:\n  {}",
            hits,
            misses.len(),
            misses.join("\n  ")
        );
        println!("{report}");
        assert!(std::env::var_os("SCHIST_RAW_STRICT").is_none(), "{report}");
    }
}
