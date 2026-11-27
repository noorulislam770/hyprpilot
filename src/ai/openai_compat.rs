use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{AiMessage, ResponseStats, StreamEvent, build_common_headers, consume_sse_stream_with_stats};
use crate::storage::config::{AuthMode, ProviderProfile};

pub async fn stream(
    client: &reqwest::Client,
    provider: &ProviderProfile,
    system_prompt: Option<&str>,
    messages: &[AiMessage],
    tx: &mpsc::UnboundedSender<StreamEvent>,
) -> Result<ResponseStats> {
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

    let mut body = json!({
        "model": provider.model,
        "messages": payload_msgs,
        "stream": true
    });
    if provider.auth_mode != AuthMode::None {
        body["stream_options"] = json!({ "include_usage": true });
    }

    let headers = build_common_headers(provider)?;
    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    consume_sse_stream_with_stats(resp, tx, extract_openai_chunk).await
}

pub fn extract_openai_chunk(payload: &str) -> Result<(Option<String>, Option<ResponseStats>)> {
    let val: Value = serde_json::from_str(payload)
        .with_context(|| format!("invalid SSE JSON chunk: {payload}"))?;
    if let Some(err) = val.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        anyhow::bail!("{err}");
    }
    let delta = val["choices"][0]["delta"]["content"]
        .as_str()
        .map(|s| s.to_string());
    let finish = val["choices"][0]["finish_reason"].as_str().map(|s| s.to_string());
    let usage = val.get("usage");
    let stats = if usage.is_some() || finish.is_some() {
        Some(ResponseStats {
            input_tokens: usage.and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()),
            output_tokens: usage.and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()),
            total_tokens: usage.and_then(|u| u.get("total_tokens")).and_then(|v| v.as_u64()),
            duration_ms: 0,
            finish_reason: finish,
        })
    } else {
        None
    };
    Ok((delta, stats))
}

pub fn extract_openai_delta(payload: &str) -> Result<Option<String>> {
    Ok(extract_openai_chunk(payload)?.0)
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
