//! Dev tool: dump the object graph of a *specific* doc.dat revision
//! (index into `Archive::revisions` order), to compare savepoints.
//!
//! ```sh
//! cargo run -p schist-codec-affinity --example afrevdump -- file.af 0
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afrevdump <file> <rev-index>");
    let idx: usize = args.next().expect("rev index").parse().unwrap();
    let bytes = std::fs::read(&path).expect("read file");
    let archive = schist_codec_affinity::Archive::parse(&bytes).expect("parse");
    let revs = archive.revisions("doc.dat");
    eprintln!(
        "{} revisions: {:?}",
        revs.len(),
        revs.iter().map(|e| e.savepoint).collect::<Vec<_>>()
    );
    let data = archive.extract(revs[idx]).expect("extract");
    let graph = schist_codec_affinity::graph::parse(&data).expect("graph");
    // Reuse the import dump printer via a minimal re-walk: print Xfrm
    // vectors with node tags in document order.
    for node in &graph.nodes {
        if let Some(schist_codec_affinity::graph::Value::VecD(v)) = node.field(b"Xfrm") {
            let vals: Vec<String> = v.iter().map(|x| format!("{x:.4}")).collect();
            println!(
                "{} [{}]",
                schist_codec_affinity::graph::tag_name(node.type_tag()),
                vals.join(", ")
            );
        }
    }
}
