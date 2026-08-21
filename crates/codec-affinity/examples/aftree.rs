//! Dev tool: print an imported document's layer tree with bounds.
fn main() {
    let path = std::env::args().nth(1).expect("usage: aftree <file>");
    let bytes = std::fs::read(&path).expect("read");
    let (doc, _) = schist_codec_affinity::read_affinity(&bytes).expect("import");
    fn walk(layers: &[schist_core::Layer], depth: usize) {
        for l in layers {
            let pad = "  ".repeat(depth);
            let bounds = l
                .as_raster()
                .map(|r| format!("{:?}", r.tiles.content_bounds()))
                .unwrap_or_default();
            let mask = l
                .mask
                .as_ref()
                .map(|m| format!(" mask={:?} en={}", m.bounds, m.enabled))
                .unwrap_or_default();
            println!(
                "{pad}{:?} kind={} vis={} clip={} opac={:.2} blend={:?} {bounds}{mask}",
                l.name,
                match &l.kind {
                    schist_core::LayerKind::Group(_) => "group",
                    schist_core::LayerKind::Adjustment(_) => "adj",
                    _ => "raster",
                },
                l.visible,
                l.clipping,
                l.opacity,
                l.blend,
            );
            if let Some(c) = l.children() {
                walk(c, depth + 1);
            }
        }
    }
    walk(&doc.tree.layers, 0);
}
