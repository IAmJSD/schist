//! Print what `decode` makes of a file, without developing it:
//! `rawinfo <file>...`. The quickest way to compare a decoder's
//! metadata against `raw-identify -v -w`.

fn main() {
    for path in std::env::args().skip(1) {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{path}: {e}");
                continue;
            }
        };
        println!("{path}: probe {:?}", schist_codec_raw::probe(&bytes));
        let started = std::time::Instant::now();
        match schist_codec_raw::decode(&bytes) {
            Ok(raw) => {
                println!(
                    "  {} / {} (table: {} / {}), decoded in {:.2?}",
                    raw.make,
                    raw.model,
                    raw.clean_make,
                    raw.clean_model,
                    started.elapsed()
                );
                println!(
                    "  frame {}x{} cpp {} cfa {:?}",
                    raw.width, raw.height, raw.cpp, raw.cfa
                );
                println!(
                    "  black {:?} white {} wb {:?} orientation {:?}",
                    raw.black_levels, raw.white_level, raw.wb_coeffs, raw.orientation
                );
                println!("  crop {:?}", raw.crop);
                println!("  matrix {:?}", raw.color_matrix);
                println!(
                    "  preview {} bytes, {:?}",
                    raw.preview.as_ref().map_or(0, |p| p.len()),
                    raw.metadata
                );
                // `RAWINFO_COLUMNS=n`: the mean of each of the last n
                // columns over the middle rows, for looking at masked
                // borders.
                if let Ok(n) = std::env::var("RAWINFO_COLUMNS") {
                    let n: usize = n.parse().unwrap_or(0);
                    let (w, h) = (raw.width, raw.height);
                    for c in w.saturating_sub(n)..w {
                        let (mut sum, mut squares, mut count) = (0f64, 0f64, 0f64);
                        for r in (h / 4..h * 3 / 4).step_by(2) {
                            let v = raw.data.get(r * w + c) as f64;
                            sum += v;
                            squares += v * v;
                            count += 1.0;
                        }
                        let mean = sum / count;
                        println!(
                            "  column {c}: {mean:.0} sd {:.1}",
                            (squares / count - mean * mean).max(0.0).sqrt()
                        );
                    }
                }
            }
            Err(e) => println!("  error: {e}"),
        }
    }
}
