//! Import an Affinity file both ways and write PNGs for comparison:
//! <out>_layered.png (structured import, composited) and
//! <out>_preview.png (the file's own embedded flattened preview).

use photoslop_plugin_api::CodecPlugin;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afrender <file> <out-prefix>");
    let out = args.next().expect("usage: afrender <file> <out-prefix>");
    let bytes = std::fs::read(&path).expect("read");

    let (doc, report) = photoslop_codec_affinity::read_affinity(&bytes).expect("structured read");
    println!(
        "layered: {}x{} raster:{} groups:{} skipped:{:?}",
        doc.width, doc.height, report.raster_layers, report.groups, report.skipped
    );
    let png = photoslop_codecs_common::PngCodec.export(&doc).expect("png");
    std::fs::write(format!("{out}_layered.png"), png).unwrap();

    // The embedded preview, via the public codec (which would fall back
    // to it): carve by re-importing with the structured path knocked out
    // is awkward, so carve directly through the codec on a truncated
    // container — instead just scan: reuse the codec import only when
    // the report is incomplete. For comparison we always want it, so use
    // the afphoto-style scan through the image crate.
    if let Some(preview) = extract_preview(&bytes) {
        std::fs::write(format!("{out}_preview.png"), preview).unwrap();
        println!("preview written");
    } else {
        println!("no embedded preview found");
    }
}

fn extract_preview(bytes: &[u8]) -> Option<Vec<u8>> {
    let sig = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut best: Option<(u64, Vec<u8>)> = None;
    let mut search = 0;
    while let Some(rel) = bytes[search..]
        .windows(8)
        .position(|w| w == sig)
    {
        let start = search + rel;
        search = start + 8;
        let Some(end_rel) = bytes[start..].windows(4).position(|w| w == b"IEND") else {
            continue;
        };
        let end = start + end_rel + 8;
        if end > bytes.len() {
            continue;
        }
        let data = &bytes[start..end];
        if let Ok(img) = image::load_from_memory(data) {
            let area = img.width() as u64 * img.height() as u64;
            if best.as_ref().is_none_or(|(a, _)| area > *a) {
                best = Some((area, data.to_vec()));
            }
        }
    }
    best.map(|(_, d)| d)
}
