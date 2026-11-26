use anyhow::{Context, Result};
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
    if matches!(provider.auth_mode, AuthMode::ApiKey | AuthMode::BearerToken)
        && provider.token.trim().is_empty()
    {
        anyhow::bail!(
            "No token/API key configured for '{}'. Open Settings to add one.",
            provider.name
        );
    }

    let base = provider.base_url.trim_end_matches('/');
    let url = format!("{base}/chat/completions");

    let mut payload_msgs = Vec::new();
    if let Some(sys) = system_prompt {
        payload_msgs.push(json!({ "role": "system", "content": sys }));
    }
    for m in messages {
        payload_msgs.push(json!({ "role": m.role, "content": m.content }));
    }

    let body = json!({
        "model": provider.model,
        "messages": payload_msgs,
        "stream": true
    });

    let headers = build_common_headers(provider)?;
    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    consume_sse_stream(resp, tx, extract_openai_delta).await
}

pub fn extract_openai_delta(payload: &str) -> Result<Option<String>> {
    let val: Value = serde_json::from_str(payload)
        .with_context(|| format!("invalid SSE JSON chunk: {payload}"))?;
    if let Some(err) = val.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        anyhow::bail!("{err}");
    }
    let delta = val["choices"][0]["delta"]["content"]
        .as_str()
        .map(|s| s.to_string());
    Ok(delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_sse_chunk() {
        let json = r#"{"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert_eq!(extract_openai_delta(json).unwrap(), Some("Hello".into()));
    }
}
