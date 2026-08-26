//! Export a document as .af — the write-path counterpart of afdump.
//!
//!     cargo run -p schist-codec-affinity --example afwrite -- in.afphoto out.af
//!     cargo run -p schist-codec-affinity --example afwrite -- --demo out.af
//!
//! With an Affinity file as input, imports it and re-exports it (the
//! full preservation cycle). `--demo` writes a small synthetic document
//! exercising rasters, groups, masks, blends and effects instead —
//! useful as a smoke file to open in real Affinity.

use schist_codec_affinity::{read_affinity, write_affinity};
use schist_color::Depth;
use schist_core::{blit_rgba8, BlendMode, Document, IntRect, Layer, LayerKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: afwrite (<in.af…> | --demo) <out.af>");
        std::process::exit(2);
    };

    let doc = if input == "--demo" {
        demo_document()
    } else {
        let bytes = std::fs::read(input).expect("read input");
        let (doc, report) = read_affinity(&bytes).expect("import input");
        if !report.complete() {
            eprintln!("note: import skipped {:?}", report.skipped);
        }
        doc
    };

    let thumbnail = render_thumbnail(&doc);
    let (bytes, report) = write_affinity(&doc, thumbnail.as_deref()).expect("export");
    for (layer, why) in &report.skipped {
        eprintln!("note: {layer:?}: {why}");
    }
    std::fs::write(output, &bytes).expect("write output");
    eprintln!("wrote {output} ({} bytes)", bytes.len());
}

fn render_thumbnail(doc: &Document) -> Option<Vec<u8>> {
    let region = doc.canvas_rect();
    if region.is_empty() {
        return None;
    }
    let pixels = schist_compositor::composite_region_f32(doc, region);
    let rgba: Vec<u8> = pixels
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect();
    let img: image::RgbaImage = image::ImageBuffer::from_raw(doc.width, doc.height, rgba)?;
    let scale = 512.0 / doc.width.max(doc.height).max(1) as f32;
    let img = if scale < 1.0 {
        image::imageops::resize(
            &img,
            ((doc.width as f32 * scale).round() as u32).max(1),
            ((doc.height as f32 * scale).round() as u32).max(1),
            image::imageops::Triangle,
        )
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn demo_document() -> Document {
    let mut doc = Document::new("schist demo", 800, 600, Depth::Eight);

    let mut bg = Layer::new_raster("Gradient background");
    {
        let tiles = &mut bg.as_raster_mut().unwrap().tiles;
        let mut px = vec![0u8; 800 * 600 * 4];
        for y in 0..600usize {
            for x in 0..800usize {
                let i = (y * 800 + x) * 4;
                px[i] = (x * 255 / 799) as u8;
                px[i + 1] = (y * 255 / 599) as u8;
                px[i + 2] = 160;
                px[i + 3] = 255;
            }
        }
        blit_rgba8(tiles, Depth::Eight, IntRect::new(0, 0, 800, 600), &px);
    }
    doc.push_layer(bg);

    let mut disc = Layer::new_raster("Sticker");
    {
        let tiles = &mut disc.as_raster_mut().unwrap().tiles;
        let mut px = vec![0u8; 300 * 300 * 4];
        for y in 0..300i32 {
            for x in 0..300i32 {
                let (dx, dy) = (x - 150, y - 150);
                if dx * dx + dy * dy < 140 * 140 {
                    let i = ((y * 300 + x) * 4) as usize;
                    px[i] = 250;
                    px[i + 1] = 210;
                    px[i + 2] = 40;
                    px[i + 3] = 255;
                }
            }
        }
        blit_rgba8(tiles, Depth::Eight, IntRect::new(250, 150, 550, 450), &px);
    }
    disc.style.stroke.enabled = true;
    disc.style.stroke.settings.size = 8.0;
    disc.style.stroke.settings.color = schist_color::Rgba::new(1.0, 1.0, 1.0, 1.0);
    disc.style.drop_shadow.enabled = true;
    disc.style.drop_shadow.settings.size = 10.0;
    disc.style.drop_shadow.settings.distance = 8.0;
    doc.push_layer(disc);

    let mut group = Layer::new_group("Overlay group");
    group.blend = BlendMode::Normal;
    group.opacity = 0.85;
    if let LayerKind::Group(g) = &mut group.kind {
        let mut stripe = Layer::new_raster("Stripe");
        {
            let tiles = &mut stripe.as_raster_mut().unwrap().tiles;
            let px = vec![[40u8, 40, 220, 255]; 700 * 80].concat();
            blit_rgba8(tiles, Depth::Eight, IntRect::new(50, 480, 750, 560), &px);
        }
        stripe.blend = BlendMode::Screen;
        let mut mask = schist_core::LayerMask::new_revealing();
        mask.bounds = IntRect::new(50, 480, 400, 560);
        for y in 480..560 {
            for x in 50..400 {
                let coord = schist_core::TileCoord::containing(x, y);
                let r = coord.rect();
                let buf = mask.tiles.get_mut_or_insert(coord);
                buf[((y - r.top) * schist_core::TILE_SIZE + (x - r.left)) as usize] =
                    ((x - 50) * 255 / 349) as u8;
            }
        }
        stripe.mask = Some(mask);
        g.children.push(stripe);
    }
    doc.push_layer(group);

    doc.damage_all();
    doc.mark_saved();
    doc
}
