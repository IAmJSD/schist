//! Live shape geometry: rebuilding each shape class as anchors, and
//! rasterizing a path to a mask.

use super::*;

/// Assemble a subpath from PCrv records.
///
/// The marker pair classifies each record: (1,0) and (2,0) are
/// on-curve points (terminal and interior respectively), (0,1) is the
/// previous point's outgoing control and (0,2) the next point's
/// incoming control. A closed path's trailing controls belong to the
/// segment joining back to the first point.
pub(super) fn subpath_from_records(
    records: &[(f32, f32, u8, u8)],
    closed: bool,
) -> Option<schist_core::SubPath> {
    use schist_core::Anchor;
    let mut anchors: Vec<Anchor> = Vec::new();
    let mut incoming: Option<(f32, f32)> = None;
    for &(x, y, m0, m1) in records {
        match (m0, m1) {
            (0, 1) => {
                if let Some(prev) = anchors.last_mut() {
                    prev.handle_out = (x - prev.point.0, y - prev.point.1);
                }
            }
            (0, 2) => incoming = Some((x, y)),
            _ => {
                let handle_in = incoming
                    .take()
                    .map(|c| (c.0 - x, c.1 - y))
                    .unwrap_or((0.0, 0.0));
                anchors.push(Anchor {
                    point: (x, y),
                    handle_in,
                    handle_out: (0.0, 0.0),
                });
            }
        }
    }
    if closed {
        if let (Some(c), Some(first)) = (incoming, anchors.first_mut()) {
            first.handle_in = (c.0 - first.point.0, c.1 - first.point.1);
        }
    }
    if anchors.len() < 2 {
        return None;
    }
    Some(schist_core::SubPath { anchors, closed })
}

/// Circle-to-Bezier handle length as a fraction of the radius.
pub(super) const KAPPA: f32 = 0.552_284_8;

/// Build a shape's outline in its local `ShpB` box, from its `Shpe`
/// parameters. Returns the display name Affinity's own panel would show
/// and the anchors of one closed subpath — or `None` for kinds whose
/// geometry we can't rebuild (those are reported, not guessed).
pub(super) type ShapeSubPaths = Vec<(Vec<schist_core::Anchor>, bool)>;

pub(super) fn shape_geometry(
    shpe: &Node,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> Option<(&'static str, ShapeSubPaths)> {
    use schist_core::Anchor;
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    let (w, h) = (x1 - x0, y1 - y0);
    let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    let closed = |a: Vec<Anchor>| vec![(a, true)];
    let f = |t: &[u8; 4], d: f32| f32_last(shpe, t).unwrap_or(d);
    let tag_bytes = shpe.type_tag().to_be_bytes();
    match &tag_bytes {
        b"ShNR" | b"ShRR" => {
            let mut radii = match shpe.field(b"ShCR") {
                Some(Value::VecF(r)) if r.len() >= 4 => [r[0], r[1], r[2], r[3]],
                Some(Value::VecF(r)) if !r.is_empty() => [r[0]; 4],
                _ => [0.0; 4],
            };
            // Designer's rectangle tool keeps default radii in ShCR but
            // only shapes corners whose type (`CTyp`, one enum per
            // corner) says so: 0 rounded · 1 straight (chamfer) ·
            // 2 concave · 3 cutout, probed with one fixture per type.
            // A CTyp-less ShNR renders sharp in Affinity's own
            // thumbnail, radii notwithstanding.
            let mut corner_types = [0u16; 4];
            let has_ctyp = matches!(shpe.field(b"CTyp"), Some(Value::Array(_)));
            match shpe.field(b"CTyp") {
                Some(Value::Array(types)) => {
                    for (i, r) in radii.iter_mut().enumerate() {
                        match types.get(i) {
                            Some(Value::Enum { id, .. }) if *id <= 3 => {
                                corner_types[i] = *id;
                            }
                            _ => *r = 0.0,
                        }
                    }
                }
                _ if &tag_bytes == b"ShNR" => radii = [0.0; 4],
                _ => {}
            }
            // "Single radius" locks every corner to the first one's
            // radius and treatment.
            if matches!(shpe.field(b"Lock"), Some(Value::Bool(true))) {
                radii = [radii[0]; 4];
                corner_types = [corner_types[0]; 4];
            }
            let scale = if matches!(shpe.field(b"AbSz"), Some(Value::Bool(true))) {
                1.0 // absolute: already in local units
            } else if has_ctyp {
                // The unified app's writer (which also writes CTyp)
                // scales radii by the full shorter side — a 25% radius
                // chamfers 33px off a 132px-tall rect — where Designer
                // 1.x scaled by half of it.
                w.min(h)
            } else {
                w.min(h) * 0.5 // fraction of half the shorter side
            };
            let radii = radii.map(|r| (r * scale).clamp(0.0, w.min(h) * 0.5));
            let kind = if radii.iter().all(|r| *r < 0.25) {
                "Rectangle"
            } else {
                "Rounded Rectangle"
            };
            Some((
                kind,
                closed(cornered_rect_anchors(x0, y0, x1, y1, radii, corner_types)),
            ))
        }
        b"ShpE" => Some(("Ellipse", closed(ellipse_anchors(x0, y0, x1, y1)))),
        b"ShSt" => {
            // Star: `Pnts` points alternating between the ellipse
            // inscribed in the box and `IRad` of it; the first point is
            // up. `CrvL`/`CrvR` bow each spike's left (notch→tip) and
            // right (tip→notch) edges sideways — positive bows to the
            // left of the direction of travel, i.e. outward.
            let cl = f32_last(shpe, b"CrvL").unwrap_or(0.0).clamp(-1.0, 1.0);
            let cr = f32_last(shpe, b"CrvR").unwrap_or(0.0).clamp(-1.0, 1.0);
            let points = u16_of(shpe, b"Pnts").unwrap_or(5).clamp(3, 100) as usize;
            let inner = f32_last(shpe, b"IRad").unwrap_or(0.5).clamp(0.0, 1.0);
            let mut anchors: Vec<Anchor> = (0..points * 2)
                .map(|i| {
                    let r = if i % 2 == 0 { 1.0 } else { inner };
                    let ang = -FRAC_PI_2 + PI * i as f32 / points as f32;
                    unit_anchor(ang.cos() * r, ang.sin() * r, x0, y0, x1, y1)
                })
                .collect();
            let n = anchors.len();
            for i in 0..n {
                // Edge i runs anchors[i] → anchors[i+1]: tips sit at
                // even indices, so even i is tip→notch (right edge).
                let c = if i % 2 == 0 { cr } else { cl };
                if c.abs() < 0.005 {
                    continue;
                }
                let (a, b) = (anchors[i].point, anchors[(i + 1) % n].point);
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                // Left of travel in screen space (y down).
                let (nx, ny) = (dy, -dx);
                let len = (nx * nx + ny * ny).sqrt().max(1e-6);
                let sag = c * (dx * dx + dy * dy).sqrt() * 0.22;
                let (mx, my) = (
                    (a.0 + b.0) * 0.5 + nx / len * 2.0 * sag,
                    (a.1 + b.1) * 0.5 + ny / len * 2.0 * sag,
                );
                // The quadratic through that sagitta, as a cubic.
                anchors[i].handle_out = ((mx - a.0) * (2.0 / 3.0), (my - a.1) * (2.0 / 3.0));
                anchors[(i + 1) % n].handle_in =
                    ((mx - b.0) * (2.0 / 3.0), (my - b.1) * (2.0 / 3.0));
            }
            Some(("Star", closed(anchors)))
        }
        b"ShSS" => Some((
            "Square Star",
            closed(square_star_anchors(shpe, x0, y0, x1, y1)),
        )),
        b"ShCl" => Some(("Cloud", closed(cloud_anchors(shpe, x0, y0, x1, y1)))),
        b"ShHt" => {
            let spread = f32_last(shpe, b"Sprd").unwrap_or(0.2).clamp(0.0, 1.0);
            Some(("Heart", closed(heart_anchors(x0, y0, x1, y1, spread))))
        }
        // The kinds below were each drawn once in Affinity itself
        // (fixtures/affinity-probe/shp_*.af) and their geometry verified
        // against those files' embedded thumbnails.
        // Triangle: apex `Pos` of the way along the top edge.
        b"ShpT" => {
            let pos = f(b"Pos ", 0.5).clamp(0.0, 1.0);
            Some((
                "Triangle",
                closed(vec![
                    Anchor::corner(x0 + pos * w, y0),
                    Anchor::corner(x1, y1),
                    Anchor::corner(x0, y1),
                ]),
            ))
        }
        // Diamond: widest at `Pos` of the height.
        b"ShpD" => {
            let pos = f(b"Pos ", 0.5).clamp(0.0, 1.0);
            let ym = y0 + pos * h;
            Some((
                "Diamond",
                closed(vec![
                    Anchor::corner(cx, y0),
                    Anchor::corner(x1, ym),
                    Anchor::corner(cx, y1),
                    Anchor::corner(x0, ym),
                ]),
            ))
        }
        // Trapezoid: the top edge runs from `PosL` to `PosR`.
        b"ShTz" => {
            let l = f(b"PosL", 0.25).clamp(0.0, 1.0);
            let r = f(b"PosR", 0.75).clamp(0.0, 1.0);
            Some((
                "Trapezoid",
                closed(vec![
                    Anchor::corner(x0 + l * w, y0),
                    Anchor::corner(x0 + r * w, y0),
                    Anchor::corner(x1, y1),
                    Anchor::corner(x0, y1),
                ]),
            ))
        }
        // Polygon: `Side` vertices on the inscribed ellipse, first up.
        // `Curv` bends the edges — unmapped, so only straight rebuilds.
        b"ShPy" => {
            if f(b"Curv", 0.0).abs() > 0.01 {
                return None;
            }
            let sides = u16_of(shpe, b"Side").unwrap_or(5).clamp(3, 100) as usize;
            let anchors = (0..sides)
                .map(|i| {
                    let ang = -FRAC_PI_2 + TAU * i as f32 / sides as f32;
                    unit_anchor(ang.cos(), ang.sin(), x0, y0, x1, y1)
                })
                .collect();
            Some(("Polygon", closed(anchors)))
        }
        // Double star: `Pnts` major tips at the rim with minor tips
        // (`PRad`) between them and notches (`IRad`) between every tip.
        b"ShDS" => {
            let points = u16_of(shpe, b"Pnts").unwrap_or(5).clamp(2, 100) as usize;
            let inner = f(b"IRad", 0.5).clamp(0.0, 1.0);
            let mid = f(b"PRad", 0.8).clamp(0.0, 1.0);
            let radii = [1.0, inner, mid, inner];
            let anchors = (0..points * 4)
                .map(|i| {
                    let r = radii[i % 4];
                    let ang = -FRAC_PI_2 + TAU * i as f32 / (points * 4) as f32;
                    unit_anchor(ang.cos() * r, ang.sin() * r, x0, y0, x1, y1)
                })
                .collect();
            Some(("Double Star", closed(anchors)))
        }
        // Pie and donut share a class: a ring sector from `AngS` to
        // `AngE` (visual angles, anticlockwise from +x) with an inner
        // radius. Equal angles mean the full ring; zero `IRad` a wedge.
        b"ShPi" => {
            let ang_s = f(b"AngS", 0.0);
            let ang_e = f(b"AngE", 0.0);
            let inner = f(b"IRad", 0.0).clamp(0.0, 0.999);
            let (rx, ry) = (w * 0.5, h * 0.5);
            let full = (ang_s - ang_e).abs() < 1e-4;
            if full {
                let mut subs = vec![(ellipse_anchors(x0, y0, x1, y1), true)];
                if inner > 0.001 {
                    subs.push((
                        ellipse_anchors(
                            cx - rx * inner,
                            cy - ry * inner,
                            cx + rx * inner,
                            cy + ry * inner,
                        ),
                        true,
                    ));
                }
                return Some((if inner > 0.001 { "Donut" } else { "Ellipse" }, subs));
            }
            // Screen space runs y-down, so a visual angle t is -t here.
            let t0 = -ang_s;
            let t1 = -(ang_e + if ang_e <= ang_s { TAU } else { 0.0 });
            let mut anchors = arc_anchors(cx, cy, rx, ry, t0, t1);
            if inner > 0.001 {
                anchors.extend(arc_anchors(cx, cy, rx * inner, ry * inner, t1, t0));
            } else {
                anchors.push(Anchor::corner(cx, cy));
            }
            Some(("Pie", closed(anchors)))
        }
        // Segment: the inscribed ellipse above a chord `Pos0` of the way
        // up (and below one at `Pos1`, when it cuts).
        b"ShSg" => {
            let pos0 = f(b"Pos0", 0.25).clamp(0.0, 1.0);
            let pos1 = f(b"Pos1", 1.0).clamp(0.0, 1.0);
            if pos1 < 0.999 {
                log::warn!("affinity: segment with a second chord; importing the first only");
            }
            let uy = (1.0 - 2.0 * pos0).clamp(-1.0, 1.0);
            let a = uy.asin();
            let (rx, ry) = (w * 0.5, h * 0.5);
            // From the left intersection, over the top, to the right.
            let anchors = arc_anchors(cx, cy, rx, ry, PI - a, TAU + a);
            Some(("Segment", closed(anchors)))
        }
        // Crescent: tips at the top and bottom centre; each boundary is
        // a circular arc (in the box's unit space) bowing sideways with
        // a sagitta of half the `ArcL`/`ArcR` value — negative bows
        // left, so the default −1 outer arc is the inscribed ellipse's
        // own left half.
        b"ShCr" => {
            let mut unit = bow_arc_unit(f(b"ArcL", -1.0), true);
            let up = bow_arc_unit(f(b"ArcR", -0.3), false);
            unit.extend(up);
            let anchors = unit
                .into_iter()
                .map(|mut a| {
                    a.point = (x0 + a.point.0 * w, y0 + a.point.1 * h);
                    a.handle_in = (a.handle_in.0 * w, a.handle_in.1 * h);
                    a.handle_out = (a.handle_out.0 * w, a.handle_out.1 * h);
                    a
                })
                .collect();
            Some(("Crescent", closed(anchors)))
        }
        // Arrow: a `Thck`-of-the-height shaft with a head at either end
        // when its style enum says so; head length is `LPr1`/`RPr1` of
        // the height.
        b"ShDA" => {
            let sh = f(b"Thck", 0.35).clamp(0.0, 1.0) * h * 0.5;
            let head = |style: Option<&Value>| match style {
                Some(Value::Enum { id, .. }) => *id != 0,
                _ => true,
            };
            let l_head = head(shpe.field(b"LSty"));
            let r_head = head(shpe.field(b"RSty"));
            let lw = if l_head {
                (f(b"LPr1", 0.5) * h).min(w * 0.45)
            } else {
                0.0
            };
            let rw = if r_head {
                (f(b"RPr1", 0.5) * h).min(w * 0.45)
            } else {
                0.0
            };
            let mut a = Vec::new();
            if l_head {
                a.push(Anchor::corner(x0, cy));
                a.push(Anchor::corner(x0 + lw, y0));
                a.push(Anchor::corner(x0 + lw, cy - sh));
            } else {
                a.push(Anchor::corner(x0, cy - sh));
            }
            if r_head {
                a.push(Anchor::corner(x1 - rw, cy - sh));
                a.push(Anchor::corner(x1 - rw, y0));
                a.push(Anchor::corner(x1, cy));
                a.push(Anchor::corner(x1 - rw, y1));
                a.push(Anchor::corner(x1 - rw, cy + sh));
            } else {
                a.push(Anchor::corner(x1, cy - sh));
                a.push(Anchor::corner(x1, cy + sh));
            }
            if l_head {
                a.push(Anchor::corner(x0 + lw, cy + sh));
                a.push(Anchor::corner(x0 + lw, y1));
            } else {
                a.push(Anchor::corner(x0, cy + sh));
            }
            Some(("Arrow", closed(a)))
        }
        // Cog: `Teth` teeth from `IRad` out to the rim — each tooth's
        // top spans `TtSz` of its period, the root gap `NtSz` — plus a
        // `Hole` bore. `Curv` bends the flanks; only straight rebuilds.
        b"ShCg" => {
            if f(b"Curv", 0.0).abs() > 0.01 {
                return None;
            }
            let teeth = u16_of(shpe, b"Teth").unwrap_or(12).clamp(3, 200) as usize;
            let root = f(b"IRad", 0.85).clamp(0.0, 1.0);
            let hole = f(b"Hole", 0.2).clamp(0.0, 0.999);
            let ts = f(b"TtSz", 0.37).clamp(0.0, 1.0);
            let ns = f(b"NtSz", 0.42).clamp(0.0, 1.0);
            let step = TAU / teeth as f32;
            let mut a = Vec::with_capacity(teeth * 4);
            for k in 0..teeth {
                let c = -FRAC_PI_2 + step * k as f32;
                let g = c + step * 0.5; // the gap between this tooth and the next
                for (r, ang) in [
                    (1.0, c - ts * step * 0.5),
                    (1.0, c + ts * step * 0.5),
                    (root, g - ns * step * 0.5),
                    (root, g + ns * step * 0.5),
                ] {
                    a.push(unit_anchor(ang.cos() * r, ang.sin() * r, x0, y0, x1, y1));
                }
            }
            let mut subs = vec![(a, true)];
            if hole > 0.001 {
                let (rx, ry) = (w * 0.5 * hole, h * 0.5 * hole);
                subs.push((ellipse_anchors(cx - rx, cy - ry, cx + rx, cy + ry), true));
            }
            Some(("Cog", subs))
        }
        // Callout (rounded rectangle): the balloon over the top
        // 1 − `TlHg` of the box, its corner radii in `ShCR`, with a tail
        // `TlWd` wide rooted at `TlRP` of the width pointing to its tip
        // at `TlEP` on the bottom edge.
        b"ShCR" => {
            let tail_h = f(b"TlHg", 0.3).clamp(0.0, 0.95);
            let tail_w = f(b"TlWd", 0.15).clamp(0.0, 1.0);
            let root = f(b"TlRP", 0.4).clamp(0.0, 1.0);
            let tip = f(b"TlEP", 0.2).clamp(0.0, 1.0);
            let yr = y1 - tail_h * h;
            let radii = match shpe.field(b"ShCR") {
                Some(Value::VecF(r)) if r.len() >= 4 => [r[0], r[1], r[2], r[3]],
                _ => [0.25; 4],
            };
            // Unlike the plain rounded rectangle, the callout's radii
            // scale by the full shorter side of the balloon (measured
            // off the fixture's own render: 0.25 → 23.9px on a 92px
            // balloon), not half of it.
            let scale = if matches!(shpe.field(b"AbSz"), Some(Value::Bool(true))) {
                1.0
            } else {
                w.min(yr - y0)
            };
            let radii = radii.map(|r| (r * scale).clamp(0.0, w.min(yr - y0) * 0.5));
            let mut a = rounded_rect_anchors(x0, y0, x1, yr, radii);
            // The bottom edge runs right-to-left; splice the tail in
            // after the bottom-right corner's anchors.
            let after_br = a
                .iter()
                .position(|an| an.point.1 >= yr - 0.01 && an.point.0 > cx)
                .map(|i| i + 1)
                .unwrap_or(a.len());
            let half = tail_w * w * 0.5;
            let rc = x0 + root * w;
            a.splice(
                after_br..after_br,
                [
                    Anchor::corner((rc + half).min(x1), yr),
                    Anchor::corner(x0 + tip * w, y1),
                    Anchor::corner((rc - half).max(x0), yr),
                ],
            );
            Some(("Callout", closed(a)))
        }
        // Callout (ellipse): the balloon over the top 1 − `TlHg`, its
        // tail rooted where the centre-to-tip direction meets the
        // ellipse, `TlAn` of parametric angle wide, tip at `TlEP` on
        // the bottom edge.
        b"ShCE" => {
            let tail_h = f(b"TlHg", 0.2).clamp(0.0, 0.95);
            let tip_x = x0 + f(b"TlEP", 0.15).clamp(0.0, 1.0) * w;
            let half_ang = (f(b"TlAn", 0.35) * 0.5).clamp(0.02, 1.5);
            let yr = y1 - tail_h * h;
            let (rx, ry) = (w * 0.5, (yr - y0) * 0.5);
            let cey = (y0 + yr) * 0.5;
            let t_dir = ((y1 - cey) / ry).atan2((tip_x - cx) / rx);
            let mut a = arc_anchors(cx, cey, rx, ry, t_dir + half_ang, t_dir - half_ang + TAU);
            a.push(Anchor::corner(tip_x, y1));
            Some(("Callout", closed(a)))
        }
        // Tear: an apex over a bulb. The geometry below reproduces the
        // default (Ball 0.25, Curv 0.3, Bend 0, Tail 0.5) exactly —
        // convex sides fitted numerically to Affinity's own render,
        // widest at 51.5% of the height, an elliptical bulb below —
        // and scales the cone with `Tail`; the other parameters warn.
        b"ShTr" => {
            let tail = f(b"Tail", 0.5).clamp(0.05, 0.95);
            if (f(b"Ball", 0.25) - 0.25).abs() > 0.02
                || (f(b"Curv", 0.3) - 0.3).abs() > 0.02
                || f(b"Bend", 0.0).abs() > 0.02
            {
                log::warn!("affinity: tear with non-default ball/curve/bend; shape approximate");
            }
            let ym = y0 + (tail * 1.03).min(0.9) * h;
            let hw = w * 0.5;
            let (dx, dy) = (0.410 * hw, 0.159 * h);
            let v = 0.161 * h;
            let mut a = vec![Anchor {
                point: (cx, y0),
                handle_in: (dx, dy),
                handle_out: (-dx, dy),
            }];
            let mut bottom = arc_anchors(cx, ym, hw, y1 - ym, PI, 0.0);
            if let Some(first) = bottom.first_mut() {
                first.handle_in = (0.0, -v);
            }
            if let Some(last) = bottom.last_mut() {
                last.handle_out = (0.0, -v);
            }
            a.extend(bottom);
            Some(("Tear", closed(a)))
        }
        _ => None,
    }
}

/// A circular arc in unit space from the top tip (0.5, 0) to the
/// bottom tip (0.5, 1) — reversed when `downward` is false — bowing
/// sideways with sagitta `bow`/2 (negative bows left). Nearly-zero bows
/// degenerate to the straight chord.
pub(super) fn bow_arc_unit(bow: f32, downward: bool) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    use std::f32::consts::PI;
    let s = (bow.abs() * 0.5).min(0.5);
    if s < 0.005 {
        let mut pts = vec![Anchor::corner(0.5, 0.0), Anchor::corner(0.5, 1.0)];
        if !downward {
            pts.reverse();
        }
        return pts;
    }
    let r = (s * s + 0.25) / (2.0 * s);
    // Bulge left: the circle's centre sits right of the chord.
    let cxu = 0.5 + (r - s);
    let phi = (0.5f32).atan2(r - s);
    let (t0, t1) = if downward {
        (PI + phi, PI - phi)
    } else {
        (PI - phi, PI + phi)
    };
    let mut a = arc_anchors(cxu, 0.5, r, r, t0, t1);
    if bow > 0.0 {
        for an in &mut a {
            an.point.0 = 1.0 - an.point.0;
            an.handle_in.0 = -an.handle_in.0;
            an.handle_out.0 = -an.handle_out.0;
        }
    }
    a
}

/// Anchors tracing the elliptical arc `t0`→`t1` (radians in screen
/// space, so y grows downward: point = centre + (rx·cos t, ry·sin t)),
/// split into ≤90° cubic segments. The endpoints carry only their
/// arc-side handle, so a straight edge can follow either end.
pub(super) fn arc_anchors(
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    t0: f32,
    t1: f32,
) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let n = ((t1 - t0).abs() / std::f32::consts::FRAC_PI_2)
        .ceil()
        .max(1.0) as usize;
    let dt = (t1 - t0) / n as f32;
    let k = 4.0 / 3.0 * (dt / 4.0).tan();
    (0..=n)
        .map(|i| {
            let t = t0 + dt * i as f32;
            let (px, py) = (cx + rx * t.cos(), cy + ry * t.sin());
            let (dx, dy) = (-rx * t.sin() * k, ry * t.cos() * k);
            let mut a = Anchor::corner(px, py);
            if i > 0 {
                a.handle_in = (-dx, -dy);
            }
            if i < n {
                a.handle_out = (dx, dy);
            }
            a
        })
        .collect()
}

/// A corner anchor at unit-circle coordinates (centre 0, radius 1)
/// mapped onto the ellipse inscribed in the box.
pub(super) fn unit_anchor(
    ux: f32,
    uy: f32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> schist_core::Anchor {
    schist_core::Anchor::corner(
        (x0 + x1) * 0.5 + ux * (x1 - x0) * 0.5,
        (y0 + y1) * 0.5 + uy * (y1 - y0) * 0.5,
    )
}

/// Square star ("ShSS"): `Side` rectangular arms radiating from the
/// centre, the first pointing down. The arms are the middle `COut` of
/// each edge of the regular `Side`-gon whose vertices sit on the
/// inscribed ellipse — flat tips at the polygon's apothem, sides
/// perpendicular to them, adjacent arms meeting in a V notch at `COut`
/// of the radius. (All measured off Affinity's own thumbnail render.)
pub(super) fn square_star_anchors(
    shpe: &Node,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> Vec<schist_core::Anchor> {
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    let sides = u16_of(shpe, b"Side").unwrap_or(4).clamp(3, 100) as usize;
    let cut = f32_last(shpe, b"COut").unwrap_or(0.5).clamp(0.01, 0.99);
    let step = TAU / sides as f32;
    let tip = (PI / sides as f32).cos(); // the polygon's apothem
    let hw = cut * (PI / sides as f32).sin(); // arm half-width
    let mut out = Vec::with_capacity(sides * 3);
    for k in 0..sides {
        let ang = FRAC_PI_2 + step * k as f32;
        let (ux, uy) = (ang.cos(), ang.sin());
        let (px, py) = (-uy, ux); // toward the next arm
        out.push(unit_anchor(
            ux * tip - px * hw,
            uy * tip - py * hw,
            x0,
            y0,
            x1,
            y1,
        ));
        out.push(unit_anchor(
            ux * tip + px * hw,
            uy * tip + py * hw,
            x0,
            y0,
            x1,
            y1,
        ));
        let na = ang + step * 0.5;
        out.push(unit_anchor(na.cos() * cut, na.sin() * cut, x0, y0, x1, y1));
    }
    out
}

/// Cloud ("ShCl"): `Bubl` circular arcs bulging outward around the
/// inscribed ellipse, meeting each other at `IRad` of the radius.
pub(super) fn cloud_anchors(
    shpe: &Node,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> Vec<schist_core::Anchor> {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let bubbles = u16_of(shpe, b"Bubl").unwrap_or(12).clamp(3, 100) as usize;
    let meet = f32_last(shpe, b"IRad").unwrap_or(0.8).clamp(0.1, 0.999);
    let step = TAU / bubbles as f32;

    // Anchors alternate meet points and bubble peaks; handles come from
    // the exact circular arc through (meet, peak, meet) in unit space.
    let mut out: Vec<schist_core::Anchor> = (0..bubbles * 2)
        .map(|i| {
            let ang = -FRAC_PI_2 + step * 0.5 * i as f32 - step * 0.5;
            let r = if i % 2 == 0 { meet } else { 1.0 };
            schist_core::Anchor::corner(ang.cos() * r, ang.sin() * r)
        })
        .collect();
    let n = out.len();
    for k in 0..bubbles {
        let p0 = out[2 * k].point;
        let p1 = out[2 * k + 1].point;
        let p2 = out[(2 * k + 2) % n].point;
        if let Some((c, r)) = circle_through(p0, p1, p2) {
            let ang = |p: (f32, f32)| (p.1 - c.1).atan2(p.0 - c.0);
            let (a0, mut a1, mut a2) = (ang(p0), ang(p1), ang(p2));
            // Walk monotonically a0 → a1 → a2 the short way round.
            while a1 - a0 > std::f32::consts::PI {
                a1 -= TAU;
            }
            while a1 - a0 < -std::f32::consts::PI {
                a1 += TAU;
            }
            while a2 - a1 > std::f32::consts::PI {
                a2 -= TAU;
            }
            while a2 - a1 < -std::f32::consts::PI {
                a2 += TAU;
            }
            let tangent = |a: f32, k: f32| (-a.sin() * k * r, a.cos() * k * r);
            let k01 = (4.0 / 3.0) * ((a1 - a0) / 4.0).tan();
            let k12 = (4.0 / 3.0) * ((a2 - a1) / 4.0).tan();
            out[2 * k].handle_out = tangent(a0, k01);
            out[2 * k + 1].handle_in = {
                let t = tangent(a1, k01);
                (-t.0, -t.1)
            };
            out[2 * k + 1].handle_out = tangent(a1, k12);
            out[(2 * k + 2) % n].handle_in = {
                let t = tangent(a2, k12);
                (-t.0, -t.1)
            };
        }
    }
    for a in &mut out {
        let mapped = unit_anchor(a.point.0, a.point.1, x0, y0, x1, y1);
        let (sx, sy) = ((x1 - x0) * 0.5, (y1 - y0) * 0.5);
        a.point = mapped.point;
        a.handle_in = (a.handle_in.0 * sx, a.handle_in.1 * sy);
        a.handle_out = (a.handle_out.0 * sx, a.handle_out.1 * sy);
    }
    out
}

/// The circle through three points, unless they are collinear.
pub(super) fn circle_through(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
) -> Option<((f32, f32), f32)> {
    let d = 2.0 * (p0.0 * (p1.1 - p2.1) + p1.0 * (p2.1 - p0.1) + p2.0 * (p0.1 - p1.1));
    if d.abs() < 1e-9 {
        return None;
    }
    let sq = |p: (f32, f32)| p.0 * p.0 + p.1 * p.1;
    let cx = (sq(p0) * (p1.1 - p2.1) + sq(p1) * (p2.1 - p0.1) + sq(p2) * (p0.1 - p1.1)) / d;
    let cy = (sq(p0) * (p2.0 - p1.0) + sq(p1) * (p0.0 - p2.0) + sq(p2) * (p1.0 - p0.0)) / d;
    Some(((cx, cy), (p0.0 - cx).hypot(p0.1 - cy)))
}

/// Heart ("ShHt"): two Bezier lobes over a point, the notch between
/// them deepening as `Sprd` grows. Proportions traced from Affinity's
/// own thumbnail render at the default spread of 0.2: lobes touch the
/// top, flanks stay near-vertical to mid-height, sides sweep gently
/// convex into the tip.
pub(super) fn heart_anchors(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    spread: f32,
) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let (w, h) = (x1 - x0, y1 - y0);
    let p = |ux: f32, uy: f32| (x0 + ux * w, y0 + uy * h);
    let v = |ux: f32, uy: f32| (ux * w, uy * h);
    let notch = (spread * 0.82).clamp(0.0, 0.6);
    let a = |pt: (f32, f32), hin: (f32, f32), hout: (f32, f32)| Anchor {
        point: pt,
        handle_in: hin,
        handle_out: hout,
    };
    vec![
        // Bottom tip; sides sweep out symmetrically.
        a(p(0.5, 1.0), v(0.26, -0.14), v(-0.26, -0.14)),
        // Left flank.
        a(p(0.0, 0.35), v(0.0, 0.25), v(0.0, -0.20)),
        // Left lobe top.
        a(p(0.25, 0.0), v(-0.14, 0.0), v(0.14, 0.0)),
        // Notch.
        a(p(0.5, notch), v(-0.045, -0.10), v(0.045, -0.10)),
        // Right lobe top.
        a(p(0.75, 0.0), v(-0.14, 0.0), v(0.14, 0.0)),
        // Right flank.
        a(p(1.0, 0.35), v(0.0, -0.20), v(0.0, 0.25)),
    ]
}

/// Anchors for a clockwise rounded rectangle with per-corner radii
/// `[top-left, top-right, bottom-right, bottom-left]` (a plain corner
/// at r = 0).
pub(super) fn rounded_rect_anchors(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: [f32; 4],
) -> Vec<schist_core::Anchor> {
    cornered_rect_anchors(x0, y0, x1, y1, r, [0; 4])
}

/// Rectangle anchors with per-corner treatments (`CTyp`): 0 rounded ·
/// 1 straight chamfer · 2 concave (the arc bends inward, centred on
/// the corner) · 3 cutout (a square bite through the inner point).
pub(super) fn cornered_rect_anchors(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: [f32; 4],
    types: [u16; 4],
) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let [tl, tr, br, bl] = r.map(|v| if v < 0.25 { 0.0 } else { v });
    let a = |px, py, ix, iy, ox, oy| Anchor {
        point: (px, py),
        handle_in: (ix, iy),
        handle_out: (ox, oy),
    };
    let mut out = Vec::with_capacity(8);
    // Each corner contributes its arc's entry and exit anchors — or,
    // when square, the corner point itself.
    for (ci, (radius, corner, entry, exit)) in [
        (tl, (x0, y0), (0.0f32, 1.0f32), (1.0f32, 0.0f32)),
        (tr, (x1, y0), (-1.0, 0.0), (0.0, 1.0)),
        (br, (x1, y1), (0.0, -1.0), (-1.0, 0.0)),
        (bl, (x0, y1), (1.0, 0.0), (0.0, -1.0)),
    ]
    .into_iter()
    .enumerate()
    {
        if radius == 0.0 {
            out.push(Anchor::corner(corner.0, corner.1));
            continue;
        }
        let ctype = types[ci];
        let (en, ex) = (
            (corner.0 + entry.0 * radius, corner.1 + entry.1 * radius),
            (corner.0 + exit.0 * radius, corner.1 + exit.1 * radius),
        );
        match ctype {
            1 => {
                // Straight: chamfer between the two radius points.
                out.push(Anchor::corner(en.0, en.1));
                out.push(Anchor::corner(ex.0, ex.1));
                continue;
            }
            2 => {
                // Concave: the arc's centre is the corner itself.
                let k = KAPPA * radius;
                out.push(a(en.0, en.1, 0.0, 0.0, exit.0 * k, exit.1 * k));
                out.push(a(ex.0, ex.1, entry.0 * k, entry.1 * k, 0.0, 0.0));
                continue;
            }
            3 => {
                // Cutout: a square bite via the inner point.
                out.push(Anchor::corner(en.0, en.1));
                out.push(Anchor::corner(
                    corner.0 + (entry.0 + exit.0) * radius,
                    corner.1 + (entry.1 + exit.1) * radius,
                ));
                out.push(Anchor::corner(ex.0, ex.1));
                continue;
            }
            _ => {}
        }
        // The straight edges either side keep zero handles; the arc
        // between the two anchors bends toward the corner point.
        let k = KAPPA * radius;
        out.push(a(
            corner.0 + entry.0 * radius,
            corner.1 + entry.1 * radius,
            0.0,
            0.0,
            -entry.0 * k,
            -entry.1 * k,
        ));
        out.push(a(
            corner.0 + exit.0 * radius,
            corner.1 + exit.1 * radius,
            -exit.0 * k,
            -exit.1 * k,
            0.0,
            0.0,
        ));
    }
    out
}

/// Anchors for a clockwise ellipse filling the box.
pub(super) fn ellipse_anchors(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<schist_core::Anchor> {
    use schist_core::Anchor;
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (kx, ky) = (KAPPA * (x1 - x0) / 2.0, KAPPA * (y1 - y0) / 2.0);
    vec![
        Anchor::smooth(cx, y0, kx, 0.0),
        Anchor::smooth(x1, cy, 0.0, ky),
        Anchor::smooth(cx, y1, -kx, 0.0),
        Anchor::smooth(x0, cy, 0.0, -ky),
    ]
}

/// Rasterize a vector shape into the layer's tiles: fill (solid or
/// gradient-shaded), then stroke composited over it, blitted once.
pub(super) fn rasterize_shape(
    layer: &mut Layer,
    shape: &schist_core::VectorShape,
    gradient: Option<&GradientFill>,
) {
    let mut builder = schist_vector::PathBuilder::new();
    for sub in &shape.path.subpaths {
        let Some(first) = sub.anchors.first() else {
            continue;
        };
        builder.move_to(first.point.0, first.point.1);
        let n = sub.anchors.len();
        for i in 1..=n {
            let from = &sub.anchors[i - 1];
            let to = &sub.anchors[i % n];
            if i == n && !sub.closed {
                break;
            }
            builder.cubic_to(
                from.point.0 + from.handle_out.0,
                from.point.1 + from.handle_out.1,
                to.point.0 + to.handle_in.0,
                to.point.1 + to.handle_in.1,
                to.point.0,
                to.point.1,
            );
        }
        if sub.closed {
            builder.close();
        }
    }
    let flat = builder.build(0.25);

    let pad = shape
        .stroke
        .map(|(_, w)| w / 2.0 + 1.0)
        .unwrap_or(1.0)
        .ceil() as i32;
    let mut rect = shape.path.bounds();
    rect.left -= pad;
    rect.top -= pad;
    rect.right += pad;
    rect.bottom += pad;
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    if w == 0 || h == 0 || w * h > (1 << 28) {
        return;
    }

    // Straight-alpha source-over of one coverage pixel.
    fn over(px: &mut [u8], c: [u8; 4], cov: u8) {
        let sa = cov as u32 * c[3] as u32 / 255;
        let da = px[3] as u32;
        let out_a = sa + da * (255 - sa) / 255;
        if out_a == 0 {
            return;
        }
        for i in 0..3 {
            px[i] = ((c[i] as u32 * sa + px[i] as u32 * da * (255 - sa) / 255) / out_a) as u8;
        }
        px[3] = out_a as u8;
    }

    let mut rgba = vec![0u8; w * h * 4];
    fn layer_in(rgba: &mut [u8], cov: Vec<u8>, color: schist_color::Rgba) {
        let c = color.to_u8();
        for (px, &a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&cov) {
            if a > 0 {
                over(px, c, a);
            }
        }
    }
    let rule = if shape.even_odd {
        schist_vector::FillRule::EvenOdd
    } else {
        schist_vector::FillRule::NonZero
    };
    if let Some(g) = gradient {
        let cov = schist_vector::rasterize(&flat, rect, rule);
        let (dx, dy) = (g.end.0 - g.start.0, g.end.1 - g.start.1);
        let len2 = (dx * dx + dy * dy).max(1e-9);
        for y in 0..h {
            for x in 0..w {
                let a = cov[y * w + x];
                if a == 0 {
                    continue;
                }
                let px = (rect.left + x as i32) as f64 + 0.5 - g.start.0;
                let py = (rect.top + y as i32) as f64 + 0.5 - g.start.1;
                let t = if g.radial {
                    ((px * px + py * py) / len2).sqrt()
                } else {
                    (px * dx + py * dy) / len2
                };
                over(&mut rgba[(y * w + x) * 4..][..4], g.color_at(t as f32), a);
            }
        }
    } else if shape.fill.a > 0.0 {
        layer_in(
            &mut rgba,
            schist_vector::rasterize(&flat, rect, rule),
            shape.fill,
        );
    }
    if let Some((color, width)) = shape.stroke {
        let stroked = schist_vector::stroke_path(
            &flat,
            schist_vector::StrokeStyle::new(width)
                .with_cap(schist_vector::LineCap::Round)
                .with_join(schist_vector::LineJoin::Round),
        );
        layer_in(
            &mut rgba,
            schist_vector::rasterize(&stroked, rect, schist_vector::FillRule::NonZero),
            color,
        );
    }
    blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        Depth::Eight,
        rect,
        &rgba,
    );
}

/// Write a gray coverage buffer (one byte per pixel, rect-sized) into
/// mask tiles at `rect`.
pub(super) fn blit_mask(tiles: &mut schist_core::MaskTileMap, rect: IntRect, gray: &[u8]) {
    use schist_core::{TileCoord, TILE_SIZE};
    let w = rect.width() as usize;
    for coord in TileCoord::covering(&rect) {
        let trect = coord.rect();
        let clip = trect.intersect(&rect);
        if clip.is_empty() {
            continue;
        }
        let buf = tiles.get_mut_or_insert(coord);
        for y in clip.top..clip.bottom {
            let sy = (y - rect.top) as usize;
            let ly = (y - trect.top) as usize;
            for x in clip.left..clip.right {
                let sx = (x - rect.left) as usize;
                let lx = (x - trect.left) as usize;
                buf[ly * TILE_SIZE as usize + lx] = gray[sy * w + sx];
            }
        }
    }
}
