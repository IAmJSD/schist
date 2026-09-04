//! Fills, gradients and colour conversion.

use super::*;

/// The colour of a fill class ("FilS" solid): its `Colr` child as RGBA
/// bytes. "None" fills and gradients give nothing.
pub(super) fn fill_color(graph: &Graph, fill: &Node) -> Option<[u8; 4]> {
    let colr = graph.child(fill, b"Colr")?;
    color_bytes(colr)
}

/// The colour behind a fill descriptor (`FDsc.FDeF`): solid fills give
/// their colour; "none" fills and gradients give nothing.
pub(super) fn descriptor_color(graph: &Graph, fdsc: &Node) -> Option<[u8; 4]> {
    fill_color(graph, graph.child(fdsc, b"FDeF")?)
}

/// Read a gradient off a fill class ("FilG"), with `host` the fill
/// descriptor when one wraps it (newer files hang the gradient's
/// transform there; older ones put it on the fill itself).
pub(super) fn gradient_fill(
    graph: &Graph,
    fill: &Node,
    host: Option<&Node>,
) -> Option<GradientFill> {
    if fill.types.iter().all(|(t, _)| *t != graph::tag(b"FilG")) {
        return None;
    }
    let radial = matches!(fill.field(b"Type"), Some(Value::Enum { id: 2.., .. }));
    let stops = gradient_stops(graph, fill)?;
    // The gradient transform hangs off the descriptor in newer files
    // and off the fill itself in older ones.
    let m: [f64; 6] = match host
        .and_then(|h| h.field(b"FDeX"))
        .or_else(|| fill.field(b"FDeX"))
    {
        Some(Value::VecD(v)) => v.first_chunk().copied()?,
        _ => return None,
    };
    Some(GradientFill {
        stops,
        start: (m[2], m[5]),
        end: (m[0] + m[2], m[3] + m[5]),
        radial,
    })
}

/// A `FilG` gradient fill's stops: `Grad.Posn` positions (each a
/// position/midpoint pair, of which we keep the position) zipped with
/// its `Cols` colours. Separate from [`gradient_fill`] because a
/// gradient *overlay effect* stores no `FDeX` axis — it runs across the
/// layer's own bounds — so it needs the stops without the geometry.
pub(super) fn gradient_stops(graph: &Graph, fill: &Node) -> Option<Vec<(f32, [u8; 4])>> {
    let grad = graph.child(fill, b"Grad")?;
    let positions: Vec<f32> = match grad.field(b"Posn") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| match v {
                Value::VecD(p) => p.first().map(|x| *x as f32),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    let colors: Vec<[u8; 4]> = graph
        .children(grad, b"Cols")
        .iter()
        .filter_map(|c| color_bytes(c))
        .collect();
    if positions.len() != colors.len() || positions.len() < 2 {
        return None;
    }
    Some(positions.into_iter().zip(colors).collect())
}

impl GradientFill {
    pub(super) fn color_at(&self, t: f32) -> [u8; 4] {
        let t = t.clamp(0.0, 1.0);
        let mut prev = self.stops[0];
        for &(pos, col) in &self.stops {
            if t <= pos {
                let span = pos - prev.0;
                let f = if span <= f32::EPSILON {
                    1.0
                } else {
                    (t - prev.0) / span
                };
                let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f + 0.5) as u8;
                return [
                    mix(prev.1[0], col[0]),
                    mix(prev.1[1], col[1]),
                    mix(prev.1[2], col[2]),
                    mix(prev.1[3], col[3]),
                ];
            }
            prev = (pos, col);
        }
        self.stops.last().unwrap().1
    }
}

/// The fill colour of a text run: the first `Objs` descriptor whose
/// `FDeF` fill carries a `Colr` class, converted to RGBA bytes.
pub(super) fn run_color(graph: &Graph, run_item: &Node) -> Option<[u8; 4]> {
    let objs = graph.children(run_item, b"Objs");
    objs.iter().find_map(|obj| descriptor_color(graph, obj))
}

/// Convert a colour class (`RGBA`/`HSLA`/`GRAY`/`CMYK`) to RGBA bytes.
/// CIELAB (D50, the ICC connection space) to sRGB, for the lens filter's
/// stored colour. `l` 0..100, `a`/`b` about -128..127.
pub(super) fn lab_to_rgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let inv = |t: f32| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f32 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    // D50 white point.
    let (xn, yn, zn) = (0.9642, 1.0, 0.8249);
    let (x, y, z) = (xn * inv(fx), yn * inv(fy), zn * inv(fz));
    // XYZ(D50) -> linear sRGB (Bradford-adapted matrix).
    let rl = 3.133_856 * x - 1.616_867 * y - 0.490_615 * z;
    let gl = -0.978_768 * x + 1.916_142 * y + 0.033_454 * z;
    let bl = 0.071_945 * x - 0.228_991 * y + 1.405_243 * z;
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.0031308 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    [enc(rl), enc(gl), enc(bl)]
}

pub(super) fn color_bytes(colr: &Node) -> Option<[u8; 4]> {
    {
        let Value::Struct(raw) = colr.field(b"_col")? else {
            return None;
        };
        let f = |i: usize| f32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap());
        let to = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        match (&colr.type_tag().to_be_bytes(), raw.len()) {
            (b"RGBA", 16) => Some([to(f(0)), to(f(1)), to(f(2)), to(f(3))]),
            (b"HSLA", 16) => {
                let (r, g, b) = hsl_to_rgb(f(0), f(1), f(2));
                Some([to(r), to(g), to(b), to(f(3))])
            }
            (b"GRAY", 8) => Some([to(f(0)), to(f(0)), to(f(0)), to(f(1))]),
            (b"CMYK", 20) => {
                let k = f(3);
                Some([
                    to((1.0 - f(0)) * (1.0 - k)),
                    to((1.0 - f(1)) * (1.0 - k)),
                    to((1.0 - f(2)) * (1.0 - k)),
                    to(f(4)),
                ])
            }
            (_, 4) => Some([raw[0], raw[1], raw[2], raw[3]]),
            _ => None,
        }
    }
}

pub(super) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h.rem_euclid(1.0)) * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r + m, g + m, b + m)
}

/// D50 Lab → sRGB, matching how Affinity displays Lab documents.
pub(super) fn lab_to_srgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let finv = |t: f32| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f32 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    // D50 white point.
    let (xn, yn, zn) = (0.9642f32, 1.0, 0.8251);
    let (x, y, z) = (xn * finv(fx), yn * finv(fy), zn * finv(fz));
    // XYZ (D50) → linear sRGB (Bradford-adapted matrix).
    let r = 3.133_856 * x - 1.616_867 * y - 0.490_615 * z;
    let g = -0.978_768 * x + 1.916_141 * y + 0.033_454 * z;
    let bl = 0.071_945 * x - 0.228_991 * y + 1.405_243 * z;
    let enc = |c: f32| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    (enc(r), enc(g), enc(bl))
}
