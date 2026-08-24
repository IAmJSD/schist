//! Content-Aware Scale throughput. Run with
//! `cargo run --release -p schist-tools-warp --example casbench`.

use schist_tools_warp::scale::Image;
use std::time::Instant;

fn subject(w: usize, h: usize) -> Image {
    let mut px = Vec::with_capacity(w * h * 4);
    let mut state = 0x2545F4914F6CDD1Du64;
    for y in 0..h {
        for x in 0..w {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let n = (state >> 40) as f32 / 16777216.0;
            // A textured subject in the middle, smooth sky either side, so
            // the carve has somewhere obvious to eat.
            let inside = x > w / 3 && x < 2 * w / 3 && y > h / 4 && y < 3 * h / 4;
            if inside {
                px.extend_from_slice(&[n, 1.0 - n, (n * 3.0) % 1.0, 1.0]);
            } else {
                let t = y as f32 / h as f32;
                px.extend_from_slice(&[0.4 + t * 0.2, 0.5 + t * 0.2, 0.9, 1.0]);
            }
        }
    }
    Image {
        width: w,
        height: h,
        px,
        protect: vec![0.0; w * h],
    }
}

fn main() {
    for (w, h, seams) in [
        (800usize, 600usize, 80usize),
        (2000, 1500, 200),
        (4000, 3000, 400),
    ] {
        let img = subject(w, h);
        let start = Instant::now();
        let mut carved = Image {
            width: img.width,
            height: img.height,
            px: img.px.clone(),
            protect: img.protect.clone(),
        };
        carved.content_aware_resize(w - seams, h);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{w}x{h} -> {}x{h} ({seams} seams): {ms:8.1} ms   {:.2} ms/seam",
            w - seams,
            ms / seams as f64
        );
    }
}
