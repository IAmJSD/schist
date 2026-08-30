//! Asking each harness which models this account actually has.
//!
//! Claude Code reports its model list in the Agent SDK's initialize
//! handshake (the same list its own `/model` picker shows — tiers,
//! display names, descriptions, all account-aware). Codex answers a
//! `model/list` request on its app-server. Either way the probe is a
//! short-lived CLI spawn on its own thread; the result lands in the
//! shared event queue and is cached for the rest of the run. On any
//! failure the static fallback catalog is reported instead, so the
//! picker is never empty.

use super::{AgentEvent, AiShared, Backend, ModelEntry};
use anyhow::{anyhow, Context as _, Result};

pub fn fetch(backend: Backend, shared: AiShared) {
    let worker = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("ai-models".into())
        .spawn(move || {
            let models = match probe(backend) {
                Ok(models) if !models.is_empty() => models,
                Ok(_) => backend.fallback_models(),
                Err(e) => {
                    log::warn!("listing {} models failed: {e:#}", backend.label());
                    backend.fallback_models()
                }
            };
            worker.push(AgentEvent::Models(backend, models));
        });
    if spawned.is_err() {
        shared.push(AgentEvent::Models(backend, backend.fallback_models()));
    }
}

fn probe(backend: Backend) -> Result<Vec<ModelEntry>> {
    match backend {
        Backend::Claude => claude_models(),
        Backend::Codex => codex_models(),
    }
}

/// Connect the Agent SDK client just long enough to read the server info
/// its initialize handshake carries, then hang up.
fn claude_models() -> Result<Vec<ModelEntry>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let options = claude_agent_sdk_rs::ClaudeAgentOptions {
            cwd: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(std::path::PathBuf::from),
            skip_version_check: true,
            ..Default::default()
        };
        let mut client = claude_agent_sdk_rs::ClaudeClient::new(options);
        client.connect().await.context("starting Claude Code")?;
        let info = client.get_server_info();
        let _ = client.disconnect().await;
        let info = info.ok_or_else(|| anyhow!("Claude Code sent no server info"))?;
        // The SDK hands back the control response's payload, which keeps
        // the initialize result under one more "response" key.
        let list = info
            .get("models")
            .or_else(|| info.get("response").and_then(|r| r.get("models")))
            .and_then(|m| m.as_array())
            .ok_or_else(|| anyhow!("no models in Claude Code's server info"))?;
        // The CLI's "default" alias is dropped — a user's coding default
        // is not presumed to be their pick here — but what it resolves to
        // marks the row that seeds the first selection.
        let recommended = list
            .iter()
            .find(|m| m.get("value").and_then(|v| v.as_str()) == Some("default"))
            .and_then(|m| m.get("resolvedModel"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(list
            .iter()
            .filter_map(|m| {
                let value = m.get("value")?.as_str()?;
                if value == "default" {
                    return None;
                }
                let resolved = m
                    .get("resolvedModel")
                    .and_then(|v| v.as_str())
                    .unwrap_or(value);
                Some(ModelEntry {
                    slug: value.to_string(),
                    name: versioned_name(resolved).unwrap_or_else(|| {
                        m.get("displayName")
                            .and_then(|v| v.as_str())
                            .unwrap_or(value)
                            .to_string()
                    }),
                    detail: m
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    recommended: !recommended.is_empty() && resolved == recommended,
                })
            })
            .collect())
    })
}

/// One `model/list` round trip on a throwaway app-server.
fn codex_models() -> Result<Vec<ModelEntry>> {
    let mut builder = codex_codes::AppServerBuilder::new();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        builder = builder.working_directory(std::path::PathBuf::from(home));
    }
    let child = builder.spawn_sync().context("starting codex app-server")?;
    let mut client =
        codex_codes::SyncClient::new(child).context("attaching to codex app-server")?;
    client.initialize(&codex_codes::InitializeParams {
        client_info: codex_codes::ClientInfo {
            name: "schist".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            title: Some("Schist".to_string()),
        },
        capabilities: None,
    })?;
    let listed: codex_codes::ModelListResponse = client.request(
        codex_codes::protocol::methods::MODEL_LIST,
        &codex_codes::ModelListParams::default(),
    )?;
    let _ = client.shutdown();
    Ok(listed
        .data
        .into_iter()
        .filter(|m| !m.hidden)
        .map(|m| ModelEntry {
            slug: m.model,
            name: m.display_name,
            detail: m.description,
            recommended: m.is_default,
        })
        .collect())
}

/// A versioned display name from a model id, the way T3-style pickers
/// spell them: `claude-opus-5[1m]` reads "Claude Opus 5" and
/// `claude-haiku-4-5-20251001` reads "Claude Haiku 4.5". `None` when the
/// id doesn't look like one, so the caller can keep the CLI's own label.
fn versioned_name(resolved: &str) -> Option<String> {
    let id = resolved.trim_end_matches("[1m]").strip_prefix("claude-")?;
    let mut words: Vec<String> = Vec::new();
    let mut version: Vec<&str> = Vec::new();
    for part in id.split('-') {
        if part.chars().all(|c| c.is_ascii_digit()) {
            // An 8-digit run is a snapshot date, not a version.
            if part.len() < 8 {
                version.push(part);
            }
        } else {
            let mut chars = part.chars();
            let first = chars.next()?;
            words.push(first.to_uppercase().collect::<String>() + chars.as_str());
        }
    }
    if words.is_empty() {
        return None;
    }
    let mut out = format!("Claude {}", words.join(" "));
    if !version.is_empty() {
        out.push(' ');
        out.push_str(&version.join("."));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::versioned_name;

    #[test]
    fn model_ids_read_as_versioned_names() {
        assert_eq!(
            versioned_name("claude-opus-5[1m]").as_deref(),
            Some("Claude Opus 5")
        );
        assert_eq!(
            versioned_name("claude-fable-5").as_deref(),
            Some("Claude Fable 5")
        );
        assert_eq!(
            versioned_name("claude-haiku-4-5-20251001").as_deref(),
            Some("Claude Haiku 4.5")
        );
        assert_eq!(
            versioned_name("claude-sonnet-5").as_deref(),
            Some("Claude Sonnet 5")
        );
        // Not a claude id: leave the CLI's label alone.
        assert_eq!(versioned_name("gpt-5.6-terra"), None);
    }
}
