//! Image-generation API. Its per-generation socket keeps the legacy
//! slot/chunk wire format; all workspace features still share the MessagePack socket.
use crate::{auth, Account};
use crate::{
    runtime,
    socket::{Message, Socket},
};
use anyhow::{anyhow, ensure, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Item {
    Text {
        title: String,
        description: String,
        required: bool,
        id: String,
    },
    Select {
        title: String,
        description: String,
        required: bool,
        id: String,
        multiple: bool,
        values: Vec<Choice>,
    },
    LiveTextPreview {
        live_preview_url: String,
    },
}
#[derive(Clone, Debug, Deserialize)]
pub struct Choice {
    pub id: String,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Multiple(Vec<String>),
}
pub type Inputs = BTreeMap<String, Input>;
#[derive(Clone, Debug, Deserialize)]
pub struct Part {
    pub part_name: String,
    pub children_count: usize,
}
pub enum Event {
    Layout(Vec<Part>),
    Image {
        index: usize,
        bytes: Vec<u8>,
    },
    Complete {
        index: usize,
        rejected: Option<String>,
    },
}
async fn request(
    account: &Account,
    method: &str,
    url: &str,
    inputs: Option<&Inputs>,
) -> Result<Vec<u8>> {
    auth::secure_url(url, "https")?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        let agent = auth::agent();
        let token = format!("Bearer {}", account.credentials.access_token);
        let mut response = if method == "POST" {
            agent
                .post(url)
                .header("Authorization", token)
                .send_json(inputs)?
        } else {
            agent.get(url).header("Authorization", token).call()?
        };
        Ok(response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_vec()?)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let body = inputs.map(serde_json::to_vec).transpose()?;
        Ok(auth::fetch(
            method,
            url,
            body.as_ref().map(|_| "application/json"),
            body.as_deref(),
            Some(&account.credentials.access_token),
            1024 * 1024,
        )
        .await?
        .bytes)
    }
}
pub async fn form(account: &Account) -> Result<Vec<Item>> {
    let items: Vec<Item> = serde_json::from_slice(
        &request(
            account,
            "GET",
            &account.credentials.generation_endpoint_url,
            None,
        )
        .await?,
    )?;
    let mut ids = HashSet::new();
    for item in &items {
        match item {
            Item::Text { id, .. } | Item::Select { id, .. } => {
                ensure!(
                    !id.is_empty() && ids.insert(id.clone()),
                    "Invalid generation field ID"
                );
            }
            Item::LiveTextPreview { live_preview_url } => {
                auth::secure_url(live_preview_url, "https")?;
            }
        }
    }
    Ok(items)
}
pub async fn preview(account: &Account, url: &str, inputs: &Inputs) -> Result<String> {
    Ok(String::from_utf8(
        request(account, "POST", url, Some(inputs)).await?,
    )?)
}
pub fn validate(items: &[Item], inputs: &Inputs) -> Result<()> {
    for item in items {
        match item {
            Item::Text {
                id,
                title,
                required,
                ..
            } => {
                if *required {
                    ensure!(
                        matches!(inputs.get(id),Some(Input::Text(s)) if !s.trim().is_empty()),
                        "Enter {title}"
                    );
                }
            }
            Item::Select {
                id,
                title,
                required,
                multiple,
                values,
                ..
            } => {
                let selected = match inputs.get(id) {
                    Some(Input::Text(s)) if !multiple => vec![s.clone()],
                    Some(Input::Multiple(v)) if *multiple => v.clone(),
                    None => vec![],
                    _ => return Err(anyhow!("Invalid value for {title}")),
                };
                ensure!(!required || !selected.is_empty(), "Choose {title}");
                ensure!(
                    selected.iter().all(|id| values.iter().any(|v| &v.id == id)),
                    "Invalid option for {title}"
                );
            }
            Item::LiveTextPreview { .. } => {}
        }
    }
    Ok(())
}
pub async fn generate(
    account: &Account,
    inputs: &Inputs,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Event),
) -> Result<()> {
    let url = String::from_utf8(
        request(
            account,
            "POST",
            &account.credentials.generation_endpoint_url,
            Some(inputs),
        )
        .await?,
    )?;
    auth::secure_url(&url, "wss")?;
    let mut ws = runtime::timeout(Duration::from_secs(15), Socket::connect(&url)).await??;
    let mut slots = None;
    let mut terminal = HashSet::new();
    let mut chunks: HashMap<usize, Vec<u8>> = HashMap::new();
    let mut last = web_time::Instant::now();
    loop {
        ensure!(!cancel.load(Ordering::Relaxed), "Generation cancelled");
        ensure!(
            last.elapsed() < Duration::from_secs(120),
            "Generation timed out"
        );
        let message = tokio::select! {
            message = ws.next() => message.ok_or_else(|| anyhow!("Generation connection closed"))??,
            _ = runtime::sleep(Duration::from_millis(100)) => continue,
        };
        last = web_time::Instant::now();
        match message {
            Message::Text(text) if slots.is_none() => {
                let layout: Vec<Part> = serde_json::from_str(&text)?;
                let count = layout
                    .iter()
                    .try_fold(0usize, |n, p| n.checked_add(p.children_count))
                    .ok_or_else(|| anyhow!("Invalid layout"))?;
                ensure!(count <= 128, "Generation layout exceeds 128 slots");
                emit(Event::Layout(layout));
                slots = Some(count);
                if count == 0 {
                    break;
                }
            }
            Message::Text(text) => {
                let status: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                ensure!((1..=2).contains(&status.len()), "Invalid slot status");
                let index = status[0]
                    .as_u64()
                    .ok_or_else(|| anyhow!("Invalid slot index"))?
                    as usize;
                ensure!(index < slots.unwrap(), "Slot outside layout");
                let rejected = if status.len() == 2 {
                    Some(
                        status[1]
                            .as_str()
                            .ok_or_else(|| anyhow!("Invalid rejection reason"))?
                            .into(),
                    )
                } else {
                    None
                };
                if terminal.insert(index) {
                    chunks.remove(&index);
                    emit(Event::Complete { index, rejected });
                }
                if terminal.len() == slots.unwrap() {
                    break;
                }
            }
            Message::Binary(frame) => {
                ensure!(
                    !frame.is_empty() && slots.is_some(),
                    "Expected layout before image bytes"
                );
                let index = (frame[0] & 127) as usize;
                ensure!(
                    index < slots.unwrap() && !terminal.contains(&index),
                    "Invalid or completed slot"
                );
                let data = chunks.entry(index).or_default();
                ensure!(
                    data.len() + frame.len() - 1 <= 64 * 1024 * 1024,
                    "Generated image exceeds 64 MiB"
                );
                data.extend_from_slice(&frame[1..]);
                if frame[0] & 128 != 0 {
                    emit(Event::Image {
                        index,
                        bytes: chunks.remove(&index).unwrap(),
                    });
                }
            }
            Message::Ping => {
                ws.flush().await?;
            }
            Message::Pong => {}
            Message::Close(_) => {
                return Err(anyhow!("Generation closed before every slot completed"))
            }
        }
    }
    let _ = ws.close().await;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn required_fields() {
        let items = vec![Item::Text {
            id: "prompt".into(),
            title: "Prompt".into(),
            description: String::new(),
            required: true,
        }];
        assert!(validate(&items, &Inputs::new()).is_err());
        assert!(validate(
            &items,
            &BTreeMap::from([("prompt".into(), Input::Text("A photo".into()))])
        )
        .is_ok());
    }
}
