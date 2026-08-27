//! The CPU-side complexity changes, each timed against the formulation it
//! replaced. Run with
//! `cargo run --release -p schist-fx --example cpubench`.
//!
//! The old implementations are inlined here rather than kept in the
//! library, the same way the equivalence tests carry them: the point is
//! to be able to re-measure the claim, not to keep the slow path alive.

use std::time::Instant;

fn noise(w: usize, h: usize) -> Vec<f32> {
    let mut state = 0x2545F4914F6CDD1Du64;
    (0..w * h * 4)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / 16777216.0
        })
        .collect()
}

fn time(label: &str, old: impl FnOnce(), new: impl FnOnce()) {
    let start = Instant::now();
    old();
    let old_ms = start.elapsed().as_secs_f64() * 1000.0;
    let start = Instant::now();
    new();
    let new_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{label:<40} before {old_ms:9.1} ms   after {new_ms:8.1} ms   {:.0}x",
        old_ms / new_ms.max(0.001)
    );
}

/// The window re-summed per pixel, single threaded, as `box_pass` was.
fn naive_box_pass(
    src: &[f32],
    dst: &mut [f32],
    width: usize,
    height: usize,
    r: usize,
    vertical: bool,
) {
    let (outer, inner) = if vertical {
        (width, height)
    } else {
        (height, width)
    };
    let stride = if vertical { width * 4 } else { 4 };
    let step = if vertical { 4 } else { width * 4 };
    let window = (r * 2 + 1) as f32;
    for o in 0..outer {
        let base = o * step;
        for i in 0..inner {
            let mut acc = [0.0f32; 4];
            for k in 0..=(r * 2) {
                let s = (i + k).saturating_sub(r).min(inner - 1);
                let at = base + s * stride;
                for c in 0..4 {
                    acc[c] += src[at + c];
                }
            }
            let at = base + i * stride;
            for c in 0..4 {
                dst[at + c] = acc[c] / window;
            }
        }
    }
}

fn naive_blur(px: &mut [f32], width: usize, height: usize, r: usize, passes: usize) {
    let mut tmp = vec![0f32; px.len()];
    for _ in 0..passes {
        naive_box_pass(px, &mut tmp, width, height, r, false);
        naive_box_pass(&tmp, px, width, height, r, true);
    }
}

fn main() {
    // 1 MP, which is a modest layer: the blur slider goes to 250.
    let (w, h) = (1000usize, 1000usize);
    let px = noise(w, h);
    for radius in [10.0f32, 50.0, 100.0] {
        let r = ((radius / 3.0f32.sqrt()).round() as usize).max(1);
        time(
            &format!("gaussian blur {w}x{h} r={radius:.0}"),
            || {
                let mut buf = px.clone();
                naive_blur(&mut buf, w, h, r, 3);
            },
            || {
                let mut buf = px.clone();
                schist_fx::blur_rgba_cpu(&mut buf, w, h, r, 3);
            },
        );
    }

    // And 12 MP, where a full-canvas preview hurts.
    let (w, h) = (4000usize, 3000usize);
    let px = noise(w, h);
    let r = ((50.0f32 / 3.0f32.sqrt()).round() as usize).max(1);
    time(
        &format!("gaussian blur {w}x{h} r=50"),
        || {
            let mut buf = px.clone();
            naive_blur(&mut buf, w, h, r, 3);
        },
        || {
            let mut buf = px.clone();
            schist_fx::blur_rgba_cpu(&mut buf, w, h, r, 3);
        },
    );
}
