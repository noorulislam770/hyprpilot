pub mod anthropic;
pub mod gemini;
pub mod models;
pub mod openai_compat;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use tokio::sync::mpsc;

use crate::storage::config::{AuthMode, ProviderKind, ProviderProfile};

#[derive(Debug, Clone)]
pub struct AiMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    Done,
    Error(String),
}

pub async fn stream_chat(
    client: reqwest::Client,
    provider: ProviderProfile,
    system_prompt: Option<String>,
    messages: Vec<AiMessage>,
    tx: mpsc::UnboundedSender<StreamEvent>,
) {
    let res = match provider.kind {
        ProviderKind::OpenAiCompatible => {
            openai_compat::stream(&client, &provider, system_prompt.as_deref(), &messages, &tx)
                .await
        }
        ProviderKind::Anthropic => {
            anthropic::stream(&client, &provider, system_prompt.as_deref(), &messages, &tx).await
        }
        ProviderKind::Gemini => {
            gemini::stream(&client, &provider, system_prompt.as_deref(), &messages, &tx).await
        }
    };

    match res {
        Ok(()) => {
            let _ = tx.send(StreamEvent::Done);
        }
        Err(err) => {
            let _ = tx.send(StreamEvent::Error(format!("{err:#}")));
        }
    }
}

pub fn build_common_headers(provider: &ProviderProfile) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let token = provider.token.trim();
    match provider.auth_mode {
        AuthMode::ApiKey | AuthMode::BearerToken => {
            if !token.is_empty() && provider.kind == ProviderKind::OpenAiCompatible {
                let val = format!("Bearer {token}");
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&val).context("invalid bearer token characters")?,
                );
            }
        }
        AuthMode::CustomHeaders | AuthMode::None => {}
    }

    for (k, v) in &provider.extra_headers {
        let name = HeaderName::from_bytes(k.trim().as_bytes())
            .with_context(|| format!("invalid header name '{k}'"))?;
        let val = HeaderValue::from_str(v.trim())
            .with_context(|| format!("invalid header value for '{k}'"))?;
        headers.insert(name, val);
    }

    Ok(headers)
}

pub async fn consume_sse_stream<F>(
    resp: reqwest::Response,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    mut parse_data: F,
) -> Result<()>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(400).collect();
        anyhow::bail!("HTTP {status}: {snippet}");
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("error reading response stream")?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim_end_matches('\r').to_string();
            buffer.drain(..=pos);

            if let Some(data) = line.strip_prefix("data:") {
                let payload = data.trim();
                if payload == "[DONE]" {
                    return Ok(());
                }
                if payload.is_empty() {
                    continue;
                }
                if let Some(delta) = parse_data(payload)? {
                    if !delta.is_empty() {
                        let _ = tx.send(StreamEvent::Delta(delta));
                    }
                }
            }
        }
    }

    Ok(())
}
