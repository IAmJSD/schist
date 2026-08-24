//! Print each class's wire-level layout: type chain with versions,
//! framing, and per-section field lists with wire types and aux data.
//! The transcription source for the exporter's node builders.
//!
//!     cargo run -p schist-codec-affinity --example afschema -- file.af [ClassTag]

use schist_codec_affinity::archive::Archive;
use schist_codec_affinity::graph::{self, tag_name, Node, Value};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: afschema <file> [ClassTag]");
    let filter = args.next();
    let bytes = std::fs::read(&path).unwrap();
    let archive = Archive::parse(&bytes).unwrap();
    let plain = archive.extract(archive.head("doc.dat").unwrap()).unwrap();
    let g = graph::parse(&plain).unwrap();

    let mut seen = std::collections::HashSet::new();
    for node in &g.nodes {
        let chain: Vec<String> = node
            .types
            .iter()
            .map(|(t, v)| format!("{}v{v}", tag_name(*t)))
            .collect();
        let key = chain.join("<");
        if let Some(f) = &filter {
            if !key.contains(f.as_str()) {
                continue;
            }
        }
        if !seen.insert(key.clone()) {
            continue;
        }
        println!(
            "[{key}] framing={:#04x} id={} chain_end={:?} sections={:?}",
            node.framing, node.id, node.chain_end, node.section_lens
        );
        let mut at = 0usize;
        let mut bounds: Vec<usize> = node.section_lens.clone();
        bounds.push(node.fields.len() - node.section_lens.iter().sum::<usize>());
        for (si, len) in bounds.iter().enumerate() {
            let label = if si < node.section_lens.len() {
                format!(
                    "section {} ({})",
                    si,
                    chain.get(si).cloned().unwrap_or_default()
                )
            } else {
                "trailing".to_string()
            };
            println!("  -- {label}: {len} fields");
            for i in at..at + len {
                let (tag, value) = &node.fields[i];
                println!(
                    "    {} wire={:#04x} aux={:#x} {}",
                    tag_name(*tag),
                    node.wire[i],
                    node.aux[i],
                    brief(&g, value)
                );
            }
            at += len;
        }
    }
}

fn brief(g: &graph::Graph, v: &Value) -> String {
    match v {
        Value::Class(Some(i)) => {
            let n: &Node = g.node(*i);
            format!(
                "class [{}]",
                n.types
                    .iter()
                    .map(|(t, v)| format!("{}v{v}", tag_name(*t)))
                    .collect::<Vec<_>>()
                    .join("<")
            )
        }
        Value::Class(None) => "null".into(),
        Value::Array(items) => format!(
            "array[{}] {}",
            items.len(),
            items.first().map(|f| brief(g, f)).unwrap_or_default()
        ),
        Value::Str(s) if s.len() > 24 => format!("Str({:?}…)", &s[..24]),
        Value::Blob(b) => format!("Blob[{}]", b.len()),
        Value::Curve(c) => format!("Curve[{}]", c.len()),
        other => format!("{other:?}"),
    }
}
