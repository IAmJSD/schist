//! Tone curves, levels, and the hue ranges an HSL adjustment edits.

/// A tone curve as up to 16 control points in 0..1, evaluated with
/// monotone-ish Catmull-Rom interpolation and cached into a LUT.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Curve {
    pub points: Vec<(f32, f32)>,
}

impl Default for Curve {
    fn default() -> Self {
        Curve {
            points: vec![(0.0, 0.0), (1.0, 1.0)],
        }
    }
}

impl Curve {
    /// Insert a control point, keeping the list sorted by x.
    ///
    /// Points nearer than `merge` in x replace each other rather than
    /// stacking, which is what stops a drag from leaving a pile of nearly
    /// coincident points behind.
    pub fn add_point(&mut self, x: f32, y: f32, merge: f32) -> usize {
        let x = x.clamp(0.0, 1.0);
        let y = y.clamp(0.0, 1.0);
        if let Some(i) = self.points.iter().position(|p| (p.0 - x).abs() <= merge) {
            self.points[i] = (x, y);
            return i;
        }
        let i = self
            .points
            .iter()
            .position(|p| p.0 > x)
            .unwrap_or(self.points.len());
        self.points.insert(i, (x, y));
        i
    }

    /// Move an existing point, keeping the order and the endpoints pinned
    /// to the ends of the range.
    pub fn move_point(&mut self, index: usize, x: f32, y: f32) {
        let n = self.points.len();
        if index >= n {
            return;
        }
        let y = y.clamp(0.0, 1.0);
        // The first and last points define the ends of the curve, so they
        // only move vertically.
        let x = if index == 0 {
            0.0
        } else if index + 1 == n {
            1.0
        } else {
            // Keep strictly between the neighbours, or the curve would
            // fold back on itself.
            let lo = self.points[index - 1].0 + 1e-3;
            let hi = self.points[index + 1].0 - 1e-3;
            x.clamp(lo.min(hi), hi.max(lo))
        };
        self.points[index] = (x, y);
    }

    /// Delete a point. The two endpoints cannot be removed.
    pub fn remove_point(&mut self, index: usize) {
        if index == 0 || index + 1 >= self.points.len() || self.points.len() <= 2 {
            return;
        }
        self.points.remove(index);
    }

    /// The point within `radius` of (x, y), if any.
    pub fn hit(&self, x: f32, y: f32, radius: f32) -> Option<usize> {
        self.points
            .iter()
            .enumerate()
            .filter(|(_, p)| (p.0 - x).hypot(p.1 - y) <= radius)
            .min_by(|a, b| {
                let da = (a.1 .0 - x).hypot(a.1 .1 - y);
                let db = (b.1 .0 - x).hypot(b.1 .1 - y);
                da.total_cmp(&db)
            })
            .map(|(i, _)| i)
    }

    pub fn reset(&mut self) {
        self.points = vec![(0.0, 0.0), (1.0, 1.0)];
    }

    pub fn is_identity(&self) -> bool {
        self.points.len() == 2
            && (self.points[0].0 - self.points[0].1).abs() < 1e-4
            && (self.points[1].0 - self.points[1].1).abs() < 1e-4
    }

    /// Evaluate at `x` (0..1) with linear interpolation between the sorted
    /// control points, clamped outside their range.
    pub fn eval(&self, x: f32) -> f32 {
        if self.points.is_empty() {
            return x;
        }
        let x = x.clamp(0.0, 1.0);
        let pts = &self.points;
        if x <= pts[0].0 {
            return pts[0].1.clamp(0.0, 1.0);
        }
        for i in 0..pts.len() - 1 {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[i + 1];
            if x > x1 {
                continue;
            }
            let t = if (x1 - x0).abs() < 1e-6 {
                0.0
            } else {
                (x - x0) / (x1 - x0)
            };
            // Two points describe a straight ramp — interpolate linearly so
            // the default curve is exactly the identity. Longer curves get
            // Catmull-Rom through the control points for a smooth shape.
            if pts.len() == 2 {
                return (y0 + (y1 - y0) * t).clamp(0.0, 1.0);
            }
            let ym1 = pts[i.saturating_sub(1)].1;
            let y2 = pts[(i + 2).min(pts.len() - 1)].1;
            let t2 = t * t;
            let t3 = t2 * t;
            let y = 0.5
                * ((2.0 * y0)
                    + (-ym1 + y1) * t
                    + (2.0 * ym1 - 5.0 * y0 + 4.0 * y1 - y2) * t2
                    + (-ym1 + 3.0 * y0 - 3.0 * y1 + y2) * t3);
            return y.clamp(0.0, 1.0);
        }
        pts[pts.len() - 1].1.clamp(0.0, 1.0)
    }
}

/// Per-channel levels: input black/white with gamma, remapped to an output
/// range.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LevelsChannel {
    pub input_black: f32,
    pub input_white: f32,
    pub gamma: f32,
    pub output_black: f32,
    pub output_white: f32,
}

impl Default for LevelsChannel {
    fn default() -> Self {
        LevelsChannel {
            input_black: 0.0,
            input_white: 1.0,
            gamma: 1.0,
            output_black: 0.0,
            output_white: 1.0,
        }
    }
}

impl LevelsChannel {
    pub fn is_identity(&self) -> bool {
        self.input_black == 0.0
            && self.input_white == 1.0
            && (self.gamma - 1.0).abs() < 1e-4
            && self.output_black == 0.0
            && self.output_white == 1.0
    }

    pub fn apply(&self, v: f32) -> f32 {
        let span = (self.input_white - self.input_black).max(1e-4);
        let t = ((v - self.input_black) / span).clamp(0.0, 1.0);
        let t = if (self.gamma - 1.0).abs() < 1e-4 {
            t
        } else {
            t.powf(1.0 / self.gamma.max(1e-3))
        };
        (self.output_black + t * (self.output_white - self.output_black)).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Levels {
    pub rgb: LevelsChannel,
    pub red: LevelsChannel,
    pub green: LevelsChannel,
    pub blue: LevelsChannel,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Curves {
    pub rgb: Curve,
    pub red: Curve,
    pub green: Curve,
    pub blue: Curve,
}

/// Which curve the editor is showing. Not part of the adjustment's
/// meaning, but it lives here so the dialog can round-trip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurveChannel {
    #[default]
    Rgb,
    Red,
    Green,
    Blue,
}

impl CurveChannel {
    pub const ALL: [CurveChannel; 4] = [
        CurveChannel::Rgb,
        CurveChannel::Red,
        CurveChannel::Green,
        CurveChannel::Blue,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CurveChannel::Rgb => "RGB",
            CurveChannel::Red => "Red",
            CurveChannel::Green => "Green",
            CurveChannel::Blue => "Blue",
        }
    }

    /// The colour to draw this channel's curve in.
    pub fn tint(self) -> u32 {
        match self {
            CurveChannel::Rgb => 0xE0E0E0,
            CurveChannel::Red => 0xE05050,
            CurveChannel::Green => 0x50C050,
            CurveChannel::Blue => 0x5080E0,
        }
    }
}

impl Curves {
    pub fn channel(&self, ch: CurveChannel) -> &Curve {
        match ch {
            CurveChannel::Rgb => &self.rgb,
            CurveChannel::Red => &self.red,
            CurveChannel::Green => &self.green,
            CurveChannel::Blue => &self.blue,
        }
    }

    pub fn channel_mut(&mut self, ch: CurveChannel) -> &mut Curve {
        match ch {
            CurveChannel::Rgb => &mut self.rgb,
            CurveChannel::Red => &mut self.red,
            CurveChannel::Green => &mut self.green,
            CurveChannel::Blue => &mut self.blue,
        }
    }
}

/// One of Affinity's six per-hue-range tweaks on an HSL adjustment.
///
/// The range is a trapezoid over the hue circle: weight 0 outside
/// `bounds[0]`..`bounds[3]`, ramping up to 1 across `bounds[0]`..
/// `bounds[1]`, flat until `bounds[2]`, ramping back down by
/// `bounds[3]` (degrees, wrapping — reds are 315, 345, 15, 45). The
/// six defaults overlap on their ramps, so the weights of the whole
/// set sum to 1 at every hue. Each range's shifts are added to the
/// master ones, weighted, before the sliders' transfer curves run.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HueRange {
    /// Trapezoid corners in degrees, ascending around the circle.
    pub bounds: [f32; 4],
    /// -180..180 degrees.
    pub hue: f32,
    /// -100..100.
    pub saturation: f32,
    /// -100..100.
    pub lightness: f32,
}

impl HueRange {
    /// How much this range claims of a pixel at hue `h` (degrees).
    pub fn weight(&self, h: f32) -> f32 {
        let [b0, b1, b2, b3] = self.bounds;
        let span = |a: f32, b: f32| (b - a).rem_euclid(360.0);
        let (d, up, flat, down) = (span(b0, h), span(b0, b1), span(b0, b2), span(b0, b3));
        if d >= down {
            0.0
        } else if d < up {
            if up > 0.0 {
                d / up
            } else {
                1.0
            }
        } else if d <= flat {
            1.0
        } else if down > flat {
            (down - d) / (down - flat)
        } else {
            1.0
        }
    }
}
