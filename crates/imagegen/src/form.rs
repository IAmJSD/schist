//! The generation form a provider describes, and its live text preview.

use crate::{http_error, Account, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A free-form text input. Its value is sent under `id`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TextBox {
    pub title: String,
    pub description: String,
    /// Generation is refused until this has a value.
    pub required: bool,
    pub id: String,
}

/// One option of a [`SelectBox`]: `id` is sent, `text` is shown.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SelectValue {
    pub id: String,
    pub text: String,
}

/// A pick-from-a-list input. The chosen option ids are sent under `id`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SelectBox {
    pub title: String,
    pub description: String,
    /// Generation is refused until something is picked.
    pub required: bool,
    pub id: String,
    /// Whether more than one option may be held at once.
    pub multiple: bool,
    pub values: Vec<SelectValue>,
}

/// One item of the form, in the order it should be shown.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t")]
pub enum FormItem {
    #[serde(rename = "text")]
    Text(TextBox),
    #[serde(rename = "select")]
    Select(SelectBox),
    /// Not an input: a block of text the provider renders from whatever
    /// the user has entered so far. The app posts the current values to
    /// `live_preview_url` when they change and shows what comes back.
    #[serde(rename = "live_text_preview")]
    LiveTextPreview { live_preview_url: String },
}

impl FormItem {
    /// The key this item's value travels under, or `None` for the ones
    /// that are not inputs.
    pub fn id(&self) -> Option<&str> {
        match self {
            FormItem::Text(t) => Some(&t.id),
            FormItem::Select(s) => Some(&s.id),
            FormItem::LiveTextPreview { .. } => None,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            FormItem::Text(t) => &t.title,
            FormItem::Select(s) => &s.title,
            FormItem::LiveTextPreview { .. } => "",
        }
    }

    pub fn required(&self) -> bool {
        match self {
            FormItem::Text(t) => t.required,
            FormItem::Select(s) => s.required,
            FormItem::LiveTextPreview { .. } => false,
        }
    }
}

/// What one field holds: the protocol's `string | string[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Text(String),
    Choices(Vec<String>),
}

impl FieldValue {
    /// Whether this counts as filled in for a `required` item.
    pub fn is_empty(&self) -> bool {
        match self {
            FieldValue::Text(t) => t.trim().is_empty(),
            FieldValue::Choices(c) => c.is_empty(),
        }
    }
}

/// The body sent to the preview and generation endpoints: every field's
/// value, keyed by its `id`.
pub type FormValues = BTreeMap<String, FieldValue>;

/// The title of the first required item `values` has nothing for.
///
/// The protocol leaves the check to the app — "the app should refuse to
/// generate until the user has given a value" — so the Generate button
/// asks this rather than letting the provider reject the request.
pub fn first_missing_required<'a>(items: &'a [FormItem], values: &FormValues) -> Option<&'a str> {
    items.iter().find_map(|item| {
        if !item.required() {
            return None;
        }
        let id = item.id()?;
        match values.get(id) {
            Some(v) if !v.is_empty() => None,
            _ => Some(item.title()),
        }
    })
}

/// Fetch the form to draw. Renews the token first if it has expired, so
/// persist `account` afterwards.
pub fn generation_structure(account: &mut Account) -> Result<Vec<FormItem>> {
    account.refresh_if_needed()?;
    ureq::get(&account.tokens.generation_endpoint_url)
        .header("User-Agent", "schist-imagegen")
        .header("Authorization", account.bearer())
        .call()
        .map_err(|e| http_error("the generation endpoint", e))?
        .body_mut()
        .read_json()
        .map_err(|e| Error::Protocol(format!("the generation form is unreadable: {e}")))
}

/// Render one `live_text_preview` item against the current values.
/// Renews the token first if it has expired, so persist `account`
/// afterwards.
pub fn live_text_preview(
    account: &mut Account,
    preview_url: &str,
    values: &FormValues,
) -> Result<String> {
    account.refresh_if_needed()?;
    ureq::post(preview_url)
        .header("User-Agent", "schist-imagegen")
        .header("Authorization", account.bearer())
        .send_json(values)
        .map_err(|e| http_error("the live preview", e))?
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Protocol(format!("the live preview is unreadable: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORM: &str = r#"[
        {"t":"text","title":"Prompt","description":"What to draw","required":true,"id":"prompt"},
        {"t":"select","title":"Style","description":"","required":false,"id":"style",
         "multiple":true,"values":[{"id":"oil","text":"Oil paint"}]},
        {"t":"live_text_preview","live_preview_url":"https://schist.app/preview"}
    ]"#;

    #[test]
    fn the_form_parses_by_its_tag() {
        let items: Vec<FormItem> = serde_json::from_str(FORM).unwrap();
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], FormItem::Text(t) if t.id == "prompt" && t.required));
        assert!(matches!(&items[1], FormItem::Select(s) if s.multiple && s.values.len() == 1));
        assert!(
            matches!(&items[2], FormItem::LiveTextPreview { live_preview_url }
                if live_preview_url == "https://schist.app/preview")
        );
        // Only the inputs contribute a key to the body.
        assert_eq!(items[2].id(), None);
    }

    #[test]
    fn an_unknown_item_kind_is_not_silently_dropped() {
        // A provider on a later spec version has to be told, not guessed
        // at: dropping the item would send a body missing a field it
        // required.
        let one = r#"[{"t":"colour_wheel","id":"tint"}]"#;
        assert!(serde_json::from_str::<Vec<FormItem>>(one).is_err());
    }

    #[test]
    fn required_items_have_to_be_filled_in() {
        let items: Vec<FormItem> = serde_json::from_str(FORM).unwrap();
        let mut values = FormValues::new();
        assert_eq!(first_missing_required(&items, &values), Some("Prompt"));
        // Whitespace is not an answer.
        values.insert("prompt".into(), FieldValue::Text("   ".into()));
        assert_eq!(first_missing_required(&items, &values), Some("Prompt"));
        values.insert("prompt".into(), FieldValue::Text("a mountain".into()));
        assert_eq!(first_missing_required(&items, &values), None);
    }

    #[test]
    fn values_serialize_as_string_or_array() {
        let mut values = FormValues::new();
        values.insert("prompt".into(), FieldValue::Text("a mountain".into()));
        values.insert(
            "style".into(),
            FieldValue::Choices(vec!["oil".into(), "ink".into()]),
        );
        assert_eq!(
            serde_json::to_string(&values).unwrap(),
            r#"{"prompt":"a mountain","style":["oil","ink"]}"#
        );
    }
}
