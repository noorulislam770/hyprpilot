use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use serde_json::Value;

use super::build_common_headers;
use crate::storage::config::{AuthMode, ProviderKind, ProviderProfile};

#[derive(Debug, Clone)]
pub struct FetchedModels {
    pub models: Vec<String>,
    pub recommended: Option<String>,
}

pub async fn fetch_available_models(
    client: &reqwest::Client,
    provider: &ProviderProfile,
) -> Result<FetchedModels> {
    let token = provider.token.trim();
    if matches!(provider.auth_mode, AuthMode::ApiKey | AuthMode::BearerToken) && token.is_empty() {
        anyhow::bail!("Add an API key/token for '{}' first.", provider.name);
    }

    let mut headers = build_common_headers(provider)?;
    let base = provider.base_url.trim_end_matches('/');

    let url = match provider.kind {
        ProviderKind::OpenAiCompatible => format!("{base}/models"),
        ProviderKind::Anthropic => {
            if !token.is_empty() {
                match provider.auth_mode {
                    AuthMode::ApiKey => {
                        headers.insert(
                            HeaderName::from_static("x-api-key"),
                            HeaderValue::from_str(token)?,
                        );
                    }
                    AuthMode::BearerToken => {
                        headers.insert(
                            AUTHORIZATION,
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
            format!("{base}/models")
        }
        ProviderKind::Gemini => {
            if !token.is_empty() {
                match provider.auth_mode {
                    AuthMode::ApiKey => {
                        headers.insert(
                            HeaderName::from_static("x-goog-api-key"),
                            HeaderValue::from_str(token)?,
                        );
                    }
                    AuthMode::BearerToken => {
                        headers.insert(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {token}"))?,
                        );
                    }
                    AuthMode::CustomHeaders | AuthMode::None => {}
                }
            }
            format!("{base}/models")
        }
    };

    let resp = client
        .get(&url)
        .headers(headers)
        .send()
        .await
        .with_context(|| format!("failed to fetch models from {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        anyhow::bail!("HTTP {status}: {snippet}");
    }

    let json: Value = resp.json().await.context("invalid models JSON response")?;
    let mut entries = parse_model_entries(&json, &provider.kind, &provider.id);

    // Sort so lightweight/low-token models surface near the top while keeping list clean
    entries.sort_by_key(|e| (score_model(e, &provider.id), e.id.clone()));
    entries.dedup_by(|a, b| a.id == b.id);

    let recommended = entries.first().map(|e| e.id.clone());
    let models = entries.into_iter().map(|e| e.id).collect();
    Ok(FetchedModels {
        models,
        recommended,
    })
}

#[derive(Debug, Clone)]
struct ModelCandidate {
    id: String,
    is_free_pricing: bool,
}

fn parse_model_entries(json: &Value, kind: &ProviderKind, provider_id: &str) -> Vec<ModelCandidate> {
    let mut out = Vec::new();
    match kind {
        ProviderKind::Gemini => {
            if let Some(arr) = json.get("models").and_then(|v| v.as_array()) {
                for item in arr {
                    let supports_gen = item
                        .get("supportedGenerationMethods")
                        .and_then(|m| m.as_array())
                        .is_some_and(|methods| {
                            methods.iter().any(|x| x.as_str() == Some("generateContent"))
                        });
                    if !supports_gen {
                        continue;
                    }
                    if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                        let clean = name.trim_start_matches("models/").to_string();
                        if !is_non_chat_gemini(&clean) {
                            out.push(ModelCandidate {
                                id: clean,
                                is_free_pricing: false,
                            });
                        }
                    }
                }
            }
        }
        ProviderKind::OpenAiCompatible | ProviderKind::Anthropic => {
            if let Some(arr) = json.get("data").and_then(|v| v.as_array()) {
                for item in arr {
                    let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if provider_id == "openai" && is_non_chat_openai(id) {
                        continue;
                    }
                    if provider_id == "groq" && is_non_chat_groq(id) {
                        continue;
                    }
                    let is_free_pricing = item
                        .get("pricing")
                        .map(|p| {
                            p.get("prompt").and_then(|x| x.as_str()) == Some("0")
                                && p.get("completion").and_then(|x| x.as_str()) == Some("0")
                        })
                        .unwrap_or(false)
                        || id.ends_with(":free");
                    out.push(ModelCandidate {
                        id: id.to_string(),
                        is_free_pricing,
                    });
                }
            } else if let Some(arr) = json.get("models").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                        out.push(ModelCandidate {
                            id: name.to_string(),
                            is_free_pricing: false,
                        });
                    }
                }
            }
        }
    }
    out
}

fn is_non_chat_openai(id: &str) -> bool {
    let l = id.to_lowercase();
    l.contains("embedding")
        || l.contains("whisper")
        || l.contains("tts")
        || l.contains("dall-e")
        || l.contains("moderation")
        || l.contains("realtime")
        || l.contains("transcribe")
        || l.contains("sora")
        || l.contains("image")
        || l.starts_with("babbage")
        || l.starts_with("davinci")
}

fn is_non_chat_groq(id: &str) -> bool {
    let l = id.to_lowercase();
    l.contains("whisper") || l.contains("guard") || l.contains("tts")
}

fn is_non_chat_gemini(id: &str) -> bool {
    let l = id.to_lowercase();
    l.contains("embedding")
        || l.contains("imagen")
        || l.contains("veo")
        || l.contains("aqa")
        || l.contains("robotics")
        || l.contains("tts")
}

/// Lower score = faster / cheaper / lower token footprint for simple Q&A tasks.
fn score_model(candidate: &ModelCandidate, provider_id: &str) -> i32 {
    let id = candidate.id.to_lowercase();
    let mut score = 100;

    if candidate.is_free_pricing || id.ends_with(":free") {
        score -= 60;
    }

    // Ultra-lightweight tiers
    if id.contains("nano") || id.contains("flash-lite") || id.contains("-lite") {
        score -= 45;
    } else if id.contains("mini") || id.contains("haiku") || id.contains("instant") {
        score -= 35;
    } else if id.contains("flash") || id.contains("small") || id.contains("8b") || id.contains("7b") {
        score -= 28;
    } else if id == "deepseek-chat" || id == "openrouter/free" {
        score -= 40;
    }

    // Prefer newer generation within same tier (subtle tie-breaker)
    if id.contains("3.8") || id.contains("4.1") || id.contains("4.5") || id.contains("gpt-5") {
        score -= 5;
    }

    // Penalize heavy / expensive / slow reasoning models for quick lookups
    if id.contains("opus") || id.contains("ultra") || id.contains("405b") || id.contains("671b") {
        score += 60;
    }
    if id.contains("reasoner") || id.contains("-r1") || id.contains("thinking") || id.starts_with("o1") || id.starts_with("o3") {
        score += 50;
    }
    if id.contains("pro") && !id.contains("flash") {
        score += 35;
    }
    if id.contains("preview") || id.contains("exp") {
        score += 12;
    }
    if provider_id == "openai" && id.contains("codex") {
        score += 30;
    }

    score
}

pub fn pick_lightweight_model(models: &[String], provider_id: &str) -> Option<String> {
    models
        .iter()
        .map(|id| ModelCandidate {
            id: id.clone(),
            is_free_pricing: id.ends_with(":free"),
        })
        .min_by_key(|c| (score_model(c, provider_id), c.id.clone()))
        .map(|c| c.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_lightweight_gemini_model() {
        let json: Value = serde_json::from_str(
            r#"{
                "models": [
                    { "name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent"] },
                    { "name": "models/gemini-3.8-flash", "supportedGenerationMethods": ["generateContent"] },
                    { "name": "models/gemini-2.5-flash-lite", "supportedGenerationMethods": ["generateContent"] },
                    { "name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"] }
                ]
            }"#,
        )
        .unwrap();
        let parsed = parse_model_entries(&json, &ProviderKind::Gemini, "gemini");
        let ids: Vec<String> = parsed.iter().map(|c| c.id.clone()).collect();
        assert!(!ids.contains(&"text-embedding-004".to_string()));
        let best = pick_lightweight_model(&ids, "gemini").unwrap();
        assert_eq!(best, "gemini-2.5-flash-lite");
    }

    #[test]
    fn picks_free_model_on_openrouter() {
        let json: Value = serde_json::from_str(
            r#"{
                "data": [
                    { "id": "anthropic/claude-opus-4", "pricing": { "prompt": "0.000015", "completion": "0.000075" } },
                    { "id": "google/gemma-3-12b-it:free", "pricing": { "prompt": "0", "completion": "0" } },
                    { "id": "openai/gpt-4o-mini", "pricing": { "prompt": "0.00000015", "completion": "0.0000006" } }
                ]
            }"#,
        )
        .unwrap();
        let mut parsed = parse_model_entries(&json, &ProviderKind::OpenAiCompatible, "openrouter");
        parsed.sort_by_key(|e| (score_model(e, "openrouter"), e.id.clone()));
        assert_eq!(parsed[0].id, "google/gemma-3-12b-it:free");
    }

    #[test]
    fn picks_lightweight_openai_and_anthropic_models() {
        let openai = vec![
            "gpt-4o".into(),
            "o3-pro".into(),
            "gpt-4.1-nano".into(),
            "gpt-4o-mini".into(),
        ];
        assert_eq!(
            pick_lightweight_model(&openai, "openai").as_deref(),
            Some("gpt-4.1-nano")
        );

        let claude = vec![
            "claude-opus-4-20250514".into(),
            "claude-sonnet-4-20250514".into(),
            "claude-3-5-haiku-latest".into(),
        ];
        assert_eq!(
            pick_lightweight_model(&claude, "anthropic").as_deref(),
            Some("claude-3-5-haiku-latest")
        );
    }
}
