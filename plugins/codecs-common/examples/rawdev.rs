//! Develop a camera raw file the way the codec does and write it as a
//! PNG, for looking at: `rawdev <raw> <out.png>`. With `--preview` the
//! embedded camera JPEG is written instead, turned upright the way the
//! gallery shows it.

use schist_plugin_api::CodecPlugin as _;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let preview = args
        .iter()
        .position(|a| a == "--preview")
        .map(|i| args.remove(i));
    let [path, out] = args.as_slice() else {
        eprintln!("usage: rawdev [--preview] <raw> <out.png>");
        std::process::exit(2);
    };
    let bytes = std::fs::read(path).expect("read");
    let started = std::time::Instant::now();
    if preview.is_some() {
        let img = schist_codecs_common::raw::embedded_preview(&bytes)
            .expect("preview")
            .expect("this file embeds no preview");
        println!(
            "preview {}x{} in {:.2?}",
            img.width(),
            img.height(),
            started.elapsed()
        );
        img.save(out).expect("write");
        return;
    }
    let doc = schist_codecs_common::RawCodec
        .import(&bytes)
        .expect("develop");
    println!(
        "developed {}x{} ({:?}) in {:.2?}",
        doc.width,
        doc.height,
        doc.depth,
        started.elapsed()
    );
    let png = schist_codecs_common::PngCodec.export(&doc).expect("png");
    std::fs::write(out, png).expect("write");
}
