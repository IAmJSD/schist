//! The Affinity object graph: "doc.dat" (and friends) deserialized.
//!
//! An entry's payload is a header (`00 FF 4B 53` magic, versions, a root
//! class tag like "Pers") followed by a stream of tagged fields. Each
//! field is one type byte — bit 7 marks an array — a 4-char tag, and a
//! self-describing value: scalars, fixed vectors, strings, enums, flag
//! sets, raw structs, or nested classes. Classes come in three framings:
//! untagged composites (0x30), shareable definitions with an id that
//! later fields reference as links (0x31), and tag+id definitions
//! (0x32). A field stream ends at type byte 0x00.
//!
//! This parser is deliberately schema-less: it builds a generic tree and
//! lets [`crate::import`] pull out the paths it understands, so unknown
//! layer kinds or new fields never break the parse.

use crate::archive::Cursor;
use crate::error::{malformed, AffinityError};
use std::collections::HashMap;

const TAG_DOC: u32 = 0x534bff00;

/// Pack a 4-char tag name into its comparable value.
pub const fn tag(name: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*name)
}

/// Render a tag for diagnostics ("Chld", "DocR"…).
pub fn tag_name(t: u32) -> String {
    t.to_be_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// One field value. Arrays of anything become [`Value::Array`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Str(String),
    Enum {
        id: u16,
        version: u16,
    },
    Flags {
        version: u16,
        bits: u64,
    },
    VecI(Vec<i32>),
    VecF(Vec<f32>),
    VecD(Vec<f64>),
    /// Curve/point records (12/16/18/24/32 bytes), kept raw.
    Curve(Vec<u8>),
    /// 0x2d binary payload.
    Blob(Vec<u8>),
    /// 0x33: a short string naming another entry in the container —
    /// this is how bitmaps reference their pixel tiles.
    Embedded {
        tag: u32,
        name: String,
    },
    /// 0x35–0x74 fixed-size struct (colors, mostly), kept raw.
    Struct(Vec<u8>),
    /// Nested class: index into [`Graph::nodes`], or None for null.
    Class(Option<usize>),
    Array(Vec<Value>),
}

/// One class instance: its type chain and flattened fields.
#[derive(Debug, Default)]
pub struct Node {
    /// (tag, version) sections, most-derived first. Base-class sections
    /// contribute their fields to the same flat list.
    pub types: Vec<(u32, u32)>,
    /// (field tag, value); tag 0 for untagged composite members.
    pub fields: Vec<(u32, Value)>,
    /// Shared-object id (0 when the class is anonymous).
    pub id: u32,
}

impl Node {
    pub fn field(&self, name: &[u8; 4]) -> Option<&Value> {
        let t = tag(name);
        self.fields.iter().find(|(ft, _)| *ft == t).map(|(_, v)| v)
    }

    pub fn type_tag(&self) -> u32 {
        self.types.first().map(|(t, _)| *t).unwrap_or(0)
    }
}

/// A parsed object graph. `nodes[0]` is the root.
pub struct Graph {
    pub nodes: Vec<Node>,
    pub file_version: u16,
    pub type_version: u16,
}

pub const ROOT: usize = 0;

impl Graph {
    pub fn node(&self, index: usize) -> &Node {
        &self.nodes[index]
    }

    /// Follow a `Class` field on `node`, resolving to the target node.
    pub fn child(&self, node: &Node, name: &[u8; 4]) -> Option<&Node> {
        match node.field(name)? {
            Value::Class(Some(i)) => Some(self.node(*i)),
            _ => None,
        }
    }

    /// A `Class` array field on `node`, as resolved nodes (nulls skipped).
    pub fn children(&self, node: &Node, name: &[u8; 4]) -> Vec<&Node> {
        match node.field(name) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::Class(Some(i)) => Some(self.node(*i)),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

pub fn parse(bytes: &[u8]) -> Result<Graph, AffinityError> {
    let mut c = Cursor::new(bytes);
    if c.u32()? != TAG_DOC {
        return Err(malformed("entry is not an object-graph document"));
    }
    let file_version = c.u16()?;
    let root_tag = c.u32()?;
    let type_version = c.u16()?;
    if file_version > 2 {
        return Err(malformed(format!(
            "object graph version {file_version} unknown"
        )));
    }
    if file_version == 2 {
        let _version = c.u32()?;
    }

    let mut p = Parser {
        c,
        nodes: vec![Node {
            types: vec![(root_tag, type_version as u32)],
            ..Node::default()
        }],
        by_id: HashMap::new(),
        depth: 0,
    };
    let mut root_fields = p.fields(true)?;
    p.nodes[ROOT].fields.append(&mut root_fields);
    Ok(Graph {
        nodes: p.nodes,
        file_version,
        type_version,
    })
}

struct Parser<'a> {
    c: Cursor<'a>,
    nodes: Vec<Node>,
    by_id: HashMap<u32, usize>,
    depth: u32,
}

impl Parser<'_> {
    /// Parse a field list until its 0x00 terminator.
    fn fields(&mut self, with_tags: bool) -> Result<Vec<(u32, Value)>, AffinityError> {
        self.depth += 1;
        // Corrupt input could otherwise recurse to a stack overflow.
        if self.depth > 200 {
            return Err(malformed("class nesting too deep"));
        }
        let mut out = Vec::new();
        loop {
            let pos = self.c.pos;
            let type_byte = self.c.u8()?;
            let array = type_byte & 0x80 != 0;
            let ty = type_byte & 0x7f;
            if ty == 0x00 {
                self.depth -= 1;
                return Ok(out);
            }
            let needs_tag = matches!(ty, 0x30..=0x33 | 0x75);
            if needs_tag && !with_tags {
                return Err(malformed(format!("untagged class field at {pos}")));
            }
            let field_tag = if with_tags { self.c.u32()? } else { 0 };
            let value = self.value(ty, array).map_err(|e| {
                malformed(format!(
                    "in field {} (type {ty:#04x}) at {pos}: {e}",
                    tag_name(field_tag)
                ))
            })?;
            out.push((field_tag, value));
        }
    }

    fn value(&mut self, ty: u8, array: bool) -> Result<Value, AffinityError> {
        match ty {
            0x01 => self.scalars(array, |p| Ok(Value::U8(p.c.u8()?))),
            0x02 => self.scalars(array, |p| Ok(Value::U16(p.c.u16()?))),
            0x03 | 0x2f | 0x34 => self.scalars(array, |p| Ok(Value::U32(p.c.u32()?))),
            0x04 => self.scalars(array, |p| Ok(Value::U64(p.c.u64()?))),
            0x05 => self.scalars(array, |p| Ok(Value::I8(p.c.u8()? as i8))),
            0x06 => self.scalars(array, |p| Ok(Value::I16(p.c.u16()? as i16))),
            0x07 => self.scalars(array, |p| Ok(Value::I32(p.c.u32()? as i32))),
            0x08 => self.scalars(array, |p| Ok(Value::I64(p.c.u64()? as i64))),
            0x09 => self.scalars(array, |p| Ok(Value::F32(f32::from_bits(p.c.u32()?)))),
            0x0a => self.scalars(array, |p| Ok(Value::F64(f64::from_bits(p.c.u64()?)))),
            0x15..=0x19 => {
                let n = (ty - 0x15 + 2) as usize;
                self.scalars(array, move |p| {
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(p.c.u32()? as i32);
                    }
                    Ok(Value::VecI(v))
                })
            }
            0x1f..=0x23 => {
                let n = (ty - 0x1f + 2) as usize;
                self.scalars(array, move |p| {
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(f32::from_bits(p.c.u32()?));
                    }
                    Ok(Value::VecF(v))
                })
            }
            0x24..=0x28 => {
                let n = (ty - 0x24 + 2) as usize;
                self.scalars(array, move |p| {
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(f64::from_bits(p.c.u64()?));
                    }
                    Ok(Value::VecD(v))
                })
            }
            0x29 => self.bools(array),
            0x2a => self.enums(array),
            0x2b | 0x2e => self.strings(array),
            0x2c => self.curves(array),
            0x2d => {
                if array {
                    return Err(malformed("binary arrays do not occur"));
                }
                let size = self.c.u32()? as usize;
                Ok(Value::Blob(self.c.take(size)?.to_vec()))
            }
            0x30..=0x32 => self.classes(ty, array),
            0x33 => {
                if array {
                    return Err(malformed("embedded-data arrays do not occur"));
                }
                let data_tag = self.c.u32()?;
                let size = self.c.u32()? as usize;
                let name = String::from_utf8_lossy(self.c.take(size)?).into_owned();
                Ok(Value::Embedded {
                    tag: data_tag,
                    name,
                })
            }
            0x35..=0x74 => {
                let size = (ty - 0x34) as usize;
                self.scalars(array, move |p| Ok(Value::Struct(p.c.take(size)?.to_vec())))
            }
            0x75 => {
                if array {
                    return Err(malformed("flag-set arrays do not occur"));
                }
                let version = self.c.u16()?;
                let count = self.c.u8()? as usize;
                if count > 8 {
                    return Err(malformed("flag set wider than 64 bits"));
                }
                let mut raw = [0u8; 8];
                raw[..count].copy_from_slice(self.c.take(count)?);
                Ok(Value::Flags {
                    version,
                    bits: u64::from_le_bytes(raw),
                })
            }
            other => Err(malformed(format!("unknown field type {other:#04x}"))),
        }
    }

    /// Uniform scalar/array framing: arrays prefix a u32 count.
    fn scalars(
        &mut self,
        array: bool,
        mut read: impl FnMut(&mut Self) -> Result<Value, AffinityError>,
    ) -> Result<Value, AffinityError> {
        if !array {
            return read(self);
        }
        let count = self.c.u32()? as usize;
        // Each element is at least a byte; a count beyond the remaining
        // input is corrupt, not a huge allocation request.
        if count > self.c.bytes.len() - self.c.pos {
            return Err(malformed(format!("array count {count} exceeds input")));
        }
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read(self)?);
        }
        Ok(Value::Array(items))
    }

    fn bools(&mut self, array: bool) -> Result<Value, AffinityError> {
        if !array {
            return Ok(Value::Bool(self.c.u8()? != 0));
        }
        let count = self.c.u32()? as usize;
        let bytes = self.c.take(count.div_ceil(8))?;
        Ok(Value::Array(
            (0..count)
                .map(|i| Value::Bool(bytes[i / 8] >> (i % 8) & 1 != 0))
                .collect(),
        ))
    }

    fn enums(&mut self, array: bool) -> Result<Value, AffinityError> {
        if !array {
            let id = self.c.u16()?;
            let version = self.c.u16()?;
            return Ok(Value::Enum { id, version });
        }
        let count = self.c.u32()? as usize;
        let version = self.c.u16()?;
        if count > self.c.bytes.len() - self.c.pos {
            return Err(malformed(format!("enum count {count} exceeds input")));
        }
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let id = self.c.u16()?;
            items.push(Value::Enum { id, version });
        }
        Ok(Value::Array(items))
    }

    fn strings(&mut self, array: bool) -> Result<Value, AffinityError> {
        let read_one = |c: &mut Cursor| -> Result<Value, AffinityError> {
            let len = c.u32()? as usize;
            Ok(Value::Str(
                String::from_utf8_lossy(c.take(len)?).into_owned(),
            ))
        };
        if !array {
            return read_one(&mut self.c);
        }
        let _total_size = self.c.u32()?;
        let count = self.c.u32()? as usize;
        if count > self.c.bytes.len() - self.c.pos {
            return Err(malformed(format!("string count {count} exceeds input")));
        }
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read_one(&mut self.c)?);
        }
        Ok(Value::Array(items))
    }

    fn curves(&mut self, array: bool) -> Result<Value, AffinityError> {
        let count = if array { self.c.u32()? as usize } else { 1 };
        let size = self.c.u16()? as usize;
        if !matches!(size, 12 | 16 | 18 | 24 | 32) {
            return Err(malformed(format!("unknown curve record size {size}")));
        }
        let mut items = Vec::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            items.push(Value::Curve(self.c.take(size)?.to_vec()));
        }
        Ok(if array {
            Value::Array(items)
        } else {
            items.pop().unwrap()
        })
    }

    fn classes(&mut self, ty: u8, array: bool) -> Result<Value, AffinityError> {
        let mut count = 1;
        let mut shared_header = None;
        if array {
            count = self.c.u32()? as usize;
            if ty == 0x32 {
                let t = self.c.u32()?;
                let id = self.c.u16()? as u32;
                shared_header = Some((t, id));
            }
            if count > self.c.bytes.len() - self.c.pos {
                return Err(malformed(format!("class count {count} exceeds input")));
            }
        }
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let item = match ty {
                0x30 => self.class_untagged()?,
                0x31 => self.class_shared()?,
                _ => self.class_tagged(shared_header)?,
            };
            items.push(Value::Class(item));
        }
        Ok(if array {
            Value::Array(items)
        } else {
            items.pop().unwrap()
        })
    }

    /// 0x30: a bare, untagged field list (known composite types).
    fn class_untagged(&mut self) -> Result<Option<usize>, AffinityError> {
        let fields = self.fields(false)?;
        Ok(Some(self.push(Node {
            fields,
            ..Node::default()
        })))
    }

    /// 0x31: null / shared definition with id / link to an earlier id.
    fn class_shared(&mut self) -> Result<Option<usize>, AffinityError> {
        match self.c.u8()? {
            0 => Ok(None),
            1 => {
                let id = self.c.u32()?;
                if self.by_id.contains_key(&id) {
                    return Err(malformed(format!("object id {id} defined twice")));
                }
                let index = self.push(Node {
                    id,
                    ..Node::default()
                });
                self.by_id.insert(id, index);
                // Type chain: sections of (tag, version [+ base fields]),
                // ending with a lone tag (1) or nothing more (2).
                loop {
                    match self.c.u8()? {
                        0 => {
                            let t = self.c.u32()?;
                            let v = self.c.u16()? as u32;
                            let mut fields = self.fields(true)?;
                            let node = &mut self.nodes[index];
                            node.types.push((t, v));
                            node.fields.append(&mut fields);
                        }
                        1 => {
                            let t = self.c.u32()?;
                            self.nodes[index].types.push((t, 0));
                            break;
                        }
                        2 => break,
                        other => {
                            return Err(malformed(format!("unknown type-section flag {other}")))
                        }
                    }
                }
                let mut fields = self.fields(true)?;
                self.nodes[index].fields.append(&mut fields);
                Ok(Some(index))
            }
            2 => {
                let id = self.c.u32()?;
                match self.by_id.get(&id) {
                    Some(&index) => Ok(Some(index)),
                    None => Err(malformed(format!("link to undefined object id {id}"))),
                }
            }
            other => Err(malformed(format!("unknown class flag {other}"))),
        }
    }

    /// 0x32: null / definition with an explicit (or array-shared) tag+id.
    fn class_tagged(&mut self, shared: Option<(u32, u32)>) -> Result<Option<usize>, AffinityError> {
        match self.c.u8()? {
            0 => Ok(None),
            1 => {
                let (t, v) = match shared {
                    Some(h) => h,
                    None => {
                        let t = self.c.u32()?;
                        let v = self.c.u16()? as u32;
                        (t, v)
                    }
                };
                let fields = self.fields(true)?;
                Ok(Some(self.push(Node {
                    types: vec![(t, v)],
                    fields,
                    ..Node::default()
                })))
            }
            other => Err(malformed(format!("unknown class flag {other}"))),
        }
    }

    fn push(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }
}
