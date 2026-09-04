//! Rank images against a text query, through the same towers and
//! preprocessing the gallery search uses:
//!
//! ```sh
//! SCHIST_MODEL_DIR=/path/with/embed-{image,text}.onnx \
//!     cargo run -p schist-neural --example embed -- "a dog" a.jpg b.png …
//! ```

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(query) = args.next() else {
        anyhow::bail!("usage: embed <query> <image>…");
    };
    let text = schist_neural::embed::embed_text(&query)
        .ok_or_else(|| anyhow::anyhow!("embed-text is not installed"))?;
    let (w, h) = schist_neural::spec("embed-image")
        .expect("catalogued")
        .input
        .dims();
    let mut ranked = Vec::new();
    for path in args {
        let img = image::open(&path)?.into_rgba8();
        let img = image::imageops::resize(
            &img,
            w as u32,
            h as u32,
            image::imageops::FilterType::Triangle,
        );
        let mut rgb = Vec::with_capacity(w * h * 3);
        for px in img.pixels() {
            rgb.extend([
                px.0[0] as f32 / 255.0,
                px.0[1] as f32 / 255.0,
                px.0[2] as f32 / 255.0,
            ]);
        }
        let vec = schist_neural::embed::embed_image(&rgb)
            .ok_or_else(|| anyhow::anyhow!("embed-image is not installed"))?;
        let score: f32 = vec.iter().zip(&text).map(|(a, b)| a * b).sum();
        ranked.push((score, path));
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (score, path) in ranked {
        println!("{score:+.4}  {path}");
    }
    Ok(())
}
