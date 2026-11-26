use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub const DEFAULT_TOGGLE_HOTKEY: &str = "ALT + SPACE";
pub const DEFAULT_TUI_TOGGLE_HOTKEY: &str = "ALT + SHIFT + SPACE";

pub const LINUX_QUICK_SYSTEM_PROMPT: &str = "\
You are HyprPilot, a concise AI assistant running on Arch Linux (Hyprland). \
Prefer direct, accurate answers with copyable shell commands when relevant. \
Keep explanations brief unless the user asks for details.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAiCompatible,
    Anthropic,
    Gemini,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    ApiKey,
    BearerToken,
    CustomHeaders,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub auth_mode: AuthMode,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub available_models: Vec<String>,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalEntry {
    pub id: String,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub toggle_hotkey: String,
    pub tui_toggle_hotkey: String,
    pub default_view: String,
    pub active_provider_id: String,
    pub active_portal_id: String,
    pub linux_quick_mode: bool,
    pub hide_on_escape: bool,
    pub theme_mode: String,
    pub custom_system_prompt: String,
    pub providers: Vec<ProviderProfile>,
    pub portals: Vec<PortalEntry>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            toggle_hotkey: DEFAULT_TOGGLE_HOTKEY.to_string(),
            tui_toggle_hotkey: DEFAULT_TUI_TOGGLE_HOTKEY.to_string(),
            default_view: "native".to_string(),
            active_provider_id: "openai".to_string(),
            active_portal_id: "chatgpt".to_string(),
            linux_quick_mode: true,
            hide_on_escape: true,
            theme_mode: "system".to_string(),
            custom_system_prompt: String::new(),
            providers: default_providers(),
            portals: default_portals(),
        }
    }
}

pub fn default_providers() -> Vec<ProviderProfile> {
    let mk = |id: &str,
              name: &str,
              kind: ProviderKind,
              auth_mode: AuthMode,
              base_url: &str,
              model: &str| ProviderProfile {
        id: id.into(),
        name: name.into(),
        kind,
        auth_mode,
        base_url: base_url.into(),
        model: model.into(),
        available_models: Vec::new(),
        token: String::new(),
        extra_headers: BTreeMap::new(),
    };
    vec![
        mk(
            "openai",
            "OpenAI",
            ProviderKind::OpenAiCompatible,
            AuthMode::ApiKey,
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ),
        mk(
            "anthropic",
            "Claude (Anthropic)",
            ProviderKind::Anthropic,
            AuthMode::ApiKey,
            "https://api.anthropic.com/v1",
            "claude-sonnet-4-20250514",
        ),
        mk(
            "gemini",
            "Google Gemini",
            ProviderKind::Gemini,
            AuthMode::ApiKey,
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-3.8-flash",
        ),
        mk(
            "deepseek",
            "DeepSeek",
            ProviderKind::OpenAiCompatible,
            AuthMode::ApiKey,
            "https://api.deepseek.com/v1",
            "deepseek-chat",
        ),
        mk(
            "groq",
            "Groq",
            ProviderKind::OpenAiCompatible,
            AuthMode::ApiKey,
            "https://api.groq.com/openai/v1",
            "llama-3.1-8b-instant",
        ),
        mk(
            "openrouter",
            "OpenRouter",
            ProviderKind::OpenAiCompatible,
            AuthMode::ApiKey,
            "https://openrouter.ai/api/v1",
            "openrouter/free",
        ),
        mk(
            "ollama",
            "Ollama (Local)",
            ProviderKind::OpenAiCompatible,
            AuthMode::None,
            "http://127.0.0.1:11434/v1",
            "llama3.2",
        ),
        mk(
            "custom_bearer",
            "Custom Bearer / Token Endpoint",
            ProviderKind::OpenAiCompatible,
            AuthMode::BearerToken,
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ),
    ]
}

pub fn default_portals() -> Vec<PortalEntry> {
    vec![
        PortalEntry {
            id: "chatgpt".into(),
            name: "ChatGPT".into(),
            url: "https://chatgpt.com".into(),
        },
        PortalEntry {
            id: "claude".into(),
            name: "Claude".into(),
            url: "https://claude.ai".into(),
        },
        PortalEntry {
            id: "copilot".into(),
            name: "Copilot".into(),
            url: "https://copilot.microsoft.com".into(),
        },
        PortalEntry {
            id: "gemini".into(),
            name: "Gemini".into(),
            url: "https://gemini.google.com".into(),
        },
        PortalEntry {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            url: "https://chat.deepseek.com".into(),
        },
    ]
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hyprpilot")
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hyprpilot")
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(data_dir)
        .join("hyprpilot")
}

pub fn socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(state_dir)
        .join("hyprpilot.sock")
}

pub fn tui_socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(state_dir)
        .join("hyprpilot-tui.sock")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            let cfg = Self::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut cfg: AppConfig = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if cfg.providers.is_empty() {
            cfg.providers = default_providers();
        }
        if cfg.portals.is_empty() {
            cfg.portals = default_portals();
        }
        if cfg.toggle_hotkey.trim().is_empty() {
            cfg.toggle_hotkey = DEFAULT_TOGGLE_HOTKEY.to_string();
        }
        if cfg.tui_toggle_hotkey.trim().is_empty() {
            cfg.tui_toggle_hotkey = DEFAULT_TUI_TOGGLE_HOTKEY.to_string();
        }
        let mut migrated = false;
        for p in &mut cfg.providers {
            if p.kind == ProviderKind::Gemini
                && matches!(
                    p.model.trim(),
                    "gemini-2.5-flash" | "models/gemini-2.5-flash" | "gemini-2.0-flash"
                )
            {
                p.model = "gemini-3.8-flash".to_string();
                migrated = true;
            }
        }
        if migrated {
            let _ = cfg.save();
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let path = config_path();
        let body = toml::to_string_pretty(self)?;
        write_private_file(&path, &body)
    }

    pub fn active_provider(&self) -> Option<&ProviderProfile> {
        self.providers
            .iter()
            .find(|p| p.id == self.active_provider_id)
            .or_else(|| self.providers.first())
    }

    pub fn effective_system_prompt(&self) -> Option<String> {
        let custom = self.custom_system_prompt.trim();
        if !custom.is_empty() {
            return Some(custom.to_string());
        }
        if self.linux_quick_mode {
            return Some(LINUX_QUICK_SYSTEM_PROMPT.to_string());
        }
        None
    }
}

pub fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)?;
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Normalizes a user hotkey like "Alt + Space" or "SUPER+SHIFT+A" into
/// (ML4W Lua format, standard Hyprland conf format).
pub fn format_hyprland_hotkey(raw: &str) -> (String, String) {
    let cleaned = raw.trim();
    let parts: Vec<String> = cleaned
        .split(|c: char| c == '+' || c == ',' || c.is_whitespace())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase())
        .collect();

    if parts.is_empty() {
        return ("ALT + SPACE".to_string(), "ALT, SPACE".to_string());
    }
    if parts.len() == 1 {
        let key = &parts[0];
        return (key.clone(), format!(", {key}"));
    }

    let lua_fmt = parts.join(" + ");
    let (mods, key) = parts.split_at(parts.len() - 1);
    let conf_fmt = format!("{}, {}", mods.join(" "), key[0]);
    (lua_fmt, conf_fmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_formatting_handles_common_variants() {
        assert_eq!(
            format_hyprland_hotkey("alt + space"),
            ("ALT + SPACE".to_string(), "ALT, SPACE".to_string())
        );
        assert_eq!(
            format_hyprland_hotkey("SUPER SHIFT A"),
            ("SUPER + SHIFT + A".to_string(), "SUPER SHIFT, A".to_string())
        );
        assert_eq!(
            format_hyprland_hotkey("Ctrl,Return"),
            ("CTRL + RETURN".to_string(), "CTRL, RETURN".to_string())
        );
        assert_eq!(
            format_hyprland_hotkey(""),
            ("ALT + SPACE".to_string(), "ALT, SPACE".to_string())
        );
    }
}
