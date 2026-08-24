//! The object-graph serializer: the exact inverse of [`crate::graph`].
//!
//! [`serialize`] re-emits a parsed [`Graph`] byte-for-byte (the codec
//! tests prove this over every fixture and corpus document), which is
//! what lets the exporter write documents Affinity's own reader shaped:
//! template graphs are parsed from real files, patched, and re-emitted
//! without guessing at any wire detail. Nodes carry their wire type
//! bytes, class framing and 0x31 section boundaries precisely so this
//! module never has to infer them.

use crate::error::{malformed, AffinityError};
use crate::graph::{tag_name, ChainEnd, Graph, Node, Value, ROOT};

const TAG_DOC: u32 = 0x534bff00;

/// Serialize a graph back into an entry payload ("doc.dat" and
/// friends).
pub fn serialize(graph: &Graph) -> Result<Vec<u8>, AffinityError> {
    let root = graph.node(ROOT);
    let (root_tag, type_version) = *root
        .types
        .first()
        .ok_or_else(|| malformed("root node has no type"))?;
    let mut w = Writer {
        graph,
        out: Vec::new(),
        emitted: vec![false; graph.nodes.len()],
    };
    w.u32(TAG_DOC);
    w.u16(graph.file_version);
    w.u32(root_tag);
    w.u16(type_version as u16);
    if graph.file_version == 2 {
        w.u32(graph.header_extra);
    }
    w.emitted[ROOT] = true;
    w.field_stream(&root.fields, &root.wire, &root.aux, true)?;
    Ok(w.out)
}

struct Writer<'a> {
    graph: &'a Graph,
    out: Vec<u8>,
    /// Which nodes have had their 0x31 definition written, so later
    /// references become links.
    emitted: Vec<bool>,
}

impl Writer<'_> {
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    /// One field list plus its 0x00 terminator.
    fn field_stream(
        &mut self,
        fields: &[(u32, Value)],
        wire: &[u8],
        aux: &[u64],
        with_tags: bool,
    ) -> Result<(), AffinityError> {
        if fields.len() != wire.len() || fields.len() != aux.len() {
            return Err(malformed("node fields and wire metadata out of step"));
        }
        for (((tag, value), &type_byte), &aux) in fields.iter().zip(wire).zip(aux) {
            self.u8(type_byte);
            if with_tags {
                self.u32(*tag);
            }
            let array = type_byte & 0x80 != 0;
            self.value(type_byte & 0x7f, array, aux, value)
                .map_err(|e| malformed(format!("emitting field {}: {e}", tag_name(*tag))))?;
        }
        self.u8(0x00);
        Ok(())
    }

    fn value(&mut self, ty: u8, array: bool, aux: u64, value: &Value) -> Result<(), AffinityError> {
        match ty {
            0x29 => return self.bools(array, aux, value),
            0x2a => return self.enums(array, aux, value),
            0x2b | 0x2e => return self.strings(array, value),
            0x2c => return self.curves(array, aux, value),
            0x30..=0x32 => return self.classes(ty, array, aux, value),
            _ => {}
        }
        if array {
            let Value::Array(items) = value else {
                return Err(malformed("array wire type on a scalar value"));
            };
            self.u32(items.len() as u32);
            for item in items {
                self.scalar(ty, item)?;
            }
            Ok(())
        } else {
            self.scalar(ty, value)
        }
    }

    /// One scalar of a type with uniform array framing.
    fn scalar(&mut self, ty: u8, value: &Value) -> Result<(), AffinityError> {
        let mismatch = || malformed(format!("value does not match wire type {ty:#04x}"));
        match (ty, value) {
            (0x01, Value::U8(v)) => self.u8(*v),
            (0x02, Value::U16(v)) => self.u16(*v),
            (0x03 | 0x2f | 0x34, Value::U32(v)) => self.u32(*v),
            (0x04, Value::U64(v)) => self.u64(*v),
            (0x05, Value::I8(v)) => self.u8(*v as u8),
            (0x06, Value::I16(v)) => self.u16(*v as u16),
            (0x07, Value::I32(v)) => self.u32(*v as u32),
            (0x08, Value::I64(v)) => self.u64(*v as u64),
            (0x09, Value::F32(v)) => self.u32(v.to_bits()),
            (0x0a, Value::F64(v)) => self.u64(v.to_bits()),
            (0x15..=0x19, Value::VecI(v)) => {
                if v.len() != (ty - 0x15 + 2) as usize {
                    return Err(mismatch());
                }
                for x in v {
                    self.u32(*x as u32);
                }
            }
            (0x1f..=0x23, Value::VecF(v)) => {
                if v.len() != (ty - 0x1f + 2) as usize {
                    return Err(mismatch());
                }
                for x in v {
                    self.u32(x.to_bits());
                }
            }
            (0x24..=0x28, Value::VecD(v)) => {
                if v.len() != (ty - 0x24 + 2) as usize {
                    return Err(mismatch());
                }
                for x in v {
                    self.u64(x.to_bits());
                }
            }
            (0x2d, Value::Blob(v)) => {
                self.u32(v.len() as u32);
                self.out.extend_from_slice(v);
            }
            (0x33, Value::Embedded { tag, name }) => {
                self.u32(*tag);
                self.u32(name.len() as u32);
                self.out.extend_from_slice(name.as_bytes());
            }
            (0x35..=0x74, Value::Struct(v)) => {
                if v.len() != (ty - 0x34) as usize {
                    return Err(mismatch());
                }
                self.out.extend_from_slice(v);
            }
            (
                0x75,
                Value::Flags {
                    version,
                    count,
                    bits,
                },
            ) => {
                self.u16(*version);
                self.u8(*count);
                let raw = bits.to_le_bytes();
                self.out.extend_from_slice(&raw[..(*count as usize).min(8)]);
            }
            _ => return Err(mismatch()),
        }
        Ok(())
    }

    fn bools(&mut self, array: bool, aux: u64, value: &Value) -> Result<(), AffinityError> {
        match (array, value) {
            // True is not canonical on the wire (0xFF occurs); aux
            // carries the original byte.
            (false, Value::Bool(true)) => self.u8(if aux != 0 { aux as u8 } else { 1 }),
            (false, Value::Bool(false)) => self.u8(0),
            (true, Value::Array(items)) => {
                self.u32(items.len() as u32);
                let mut bytes = vec![0u8; items.len().div_ceil(8)];
                for (i, item) in items.iter().enumerate() {
                    let Value::Bool(v) = item else {
                        return Err(malformed("bool array holds a non-bool"));
                    };
                    bytes[i / 8] |= (*v as u8) << (i % 8);
                }
                self.out.extend_from_slice(&bytes);
            }
            _ => return Err(malformed("value does not match bool wire type")),
        }
        Ok(())
    }

    fn enums(&mut self, array: bool, aux: u64, value: &Value) -> Result<(), AffinityError> {
        match (array, value) {
            (false, Value::Enum { id, version }) => {
                self.u16(*id);
                self.u16(*version);
            }
            (true, Value::Array(items)) => {
                self.u32(items.len() as u32);
                // The version is stored once for the whole array; the
                // parser recorded it in the field's aux word so even an
                // empty array keeps it.
                let version = items
                    .first()
                    .map(|item| match item {
                        Value::Enum { version, .. } => Ok(*version),
                        _ => Err(malformed("enum array holds a non-enum")),
                    })
                    .transpose()?
                    .unwrap_or(aux as u16);
                self.u16(version);
                for item in items {
                    let Value::Enum { id, .. } = item else {
                        return Err(malformed("enum array holds a non-enum"));
                    };
                    self.u16(*id);
                }
            }
            _ => return Err(malformed("value does not match enum wire type")),
        }
        Ok(())
    }

    fn strings(&mut self, array: bool, value: &Value) -> Result<(), AffinityError> {
        match (array, value) {
            (false, Value::Str(s)) => {
                self.u32(s.len() as u32);
                self.out.extend_from_slice(s.as_bytes());
            }
            (true, Value::Array(items)) => {
                let mut total = 0u32;
                for item in items {
                    let Value::Str(s) = item else {
                        return Err(malformed("string array holds a non-string"));
                    };
                    total += 4 + s.len() as u32;
                }
                self.u32(total);
                self.u32(items.len() as u32);
                for item in items {
                    let Value::Str(s) = item else { unreachable!() };
                    self.u32(s.len() as u32);
                    self.out.extend_from_slice(s.as_bytes());
                }
            }
            _ => return Err(malformed("value does not match string wire type")),
        }
        Ok(())
    }

    fn curves(&mut self, array: bool, aux: u64, value: &Value) -> Result<(), AffinityError> {
        match (array, value) {
            (false, Value::Curve(v)) => {
                self.u16(v.len() as u16);
                self.out.extend_from_slice(v);
            }
            (true, Value::Array(items)) => {
                self.u32(items.len() as u32);
                // The record size is stored once; the parser recorded it
                // in the field's aux word so even an empty array keeps
                // it (falling back to the smallest legal size).
                let size = items
                    .first()
                    .map(|item| match item {
                        Value::Curve(v) => Ok(v.len() as u16),
                        _ => Err(malformed("curve array holds a non-curve")),
                    })
                    .transpose()?
                    .unwrap_or(if aux != 0 { aux as u16 } else { 12 });
                self.u16(size);
                for item in items {
                    let Value::Curve(v) = item else {
                        return Err(malformed("curve array holds a non-curve"));
                    };
                    if v.len() != size as usize {
                        return Err(malformed("curve records differ in size"));
                    }
                    self.out.extend_from_slice(v);
                }
            }
            _ => return Err(malformed("value does not match curve wire type")),
        }
        Ok(())
    }

    fn classes(
        &mut self,
        ty: u8,
        array: bool,
        aux: u64,
        value: &Value,
    ) -> Result<(), AffinityError> {
        if !array {
            return self.class(ty, None, value);
        }
        let Value::Array(items) = value else {
            return Err(malformed("array wire type on a scalar class"));
        };
        self.u32(items.len() as u32);
        let mut shared = None;
        if ty == 0x32 {
            // Arrays of 0x32 hoist one (tag, id) header shared by every
            // element. The parser recorded it in the field's aux word;
            // the first definition's type is an equivalent fallback for
            // graphs built by hand.
            let header = if aux != 0 {
                ((aux >> 16) as u32, (aux & 0xFFFF) as u32)
            } else {
                items
                    .iter()
                    .find_map(|item| match item {
                        Value::Class(Some(i)) => self.graph.node(*i).types.first().copied(),
                        _ => None,
                    })
                    .unwrap_or((0, 0))
            };
            self.u32(header.0);
            self.u16(header.1 as u16);
            shared = Some(header);
        }
        for item in items {
            self.class(ty, shared, item)?;
        }
        Ok(())
    }

    fn class(
        &mut self,
        ty: u8,
        shared: Option<(u32, u32)>,
        value: &Value,
    ) -> Result<(), AffinityError> {
        let index = match value {
            Value::Class(None) => {
                // Null: 0x30 has no null form; 0x31/0x32 write status 0.
                if ty == 0x30 {
                    return Err(malformed("null class in an untagged field"));
                }
                self.u8(0);
                return Ok(());
            }
            Value::Class(Some(i)) => *i,
            _ => return Err(malformed("value does not match class wire type")),
        };
        let node = self.graph.node(index);
        match ty {
            0x30 => self.field_stream(&node.fields, &node.wire, &node.aux, false),
            0x31 => {
                if self.emitted[index] {
                    self.u8(2);
                    self.u32(node.id);
                    return Ok(());
                }
                self.emitted[index] = true;
                self.u8(1);
                self.u32(node.id);
                self.class_sections(node)
            }
            0x32 => {
                if self.emitted[index] {
                    return Err(malformed(
                        "0x32 class referenced twice cannot be re-emitted",
                    ));
                }
                self.emitted[index] = true;
                self.u8(1);
                if shared.is_none() {
                    let (t, v) = node
                        .types
                        .first()
                        .copied()
                        .ok_or_else(|| malformed("tagged class node has no type"))?;
                    self.u32(t);
                    self.u16(v as u16);
                }
                self.field_stream(&node.fields, &node.wire, &node.aux, true)
            }
            _ => unreachable!(),
        }
    }

    /// A 0x31 definition's type sections, chain terminator, and
    /// trailing field stream.
    fn class_sections(&mut self, node: &Node) -> Result<(), AffinityError> {
        let mut at = 0usize;
        for (i, len) in node.section_lens.iter().enumerate() {
            let (t, v) = *node
                .types
                .get(i)
                .ok_or_else(|| malformed("more sections than types"))?;
            self.u8(0);
            self.u32(t);
            self.u16(v as u16);
            let end = at + len;
            if end > node.fields.len() {
                return Err(malformed("section lengths exceed fields"));
            }
            self.field_stream(
                &node.fields[at..end],
                &node.wire[at..end],
                &node.aux[at..end],
                true,
            )?;
            at = end;
        }
        match node.chain_end {
            ChainEnd::LoneTag => {
                let (t, _) = *node
                    .types
                    .last()
                    .ok_or_else(|| malformed("lone-tag chain with no types"))?;
                self.u8(1);
                self.u32(t);
            }
            ChainEnd::Closed => self.u8(2),
            ChainEnd::None => {
                return Err(malformed("0x31 definition without chain info"));
            }
        }
        self.field_stream(&node.fields[at..], &node.wire[at..], &node.aux[at..], true)
    }
}
