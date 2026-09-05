//! Detect the faces in photographs and compare them, through the same
//! models and preprocessing the gallery's People feature uses:
//!
//! ```sh
//! SCHIST_MODEL_DIR=/path/with/face.onnx,face-embed.onnx \
//!     cargo run -p schist-neural --example faces -- a.jpg b.jpg …
//! ```
//!
//! Prints each face found with its box, then the cosine between every
//! pair of faces — same person should land above ~0.36.

fn main() -> anyhow::Result<()> {
    let detector =
        schist_neural::get("face").ok_or_else(|| anyhow::anyhow!("face is not installed"))?;
    let recogniser = schist_neural::get("face-embed");
    if recogniser.is_none() {
        eprintln!("face-embed is not installed: detecting only");
    }
    let mut found: Vec<(String, Vec<f32>)> = Vec::new();
    for path in std::env::args().skip(1) {
        let img = image::open(&path)?.into_rgb8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let rgb: Vec<f32> = img.as_raw().iter().map(|v| *v as f32 / 255.0).collect();
        let started = std::time::Instant::now();
        let faces = schist_neural::faces(&detector, &rgb, w, h)?;
        println!("{path}: {} face(s) in {:?}", faces.len(), started.elapsed());
        for (i, face) in faces.iter().enumerate() {
            println!(
                "  #{i} at ({:.0},{:.0}) {:.0}x{:.0} score {:.2}",
                face.x, face.y, face.width, face.height, face.score
            );
            let Some(model) = &recogniser else { continue };
            // A square round the box, a touch larger, resized to the
            // recogniser's frame — the gallery's own crop.
            let side = face.width.max(face.height) * 1.1;
            let cx = face.x + face.width / 2.0;
            let cy = face.y + face.height / 2.0;
            let x0 = (cx - side / 2.0).round().max(0.0) as u32;
            let y0 = (cy - side / 2.0).round().max(0.0) as u32;
            let x1 = ((cx + side / 2.0).round() as u32).min(w as u32);
            let y1 = ((cy + side / 2.0).round() as u32).min(h as u32);
            let crop = image::imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image();
            let (mw, mh) = model.spec.input.dims();
            let crop = image::imageops::resize(
                &crop,
                mw as u32,
                mh as u32,
                image::imageops::FilterType::Triangle,
            );
            let crop_rgb: Vec<f32> = crop.as_raw().iter().map(|v| *v as f32 / 255.0).collect();
            let started = std::time::Instant::now();
            let vector = schist_neural::embed_face(model, &crop_rgb)?;
            println!("     embedded in {:?}", started.elapsed());
            found.push((format!("{path}#{i}"), vector));
        }
    }
    for (a, va) in &found {
        for (b, vb) in &found {
            if a < b {
                let cos: f32 = va.iter().zip(vb).map(|(x, y)| x * y).sum();
                println!("{cos:+.3}  {a}  {b}");
            }
        }
    }
    Ok(())
}
