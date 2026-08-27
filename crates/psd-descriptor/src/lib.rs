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
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
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

/// Most items one descriptor or list may hold. Real Photoshop descriptors
/// are far smaller; a count past this means the stream is not what we
/// think it is.
const MAX_ITEMS: usize = 4096;

fn read_descriptor(cur: &mut Cur) -> Option<Descriptor> {
    let _name = cur.unicode()?;
    let class = cur.key()?;
    let count = cur.u32()? as usize;
    // A corrupt count claiming millions of entries used to be truncated to
    // 4096 and parsing continued, which left the cursor mid-stream: a
    // nested descriptor's parent then read the remains as its own fields.
    // An over-count is a parse failure, not something to carry on from.
    if count > MAX_ITEMS {
        return None;
    }
    let mut items = HashMap::new();
    for _ in 0..count {
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
            if count > MAX_ITEMS {
                return None;
            }
            let mut out = Vec::new();
            for _ in 0..count {
                out.push(read_value(cur)?);
            }
            Value::List(out)
        }
        "Objc" | "GlbO" => Value::Object(read_descriptor(cur)?),
        // Types we don't need: consume their fixed payloads so the walk
        // stays in sync, or bail out if the size isn't knowable.
        "obj " | "type" | "GlbC" | "alis" | "tdta" => return None,
        // An unrecognised signature has an unknown payload size, so the
        // cursor is now pointing into the middle of it. Carrying on read
        // that payload as the next key and value, and the nonsense was
        // then re-encoded on save. Bail out the way the known-unhandled
        // types above already do.
        _ => return None,
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

// ===================================================================
// Encoding
// ===================================================================

/// Builds a descriptor payload.
///
/// Descriptors are order-sensitive in practice -- Photoshop writes its keys
/// in a fixed order and some readers are fussy -- so this appends as it
/// goes rather than round-tripping through the `HashMap` that decoding
/// produces.
#[derive(Debug, Default)]
pub struct Builder {
    class: String,
    /// Encoded key/value pairs, in the order they were added.
    body: Vec<u8>,
    count: u32,
}

impl Builder {
    /// A descriptor of the given class. Photoshop uses `"null"` for the
    /// outermost one in most blocks.
    pub fn new(class: &str) -> Builder {
        Builder {
            class: class.to_string(),
            body: Vec::new(),
            count: 0,
        }
    }

    fn key(&mut self, key: &str) {
        // A four-character key is written with a zero length; anything
        // else is length-prefixed.
        if key.len() == 4 {
            self.body.extend_from_slice(&0u32.to_be_bytes());
            self.body.extend_from_slice(key.as_bytes());
        } else {
            self.body
                .extend_from_slice(&(key.len() as u32).to_be_bytes());
            self.body.extend_from_slice(key.as_bytes());
        }
        self.count += 1;
    }

    fn ty(&mut self, ty: &str) {
        self.body.extend_from_slice(ty.as_bytes());
    }

    pub fn double(&mut self, key: &str, v: f64) -> &mut Self {
        self.key(key);
        self.ty("doub");
        self.body.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn integer(&mut self, key: &str, v: i32) -> &mut Self {
        self.key(key);
        self.ty("long");
        self.body.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn bool(&mut self, key: &str, v: bool) -> &mut Self {
        self.key(key);
        self.ty("bool");
        self.body.push(v as u8);
        self
    }

    pub fn text(&mut self, key: &str, v: &str) -> &mut Self {
        self.key(key);
        self.ty("TEXT");
        write_unicode(&mut self.body, v);
        self
    }

    /// An enumerated value, e.g. `enumerated("Md  ", "BlnM", "Mltp")` for
    /// the Multiply blend mode.
    pub fn enumerated(&mut self, key: &str, ty: &str, value: &str) -> &mut Self {
        self.key(key);
        self.ty("enum");
        write_key(&mut self.body, ty);
        write_key(&mut self.body, value);
        self
    }

    /// A value with a unit, e.g. `unit("Opct", "#Prc", 75.0)` for 75%.
    pub fn unit(&mut self, key: &str, unit: &str, v: f64) -> &mut Self {
        self.key(key);
        self.ty("UntF");
        self.body.extend_from_slice(unit.as_bytes());
        self.body.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn percent(&mut self, key: &str, v: f64) -> &mut Self {
        self.unit(key, "#Prc", v)
    }

    pub fn pixels(&mut self, key: &str, v: f64) -> &mut Self {
        self.unit(key, "#Pxl", v)
    }

    pub fn angle(&mut self, key: &str, v: f64) -> &mut Self {
        self.unit(key, "#Ang", v)
    }

    /// A nested descriptor.
    pub fn object(&mut self, key: &str, child: Builder) -> &mut Self {
        self.key(key);
        self.ty("Objc");
        self.body.extend_from_slice(&child.finish_body());
        self
    }

    /// A list of nested descriptors, the shape lists take in practice.
    pub fn object_list(&mut self, key: &str, children: Vec<Builder>) -> &mut Self {
        self.key(key);
        self.ty("VlLs");
        self.body
            .extend_from_slice(&(children.len() as u32).to_be_bytes());
        for child in children {
            self.body.extend_from_slice(b"Objc");
            self.body.extend_from_slice(&child.finish_body());
        }
        self
    }

    /// An RGB colour descriptor, the `RGBC` object Photoshop uses for every
    /// colour in an effects block. Components are 0..=255.
    pub fn color(&mut self, key: &str, r: f64, g: f64, b: f64) -> &mut Self {
        let mut c = Builder::new("RGBC");
        c.double("Rd  ", r).double("Grn ", g).double("Bl  ", b);
        self.object(key, c)
    }

    /// The descriptor body: name, class, count, then the pairs. This is
    /// what a nested `Objc` value contains.
    fn finish_body(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 32);
        // Empty unicode name.
        write_unicode(&mut out, "");
        write_key(&mut out, &self.class);
        out.extend_from_slice(&self.count.to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }

    /// A standalone descriptor payload, without any version prefix.
    pub fn finish(self) -> Vec<u8> {
        self.finish_body()
    }

    /// A payload prefixed the way most layer blocks want it: a u32
    /// descriptor version of 16, then the descriptor.
    pub fn finish_versioned(self) -> Vec<u8> {
        let mut out = 16u32.to_be_bytes().to_vec();
        out.extend_from_slice(&self.finish_body());
        out
    }
}

fn write_key(out: &mut Vec<u8>, key: &str) {
    if key.len() == 4 {
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(key.as_bytes());
    } else {
        out.extend_from_slice(&(key.len() as u32).to_be_bytes());
        out.extend_from_slice(key.as_bytes());
    }
}

fn write_unicode(out: &mut Vec<u8>, s: &str) {
    let units: Vec<u16> = s.encode_utf16().collect();
    // The count includes the terminating NUL.
    out.extend_from_slice(&((units.len() + 1) as u32).to_be_bytes());
    for u in units {
        out.extend_from_slice(&u.to_be_bytes());
    }
    out.extend_from_slice(&0u16.to_be_bytes());
}

#[cfg(test)]
mod encode_tests {
    use super::*;

    #[test]
    fn scalars_round_trip() {
        let mut b = Builder::new("null");
        b.double("dbl ", 1.5)
            .integer("int ", -7)
            .bool("bool", true)
            .text("text", "hello")
            .enumerated("Md  ", "BlnM", "Mltp")
            .percent("Opct", 75.0)
            .pixels("Dstn", 12.0);
        let bytes = b.finish();
        let d = parse(&bytes).expect("parses back");

        assert_eq!(d.class, "null");
        assert_eq!(d.number("dbl "), Some(1.5));
        assert_eq!(d.number("int "), Some(-7.0));
        assert_eq!(d.get("bool").unwrap().as_bool(), Some(true));
        assert_eq!(d.get("text"), Some(&Value::Text("hello".into())));
        assert_eq!(
            d.get("Md  "),
            Some(&Value::Enum("BlnM".into(), "Mltp".into()))
        );
        assert_eq!(d.number("Opct"), Some(75.0));
        assert_eq!(d.number("Dstn"), Some(12.0));
    }

    #[test]
    fn nested_objects_and_lists_round_trip() {
        let mut inner = Builder::new("DrSh");
        inner.bool("enab", true).percent("Opct", 50.0);
        let mut b = Builder::new("null");
        b.object("DrSh", inner);
        let mut one = Builder::new("Grad");
        one.double("Lctn", 0.0);
        let mut two = Builder::new("Grad");
        two.double("Lctn", 4096.0);
        b.object_list("Clrs", vec![one, two]);

        let d = parse(&b.finish()).expect("parses back");
        let shadow = d.get("DrSh").unwrap().as_object().unwrap();
        assert_eq!(shadow.class, "DrSh");
        assert_eq!(shadow.number("Opct"), Some(50.0));
        let stops = d.get("Clrs").unwrap().as_list().unwrap();
        assert_eq!(stops.len(), 2);
        assert_eq!(
            stops[1].as_object().and_then(|o| o.number("Lctn")),
            Some(4096.0)
        );
    }

    #[test]
    fn colours_use_the_rgbc_shape_photoshop_expects() {
        let mut b = Builder::new("null");
        b.color("Clr ", 255.0, 128.0, 0.0);
        let d = parse(&b.finish()).unwrap();
        let c = d.get("Clr ").unwrap().as_object().unwrap();
        assert_eq!(c.class, "RGBC");
        assert_eq!(c.number("Rd  "), Some(255.0));
        assert_eq!(c.number("Grn "), Some(128.0));
        assert_eq!(c.number("Bl  "), Some(0.0));
    }

    #[test]
    fn versioned_payloads_round_trip_through_the_versioned_parser() {
        let mut b = Builder::new("null");
        b.percent("Scl ", 100.0);
        // parse_versioned skips a u16 block version plus a u32 descriptor
        // version, so prepend the u16 the layer block itself carries.
        let mut payload = 0u16.to_be_bytes().to_vec();
        payload.extend_from_slice(&b.finish_versioned());
        let d = parse_versioned(&payload).expect("parses back");
        assert_eq!(d.number("Scl "), Some(100.0));
    }

    #[test]
    fn long_keys_are_length_prefixed() {
        let mut b = Builder::new("null");
        b.bool("masterFXSwitch", true);
        let d = parse(&b.finish()).unwrap();
        assert_eq!(d.get("masterFXSwitch").unwrap().as_bool(), Some(true));
    }
    /// A descriptor whose first value has an unrecognised type, and whose
    /// payload happens to spell a valid key/value pair.
    ///
    /// That is the case that matters: skipping the signature but not the
    /// payload let the walk read the payload as the *next* item and carry
    /// on, so the parse succeeded with values that were never in the file.
    fn descriptor_with_unknown_type() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_be_bytes()); // empty unicode name
        b.extend_from_slice(&0u32.to_be_bytes()); // class key length
        b.extend_from_slice(b"null");
        b.extend_from_slice(&2u32.to_be_bytes()); // two items
                                                  // Item 1: an unknown value type whose 16-byte payload reads as a
                                                  // complete key + long value.
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"Ky01");
        b.extend_from_slice(b"ZZZZ");
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"Ky02");
        b.extend_from_slice(b"long");
        b.extend_from_slice(&7i32.to_be_bytes());
        b
    }

    #[test]
    fn an_unknown_value_type_stops_rather_than_desyncing() {
        // Previously the cursor consumed the 4-byte signature but not the
        // payload, so the walk read that payload as the next item and
        // returned a descriptor holding a value the file never contained.
        // A layer style or SoCo fill using an unhandled type therefore
        // came back with arbitrary colours and offsets, which were then
        // re-encoded on save.
        let bytes = descriptor_with_unknown_type();
        let parsed = parse(&bytes);
        assert!(
            parsed.is_none(),
            "an unknown type must fail the parse rather than inventing \
             items, got {parsed:?}"
        );
    }

    #[test]
    fn an_absurd_item_count_fails_rather_than_truncating() {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"null");
        b.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&b).is_none(), "an over-count must not be truncated");
    }
}
