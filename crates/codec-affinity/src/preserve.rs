//! Keep the native Affinity data an import can't express, so a future
//! `.af` exporter can round-trip it.
//!
//! Imported layers store the source subtrees they were built from —
//! an adjustment's `AdjP` class, a live shape's `Shpe` class — as JSON
//! in the layer's side-channels (`AdjustmentData::raw`, `Layer::extras`),
//! behind the [`MAGIC`] prefix. The encoding is a faithful, typed image
//! of the object-graph field stream: every field keeps its tag and its
//! wire type, class hierarchies keep their (tag, version) chains, and
//! `Class` references are inlined, so re-emitting the stream needs no
//! information beyond what is stored here.

use crate::graph::{tag_name, Graph, Node, Value};

/// Prefix identifying a preserved-Affinity JSON payload in a raw block.
pub const MAGIC: &[u8; 4] = b"AFJ1";

/// Serialize `node`'s subtree, wrapped with the owning layer's type tag
/// and the field key it hung from, prefixed with [`MAGIC`].
pub fn preserved_block(graph: &Graph, layer_type: u32, key: &[u8; 4], node: &Node) -> Vec<u8> {
    let wrapped = serde_json::json!({
        "layer_type": tag_name(layer_type),
        "key": tag_name(crate::graph::tag(key)),
        "node": node_json(graph, node, 0),
    });
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(wrapped.to_string().as_bytes());
    out
}

/// The node as JSON: type chain, shared id, and typed fields.
fn node_json(graph: &Graph, node: &Node, depth: usize) -> serde_json::Value {
    let types: Vec<serde_json::Value> = node
        .types
        .iter()
        .map(|(t, v)| serde_json::json!([tag_name(*t), v]))
        .collect();
    let fields: Vec<serde_json::Value> = node
        .fields
        .iter()
        .map(|(t, v)| serde_json::json!([tag_name(*t), value_json(graph, v, depth)]))
        .collect();
    serde_json::json!({ "types": types, "id": node.id, "fields": fields })
}

fn value_json(graph: &Graph, value: &Value, depth: usize) -> serde_json::Value {
    use serde_json::json;
    // A malicious or cyclic graph must not recurse away; real subtrees
    // are a handful of levels deep.
    if depth > 32 {
        return json!({ "Truncated": true });
    }
    match value {
        Value::U8(v) => json!({ "U8": v }),
        Value::U16(v) => json!({ "U16": v }),
        Value::U32(v) => json!({ "U32": v }),
        Value::U64(v) => json!({ "U64": v }),
        Value::I8(v) => json!({ "I8": v }),
        Value::I16(v) => json!({ "I16": v }),
        Value::I32(v) => json!({ "I32": v }),
        Value::I64(v) => json!({ "I64": v }),
        Value::F32(v) => json!({ "F32": v }),
        Value::F64(v) => json!({ "F64": v }),
        Value::Bool(v) => json!({ "Bool": v }),
        Value::Str(v) => json!({ "Str": v }),
        Value::Enum { id, version } => json!({ "Enum": [id, version] }),
        Value::Flags { version, bits } => json!({ "Flags": [version, bits] }),
        Value::VecI(v) => json!({ "VecI": v }),
        Value::VecF(v) => json!({ "VecF": v }),
        Value::VecD(v) => json!({ "VecD": v }),
        Value::Curve(v) => json!({ "Curve": v }),
        Value::Blob(v) => json!({ "Blob": v }),
        Value::Embedded { tag, name } => {
            json!({ "Embedded": [tag_name(*tag), name] })
        }
        Value::Struct(v) => json!({ "Struct": v }),
        Value::Class(None) => json!({ "Class": null }),
        Value::Class(Some(i)) => {
            json!({ "Class": node_json(graph, graph.node(*i), depth + 1) })
        }
        Value::Array(items) => json!({
            "Array": items
                .iter()
                .map(|v| value_json(graph, v, depth + 1))
                .collect::<Vec<_>>()
        }),
    }
}
