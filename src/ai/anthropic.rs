use anyhow::{Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{AiMessage, StreamEvent, build_common_headers, consume_sse_stream};
use crate::storage::config::{AuthMode, ProviderProfile};

pub async fn stream(
    client: &reqwest::Client,
    provider: &ProviderProfile,
    system_prompt: Option<&str>,
    messages: &[AiMessage],
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> Result<()> {
    let token = provider.token.trim();
    if token.is_empty() && provider.auth_mode != AuthMode::None {
        anyhow::bail!(
            "No API key/token configured for '{}'. Open Settings to add one.",
            provider.name
        );
    }

    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{base}/messages");

    let api_msgs: Vec<Value> = messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect();

    let mut body = json!({
        "model": provider.model,
        "max_tokens": 4096,
        "messages": api_msgs,
        "stream": true
    });
    if let Some(sys) = system_prompt {
        body["system"] = json!(sys);
    }

    let mut headers = build_common_headers(provider)?;
    if !token.is_empty() {
        match provider.auth_mode {
            AuthMode::ApiKey => {
                headers.insert(
                    HeaderName::from_static("x-api-key"),
                    HeaderValue::from_str(token).context("invalid Anthropic key")?,
                );
            }
            AuthMode::BearerToken => {
                headers.insert(
                    reqwest::header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {token}"))?,
                );
            }
            AuthMode::CustomHeaders | AuthMode::None => {}
        }
    }
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );

    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    consume_sse_stream(resp, tx, extract_anthropic_delta).await
}

pub fn extract_anthropic_delta(payload: &str) -> Result<Option<String>> {
    let val: Value = serde_json::from_str(payload)
        .with_context(|| format!("invalid Anthropic SSE JSON: {payload}"))?;
    if val["type"] == "error" {
        let msg = val["error"]["message"].as_str().unwrap_or("Anthropic error");
        anyhow::bail!("{msg}");
    }
    if val["type"] == "content_block_delta" {
        return Ok(val["delta"]["text"].as_str().map(|s| s.to_string()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anthropic_delta() {
        let json = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Arch Linux"}}"#;
        assert_eq!(
            extract_anthropic_delta(json).unwrap(),
            Some("Arch Linux".into())
        );
    }
}
