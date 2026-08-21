//! Dev tool: render an imported document's leaf rasters next to the
//! file's embedded thumbnail, for eyeballing geometry mismatches.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afdiff -- file.afdesign out.png
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afdiff <file> <out.png>");
    let out_path = args.next().expect("usage: afdiff <file> <out.png>");
    let bytes = std::fs::read(&path).expect("read");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let thumb = image::load_from_memory(archive.thumbnail().expect("thumb"))
        .expect("decode")
        .to_rgba8();
    let (doc, report) = schist_codec_affinity::read_affinity(&bytes).expect("import");
    eprintln!("{report:?}");

    let (w, h) = (doc.width, doc.height);
    // Left: our import composited naively. Right: thumbnail downscaled.
    let mut img = image::RgbaImage::from_pixel(w * 2, h, image::Rgba([255, 255, 255, 255]));
    fn walk(layers: &[schist_core::Layer], img: &mut image::RgbaImage, w: u32, h: u32) {
        for l in layers {
            if let Some(children) = l.children() {
                walk(children, img, w, h);
            } else if let Some(r) = l.as_raster() {
                for y in 0..h {
                    for x in 0..w {
                        let p = r.tiles.pixel(x as i32, y as i32).to_u8();
                        if p[3] > 0 {
                            let dst = img.get_pixel_mut(x, y);
                            let a = p[3] as u32;
                            for (d, s) in dst.0.iter_mut().zip(p).take(3) {
                                *d = ((s as u32 * a + *d as u32 * (255 - a)) / 255) as u8;
                            }
                        }
                    }
                }
            }
        }
    }
    walk(&doc.tree.layers, &mut img, w, h);
    for y in 0..h {
        for x in 0..w {
            let tx = x * thumb.width() / w;
            let ty = y * thumb.height() / h;
            let p = *thumb.get_pixel(tx, ty);
            img.put_pixel(w + x, y, p);
        }
    }
    img.save(&out_path).expect("save");
    eprintln!("wrote {out_path}");
}
