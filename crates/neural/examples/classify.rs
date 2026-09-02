//! Print a classifier model's scores for an image, through the same
//! loader and preprocessing the app uses:
//!
//! ```sh
//! SCHIST_MODEL_DIR=/path/with/nsfw.onnx \
//!     cargo run -p schist-neural --example classify -- nsfw photo.png
//! ```

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(id), Some(path)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: classify <model-id> <image>");
    };
    let spec = schist_neural::spec(Box::leak(id.into_boxed_str()))
        .ok_or_else(|| anyhow::anyhow!("no such model in the catalogue"))?;
    let bytes = std::fs::read(schist_neural::path_of(spec))?;
    let model = schist_neural::Model::from_bytes(spec, &bytes)?;
    let (w, h) = (224usize, 224usize);
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
    let scores = model.run_scores(&rgb)?;
    println!("{path}: {scores:?}");
    Ok(())
}
