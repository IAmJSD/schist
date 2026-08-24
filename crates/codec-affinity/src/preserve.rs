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

use crate::graph::{tag, tag_name, ChainEnd, Graph, Node, Value};

/// Prefix identifying a preserved-Affinity JSON payload in a raw block.
pub const MAGIC: &[u8; 4] = b"AFJ1";

/// Serialize `node`'s subtree, wrapped with the owning layer's type
/// chain (tags and versions) and the field key it hung from, prefixed
/// with [`MAGIC`].
pub fn preserved_block(
    graph: &Graph,
    layer_types: &[(u32, u32)],
    key: &[u8; 4],
    node: &Node,
) -> Vec<u8> {
    let wrapped = serde_json::json!({
        "layer_type": layer_types
            .first()
            .map(|(t, _)| tag_name(*t))
            .unwrap_or_default(),
        "layer_types": layer_types
            .iter()
            .map(|(t, v)| serde_json::json!([tag_name(*t), v]))
            .collect::<Vec<_>>(),
        "key": tag_name(crate::graph::tag(key)),
        "node": node_json(graph, node, 0),
    });
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(wrapped.to_string().as_bytes());
    out
}

/// The node as JSON: type chain, shared id, typed fields, and the
/// wire-level detail (field type bytes, framing, section boundaries)
/// that a re-emit needs to reproduce the stream exactly.
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
    serde_json::json!({
        "types": types,
        "id": node.id,
        "fields": fields,
        "wire": node.wire,
        "framing": node.framing,
        "sections": node.section_lens,
        "chain_end": match node.chain_end {
            ChainEnd::None => 0,
            ChainEnd::LoneTag => 1,
            ChainEnd::Closed => 2,
        },
    })
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
        Value::Flags {
            version,
            count,
            bits,
        } => json!({ "Flags": [version, count, bits] }),
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

/// A preserved block decoded back into graph nodes.
pub struct Decoded {
    /// The owning layer's type chain, versions included when the block
    /// was written by a version that recorded them.
    pub layer_types: Vec<(u32, u32)>,
    /// The field the subtree hung from ("AdjP" / "NAjP" / "Shpe").
    pub key: u32,
    /// Index of the rebuilt subtree's root node in the graph.
    pub root: usize,
}

/// Decode an [`preserved_block`] payload, rebuilding its node subtree
/// inside `graph`. Every rebuilt 0x31 definition gets a fresh id from
/// `next_id` so grafted subtrees cannot collide with existing nodes.
pub fn decode(raw: &[u8], graph: &mut Graph, next_id: &mut impl FnMut() -> u32) -> Option<Decoded> {
    let json = raw.strip_prefix(MAGIC.as_slice())?;
    let wrapped: serde_json::Value = serde_json::from_slice(json).ok()?;
    let key = tag(tag_of(wrapped.get("key")?)?.as_slice().try_into().ok()?);
    let layer_types = match wrapped.get("layer_types") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|t| {
                let pair = t.as_array()?;
                let name: [u8; 4] = pair.first()?.as_str()?.as_bytes().try_into().ok()?;
                Some((tag(&name), pair.get(1)?.as_u64()? as u32))
            })
            .collect(),
        // Old blocks stored only the most-derived tag, version unknown.
        _ => {
            let name = tag_of(wrapped.get("layer_type")?)?;
            vec![(tag(name.as_slice().try_into().ok()?), 1)]
        }
    };
    let root = decode_node(wrapped.get("node")?, graph, next_id)?;
    Some(Decoded {
        layer_types,
        key,
        root,
    })
}

fn tag_of(v: &serde_json::Value) -> Option<Vec<u8>> {
    let s = v.as_str()?;
    (s.len() == 4).then(|| s.as_bytes().to_vec())
}

fn decode_node(
    v: &serde_json::Value,
    graph: &mut Graph,
    next_id: &mut impl FnMut() -> u32,
) -> Option<usize> {
    let types: Vec<(u32, u32)> = v
        .get("types")?
        .as_array()?
        .iter()
        .filter_map(|t| {
            let pair = t.as_array()?;
            let name: [u8; 4] = pair.first()?.as_str()?.as_bytes().try_into().ok()?;
            Some((tag(&name), pair.get(1)?.as_u64()? as u32))
        })
        .collect();
    let json_fields = v.get("fields")?.as_array()?;

    let mut fields = Vec::with_capacity(json_fields.len());
    let mut values_json = Vec::with_capacity(json_fields.len());
    for f in json_fields {
        let pair = f.as_array()?;
        let name: [u8; 4] = pair.first()?.as_str()?.as_bytes().try_into().ok()?;
        values_json.push(pair.get(1)?);
        fields.push(tag(&name));
    }

    // Wire-level detail: recorded by current writers; inferred from the
    // value shapes for blocks preserved before it was recorded.
    let wire: Option<Vec<u8>> = v.get("wire").and_then(|w| {
        w.as_array().map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|x| x as u8))
                .collect()
        })
    });
    let framing = v
        .get("framing")
        .and_then(|f| f.as_u64())
        .map(|f| f as u8)
        .unwrap_or(if types.is_empty() { 0x30 } else { 0x32 });
    let section_lens: Vec<usize> = v
        .get("sections")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|x| x as usize))
                .collect()
        })
        .unwrap_or_default();
    let chain_end = match v.get("chain_end").and_then(|c| c.as_u64()) {
        Some(1) => ChainEnd::LoneTag,
        Some(2) => ChainEnd::Closed,
        Some(_) => ChainEnd::None,
        // Old multi-type chains have to become 0x31 definitions; give
        // them empty sections and a lone-tag terminator.
        None if framing == 0x31 || types.len() > 1 => ChainEnd::LoneTag,
        None => ChainEnd::None,
    };
    let framing = if types.len() > 1 && framing == 0x32 {
        0x31
    } else {
        framing
    };
    let section_lens = if framing == 0x31 && section_lens.is_empty() && types.len() > 1 {
        vec![0; types.len() - 1]
    } else {
        section_lens
    };

    // Values (and, when inferring, their wire bytes) — children first,
    // since Class values need their node indices.
    let mut values = Vec::with_capacity(values_json.len());
    for vj in &values_json {
        values.push(decode_value(vj, graph, next_id)?);
    }
    let wire = match wire {
        Some(w) if w.len() == values.len() => w,
        _ => values
            .iter()
            .map(|val| infer_wire(val, graph))
            .collect::<Option<Vec<u8>>>()?,
    };
    let aux = vec![0u64; values.len()];

    let id = if framing == 0x31 { next_id() } else { 0 };
    graph.nodes.push(Node {
        types,
        fields: fields.into_iter().zip(values).collect(),
        id,
        wire,
        aux,
        framing,
        section_lens,
        chain_end,
    });
    Some(graph.nodes.len() - 1)
}

fn decode_value(
    v: &serde_json::Value,
    graph: &mut Graph,
    next_id: &mut impl FnMut() -> u32,
) -> Option<Value> {
    let (kind, body) = v.as_object()?.iter().next()?;
    Some(match kind.as_str() {
        "U8" => Value::U8(body.as_u64()? as u8),
        "U16" => Value::U16(body.as_u64()? as u16),
        "U32" => Value::U32(body.as_u64()? as u32),
        "U64" => Value::U64(body.as_u64()?),
        "I8" => Value::I8(body.as_i64()? as i8),
        "I16" => Value::I16(body.as_i64()? as i16),
        "I32" => Value::I32(body.as_i64()? as i32),
        "I64" => Value::I64(body.as_i64()?),
        "F32" => Value::F32(body.as_f64()? as f32),
        "F64" => Value::F64(body.as_f64()?),
        "Bool" => Value::Bool(body.as_bool()?),
        "Str" => Value::Str(body.as_str()?.to_string()),
        "Enum" => {
            let pair = body.as_array()?;
            Value::Enum {
                id: pair.first()?.as_u64()? as u16,
                version: pair.get(1)?.as_u64()? as u16,
            }
        }
        "Flags" => {
            let parts = body.as_array()?;
            match parts.len() {
                3 => Value::Flags {
                    version: parts[0].as_u64()? as u16,
                    count: parts[1].as_u64()? as u8,
                    bits: parts[2].as_u64()?,
                },
                // Old two-part form: version, bits — count inferred.
                _ => {
                    let bits = parts.get(1)?.as_u64()?;
                    Value::Flags {
                        version: parts.first()?.as_u64()? as u16,
                        count: (64 - bits.leading_zeros() as usize).div_ceil(8).max(1) as u8,
                        bits,
                    }
                }
            }
        }
        "VecI" => Value::VecI(
            body.as_array()?
                .iter()
                .map(|x| x.as_i64().map(|x| x as i32))
                .collect::<Option<_>>()?,
        ),
        "VecF" => Value::VecF(
            body.as_array()?
                .iter()
                .map(|x| x.as_f64().map(|x| x as f32))
                .collect::<Option<_>>()?,
        ),
        "VecD" => Value::VecD(
            body.as_array()?
                .iter()
                .map(|x| x.as_f64())
                .collect::<Option<_>>()?,
        ),
        "Curve" => Value::Curve(bytes_of(body)?),
        "Blob" => Value::Blob(bytes_of(body)?),
        "Struct" => Value::Struct(bytes_of(body)?),
        "Embedded" => {
            let pair = body.as_array()?;
            let name: [u8; 4] = pair.first()?.as_str()?.as_bytes().try_into().ok()?;
            Value::Embedded {
                tag: tag(&name),
                name: pair.get(1)?.as_str()?.to_string(),
            }
        }
        "Class" if body.is_null() => Value::Class(None),
        "Class" => Value::Class(Some(decode_node(body, graph, next_id)?)),
        "Array" => Value::Array(
            body.as_array()?
                .iter()
                .map(|item| decode_value(item, graph, next_id))
                .collect::<Option<_>>()?,
        ),
        "Truncated" => return None,
        _ => return None,
    })
}

fn bytes_of(v: &serde_json::Value) -> Option<Vec<u8>> {
    v.as_array()?
        .iter()
        .map(|x| x.as_u64().map(|x| x as u8))
        .collect()
}

/// Fallback wire types for blocks preserved before the exact byte was
/// recorded: the canonical encoding of each value shape.
fn infer_wire(value: &Value, graph: &Graph) -> Option<u8> {
    Some(match value {
        Value::U8(_) => 0x01,
        Value::U16(_) => 0x02,
        Value::U32(_) => 0x03,
        Value::U64(_) => 0x04,
        Value::I8(_) => 0x05,
        Value::I16(_) => 0x06,
        Value::I32(_) => 0x07,
        Value::I64(_) => 0x08,
        Value::F32(_) => 0x09,
        Value::F64(_) => 0x0a,
        Value::Bool(_) => 0x29,
        Value::Str(_) => 0x2b,
        Value::Enum { .. } => 0x2a,
        Value::Flags { .. } => 0x75,
        Value::VecI(v) => 0x15 + (v.len().clamp(2, 6) as u8 - 2),
        Value::VecF(v) => 0x1f + (v.len().clamp(2, 6) as u8 - 2),
        Value::VecD(v) => 0x24 + (v.len().clamp(2, 6) as u8 - 2),
        Value::Curve(_) => 0x2c,
        Value::Blob(_) => 0x2d,
        Value::Embedded { .. } => 0x33,
        Value::Struct(v) => 0x34 + v.len().clamp(1, 0x40) as u8,
        Value::Class(Some(i)) => match graph.nodes.get(*i)?.framing {
            0 => 0x32,
            f => f,
        },
        Value::Class(None) => 0x31,
        Value::Array(items) => {
            let inner = items
                .first()
                .map(|f| infer_wire(f, graph))
                .unwrap_or(Some(0x01))?;
            inner | 0x80
        }
    })
}
