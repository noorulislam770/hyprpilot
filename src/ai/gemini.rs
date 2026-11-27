use anyhow::{Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
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
    let token = provider.token.trim();
    if token.is_empty() && provider.auth_mode != AuthMode::None {
        anyhow::bail!(
            "No API key/token configured for '{}'. Open Settings to add one.",
            provider.name
        );
    }

    let base = provider.base_url.trim_end_matches('/');
    let model = provider.model.trim().trim_start_matches("models/");
    let url = format!("{base}/models/{model}:streamGenerateContent?alt=sse");

    let contents: Vec<Value> = messages
        .iter()
        .map(|m| {
            let role = if m.role == "assistant" { "model" } else { "user" };
            json!({
                "role": role,
                "parts": [{ "text": m.content }]
            })
        })
        .collect();

    let mut body = json!({ "contents": contents });
    if let Some(sys) = system_prompt {
        body["systemInstruction"] = json!({ "parts": [{ "text": sys }] });
    }

    let mut headers = build_common_headers(provider)?;
    if !token.is_empty() {
        match provider.auth_mode {
            AuthMode::ApiKey => {
                headers.insert(
                    HeaderName::from_static("x-goog-api-key"),
                    HeaderValue::from_str(token).context("invalid Gemini API key")?,
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

    let resp = client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    consume_sse_stream_with_stats(resp, tx, extract_gemini_chunk).await
}

pub fn extract_gemini_chunk(payload: &str) -> Result<(Option<String>, Option<ResponseStats>)> {
    let val: Value = serde_json::from_str(payload)
        .with_context(|| format!("invalid Gemini SSE JSON: {payload}"))?;
    if let Some(err) = val.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        anyhow::bail!("{err}");
    }
    let text = val["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .map(|s| s.to_string());
    let finish = val["candidates"][0]["finishReason"].as_str().map(|s| s.to_string());
    let um = val.get("usageMetadata");
    let stats = if um.is_some() || finish.is_some() {
        Some(ResponseStats {
            input_tokens: um.and_then(|u| u.get("promptTokenCount")).and_then(|v| v.as_u64()),
            output_tokens: um.and_then(|u| u.get("candidatesTokenCount")).and_then(|v| v.as_u64()),
            total_tokens: um.and_then(|u| u.get("totalTokenCount")).and_then(|v| v.as_u64()),
            duration_ms: 0,
            finish_reason: finish,
        })
    } else {
        None
    };
    Ok((text, stats))
}

pub fn extract_gemini_delta(payload: &str) -> Result<Option<String>> {
    Ok(extract_gemini_chunk(payload)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gemini_delta() {
        let json = r#"{"candidates":[{"content":{"parts":[{"text":"find . -type d"}]}}]}"#;
        assert_eq!(
            extract_gemini_delta(json).unwrap(),
            Some("find . -type d".into())
        );
    }
}
