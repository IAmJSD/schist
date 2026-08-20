// Render a contact sheet of every filter for eyeballing.
fn main() {
    use photoslop_plugin_api::{FilterValues, PluginManifest, PluginRegistry};
    let mut reg = PluginRegistry::default();
    photoslop_filters_core::CoreFiltersPlugin.register(&mut reg);
    let (w, h) = (96usize, 96usize);
    let base = {
        let mut px = vec![0.0f32; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let cx = x as f32 - 48.0;
                let cy = y as f32 - 48.0;
                let disc = (cx * cx + cy * cy).sqrt() < 30.0;
                let checker = ((x / 12) + (y / 12)) % 2 == 0;
                px[i] = if disc {
                    0.9
                } else if checker {
                    0.25
                } else {
                    0.6
                };
                px[i + 1] = if disc { 0.35 } else { y as f32 / h as f32 };
                px[i + 2] = if disc { 0.1 } else { 0.75 };
                px[i + 3] = 1.0;
            }
        }
        px
    };
    let mut names: Vec<String> = Vec::new();
    let mut tiles: Vec<Vec<f32>> = Vec::new();
    let mut ids: Vec<&str> = reg.filters().map(|f| f.id()).collect();
    ids.sort();
    for id in ids {
        let f = reg.filters().find(|f| f.id() == id).unwrap();
        let mut px = base.clone();
        f.apply(&mut px, w, h, &FilterValues::defaults(&f.params()));
        names.push(f.name().to_string());
        tiles.push(px);
    }
    // Pack into a grid PPM.
    let cols = 8usize;
    let rows = tiles.len().div_ceil(cols);
    let (gw, gh) = (cols * w, rows * h);
    let mut out = vec![0u8; gw * gh * 3];
    for (n, t) in tiles.iter().enumerate() {
        let (gx, gy) = ((n % cols) * w, (n / cols) * h);
        for y in 0..h {
            for x in 0..w {
                let s = (y * w + x) * 4;
                let d = ((gy + y) * gw + gx + x) * 3;
                for c in 0..3 {
                    out[d + c] = (t[s + c].clamp(0.0, 1.0) * 255.0) as u8;
                }
            }
        }
    }
    print!("P6\n{gw} {gh}\n255\n");
    use std::io::Write;
    std::io::stdout().write_all(&out).unwrap();
    eprintln!("{}", names.join(" | "));
}
