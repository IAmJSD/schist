//! Filling a hole with what was probably behind it.
//!
//! Three stages, each doing the part the next one cannot.
//!
//! 1. **A network** predicts the region back, hole included. It sees the
//!    hole and its surroundings at 160 pixels square, so what it is good
//!    at is *layout* -- a horizon that continues, a wall that stays a
//!    wall, a shadow that keeps its direction. What it is bad at is fine
//!    texture, because a network fitted to absolute error always is.
//! 2. **Patch synthesis** fixes that. It is the same idea Photoshop's
//!    Content-Aware Fill is built on: every patch of the hole is matched
//!    against the picture around it, and the hole is rebuilt out of the
//!    best matches, so the texture in it is *real texture from this
//!    photograph* rather than a network's average of every photograph.
//!    Left to itself that wanders -- a search with nothing to aim at
//!    finds a plausible patch of the wrong thing -- so the network's
//!    answer is both where it starts and what it is pulled towards.
//! 3. **Diffusion** is what happens with neither: relaxation inwards
//!    from the boundary, which is right for smooth surroundings and
//!    blurry over texture. It is the seed when there is no model, and it
//!    is the whole answer when there is nothing to copy from either.
//!
//! The division of labour is the point. A small network cannot invent
//! texture and patch synthesis cannot invent structure, and a hole needs
//! both.

use rayon::prelude::*;
use schist_color::Rgba;
use schist_core::{IntRect, TileMap};

/// Fill the pixels where `hole` is true, over `rect`.
pub fn inpaint(tiles: &TileMap, rect: IntRect, hole: &[bool]) -> Vec<Rgba> {
    let (w, h) = (rect.width().max(0) as usize, rect.height().max(0) as usize);
    let mut buf: Vec<Rgba> = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            buf.push(tiles.pixel(rect.left + x as i32, rect.top + y as i32));
        }
    }
    if w == 0 || h == 0 || hole.len() != w * h || !hole.iter().any(|&v| v) {
        return buf;
    }

    let guide = model_guess(tiles, rect, hole);
    match &guide {
        Some(g) => {
            for (i, &gone) in hole.iter().enumerate() {
                if gone {
                    buf[i] = Rgba::new(
                        g[i * 3],
                        g[i * 3 + 1],
                        g[i * 3 + 2],
                        boundary_alpha(&buf, hole, w, h, i),
                    );
                }
            }
        }
        // Without a network the hole starts smooth, which is both the
        // seed the synthesis needs and the whole answer if there turns
        // out to be nothing to copy from.
        None => diffuse(&mut buf, hole, w, h),
    }
    synthesise(&mut buf, hole, guide.as_deref(), w, h);
    settle_seam(&mut buf, hole, w, h);
    buf
}

/// Take the step out of the seam.
///
/// Everything above copies real pixels from elsewhere in the photograph,
/// and elsewhere was lit slightly differently -- so the fill can be the
/// right texture and still arrive as a visible rectangle a shade off its
/// surroundings. This measures that mismatch all the way round the
/// boundary, relaxes it across the hole, and adds it: a smooth field, so
/// the texture survives and only the tone moves.
///
/// It is the correction half of seamless cloning, and relaxation is
/// exactly the tool for it -- which is the one thing diffusion was
/// always good at.
fn settle_seam(buf: &mut [Rgba], hole: &[bool], w: usize, h: usize) {
    let mut fix = vec![[0f32; 3]; w * h];
    let mut held = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if hole[i] {
                continue;
            }
            let (mut acc, mut n) = ([0f32; 3], 0f32);
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                if sx < 0 || sy < 0 || sx as usize >= w || sy as usize >= h {
                    continue;
                }
                let j = sy as usize * w + sx as usize;
                if !hole[j] {
                    continue;
                }
                acc[0] += buf[i].r - buf[j].r;
                acc[1] += buf[i].g - buf[j].g;
                acc[2] += buf[i].b - buf[j].b;
                n += 1.0;
            }
            if n > 0.0 {
                fix[i] = [acc[0] / n, acc[1] / n, acc[2] / n];
                held[i] = true;
            }
        }
    }
    if !held.iter().any(|&v| v) {
        return;
    }
    let passes = (w.min(h) as u32).clamp(8, 160);
    let mut next = fix.clone();
    for _ in 0..passes {
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if !hole[i] {
                    continue;
                }
                let (mut acc, mut n) = ([0f32; 3], 0f32);
                for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    if sx < 0 || sy < 0 || sx as usize >= w || sy as usize >= h {
                        continue;
                    }
                    let j = sy as usize * w + sx as usize;
                    if !hole[j] && !held[j] {
                        continue;
                    }
                    for c in 0..3 {
                        acc[c] += fix[j][c];
                    }
                    n += 1.0;
                }
                if n > 0.0 {
                    next[i] = [acc[0] / n, acc[1] / n, acc[2] / n];
                }
            }
        }
        std::mem::swap(&mut fix, &mut next);
    }
    for (i, &gone) in hole.iter().enumerate() {
        if gone {
            buf[i] = Rgba::new(
                (buf[i].r + fix[i][0]).clamp(0.0, 1.0),
                (buf[i].g + fix[i][1]).clamp(0.0, 1.0),
                (buf[i].b + fix[i][2]).clamp(0.0, 1.0),
                buf[i].a,
            );
        }
    }
}

/// Alpha for a filled pixel: whatever the region around the hole has.
///
/// The network answers in colour only, and a hole punched through a
/// layer that is transparent in places would otherwise come back opaque
/// black where the fill met it.
fn boundary_alpha(buf: &[Rgba], hole: &[bool], w: usize, h: usize, at: usize) -> f32 {
    let (x, y) = (at % w, at / w);
    let (mut acc, mut n) = (0.0f32, 0.0f32);
    for r in [1usize, 4, 16] {
        for (dx, dy) in [
            (r as i32, 0i32),
            (-(r as i32), 0),
            (0, r as i32),
            (0, -(r as i32)),
        ] {
            let (sx, sy) = (x as i32 + dx, y as i32 + dy);
            if sx < 0 || sy < 0 || sx as usize >= w || sy as usize >= h {
                continue;
            }
            let i = sy as usize * w + sx as usize;
            if !hole[i] {
                acc += buf[i].a;
                n += 1.0;
            }
        }
        if n > 0.0 {
            break;
        }
    }
    match n > 0.0 {
        true => acc / n,
        false => 1.0,
    }
}

/// The network's guess at the whole region, as interleaved RGB over
/// `rect`, or `None` when there is no model or it could not run.
fn model_guess(tiles: &TileMap, rect: IntRect, hole: &[bool]) -> Option<Vec<f32>> {
    let model = schist_neural::get("inpaint")?;
    // The network is shown more than the hole, because what goes in a
    // hole is decided by what is around it. Half the hole again on each
    // side, kept inside the pixels the layer actually has.
    let grow = (rect.width().max(rect.height()) / 2).clamp(16, 512);
    let have = tiles.content_bounds();
    let win = IntRect::new(
        (rect.left - grow).max(have.left.min(rect.left)),
        (rect.top - grow).max(have.top.min(rect.top)),
        (rect.right + grow).min(have.right.max(rect.right)),
        (rect.bottom + grow).min(have.bottom.max(rect.bottom)),
    );
    let (ww, wh) = (win.width().max(0) as usize, win.height().max(0) as usize);
    if ww < 8 || wh < 8 {
        return None;
    }

    let (w, h) = (rect.width() as usize, rect.height() as usize);
    let (ox, oy) = (
        (rect.left - win.left) as usize,
        (rect.top - win.top) as usize,
    );
    let mut rgb = Vec::with_capacity(ww * wh * 3);
    let mut gone = vec![false; ww * wh];
    for y in 0..wh {
        for x in 0..ww {
            let c = tiles.pixel(win.left + x as i32, win.top + y as i32);
            rgb.extend_from_slice(&[c.r, c.g, c.b]);
            let (ix, iy) = (x as i32 - ox as i32, y as i32 - oy as i32);
            if ix >= 0 && iy >= 0 && (ix as usize) < w && (iy as usize) < h {
                gone[y * ww + x] = hole[iy as usize * w + ix as usize];
            }
        }
    }
    let filled = schist_neural::inpaint(&model, &rgb, ww, wh, &gone)
        .map_err(|e| log::warn!("content-aware fill: {e:#}"))
        .ok()?;

    let mut out = vec![0.0f32; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let from = ((oy + y) * ww + ox + x) * 3;
            out[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(&filled[from..from + 3]);
        }
    }
    Some(out)
}

/// Jacobi relaxation over the hole: every pass replaces each hole pixel
/// with the average of its four neighbours, so colour seeps inwards from
/// the boundary until the patch is smooth.
fn diffuse(buf: &mut Vec<Rgba>, hole: &[bool], w: usize, h: usize) {
    // Seed with the mean of the boundary so relaxation starts somewhere
    // sensible instead of from whatever was there.
    let (mut acc, mut n) = ([0f32; 4], 0f32);
    for y in 0..h {
        for x in 0..w {
            if hole[y * w + x] {
                continue;
            }
            let touching = [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dy)| {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    sx >= 0
                        && sy >= 0
                        && (sx as usize) < w
                        && (sy as usize) < h
                        && hole[sy as usize * w + sx as usize]
                });
            if touching {
                let c = buf[y * w + x];
                acc[0] += c.r;
                acc[1] += c.g;
                acc[2] += c.b;
                acc[3] += c.a;
                n += 1.0;
            }
        }
    }
    if n == 0.0 {
        return;
    }
    let seed = Rgba::new(acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n);
    for i in 0..buf.len() {
        if hole[i] {
            buf[i] = seed;
        }
    }
    let passes = (w.min(h) as u32).clamp(8, 160);
    let mut next = buf.clone();
    for _ in 0..passes {
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                if !hole[i] {
                    continue;
                }
                let (mut acc, mut n) = ([0f32; 4], 0f32);
                for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    if sx < 0 || sy < 0 || sx as usize >= w || sy as usize >= h {
                        continue;
                    }
                    let c = buf[sy as usize * w + sx as usize];
                    acc[0] += c.r;
                    acc[1] += c.g;
                    acc[2] += c.b;
                    acc[3] += c.a;
                    n += 1.0;
                }
                if n > 0.0 {
                    next[i] = Rgba::new(acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n);
                }
            }
        }
        std::mem::swap(buf, &mut next);
    }
}

/// Half the width of a patch. Seven across: small enough to bend round a
/// curve, big enough that a match means something.
const RADIUS: usize = 3;
/// Most candidate patches a single search will look at. A patch that is
/// the answer has neighbours that are nearly the answer, so thinning the
/// list costs very little and keeps a big fill from taking minutes.
///
/// The budget is for the whole fill rather than for one search, because
/// a big hole is a lot of searches -- and a big hole also has a lot of
/// picture round it, so each search can afford to look at less of it.
const CANDIDATES: usize = 6000;
/// Candidate comparisons the whole fill is allowed, shared out between
/// its searches.
const SEARCH_BUDGET: usize = 24_000_000;
/// What the network's answer is worth, against a real pixel, when
/// scoring a candidate. Under one because a real pixel is evidence and
/// the network's answer is an opinion.
const GUIDE: f32 = 0.4;

/// Rebuild the hole out of patches of the picture around it.
///
/// Exemplar-based inpainting (Criminisi et al.): fill from the edge
/// inwards, and at every step take the piece of boundary with the most
/// to go on -- the most surrounding pixels already settled, and the
/// strongest edge running into it -- find the patch of the real picture
/// that best continues it, and copy that patch in whole. Structure gets
/// filled before texture, which is why a line across a hole comes out a
/// line rather than a smudge.
///
/// Copying whole patches, rather than averaging the good ones, is the
/// whole difference between this and a blur: every pixel that comes out
/// is a pixel that was somewhere in this photograph.
///
/// The network's answer, where there is one, is what the search is
/// scored against inside the hole -- so it decides *what* gets copied
/// where, and the photograph decides what that looks like.
fn synthesise(buf: &mut [Rgba], hole: &[bool], guide: Option<&[f32]>, w: usize, h: usize) {
    let r = RADIUS;
    if w < 2 * r + 3 || h < 2 * r + 3 {
        return;
    }
    let mut known: Vec<bool> = hole.iter().map(|&g| !g).collect();
    let mut sources = Vec::new();
    for y in r..h - r {
        for x in r..w - r {
            // A source patch has to be entirely real, or the synthesis
            // copies its own guesses back over itself.
            if (y - r..=y + r).all(|sy| (x - r..=x + r).all(|sx| known[sy * w + sx])) {
                sources.push(y * w + x);
            }
        }
    }
    if sources.len() < 16 {
        return;
    }
    // A patch placement settles about r^2 pixels, which is how many
    // searches this fill is going to want.
    let searches = (hole.iter().filter(|&&g| g).count() / (r * r)).max(1);
    let want = (SEARCH_BUDGET / searches).clamp(600, CANDIDATES);
    let stride = sources.len().div_ceil(want);
    let sources: Vec<usize> = sources.into_iter().step_by(stride).collect();

    // Candidates are read from the picture as it arrived, never from
    // what has been filled in so far, so nothing compounds.
    let original = buf.to_vec();
    let mut conf: Vec<f32> = known.iter().map(|&k| k as u8 as f32).collect();
    let mut left = hole.iter().filter(|&&g| g).count();

    // The edge of the hole, kept rather than rediscovered. Rescanning
    // the whole region for it after every patch is what turns a big
    // fill from a second into half a minute.
    let mut edge = vec![false; w * h];
    let mut boundary = Vec::new();
    for y in r..h - r {
        for x in r..w - r {
            let i = y * w + x;
            if !known[i] && touches_known(&known, w, i) {
                edge[i] = true;
                boundary.push(i);
            }
        }
    }
    // Priorities, recomputed only where the picture under them changed.
    let mut prio = vec![f32::NAN; w * h];

    while left > 0 && !boundary.is_empty() {
        boundary.retain(|&i| !known[i]);
        let mut at = usize::MAX;
        let mut top = f32::NEG_INFINITY;
        for &i in boundary.iter() {
            if prio[i].is_nan() {
                prio[i] = priority(buf, &known, &conf, w, i, r);
            }
            if prio[i] > top {
                top = prio[i];
                at = i;
            }
        }
        if at == usize::MAX {
            break;
        }
        let (tx, ty) = (at % w, at / w);
        let best = sources
            .par_chunks(256)
            .map(|chunk| {
                chunk
                    .iter()
                    .fold((f32::INFINITY, chunk[0]), |(low, pick), &s| {
                        let c = fit(buf, &original, guide, &known, w, at, s, r, low);
                        match c < low {
                            true => (c, s),
                            false => (low, pick),
                        }
                    })
            })
            .reduce(
                || (f32::INFINITY, sources[0]),
                |a, b| match b.0 < a.0 {
                    true => b,
                    false => a,
                },
            )
            .1;

        // Copy the patch in, but only over what is still missing.
        let settled = patch_confidence(&conf, &known, w, at, r);
        for dy in 0..2 * r + 1 {
            for dx in 0..2 * r + 1 {
                let to = (ty + dy - r) * w + tx + dx - r;
                if known[to] {
                    continue;
                }
                buf[to] = original[(best / w + dy - r) * w + best % w + dx - r];
                known[to] = true;
                conf[to] = settled;
                left -= 1;
            }
        }

        // Everything whose patch overlaps what just changed has a stale
        // priority, and the hole has a new edge where the patch stopped.
        let reach = 2 * r + 1;
        for y in ty.saturating_sub(reach)..(ty + reach + 1).min(h) {
            for x in tx.saturating_sub(reach)..(tx + reach + 1).min(w) {
                let i = y * w + x;
                prio[i] = f32::NAN;
                if !known[i]
                    && !edge[i]
                    && y >= r
                    && x >= r
                    && y + r < h
                    && x + r < w
                    && touches_known(&known, w, i)
                {
                    edge[i] = true;
                    boundary.push(i);
                }
            }
        }
    }
}

/// Whether a missing pixel has a settled pixel beside it.
fn touches_known(known: &[bool], w: usize, at: usize) -> bool {
    [1i32, -1, w as i32, -(w as i32)]
        .iter()
        .any(|d| known[(at as i32 + d) as usize])
}

/// How much this bit of the hole's edge has to go on: how settled its
/// neighbours are, times how strong an edge runs into it.
///
/// The second factor is what makes a line across a hole get filled
/// before the flat parts either side of it. Fill the flat parts first
/// and the line has nowhere left to go.
fn priority(buf: &[Rgba], known: &[bool], conf: &[f32], w: usize, at: usize, r: usize) -> f32 {
    let c = patch_confidence(conf, known, w, at, r);
    // The boundary's normal, from the shape of what is known.
    let nx = known[at + 1] as i32 as f32 - known[at - 1] as i32 as f32;
    let ny = known[at + w] as i32 as f32 - known[at - w] as i32 as f32;
    let n = (nx * nx + ny * ny).sqrt().max(1e-6);
    // The picture's own edge direction, turned ninety degrees.
    let lum = |j: usize| 0.299 * buf[j].r + 0.587 * buf[j].g + 0.114 * buf[j].b;
    let (gx, gy) = (lum(at + 1) - lum(at - 1), lum(at + w) - lum(at - w));
    c * ((-gy * nx + gx * ny) / n).abs().max(0.02)
}

/// How much of the patch around `at` is already settled, and how sure it
/// is: a patch mostly made of earlier guesses is worth less than one
/// mostly made of the photograph.
fn patch_confidence(conf: &[f32], known: &[bool], w: usize, at: usize, r: usize) -> f32 {
    let (x, y) = (at % w, at / w);
    let (mut acc, mut n) = (0.0f32, 0.0f32);
    for dy in y - r..=y + r {
        for dx in x - r..=x + r {
            if known[dy * w + dx] {
                acc += conf[dy * w + dx];
            }
            n += 1.0;
        }
    }
    acc / n
}

/// How badly a candidate patch fits, abandoned as soon as it passes
/// `cap`.
///
/// Settled pixels are compared against the fill as it stands -- which
/// includes what earlier steps put there, or the run would keep matching
/// against the empty hole it started with -- and missing ones against
/// the network's answer, if there is one, which is how a hole with no
/// evidence in it still gets the right thing copied into it.
#[allow(clippy::too_many_arguments)]
fn fit(
    buf: &[Rgba],
    original: &[Rgba],
    guide: Option<&[f32]>,
    known: &[bool],
    w: usize,
    at: usize,
    s: usize,
    r: usize,
    cap: f32,
) -> f32 {
    let (tx, ty) = (at % w, at / w);
    let (sx, sy) = (s % w, s / w);
    let mut total = 0.0f32;
    for dy in 0..2 * r + 1 {
        let ti = (ty + dy - r) * w + tx - r;
        let si = (sy + dy - r) * w + sx - r;
        for dx in 0..2 * r + 1 {
            let b = original[si + dx];
            let (want, weight) = match (known[ti + dx], guide) {
                (true, _) => {
                    let a = buf[ti + dx];
                    ([a.r, a.g, a.b], 1.0)
                }
                (false, Some(g)) => (
                    [g[(ti + dx) * 3], g[(ti + dx) * 3 + 1], g[(ti + dx) * 3 + 2]],
                    GUIDE,
                ),
                (false, None) => continue,
            };
            total += weight
                * ((b.r - want[0]).powi(2) + (b.g - want[1]).powi(2) + (b.b - want[2]).powi(2));
        }
        if total >= cap {
            return total;
        }
    }
    total
}
