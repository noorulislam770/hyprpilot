pub mod anthropic;
pub mod gemini;
pub mod models;
pub mod openai_compat;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::storage::config::{AuthMode, ProviderKind, ProviderProfile};

#[derive(Debug, Clone)]
pub struct AiMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseStats {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl ResponseStats {
    pub fn merge(&mut self, other: ResponseStats) {
        if other.input_tokens.is_some() {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens.is_some() {
            self.output_tokens = other.output_tokens;
        }
        if other.total_tokens.is_some() {
            self.total_tokens = other.total_tokens;
        }
        if other.finish_reason.is_some() {
            self.finish_reason = other.finish_reason;
        }
        if self.total_tokens.is_none() {
            if let (Some(i), Some(o)) = (self.input_tokens, self.output_tokens) {
                self.total_tokens = Some(i + o);
            }
        }
    }

    pub fn format_compact(&self) -> String {
        let mut parts = Vec::new();
        if self.duration_ms > 0 {
            parts.push(format!("{:.1}s", self.duration_ms as f64 / 1000.0));
        }
        match (self.input_tokens, self.output_tokens, self.total_tokens) {
            (Some(i), Some(o), Some(t)) => parts.push(format!("↑{i} ↓{o} ({t} tok)")),
            (Some(i), Some(o), None) => parts.push(format!("↑{i} ↓{o} tok")),
            (_, Some(o), _) => parts.push(format!("↓{o} tok")),
            (_, _, Some(t)) => parts.push(format!("{t} tok")),
            _ => {}
        }
        if let Some(ref r) = self.finish_reason {
            if !r.is_empty() {
                parts.push(r.to_ascii_lowercase());
            }
        }
        parts.join(" • ")
    }
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    Stats(ResponseStats),
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
    let started = Instant::now();
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
        Ok(mut stats) => {
            stats.duration_ms = started.elapsed().as_millis() as u64;
            let _ = tx.send(StreamEvent::Stats(stats));
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

pub async fn consume_sse_stream_with_stats<F>(
    resp: reqwest::Response,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    mut parse_data: F,
) -> Result<ResponseStats>
where
    F: FnMut(&str) -> Result<(Option<String>, Option<ResponseStats>)>,
{
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(400).collect();
        anyhow::bail!("HTTP {status}: {snippet}");
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut agg_stats = ResponseStats::default();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("error reading response stream")?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim_end_matches('\r').to_string();
            buffer.drain(..=pos);

            if let Some(data) = line.strip_prefix("data:") {
                let payload = data.trim();
                if payload == "[DONE]" {
                    return Ok(agg_stats);
                }
                if payload.is_empty() {
                    continue;
                }
                let (delta_opt, stats_opt) = parse_data(payload)?;
                if let Some(st) = stats_opt {
                    agg_stats.merge(st);
                }
                if let Some(delta) = delta_opt {
                    if !delta.is_empty() {
                        let _ = tx.send(StreamEvent::Delta(delta));
                    }
                }
            }
        }
    }

    Ok(agg_stats)
}

pub async fn consume_sse_stream<F>(
    resp: reqwest::Response,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    mut parse_data: F,
) -> Result<()>
where
    F: FnMut(&str) -> Result<Option<String>>,
{
    consume_sse_stream_with_stats(resp, tx, |payload| Ok((parse_data(payload)?, None)))
        .await
        .map(|_| ())
}
