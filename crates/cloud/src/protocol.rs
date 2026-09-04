use anyhow::{bail, ensure, Result};
pub use rmpv::Value;
use serde::{Deserialize, Serialize};

pub const DEFAULT_DOMAIN: &str = "schist.app";
pub const MAX_FRAME: usize = 256 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: f64,
    pub generation_endpoint_url: String,
    pub logout_url: String,
    pub workspace_websocket_url: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub domain: String,
    pub exchange_url: String,
    pub credentials: Credentials,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Folder {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Bucket {
    pub id: String,
    pub name: String,
    pub revision: u64,
    pub rule: Option<Rule>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub scope: Scope,
    pub text: String,
    pub filters: Filters,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Asset {
    pub id: String,
    pub folder_id: Option<String>,
    pub name: String,
    pub mime_type: String,
    pub revision: u64,
    pub size: u64,
    pub edited: bool,
    pub tags: Vec<String>,
    pub rating: u8,
    pub captured_at: Option<u64>,
    pub modified_at: u64,
    pub thumbnail_url: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Library,
    Folder {
        id: String,
        recursive: bool,
    },
    Bucket {
        id: String,
    },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Filters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_rating: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Bounds {
    pub south: f64,
    pub north: f64,
    pub west: f64,
    pub east: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AssetQuery {
    pub scope: Scope,
    pub text: String,
    pub filters: Filters,
    pub sort: String,
    pub offset: u64,
    pub limit: u64,
}
impl Default for AssetQuery {
    fn default() -> Self {
        Self {
            scope: Scope::Library,
            text: String::new(),
            filters: Filters::default(),
            sort: "name".into(),
            offset: 0,
            limit: 100,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogueQuery {
    pub text: String,
    pub offset: u64,
    pub limit: u64,
}
impl Default for CatalogueQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            offset: 0,
            limit: 500,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchQuery {
    Folders { query: CatalogueQuery },
    Buckets { query: CatalogueQuery },
    Assets { query: Box<AssetQuery> },
}
#[derive(Clone, Debug, Deserialize)]
pub struct Snapshot {
    pub kind: String,
    pub revision: u64,
    pub total: u64,
    pub offset: u64,
    pub items: Vec<Value>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Ready {
        protocol: u32,
    },
    Result {
        id: String,
        value: Value,
    },
    Error {
        id: String,
        error: Failure,
    },
    Snapshot {
        subscription_id: String,
        snapshot: Snapshot,
    },
    WatchError {
        subscription_id: String,
        error: Failure,
    },
    DocumentUpdate {
        document_id: String,
        #[serde(with = "serde_bytes")]
        update: Vec<u8>,
    },
    DocumentError {
        document_id: String,
        error: Failure,
    },
    AuthExpiring,
    Pong,
}

pub fn value<T: Serialize>(v: T) -> Value {
    rmp_serde::from_slice(&rmp_serde::to_vec_named(&v).expect("serializable protocol value"))
        .expect("valid MessagePack value")
}
pub fn parse<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T> {
    Ok(rmpv::ext::from_value(v)?)
}
pub fn map(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Map(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
pub fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value> {
    v.as_map()
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .map(|(_, v)| v)
        })
        .ok_or_else(|| anyhow::anyhow!("missing {name}"))
}
pub fn bytes(v: &Value, name: &str) -> Result<Vec<u8>> {
    match field(v, name)? {
        Value::Binary(b) => Ok(b.clone()),
        _ => bail!("{name} must be MessagePack binary"),
    }
}
pub fn string(v: &Value, name: &str) -> Result<String> {
    Ok(field(v, name)?
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("{name} must be a string"))?
        .into())
}
pub fn encode(v: &Value) -> Result<Vec<u8>> {
    let b = rmp_serde::to_vec_named(v)?;
    ensure!(b.len() <= MAX_FRAME, "Message exceeds 256 MiB");
    Ok(b)
}
pub fn decode(b: &[u8]) -> Result<ServerMessage> {
    ensure!(b.len() <= MAX_FRAME, "Message exceeds 256 MiB");
    let mut cursor = std::io::Cursor::new(b);
    let v = rmpv::decode::read_value_with_max_depth(&mut cursor, 64)?;
    ensure!(
        cursor.position() as usize == b.len(),
        "Trailing MessagePack data"
    );
    // serde_bytes also accepts arrays, but the wire contract specifically requires bin.
    if field(&v, "type")?.as_str() == Some("document_update") {
        bytes(&v, "update")?;
    }
    parse(v)
}
pub fn parse_date(raw: &str, end: bool) -> Result<Option<u64>> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")?;
    let time = date
        .and_hms_opt(
            if end { 23 } else { 0 },
            if end { 59 } else { 0 },
            if end { 59 } else { 0 },
        )
        .unwrap()
        .and_utc()
        .timestamp();
    ensure!(time >= 0, "Date precedes 1970");
    Ok(Some(time as u64))
}
pub fn format_date(t: u64) -> String {
    chrono::DateTime::from_timestamp(t as i64, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_round_trip_and_trailing_data() {
        let b: Vec<u8> = (0..=255).collect();
        let v = map([
            ("type", "document_update".into()),
            ("document_id", "asset".into()),
            ("update", Value::Binary(b.clone())),
        ]);
        let encoded = encode(&v).unwrap();
        match decode(&encoded).unwrap() {
            ServerMessage::DocumentUpdate { update, .. } => assert_eq!(update, b),
            _ => panic!(),
        }
        let mut trailing = encoded;
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let array = map([
            ("type", "document_update".into()),
            ("document_id", "asset".into()),
            ("update", Value::Array(vec![1.into(), 2.into()])),
        ]);
        assert!(decode(&encode(&array).unwrap()).is_err());
    }
    #[test]
    fn absent_filters_are_not_nil() {
        let v = value(Filters::default());
        assert!(v.as_map().unwrap().is_empty());
    }
}
