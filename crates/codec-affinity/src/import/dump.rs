//! A readable dump of the object graph, for forensics on new files.

use super::*;

/// Render the parsed graph as an indented outline — the debugging view
/// used while reverse engineering, kept for `--features`-free forensics.
pub fn dump(bytes: &[u8]) -> Result<String, AffinityError> {
    use std::fmt::Write as _;
    let archive = Archive::parse(bytes)?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "container v{} class {}",
        archive.version,
        tag_name(archive.class_tag)
    );
    let _ = writeln!(
        out,
        "entries: {}",
        archive.names().collect::<Vec<_>>().join(", ")
    );
    let entry = archive
        .head("doc.dat")
        .ok_or_else(|| malformed("no doc.dat"))?;
    let doc = archive.extract(entry)?;
    let graph = graph::parse(&doc)?;
    dump_node(
        &graph,
        graph::ROOT,
        0,
        &mut out,
        &mut vec![false; graph.nodes.len()],
    );
    Ok(out)
}

pub(super) fn dump_node(
    graph: &Graph,
    index: usize,
    depth: usize,
    out: &mut String,
    seen: &mut Vec<bool>,
) {
    use std::fmt::Write as _;
    let node = graph.node(index);
    let pad = "  ".repeat(depth);
    let types: Vec<String> = node.types.iter().map(|(t, _)| tag_name(*t)).collect();
    let _ = writeln!(out, "{pad}[{}] id={}", types.join("<"), node.id);
    if seen[index] {
        let _ = writeln!(out, "{pad}  (already shown)");
        return;
    }
    seen[index] = true;
    for (tag, value) in &node.fields {
        let _ = write!(out, "{pad}  {} = ", tag_name(*tag));
        dump_value(graph, value, depth, out, seen);
    }
}

pub(super) fn dump_value(
    graph: &Graph,
    value: &Value,
    depth: usize,
    out: &mut String,
    seen: &mut Vec<bool>,
) {
    use std::fmt::Write as _;
    match value {
        Value::Class(Some(i)) => {
            let _ = writeln!(out, "class:");
            dump_node(graph, *i, depth + 2, out, seen);
        }
        Value::Class(None) => {
            let _ = writeln!(out, "null");
        }
        Value::Array(items) => {
            let _ = writeln!(out, "array[{}]:", items.len());
            let scalar = !items
                .iter()
                .any(|v| matches!(v, Value::Class(_) | Value::Array(_)));
            if scalar {
                let mut line = String::new();
                for v in items.iter().take(64) {
                    let _ = write!(line, "{v:?} ");
                }
                let _ = writeln!(out, "{}    {}", "  ".repeat(depth), line);
            } else {
                for v in items.iter().take(32) {
                    let _ = write!(out, "{}    - ", "  ".repeat(depth));
                    dump_value(graph, v, depth + 2, out, seen);
                }
                if items.len() > 32 {
                    let _ = writeln!(out, "{}    …", "  ".repeat(depth));
                }
            }
        }
        other => {
            let _ = writeln!(out, "{other:?}");
        }
    }
}
