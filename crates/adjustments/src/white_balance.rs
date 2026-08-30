//! Warmth and tint, from gain tables measured against Photoshop.

use super::*;

/// Affinity's warmth slider, measured: the linear-light grey log-gains
/// it applies at every tenth of its range, from -100 (cool) to +100
/// (warm). Solved per fixture by fitting the gains of the Bradford
/// adaptation below to Affinity's own render of the probe test card —
/// each fit lands within 0.3/255 RMS, so the adaptation *is* the
/// operation and only these gains were ever in question. The curve is
/// markedly asymmetric (+10 moves nearly three times as far as -10, and
/// warming saturates while cooling runs away), which is why the earlier
/// mirrored-quadratic fit missed the cooling half.
pub(crate) const WARMTH_LOG_GAINS: [[f32; 3]; 21] = [
    [-1.066_140_8, 0.124_861_58, 1.085_302_5],
    [-0.771_848_3, 0.101_186_51, 0.903_488_3],
    [-0.579_433_7, 0.081_628_76, 0.747_089],
    [-0.441_393_9, 0.064_658_24, 0.610_910_9],
    [-0.335_628_2, 0.051_187_27, 0.491_378_4],
    [-0.251_684_2, 0.038_791_68, 0.385_404],
    [-0.182_746_1, 0.028_956_54, 0.291_194_9],
    [-0.126_031_4, 0.019_863_86, 0.207_869_5],
    [-0.078_470_71, 0.011_565_35, 0.131_245_84],
    [-0.036_664_08, 0.005_242_18, 0.062_403_7],
    [0.0, 0.0, 0.0],
    [0.115_865_47, -0.017_772_31, -0.221_109_93],
    [0.188_446_02, -0.028_601_73, -0.377_630_32],
    [0.238_142_49, -0.034_957_6, -0.494_250_24],
    [0.273_799_57, -0.038_461_82, -0.584_949_9],
    [0.300_928_86, -0.040_617_4, -0.655_367_1],
    [0.321_281_37, -0.042_763_06, -0.713_570_9],
    [0.337_165_5, -0.043_341_38, -0.763_241_7],
    [0.349_603_6, -0.044_069_05, -0.803_047_2],
    [0.359_788_2, -0.043_880_67, -0.838_493_2],
    [0.369_014, -0.043_403_02, -0.867_859_36],
];

/// The tint slider's grey log-gains, measured the same way at every
/// fifth of its range. Note the sign: `WBTi` on disk is the *negation*
/// of the Tint field Affinity shows, so this table is indexed by the
/// stored value (positive = the UI's green direction), matching what
/// the importer hands us. Like warmth, it is neither linear nor
/// symmetric.
pub(crate) const TINT_LOG_GAINS: [[f32; 3]; 11] = [
    [0.188_686_56, -0.094_765_3, 0.348_664_34],
    [0.154_797_69, -0.076_550_72, 0.272_947_67],
    [0.119_324_28, -0.056_674_22, 0.200_040_82],
    [0.082_791_54, -0.037_263_72, 0.131_661_3],
    [0.041_722_29, -0.019_361_82, 0.064_003_38],
    [0.0, 0.0, 0.0],
    [-0.044_928_24, 0.020_042_06, -0.062_391_93],
    [-0.094_376_62, 0.039_050_96, -0.123_218_58],
    [-0.145_803_21, 0.059_505_64, -0.182_253_42],
    [-0.203_076_29, 0.078_741_57, -0.240_934_16],
    [-0.263_430_95, 0.100_587_59, -0.298_225_22],
];

/// Read one of the measured tables at `pos` in -1..=1, interpolating
/// linearly between its knots.
pub(crate) fn slider_log_gains(table: &[[f32; 3]], pos: f32) -> [f32; 3] {
    let last = table.len() - 1;
    let x = ((pos.clamp(-1.0, 1.0) + 1.0) / 2.0) * last as f32;
    let i = (x.floor() as usize).min(last - 1);
    let f = x - i as f32;
    let (a, b) = (table[i], table[i + 1]);
    [
        a[0] + (b[0] - a[0]) * f,
        a[1] + (b[1] - a[1]) * f,
        a[2] + (b[2] - a[2]) * f,
    ]
}

/// Rec. 601 luminance, which is what Photoshop's adjustments weight by.
/// Bradford chromatic adaptation for [`Params::WhiteBalance`]. The
/// diagonal cone gains are chosen so the grey axis moves by the
/// calibrated per-channel linear gains for this warmth/tint.
/// How much the two White Balance sliders pull each other, as log
/// cone-gain corrections on a 5x5 grid of (warmth, tint) at -100, -50,
/// 0, 50, 100 — measured from `wbgrid` probes over a full grey and
/// primary ramp, one document per cell, with the clipped samples
/// dropped so the extreme gains are not dragged in.
///
/// Zero all along both axes, which is where the one-dimensional tables
/// above were calibrated and are exact. The corner that matters is
/// cool-and-magenta: at (-100, +100) Affinity's S-cone gain is over
/// three times the product of the two sliders taken separately.
pub(crate) const WB_INTERACTION: [[[f32; 3]; 5]; 5] = [
    [
        [-0.028_16, -0.020_48, -0.330_81],
        [-0.011_48, -0.009_38, -0.200_25],
        [0.0, 0.0, 0.0],
        [0.005_43, 0.006_48, 0.339_99],
        [0.003_97, 0.009_83, 1.129_74],
    ],
    [
        [-0.002_86, -0.011_16, -0.089_41],
        [-0.000_30, -0.005_07, -0.052_52],
        [0.0, 0.0, 0.0],
        [-0.001_05, 0.004_63, 0.073_44],
        [-0.004_17, 0.007_44, 0.179_94],
    ],
    [
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
    ],
    [
        [-0.016_25, 0.025_91, 0.107_43],
        [-0.010_82, 0.010_77, 0.057_62],
        [0.0, 0.0, 0.0],
        [0.013_21, -0.011_90, -0.072_17],
        [0.030_10, -0.021_23, -0.160_45],
    ],
    [
        [-0.027_59, 0.035_72, 0.137_72],
        [-0.016_19, 0.017_59, 0.073_46],
        [0.0, 0.0, 0.0],
        [0.022_30, -0.015_61, -0.089_02],
        [0.049_94, -0.028_37, -0.194_16],
    ],
];

/// Read [`WB_INTERACTION`] at an arbitrary warmth and tint, bilinearly.
/// The table's tint axis is the *panel's*, and the file (so our
/// parameter) stores that negated, hence the flip.
pub(crate) fn white_balance_interaction(warmth: f32, tint: f32) -> [f32; 3] {
    let tint = -tint;
    let at = |v: f32| {
        let x = ((v.clamp(-100.0, 100.0) + 100.0) / 50.0).clamp(0.0, 4.0);
        let i = (x.floor() as usize).min(3);
        (i, x - i as f32)
    };
    let ((iw, fw), (it, ft)) = (at(warmth), at(tint));
    let mut out = [0.0f32; 3];
    for (k, o) in out.iter_mut().enumerate() {
        let a = WB_INTERACTION[iw][it][k] * (1.0 - ft) + WB_INTERACTION[iw][it + 1][k] * ft;
        let b = WB_INTERACTION[iw + 1][it][k] * (1.0 - ft) + WB_INTERACTION[iw + 1][it + 1][k] * ft;
        *o = a * (1.0 - fw) + b * fw;
    }
    out
}

pub(crate) fn white_balance(px: Rgba, warmth: f32, tint: f32) -> Rgba {
    // Each slider's measured grey gains, multiplied together.
    let (kw, kt) = (
        slider_log_gains(&WARMTH_LOG_GAINS, warmth / 100.0),
        slider_log_gains(&TINT_LOG_GAINS, tint / 100.0),
    );
    let g = [
        (kw[0] + kt[0]).exp(),
        (kw[1] + kt[1]).exp(),
        (kw[2] + kt[2]).exp(),
    ];
    // Bradford cone matrix times sRGB->XYZ (D65), and its inverse.
    const B: [[f32; 3]; 3] = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    const R2X: [[f32; 3]; 3] = [
        [0.412_456, 0.357_576, 0.180_437],
        [0.212_673, 0.715_152, 0.072_175],
        [0.019_334, 0.119_192, 0.950_304],
    ];
    fn matmul(a: &[[f32; 3]; 3], b: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
        let mut out = [[0.0f32; 3]; 3];
        for (i, row) in out.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        out
    }
    fn matvec(a: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
        [
            a[0][0] * v[0] + a[0][1] * v[1] + a[0][2] * v[2],
            a[1][0] * v[0] + a[1][1] * v[1] + a[1][2] * v[2],
            a[2][0] * v[0] + a[2][1] * v[1] + a[2][2] * v[2],
        ]
    }
    fn inv3(m: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
        let c = |r: usize, cc: usize| m[r][cc];
        let det = c(0, 0) * (c(1, 1) * c(2, 2) - c(1, 2) * c(2, 1))
            - c(0, 1) * (c(1, 0) * c(2, 2) - c(1, 2) * c(2, 0))
            + c(0, 2) * (c(1, 0) * c(2, 1) - c(1, 1) * c(2, 0));
        let d = 1.0 / det;
        [
            [
                (c(1, 1) * c(2, 2) - c(1, 2) * c(2, 1)) * d,
                (c(0, 2) * c(2, 1) - c(0, 1) * c(2, 2)) * d,
                (c(0, 1) * c(1, 2) - c(0, 2) * c(1, 1)) * d,
            ],
            [
                (c(1, 2) * c(2, 0) - c(1, 0) * c(2, 2)) * d,
                (c(0, 0) * c(2, 2) - c(0, 2) * c(2, 0)) * d,
                (c(0, 2) * c(1, 0) - c(0, 0) * c(1, 2)) * d,
            ],
            [
                (c(1, 0) * c(2, 1) - c(1, 1) * c(2, 0)) * d,
                (c(0, 1) * c(2, 0) - c(0, 0) * c(2, 1)) * d,
                (c(0, 0) * c(1, 1) - c(0, 1) * c(1, 0)) * d,
            ],
        ]
    }
    let bm = matmul(&B, &R2X);
    let u = matvec(&bm, [1.0, 1.0, 1.0]);
    let bg = matvec(&bm, g);
    // The two sliders are not independent: Affinity moves one white
    // point in two dimensions rather than adapting twice, so the
    // product of the two tables is only right on the axes. Correct it
    // by the measured interaction (zero along both axes by
    // construction).
    let c = white_balance_interaction(warmth, tint);
    let d = [
        bg[0] / u[0] * c[0].exp(),
        bg[1] / u[1] * c[1].exp(),
        bg[2] / u[2] * c[2].exp(),
    ];
    let dec = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    let lin = [dec(px.r), dec(px.g), dec(px.b)];
    let lms = matvec(&bm, lin);
    let adapted = [lms[0] * d[0], lms[1] * d[1], lms[2] * d[2]];
    let out = matvec(&inv3(&bm), adapted);
    Rgba {
        r: enc(out[0]),
        g: enc(out[1]),
        b: enc(out[2]),
        a: px.a,
    }
}
