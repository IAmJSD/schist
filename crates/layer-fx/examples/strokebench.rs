//! Layer-style stroke cost against the window search it replaced. Run
//! with `cargo run --release -p schist-layer-fx --example strokebench`.
//!
//! `signed_distance` used to scan a `(2r+1)²` window per pixel; the early
//! break only fires within one pixel of an edge, so every other pixel
//! paid the whole window and the cost grew as r². The old formulation is
//! inlined here, exactly as the equivalence test carries it.

use std::hint::black_box;
use std::time::Instant;

/// A disc, so most of the plane is far from the edge -- the case the
/// early break never helped with.
fn disc(w: usize, h: usize) -> Vec<f32> {
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let r = (w.min(h) as f32) * 0.35;
    (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            if (x - cx).hypot(y - cy) <= r {
                1.0
            } else {
                0.0
            }
        })
        .collect()
}

/// The `(2r+1)²` window search, as `signed_distance` used to be.
fn brute_force_signed_distance(alpha: &[f32], w: usize, h: usize, limit: f32) -> Vec<f32> {
    let r = limit.ceil() as i32;
    let inside = |x: i32, y: i32| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < w
            && (y as usize) < h
            && alpha[y as usize * w + x as usize] >= 0.5
    };
    let mut out = vec![0f32; w * h];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let here = inside(x, y);
            let mut best = limit;
            'search: for dy in -r..=r {
                for dx in -r..=r {
                    if inside(x + dx, y + dy) == here {
                        continue;
                    }
                    let d = ((dx * dx + dy * dy) as f32).sqrt();
                    if d < best {
                        best = d;
                        if best <= 1.0 {
                            break 'search;
                        }
                    }
                }
            }
            out[y as usize * w + x as usize] = if here { -best } else { best };
        }
    }
    out
}

fn main() {
    let (w, h) = (1000usize, 1000usize);
    let alpha = disc(w, h);
    for size in [4.0f32, 12.0, 30.0, 250.0] {
        let limit = size + 2.0;
        let start = Instant::now();
        black_box(brute_force_signed_distance(&alpha, w, h, limit));
        let old_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        black_box(schist_layer_fx::signed_distance(&alpha, w, h, limit));
        let new_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "outside stroke {w}x{h} size={size:<5}   before {old_ms:9.1} ms   after {new_ms:8.1} ms   {:.0}x",
            old_ms / new_ms.max(0.001)
        );
    }
}
