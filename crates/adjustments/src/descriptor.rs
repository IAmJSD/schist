//! Minimal reader for PSD "descriptors" — the key/value trees Photoshop
//! uses for most modern adjustment payloads (`blwh`, `SoCo`, newer `curv`).
//!
//! Only the value types adjustments actually use are decoded; anything else
//! is skipped structurally so parsing never derails.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Double(f64),
    Integer(i32),
    Bool(bool),
    Text(String),
    /// (enum type, enum value)
    Enum(String, String),
    /// (unit id, value) — e.g. ("#Prc", 50.0) for 50%.
    Unit(String, f64),
    List(Vec<Value>),
    Object(Descriptor),
    Unknown,
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Double(v) | Value::Unit(_, v) => Some(*v),
            Value::Integer(v) => Some(*v as f64),
            Value::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(v) => Some(*v),
            Value::Integer(v) => Some(*v != 0),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Descriptor> {
        match self {
            Value::Object(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Descriptor {
    pub class: String,
    pub items: HashMap<String, Value>,
}

impl Descriptor {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.items.get(key)
    }

    pub fn number(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }
}

/// Bounds-checked big-endian cursor.
struct Cur<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(data: &'a [u8]) -> Cur<'a> {
        Cur { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn sig4(&mut self) -> Option<String> {
        Some(String::from_utf8_lossy(self.take(4)?).into_owned())
    }

    /// A "key": 4-byte signature, or a length-prefixed string when the
    /// length field is non-zero.
    fn key(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        if len == 0 {
            self.sig4()
        } else {
            Some(String::from_utf8_lossy(self.take(len)?).into_owned())
        }
    }

    /// UTF-16BE string with a u32 character count, NUL-terminated.
    fn unicode(&mut self) -> Option<String> {
        let count = self.u32()? as usize;
        let bytes = self.take(count.checked_mul(2)?)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16_lossy(&units);
        Some(s.trim_end_matches('\0').to_string())
    }
}

/// Parse a descriptor payload (without the leading version field).
pub fn parse(data: &[u8]) -> Option<Descriptor> {
    let mut cur = Cur::new(data);
    read_descriptor(&mut cur)
}

/// Parse a payload that begins with a 4-byte descriptor version (the shape
/// most adjustment blocks use: u16 block version, u32 descriptor version).
pub fn parse_versioned(data: &[u8]) -> Option<Descriptor> {
    if data.len() < 6 {
        return None;
    }
    // u16 layer-block version, then u32 descriptor version.
    let mut cur = Cur::new(&data[6..]);
    read_descriptor(&mut cur)
}

fn read_descriptor(cur: &mut Cur) -> Option<Descriptor> {
    let _name = cur.unicode()?;
    let class = cur.key()?;
    let count = cur.u32()? as usize;
    let mut items = HashMap::new();
    // Guard against corrupt counts claiming millions of entries.
    for _ in 0..count.min(4096) {
        let key = cur.key()?;
        let value = read_value(cur)?;
        items.insert(key, value);
    }
    Some(Descriptor { class, items })
}

fn read_value(cur: &mut Cur) -> Option<Value> {
    let ty = cur.sig4()?;
    Some(match ty.as_str() {
        "doub" => Value::Double(cur.f64()?),
        "long" => Value::Integer(cur.i32()?),
        "bool" => Value::Bool(cur.u8()? != 0),
        "TEXT" => Value::Text(cur.unicode()?),
        "enum" => {
            let ty = cur.key()?;
            let val = cur.key()?;
            Value::Enum(ty, val)
        }
        "UntF" => {
            let unit = cur.sig4()?;
            Value::Unit(unit, cur.f64()?)
        }
        "VlLs" => {
            let count = cur.u32()? as usize;
            let mut out = Vec::new();
            for _ in 0..count.min(4096) {
                out.push(read_value(cur)?);
            }
            Value::List(out)
        }
        "Objc" | "GlbO" => Value::Object(read_descriptor(cur)?),
        // Types we don't need: consume their fixed payloads so the walk
        // stays in sync, or bail out if the size isn't knowable.
        "obj " | "type" | "GlbC" | "alis" | "tdta" => return None,
        _ => Value::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal descriptor payload for tests.
    fn build(class: &str, items: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_be_bytes()); // empty unicode name
        out.extend_from_slice(&0u32.to_be_bytes()); // class as 4-byte sig
        out.extend_from_slice(class.as_bytes());
        out.extend_from_slice(&(items.len() as u32).to_be_bytes());
        for (key, payload) in items {
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(payload);
        }
        out
    }

    fn doub(v: f64) -> Vec<u8> {
        let mut o = b"doub".to_vec();
        o.extend_from_slice(&v.to_be_bytes());
        o
    }

    #[test]
    fn reads_scalars() {
        let mut long = b"long".to_vec();
        long.extend_from_slice(&42i32.to_be_bytes());
        let data = build("Lvls", &[("Rd  ", &doub(12.5)), ("Grn ", &long)]);
        let d = parse(&data).expect("parses");
        assert_eq!(d.class, "Lvls");
        assert_eq!(d.number("Rd  "), Some(12.5));
        assert_eq!(d.number("Grn "), Some(42.0));
    }

    #[test]
    fn reads_units_bools_and_enums() {
        let mut unit = b"UntF".to_vec();
        unit.extend_from_slice(b"#Prc");
        unit.extend_from_slice(&50.0f64.to_be_bytes());
        let mut en = b"enum".to_vec();
        en.extend_from_slice(&0u32.to_be_bytes());
        en.extend_from_slice(b"Md  ");
        en.extend_from_slice(&0u32.to_be_bytes());
        en.extend_from_slice(b"Nrml");
        let data = build(
            "Test",
            &[("Opct", &unit), ("bool", b"bool\x01"), ("Md  ", &en)],
        );
        let d = parse(&data).unwrap();
        assert_eq!(d.number("Opct"), Some(50.0));
        assert_eq!(d.get("bool").unwrap().as_bool(), Some(true));
        assert_eq!(
            d.get("Md  "),
            Some(&Value::Enum("Md  ".into(), "Nrml".into()))
        );
    }

    #[test]
    fn reads_nested_objects_and_lists() {
        let inner = {
            let mut o = b"Objc".to_vec();
            o.extend_from_slice(&build("RGBC", &[("Rd  ", &doub(255.0))]));
            o
        };
        let list = {
            let mut o = b"VlLs".to_vec();
            o.extend_from_slice(&2u32.to_be_bytes());
            o.extend_from_slice(&doub(1.0));
            o.extend_from_slice(&doub(2.0));
            o
        };
        let data = build("Test", &[("Clr ", &inner), ("list", &list)]);
        let d = parse(&data).unwrap();
        let color = d.get("Clr ").unwrap().as_object().unwrap();
        assert_eq!(color.number("Rd  "), Some(255.0));
        assert_eq!(d.get("list").unwrap().as_list().unwrap().len(), 2);
    }

    #[test]
    fn truncated_input_is_none_not_a_panic() {
        let data = build("Test", &[("Rd  ", &doub(1.0))]);
        for cut in 0..data.len() {
            let _ = parse(&data[..cut]);
        }
    }
}
