use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use hyprpilot::ai::models::fetch_available_models;
use hyprpilot::ai::{AiMessage, StreamEvent, stream_chat};
use hyprpilot::markdown::{render_markdown_ansi, render_markdown_lines};
use hyprpilot::preview::{
    PreviewKind, detect_preview_kind, expand_user_path, extract_pdf_text,
    html_to_readable_markdown, inspect_pdf, render_pdf_page_png,
};
use hyprpilot::storage::config::{
    AppConfig, AuthMode, TuiConfig, TuiKeybinds, state_dir, tui_config_path, tui_socket_path,
};
use hyprpilot::storage::db::{ConversationSummary, HistoryDb, derive_title_from_prompt};

// ---------------------------------------------------------------------------
// Daemon <-> TUI Client Protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireMessage {
    role: String,
    content: String,
}

fn default_pane_ratios() -> [u16; 3] {
    [20, 60, 20]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionSnapshot {
    provider_id: String,
    provider_name: String,
    #[serde(default)]
    provider_kind: String,
    #[serde(default)]
    provider_auth: String,
    #[serde(default)]
    provider_base_url: String,
    model: String,
    linux_quick_mode: bool,
    #[serde(default)]
    preview_side: String,
    #[serde(default = "default_pane_ratios")]
    pane_ratios: [u16; 3],
    #[serde(default)]
    keybinds: TuiKeybinds,
    #[serde(default = "default_true")]
    auto_preview_md: bool,
    #[serde(default = "default_start_mode")]
    start_mode: String,
    #[serde(default = "default_true")]
    show_tips: bool,
    active_conversation_id: Option<i64>,
    messages: Vec<WireMessage>,
    streaming: bool,
    streaming_reply: String,
    status: String,
    providers: Vec<(String, String, String)>, // (id, name, model)
    models: Vec<String>,
    history: Vec<WireConversation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireConversation {
    id: i64,
    title: String,
    model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum ClientCmd {
    GetState,
    SendPrompt { prompt: String },
    NewChat,
    SelectProvider { id: String },
    SelectModel { model: String },
    FetchModels,
    ToggleLinuxQuick,
    SetPreviewSide { side: String },
    SetPaneRatios { ratios: [u16; 3] },
    UpdateKeybind { action: String, key: String },
    ResetKeybinds,
    ToggleAutoPreview,
    ToggleStartMode,
    ToggleShowTips,
    LoadConversation { id: i64 },
    QuitDaemon,
}

fn default_true() -> bool {
    true
}

fn default_start_mode() -> String {
    "insert".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
enum ServerEvent {
    Snapshot(SessionSnapshot),
    Delta { text: String },
    Shutdown,
}

struct DaemonState {
    config: AppConfig,
    tui_config: TuiConfig,
    db: Option<HistoryDb>,
    active_conversation_id: Option<i64>,
    messages: Vec<AiMessage>,
    streaming: bool,
    streaming_reply: String,
    status: String,
    clients: Vec<mpsc::UnboundedSender<ServerEvent>>,
}

impl DaemonState {
    fn snapshot(&self) -> SessionSnapshot {
        let (provider_id, provider_name, provider_kind, provider_auth, provider_base_url, model, models) =
            match self.config.active_provider() {
                Some(p) => {
                    let mut list = p.available_models.clone();
                    if list.is_empty() && !p.model.trim().is_empty() {
                        list.push(p.model.trim().to_string());
                    } else if !p.model.trim().is_empty() && !list.iter().any(|m| m == p.model.trim()) {
                        list.insert(0, p.model.trim().to_string());
                    }
                    let kind = format!("{:?}", p.kind);
                    let auth = format!("{:?}", p.auth_mode);
                    (
                        p.id.clone(),
                        p.name.clone(),
                        kind,
                        auth,
                        p.base_url.clone(),
                        p.model.clone(),
                        list,
                    )
                }
                None => (
                    String::new(),
                    "None".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    Vec::new(),
                ),
            };
        let providers = self
            .config
            .providers
            .iter()
            .map(|p| (p.id.clone(), p.name.clone(), p.model.clone()))
            .collect();
        let history = self
            .db
            .as_ref()
            .and_then(|db| db.list_conversations("").ok())
            .unwrap_or_default()
            .into_iter()
            .map(|c: ConversationSummary| WireConversation {
                id: c.id,
                title: c.title,
                model: c.model,
            })
            .collect();

        SessionSnapshot {
            provider_id,
            provider_name,
            provider_kind,
            provider_auth,
            provider_base_url,
            model,
            linux_quick_mode: self.config.linux_quick_mode,
            preview_side: self.tui_config.preview_side.clone(),
            pane_ratios: self.tui_config.pane_ratios,
            keybinds: self.tui_config.keybinds.clone(),
            auto_preview_md: self.tui_config.auto_preview_md,
            start_mode: self.tui_config.start_mode.clone(),
            show_tips: self.tui_config.show_tips,
            active_conversation_id: self.active_conversation_id,
            messages: self
                .messages
                .iter()
                .map(|m| WireMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                })
                .collect(),
            streaming: self.streaming,
            streaming_reply: self.streaming_reply.clone(),
            status: self.status.clone(),
            providers,
            models,
            history,
        }
    }

    fn broadcast(&mut self, ev: ServerEvent) {
        self.clients.retain(|tx| tx.send(ev.clone()).is_ok());
    }

    fn broadcast_snapshot(&mut self) {
        let snap = self.snapshot();
        self.broadcast(ServerEvent::Snapshot(snap));
    }
}

fn run_daemon() -> Result<()> {
    let sock = tui_socket_path();
    if let Some(parent) = sock.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&sock);

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("failed to bind TUI daemon socket {}", sock.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&sock, fs::Permissions::from_mode(0o600));
    }

    let config = AppConfig::load().unwrap_or_default();
    let tui_config = TuiConfig::load().unwrap_or_default();
    let db = HistoryDb::open_default().ok();
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );
    let http = reqwest::Client::new();

    let state = Arc::new(Mutex::new(DaemonState {
        config,
        tui_config,
        db,
        active_conversation_id: None,
        messages: Vec::new(),
        streaming: false,
        streaming_reply: String::new(),
        status: "Ready — ask anything or press Ctrl+P / Ctrl+M".to_string(),
        clients: Vec::new(),
    }));

    // Auto-fetch models if active provider has token and empty model cache
    maybe_trigger_daemon_fetch(&state, &runtime, &http);

    for stream in listener.incoming().flatten() {
        let st = Arc::clone(&state);
        let rt = Arc::clone(&runtime);
        let client_http = http.clone();
        let sock_clone = sock.clone();
        thread::spawn(move || {
            let Ok(write_stream) = stream.try_clone() else {
                return;
            };
            let (tx, mut rx) = mpsc::unbounded_channel::<ServerEvent>();
            {
                let mut guard = st.lock().unwrap();
                guard.clients.push(tx.clone());
                let _ = tx.send(ServerEvent::Snapshot(guard.snapshot()));
            }

            // Writer thread pushes ServerEvents to this client socket
            let writer_handle = thread::spawn(move || {
                let mut w = write_stream;
                while let Some(ev) = rx.blocking_recv() {
                    let Ok(mut line) = serde_json::to_string(&ev) else {
                        continue;
                    };
                    line.push('\n');
                    if w.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
            });

            let mut reader = BufReader::new(stream);
            let mut buf = String::new();
            while reader.read_line(&mut buf).is_ok() {
                if buf.is_empty() {
                    break;
                }
                let trimmed = buf.trim();
                if let Ok(cmd) = serde_json::from_str::<ClientCmd>(trimmed) {
                    if matches!(cmd, ClientCmd::QuitDaemon) {
                        {
                            let mut guard = st.lock().unwrap();
                            guard.broadcast(ServerEvent::Shutdown);
                        }
                        let _ = fs::remove_file(&sock_clone);
                        std::process::exit(0);
                    }
                    handle_daemon_cmd(&st, &rt, &client_http, cmd);
                }
                buf.clear();
            }
            drop(writer_handle);
        });
    }

    Ok(())
}

fn maybe_trigger_daemon_fetch(
    state: &Arc<Mutex<DaemonState>>,
    runtime: &Arc<Runtime>,
    http: &reqwest::Client,
) {
    let provider_opt = {
        let guard = state.lock().unwrap();
        guard
            .config
            .active_provider()
            .filter(|p| {
                p.available_models.is_empty()
                    && (!p.token.trim().is_empty() || p.auth_mode == AuthMode::None)
            })
            .cloned()
    };
    if let Some(provider) = provider_opt {
        spawn_daemon_fetch(state, runtime, http, provider);
    }
}

fn spawn_daemon_fetch(
    state: &Arc<Mutex<DaemonState>>,
    runtime: &Arc<Runtime>,
    http: &reqwest::Client,
    provider: hyprpilot::storage::config::ProviderProfile,
) {
    let target_id = provider.id.clone();
    {
        let mut guard = state.lock().unwrap();
        guard.status = format!("Fetching models for {}...", provider.name);
        guard.broadcast_snapshot();
    }
    let st = Arc::clone(state);
    let client = http.clone();
    runtime.spawn(async move {
        let res = fetch_available_models(&client, &provider).await;
        let mut guard = st.lock().unwrap();
        match res {
            Ok(fetched) => {
                let count = fetched.models.len();
                let chosen = fetched.recommended.clone().unwrap_or_default();
                if let Some(p) = guard
                    .config
                    .providers
                    .iter_mut()
                    .find(|p| p.id == target_id)
                {
                    p.available_models = fetched.models;
                    if !chosen.is_empty() {
                        p.model = chosen.clone();
                    }
                }
                let _ = guard.config.save();
                guard.status = format!("Fetched {count} models — selected {chosen}");
            }
            Err(e) => {
                guard.status = format!("Model fetch failed: {e:#}");
            }
        }
        guard.broadcast_snapshot();
    });
}

fn handle_daemon_cmd(
    state: &Arc<Mutex<DaemonState>>,
    runtime: &Arc<Runtime>,
    http: &reqwest::Client,
    cmd: ClientCmd,
) {
    match cmd {
        ClientCmd::GetState => {
            let mut guard = state.lock().unwrap();
            guard.broadcast_snapshot();
        }
        ClientCmd::NewChat => {
            let mut guard = state.lock().unwrap();
            if guard.streaming {
                return;
            }
            guard.active_conversation_id = None;
            guard.messages.clear();
            guard.streaming_reply.clear();
            guard.status = "Started new chat".to_string();
            guard.broadcast_snapshot();
        }
        ClientCmd::ToggleLinuxQuick => {
            let mut guard = state.lock().unwrap();
            guard.config.linux_quick_mode = !guard.config.linux_quick_mode;
            let _ = guard.config.save();
            guard.status = format!(
                "Linux Quick Mode: {}",
                if guard.config.linux_quick_mode {
                    "ON"
                } else {
                    "OFF"
                }
            );
            guard.broadcast_snapshot();
        }
        ClientCmd::SetPreviewSide { side } => {
            let mut guard = state.lock().unwrap();
            let normalized = if side.eq_ignore_ascii_case("left") {
                "left".to_string()
            } else {
                "right".to_string()
            };
            guard.tui_config.preview_side = normalized.clone();
            let _ = guard.tui_config.save();
            guard.status = format!("Preview side: {normalized} (saved to tui.toml)");
            guard.broadcast_snapshot();
        }
        ClientCmd::SetPaneRatios { ratios } => {
            let mut guard = state.lock().unwrap();
            let sum: u16 = ratios.iter().sum();
            if sum == 100 && ratios.iter().all(|&r| r >= 10) {
                guard.tui_config.pane_ratios = ratios;
                let _ = guard.tui_config.save();
                guard.status = format!("Layout: {}/{}/{} (saved to tui.toml)", ratios[0], ratios[1], ratios[2]);
            } else {
                guard.status = "Layout ratios must sum to 100 (min 10 each)".to_string();
            }
            guard.broadcast_snapshot();
        }
        ClientCmd::UpdateKeybind { action, key } => {
            let mut guard = state.lock().unwrap();
            let kb = &mut guard.tui_config.keybinds;
            match action.as_str() {
                "toggle_preview" => kb.toggle_preview = key.clone(),
                "swap_side" => kb.swap_side = key.clone(),
                "copy_section" => kb.copy_section = key.clone(),
                "search" => kb.search = key.clone(),
                "command" => kb.command = key.clone(),
                "help" => kb.help = key.clone(),
                "focus_left" => kb.focus_left = key.clone(),
                "focus_right" => kb.focus_right = key.clone(),
                "new_chat" => kb.new_chat = key.clone(),
                _ => {}
            }
            let _ = guard.tui_config.save();
            guard.status = format!("Rebound {action} -> {key} (tui.toml)");
            guard.broadcast_snapshot();
        }
        ClientCmd::ResetKeybinds => {
            let mut guard = state.lock().unwrap();
            guard.tui_config = TuiConfig::default();
            let _ = guard.tui_config.save();
            guard.status = "Reset tui.toml to defaults (20/60/20)".to_string();
            guard.broadcast_snapshot();
        }
        ClientCmd::ToggleAutoPreview => {
            let mut guard = state.lock().unwrap();
            guard.tui_config.auto_preview_md = !guard.tui_config.auto_preview_md;
            let _ = guard.tui_config.save();
            guard.status = format!(
                "Auto-preview MD artifacts: {}",
                if guard.tui_config.auto_preview_md { "ON" } else { "OFF" }
            );
            guard.broadcast_snapshot();
        }
        ClientCmd::ToggleStartMode => {
            let mut guard = state.lock().unwrap();
            guard.tui_config.start_mode = if guard.tui_config.start_mode == "normal" {
                "insert".to_string()
            } else {
                "normal".to_string()
            };
            let _ = guard.tui_config.save();
            guard.status = format!("TUI start mode: {}", guard.tui_config.start_mode);
            guard.broadcast_snapshot();
        }
        ClientCmd::ToggleShowTips => {
            let mut guard = state.lock().unwrap();
            guard.tui_config.show_tips = !guard.tui_config.show_tips;
            let _ = guard.tui_config.save();
            guard.status = format!(
                "Inspector Tips panel: {}",
                if guard.tui_config.show_tips { "ON" } else { "OFF" }
            );
            guard.broadcast_snapshot();
        }
        ClientCmd::SelectProvider { id } => {
            let should_fetch = {
                let mut guard = state.lock().unwrap();
                if let Some(p) = guard.config.providers.iter().find(|p| p.id == id).cloned() {
                    guard.config.active_provider_id = p.id.clone();
                    let _ = guard.config.save();
                    guard.status = format!("Switched to {} ({})", p.name, p.model);
                    guard.broadcast_snapshot();
                    p.available_models.is_empty()
                        && (!p.token.trim().is_empty() || p.auth_mode == AuthMode::None)
                } else {
                    false
                }
            };
            if should_fetch {
                maybe_trigger_daemon_fetch(state, runtime, http);
            }
        }
        ClientCmd::SelectModel { model } => {
            let mut guard = state.lock().unwrap();
            let active_id = guard.config.active_provider_id.clone();
            if let Some(p) = guard
                .config
                .providers
                .iter_mut()
                .find(|p| p.id == active_id)
            {
                p.model = model.clone();
            }
            let _ = guard.config.save();
            guard.status = format!("Selected model: {model}");
            guard.broadcast_snapshot();
        }
        ClientCmd::FetchModels => {
            let prov = {
                let guard = state.lock().unwrap();
                guard.config.active_provider().cloned()
            };
            if let Some(p) = prov {
                spawn_daemon_fetch(state, runtime, http, p);
            }
        }
        ClientCmd::LoadConversation { id } => {
            let mut guard = state.lock().unwrap();
            if guard.streaming {
                return;
            }
            if let Some(db) = &guard.db {
                let msgs = db.get_messages(id).unwrap_or_default();
                guard.active_conversation_id = Some(id);
                guard.messages = msgs
                    .into_iter()
                    .map(|m| AiMessage {
                        role: m.role,
                        content: m.content,
                    })
                    .collect();
                guard.streaming_reply.clear();
                guard.status = "Loaded conversation from history".to_string();
                guard.broadcast_snapshot();
            }
        }
        ClientCmd::SendPrompt { prompt } => {
            let prompt = prompt.trim().to_string();
            if prompt.is_empty() {
                return;
            }
            let (provider, sys, history, conv_id) = {
                let mut guard = state.lock().unwrap();
                if guard.streaming {
                    return;
                }
                // Reload config in case user edited keys/model in GUI Settings
                if let Ok(fresh) = AppConfig::load() {
                    guard.config = fresh;
                }
                let Some(provider) = guard.config.active_provider().cloned() else {
                    guard.status = "No provider configured".to_string();
                    guard.broadcast_snapshot();
                    return;
                };
                let conv_id = if let Some(cid) = guard.active_conversation_id {
                    if let Some(db) = &guard.db {
                        let _ =
                            db.update_conversation_meta(cid, None, &provider.id, &provider.model);
                    }
                    Some(cid)
                } else if let Some(db) = &guard.db {
                    let title = derive_title_from_prompt(&prompt);
                    match db.create_conversation(&title, &provider.id, &provider.model) {
                        Ok(cid) => {
                            guard.active_conversation_id = Some(cid);
                            Some(cid)
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };
                if let (Some(db), Some(cid)) = (&guard.db, conv_id) {
                    let _ = db.add_message(cid, "user", &prompt);
                }
                guard.messages.push(AiMessage {
                    role: "user".into(),
                    content: prompt,
                });
                guard.streaming = true;
                guard.streaming_reply.clear();
                guard.status = format!("Streaming from {} ({})...", provider.name, provider.model);
                guard.broadcast_snapshot();
                (
                    provider,
                    guard.config.effective_system_prompt(),
                    guard.messages.clone(),
                    conv_id,
                )
            };

            let (tx, mut rx) = mpsc::unbounded_channel();
            let client = http.clone();
            runtime.spawn(stream_chat(client, provider, sys, history, tx));

            let st = Arc::clone(state);
            runtime.spawn(async move {
                let mut err_msg: Option<String> = None;
                while let Some(ev) = rx.recv().await {
                    match ev {
                        StreamEvent::Delta(chunk) => {
                            let mut guard = st.lock().unwrap();
                            guard.streaming_reply.push_str(&chunk);
                            guard.broadcast(ServerEvent::Delta { text: chunk });
                        }
                        StreamEvent::Done => break,
                        StreamEvent::Error(e) => {
                            err_msg = Some(e);
                            break;
                        }
                    }
                }
                let mut guard = st.lock().unwrap();
                guard.streaming = false;
                let final_text = if let Some(err) = err_msg {
                    guard.status = "Request failed".to_string();
                    if guard.streaming_reply.is_empty() {
                        format!("[Error: {err}]")
                    } else {
                        format!("{}\n\n[Error: {err}]", guard.streaming_reply)
                    }
                } else {
                    guard.status = "Done (Ctrl+Y to copy last reply)".to_string();
                    std::mem::take(&mut guard.streaming_reply)
                };
                guard.streaming_reply.clear();
                if let (Some(db), Some(cid)) = (&guard.db, conv_id) {
                    let _ = db.add_message(cid, "assistant", &final_text);
                }
                guard.messages.push(AiMessage {
                    role: "assistant".into(),
                    content: final_text,
                });
                guard.broadcast_snapshot();
            });
        }
        ClientCmd::QuitDaemon => {}
    }
}

fn connect_or_spawn_daemon() -> Result<(UnixStream, mpsc::UnboundedReceiver<ServerEvent>)> {
    let sock = tui_socket_path();
    let stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => {
            let exe = std::env::current_exe()?;
            let mut cmd = Command::new(exe);
            cmd.arg("--daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            cmd.spawn()
                .context("failed to start hyprpilot-tui daemon")?;

            let mut connected = None;
            for _ in 0..40 {
                thread::sleep(Duration::from_millis(25));
                if let Ok(s) = UnixStream::connect(&sock) {
                    connected = Some(s);
                    break;
                }
            }
            connected.context("timed out waiting for hyprpilot-tui daemon")?
        }
    };

    let read_stream = stream.try_clone()?;
    let (tx, rx) = mpsc::unbounded_channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(read_stream);
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok() {
            if line.is_empty() {
                break;
            }
            if let Ok(ev) = serde_json::from_str::<ServerEvent>(line.trim()) {
                if tx.send(ev).is_err() {
                    break;
                }
            }
            line.clear();
        }
    });

    Ok((stream, rx))
}

// ---------------------------------------------------------------------------
// Interactive Ratatui TUI Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CopySection {
    label: String,
    kind: &'static str,
    content: String,
}

fn extract_copy_sections(markdown: &str) -> Vec<CopySection> {
    let mut sections = Vec::new();
    let trimmed_full = markdown.trim();
    if trimmed_full.is_empty() {
        return sections;
    }

    sections.push(CopySection {
        label: "Full Reply (Clean Markdown)".to_string(),
        kind: "ALL",
        content: trimmed_full.to_string(),
    });

    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut para_buf = String::new();

    let flush_para = |buf: &mut String, out: &mut Vec<CopySection>| {
        let p = buf.trim();
        if !p.is_empty() {
            let first_line = p.lines().next().unwrap_or("").trim();
            let preview: String = first_line.chars().take(54).collect();
            let ellipsis = if first_line.chars().count() > 54 { "…" } else { "" };
            let kind = if first_line.starts_with('#') {
                "HEAD"
            } else {
                "TEXT"
            };
            out.push(CopySection {
                label: format!("{preview}{ellipsis}"),
                kind,
                content: p.to_string(),
            });
        }
        buf.clear();
    };

    for line in trimmed_full.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            if in_code {
                in_code = false;
                let lang = if code_lang.is_empty() { "code" } else { &code_lang };
                let code_trimmed = code_buf.trim_end_matches('\n').to_string();
                if !code_trimmed.is_empty() {
                    let first = code_trimmed
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(40)
                        .collect::<String>();
                    sections.push(CopySection {
                        label: format!("[{lang}] {first}"),
                        kind: "CODE",
                        content: code_trimmed,
                    });
                }
                code_buf.clear();
                code_lang.clear();
            } else {
                flush_para(&mut para_buf, &mut sections);
                in_code = true;
                code_lang = t.trim_start_matches('`').trim().to_string();
            }
            continue;
        }
        if in_code {
            code_buf.push_str(line);
            code_buf.push('\n');
        } else if line.trim().is_empty() {
            flush_para(&mut para_buf, &mut sections);
        } else {
            if !para_buf.is_empty() {
                para_buf.push('\n');
            }
            para_buf.push_str(line);
        }
    }
    if in_code && !code_buf.trim().is_empty() {
        let lang = if code_lang.is_empty() { "code" } else { &code_lang };
        sections.push(CopySection {
            label: format!("[{lang}] (unclosed)"),
            kind: "CODE",
            content: code_buf.trim_end_matches('\n').to_string(),
        });
    } else {
        flush_para(&mut para_buf, &mut sections);
    }

    sections
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimMode {
    Insert,
    Normal,
    Search,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseAction {
    ToggleMode,
    OpenProvider,
    OpenModel,
    ToggleLinux,
    TogglePreview,
    SwapSide,
    OpenHistory,
    OpenCopy,
    OpenHelp,
    EnterInsert,
    EnterSearch,
    EnterCommand,
    CycleFocus,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Chat,
    History,
    Inspector,
}

#[derive(Clone)]
struct TuiArtifact {
    title: String,
    kind: String, // "markdown", "html", "svg", "code"
    content: String,
}

fn extract_artifacts(messages: &[WireMessage]) -> Vec<TuiArtifact> {
    let Some(last) = messages.iter().rev().find(|m| m.role == "assistant") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut in_code = false;
    let mut lang = String::new();
    let mut buf = String::new();

    for line in last.content.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            if in_code {
                in_code = false;
                let norm = lang.trim().to_ascii_lowercase();
                let body = buf.trim_end_matches('\n').to_string();
                let is_rich = matches!(norm.as_str(), "md" | "markdown" | "html" | "htm" | "svg");
                if !body.trim().is_empty() && (is_rich || body.lines().count() >= 4) {
                    let kind = match norm.as_str() {
                        "md" | "markdown" => "markdown",
                        "html" | "htm" => "html",
                        "svg" => "svg",
                        _ => "code",
                    }
                    .to_string();
                    let label_lang = if norm.is_empty() { "snippet" } else { &norm };
                    out.push(TuiArtifact {
                        title: format!("Artifact #{} ({label_lang})", out.len() + 1),
                        kind,
                        content: body,
                    });
                }
                lang.clear();
                buf.clear();
            } else {
                in_code = true;
                lang = t.trim_start_matches('`').trim().to_string();
            }
            continue;
        }
        if in_code {
            buf.push_str(line);
            buf.push('\n');
        }
    }

    // Fallback: if no fenced artifact, offer full assistant markdown reply as an on-demand artifact
    if out.is_empty() && !last.content.trim().is_empty() {
        out.push(TuiArtifact {
            title: "Reply Markdown".to_string(),
            kind: "markdown".to_string(),
            content: last.content.clone(),
        });
    }
    out
}

fn has_rich_md_artifact(messages: &[WireMessage]) -> bool {
    let Some(last) = messages.iter().rev().find(|m| m.role == "assistant") else {
        return false;
    };
    let lc = last.content.to_ascii_lowercase();
    lc.contains("```markdown")
        || lc.contains("```md")
        || lc.contains("```html")
        || lc.contains("```svg")
}

fn infer_model_specs(model: &str, provider_kind: &str) -> (&'static str, &'static str, &'static str) {
    let m = model.to_ascii_lowercase();
    let ctx = if m.contains("gemini") {
        "1M tokens"
    } else if m.contains("claude") {
        "200K tokens"
    } else if m.contains("gpt-4") || m.contains("o1") || m.contains("o3") || m.contains("o4") {
        "128K tokens"
    } else if m.contains("deepseek") || m.contains("llama") || m.contains("qwen") {
        "128K tokens"
    } else {
        "Standard"
    };

    let tier = if m.contains("free") {
        "Free Tier"
    } else if m.contains("flash-lite")
        || m.contains("mini")
        || m.contains("nano")
        || m.contains("haiku")
        || m.contains("8b")
        || m.contains("instant")
        || m.contains("flash")
    {
        "Fast / Low-Token"
    } else if m.contains("opus") || m.contains("pro") || m.contains("sonnet") || m.contains("r1") {
        "Reasoning / Pro"
    } else {
        "Balanced"
    };

    let api = match provider_kind {
        "Anthropic" => "Anthropic Messages",
        "Gemini" => "Gemini GenerateContent",
        _ => "OpenAI Chat Completions",
    };

    (ctx, tier, api)
}

enum Modal {
    None,
    Provider(ListState),
    Model(ListState),
    History(ListState),
    Help {
        tab: usize,
        selected: usize,
        scroll: u16,
        awaiting_key: bool,
    },
    MarkdownPicker {
        files: Vec<String>,
        list_state: ListState,
        path_input: String,
    },
    MarkdownViewer {
        title: String,
        lines: Vec<Line<'static>>,
        scroll: u16,
    },
    CopyPicker {
        sections: Vec<CopySection>,
        list_state: ListState,
        raw_reply: String,
    },
    CopyLines {
        lines: Vec<String>,
        cursor: usize,
        anchor: Option<usize>,
        scroll: u16,
    },
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

struct TuiClient {
    tx_stream: UnixStream,
    rx: mpsc::UnboundedReceiver<ServerEvent>,
    snap: SessionSnapshot,
    input: String,
    scroll_offset: u16,
    auto_scroll: bool,
    modal: Modal,
    should_quit: bool,
    tick: usize,
    local_notice: Option<String>,
    vim_mode: VimMode,
    focus: FocusPane,
    pending_g: bool,
    pending_w: bool,
    search_query: String,
    search_matches: Vec<usize>,
    search_match_idx: usize,
    command_buf: String,
    preview_open: bool,
    preview_artifacts: Vec<TuiArtifact>,
    preview_idx: usize,
    preview_scroll: u16,
    was_streaming: bool,
    history_state: ListState,
    inspector_model_state: ListState,
    force_panes: Option<bool>,
    chat_zoomed: bool,
    last_transcript_rect: Rect,
    last_left_rect: Option<Rect>,
    last_right_rect: Option<Rect>,
    rendered_transcript_lines: Vec<String>,
    drag_anchor: Option<(usize, usize)>,
    drag_cursor: Option<(usize, usize)>,
    dragging: bool,
    last_header_rect: Rect,
    last_input_rect: Rect,
    last_footer_rect: Rect,
    last_specs_rect: Option<Rect>,
    last_models_rect: Option<Rect>,
    last_tips_rect: Option<Rect>,
    last_modal_rect: Option<Rect>,
    header_hitboxes: Vec<(u16, u16, MouseAction)>,
    footer_hitboxes: Vec<(u16, u16, MouseAction)>,
    help_tab_hitboxes: Vec<(u16, u16, u16, usize)>,
}

impl TuiClient {
    fn connect() -> Result<Self> {
        let (tx_stream, rx) = connect_or_spawn_daemon()?;
        let empty = SessionSnapshot {
            provider_id: String::new(),
            provider_name: "Connecting...".into(),
            provider_kind: "-".into(),
            provider_auth: "-".into(),
            provider_base_url: "-".into(),
            model: "-".into(),
            linux_quick_mode: true,
            preview_side: "right".into(),
            pane_ratios: [20, 60, 20],
            keybinds: TuiKeybinds::default(),
            auto_preview_md: true,
            start_mode: "insert".into(),
            show_tips: true,
            active_conversation_id: None,
            messages: Vec::new(),
            streaming: false,
            streaming_reply: String::new(),
            status: "Connecting to background session...".into(),
            providers: Vec::new(),
            models: Vec::new(),
            history: Vec::new(),
        };
        let mut client = Self {
            tx_stream,
            rx,
            snap: empty,
            input: String::new(),
            scroll_offset: 0,
            auto_scroll: true,
            modal: Modal::None,
            should_quit: false,
            tick: 0,
            local_notice: None,
            vim_mode: VimMode::Insert,
            focus: FocusPane::Chat,
            pending_g: false,
            pending_w: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_match_idx: 0,
            command_buf: String::new(),
            preview_open: false,
            preview_artifacts: Vec::new(),
            preview_idx: 0,
            preview_scroll: 0,
            was_streaming: false,
            history_state: ListState::default(),
            inspector_model_state: ListState::default(),
            force_panes: None,
            chat_zoomed: false,
            last_transcript_rect: Rect::default(),
            last_left_rect: None,
            last_right_rect: None,
            rendered_transcript_lines: Vec::new(),
            drag_anchor: None,
            drag_cursor: None,
            dragging: false,
            last_header_rect: Rect::default(),
            last_input_rect: Rect::default(),
            last_footer_rect: Rect::default(),
            last_specs_rect: None,
            last_models_rect: None,
            last_tips_rect: None,
            last_modal_rect: None,
            header_hitboxes: Vec::new(),
            footer_hitboxes: Vec::new(),
            help_tab_hitboxes: Vec::new(),
        };
        client.send_cmd(&ClientCmd::GetState);
        Ok(client)
    }

    fn send_cmd(&mut self, cmd: &ClientCmd) {
        if let Ok(mut json) = serde_json::to_string(cmd) {
            json.push('\n');
            let _ = self.tx_stream.write_all(json.as_bytes());
            let _ = self.tx_stream.flush();
        }
    }

    fn poll_server(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                ServerEvent::Snapshot(s) => {
                    let finished_stream = self.was_streaming && !s.streaming;
                    self.was_streaming = s.streaming;
                    self.snap = s;
                    self.local_notice = None;
                    if self.snap.streaming {
                        self.auto_scroll = true;
                    }
                    // Sync sidebar selection indices
                    if let Some(active_id) = self.snap.active_conversation_id {
                        if let Some(pos) = self.snap.history.iter().position(|c| c.id == active_id)
                        {
                            self.history_state.select(Some(pos));
                        }
                    } else if !self.snap.history.is_empty() && self.history_state.selected().is_none()
                    {
                        self.history_state.select(Some(0));
                    }
                    if let Some(pos) = self.snap.models.iter().position(|m| m == &self.snap.model) {
                        self.inspector_model_state.select(Some(pos));
                    } else if !self.snap.models.is_empty()
                        && self.inspector_model_state.selected().is_none()
                    {
                        self.inspector_model_state.select(Some(0));
                    }
                    // Refresh artifacts; auto-open when stream finishes with an explicit MD/HTML/SVG artifact
                    self.preview_artifacts = extract_artifacts(&self.snap.messages);
                    if self.preview_idx >= self.preview_artifacts.len() {
                        self.preview_idx = 0;
                    }
                    if finished_stream
                        && self.snap.auto_preview_md
                        && has_rich_md_artifact(&self.snap.messages)
                    {
                        self.preview_open = true;
                        self.preview_scroll = 0;
                    }
                }
                ServerEvent::Delta { text } => {
                    self.was_streaming = true;
                    self.snap.streaming_reply.push_str(&text);
                    self.snap.streaming = true;
                    self.auto_scroll = true;
                }
                ServerEvent::Shutdown => {
                    self.should_quit = true;
                }
            }
        }
    }

    fn delete_prev_word(&mut self) {
        while self.input.ends_with(char::is_whitespace) {
            self.input.pop();
        }
        while let Some(c) = self.input.chars().next_back() {
            if c.is_whitespace() {
                break;
            }
            self.input.pop();
        }
    }

    fn copy_text_to_clipboard(&mut self, text: &str, notice: &str) {
        if let Ok(mut child) = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            self.local_notice = Some(notice.to_string());
        } else {
            self.local_notice = Some("wl-copy not available".into());
        }
    }

    fn trigger_mouse_action(&mut self, action: MouseAction) {
        match action {
            MouseAction::ToggleMode => {
                self.vim_mode = if self.vim_mode == VimMode::Insert {
                    VimMode::Normal
                } else {
                    VimMode::Insert
                };
            }
            MouseAction::OpenProvider => self.open_provider_modal(),
            MouseAction::OpenModel => self.open_model_modal(),
            MouseAction::ToggleLinux => self.send_cmd(&ClientCmd::ToggleLinuxQuick),
            MouseAction::TogglePreview => self.toggle_artifact_preview(),
            MouseAction::SwapSide => {
                let next_side = if self.snap.preview_side == "left" {
                    "right"
                } else {
                    "left"
                };
                self.send_cmd(&ClientCmd::SetPreviewSide {
                    side: next_side.to_string(),
                });
            }
            MouseAction::OpenHistory => self.open_history_modal(),
            MouseAction::OpenCopy => self.open_copy_modal(),
            MouseAction::OpenHelp => {
                self.modal = Modal::Help {
                    tab: 0,
                    selected: 0,
                    scroll: 0,
                    awaiting_key: false,
                };
            }
            MouseAction::EnterInsert => {
                self.focus = FocusPane::Chat;
                self.vim_mode = VimMode::Insert;
            }
            MouseAction::EnterSearch => {
                self.search_query.clear();
                self.search_matches.clear();
                self.vim_mode = VimMode::Search;
            }
            MouseAction::EnterCommand => {
                self.command_buf.clear();
                self.vim_mode = VimMode::Command;
            }
            MouseAction::CycleFocus => {
                self.vim_mode = VimMode::Normal;
                self.focus = match self.focus {
                    FocusPane::History => FocusPane::Chat,
                    FocusPane::Chat => FocusPane::Inspector,
                    FocusPane::Inspector => FocusPane::History,
                };
            }
            MouseAction::Quit => self.should_quit = true,
        }
    }

    fn handle_modal_mouse(&mut self, me: MouseEvent) {
        let in_rect = |r: Rect| {
            me.column >= r.x
                && me.column < r.x + r.width
                && me.row >= r.y
                && me.row < r.y + r.height
        };
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(mr) = self.last_modal_rect {
                    if !in_rect(mr) {
                        self.modal = Modal::None;
                        return;
                    }
                }
                if let Modal::Help {
                    tab,
                    selected,
                    scroll,
                    awaiting_key,
                } = &mut self.modal
                {
                    for &(ty, x0, x1, tidx) in &self.help_tab_hitboxes {
                        if me.row == ty && me.column >= x0 && me.column < x1 {
                            *tab = tidx;
                            *scroll = 0;
                            *awaiting_key = false;
                            return;
                        }
                    }
                    if *tab == 4 {
                        if let Some(mr) = self.last_modal_rect {
                            let list_top = mr.y + 4;
                            if me.row >= list_top && me.row < list_top + 13 {
                                *selected = (me.row - list_top) as usize;
                                self.handle_modal_key(KeyCode::Enter);
                            }
                        }
                    }
                    return;
                }

                // List-based modals: click directly selects & activates item
                if let Some(mr) = self.last_modal_rect {
                    let list_top = mr.y + 1;
                    let list_bot = mr.y + mr.height.saturating_sub(1);
                    if me.row >= list_top && me.row < list_bot {
                        let rel = (me.row - list_top) as usize;
                        match &mut self.modal {
                            Modal::Provider(st) => {
                                if rel < self.snap.providers.len() {
                                    st.select(Some(rel));
                                    self.handle_modal_key(KeyCode::Enter);
                                }
                            }
                            Modal::Model(st) => {
                                if rel < self.snap.models.len() {
                                    st.select(Some(rel));
                                    self.handle_modal_key(KeyCode::Enter);
                                }
                            }
                            Modal::History(st) => {
                                if rel < self.snap.history.len() {
                                    st.select(Some(rel));
                                    self.handle_modal_key(KeyCode::Enter);
                                }
                            }
                            Modal::CopyPicker {
                                sections,
                                list_state,
                                ..
                            } => {
                                if rel < sections.len() {
                                    list_state.select(Some(rel));
                                    self.handle_modal_key(KeyCode::Enter);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => self.handle_modal_key(KeyCode::Up),
            MouseEventKind::ScrollDown => self.handle_modal_key(KeyCode::Down),
            _ => {}
        }
    }

    fn handle_mouse(&mut self, me: MouseEvent) {
        if !matches!(self.modal, Modal::None) {
            self.handle_modal_mouse(me);
            return;
        }
        let in_rect = |r: Rect| {
            me.column >= r.x
                && me.column < r.x + r.width
                && me.row >= r.y
                && me.row < r.y + r.height
        };
        let tr = self.last_transcript_rect;
        let inner_x_min = tr.x.saturating_add(1);
        let inner_x_max = tr.x.saturating_add(tr.width.saturating_sub(2)).max(inner_x_min);
        let inner_y_min = tr.y.saturating_add(1);
        let inner_y_max = tr.y.saturating_add(tr.height.saturating_sub(2)).max(inner_y_min);

        let clamp_to_transcript = |col: u16, row: u16| -> (usize, usize) {
            let cx = col.clamp(inner_x_min, inner_x_max).saturating_sub(inner_x_min) as usize;
            let cy = row.clamp(inner_y_min, inner_y_max).saturating_sub(inner_y_min) as usize;
            (cy + self.scroll_offset as usize, cx)
        };

        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag_anchor = None;
                self.drag_cursor = None;
                self.dragging = false;

                // 1. Center Transcript: start pane-clamped drag selection
                if in_rect(tr) {
                    let pos = clamp_to_transcript(me.column, me.row);
                    self.drag_anchor = Some(pos);
                    self.drag_cursor = Some(pos);
                    self.dragging = true;
                    self.focus = FocusPane::Chat;
                    return;
                }

                // 2. Top Header Pills
                if in_rect(self.last_header_rect) {
                    for &(x0, x1, act) in &self.header_hitboxes.clone() {
                        if me.column >= x0 && me.column < x1 {
                            self.trigger_mouse_action(act);
                            return;
                        }
                    }
                    return;
                }

                // 3. Input Box: focus & enter Insert mode
                if in_rect(self.last_input_rect) {
                    self.focus = FocusPane::Chat;
                    self.vim_mode = VimMode::Insert;
                    return;
                }

                // 4. Bottom Shortcut Footer chips
                if in_rect(self.last_footer_rect) {
                    for &(x0, x1, act) in &self.footer_hitboxes.clone() {
                        if me.column >= x0 && me.column < x1 {
                            self.trigger_mouse_action(act);
                            return;
                        }
                    }
                    return;
                }

                // 5. Left Sidebar: Recent Chats (click row to load)
                if let Some(lr) = self.last_left_rect {
                    if in_rect(lr) && !(self.preview_open && self.snap.preview_side == "left") {
                        self.focus = FocusPane::History;
                        self.vim_mode = VimMode::Normal;
                        if me.row > lr.y && me.row + 1 < lr.y + lr.height {
                            let idx = ((me.row - lr.y - 1) / 2) as usize;
                            if let Some(conv) = self.snap.history.get(idx) {
                                let id = conv.id;
                                self.history_state.select(Some(idx));
                                self.auto_scroll = true;
                                self.send_cmd(&ClientCmd::LoadConversation { id });
                            }
                        }
                        return;
                    }
                }

                // 6. Right Inspector: Quick Model Switcher, Specs, or Tips
                if let Some(mr) = self.last_models_rect {
                    if in_rect(mr) {
                        self.focus = FocusPane::Inspector;
                        self.vim_mode = VimMode::Normal;
                        if me.row > mr.y && me.row + 1 < mr.y + mr.height {
                            let idx = (me.row - mr.y - 1) as usize;
                            if let Some(model) = self.snap.models.get(idx).cloned() {
                                self.inspector_model_state.select(Some(idx));
                                self.send_cmd(&ClientCmd::SelectModel { model });
                            }
                        }
                        return;
                    }
                }
                if let Some(tr_tips) = self.last_tips_rect {
                    if in_rect(tr_tips) {
                        self.trigger_mouse_action(MouseAction::OpenHelp);
                        return;
                    }
                }
                if let Some(sr) = self.last_specs_rect {
                    if in_rect(sr) {
                        self.open_provider_modal();
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                self.drag_cursor = Some(clamp_to_transcript(me.column, me.row));
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                self.drag_cursor = Some(clamp_to_transcript(me.column, me.row));
                if let (Some(a), Some(b)) = (self.drag_anchor, self.drag_cursor) {
                    if a != b {
                        let text = extract_pane_selection(&self.rendered_transcript_lines, a, b);
                        if !text.trim().is_empty() {
                            let len = text.chars().count();
                            self.copy_text_to_clipboard(
                                &text,
                                &format!("Copied {len} chars from chat (sidebars excluded)"),
                            );
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some(lr) = self.last_left_rect {
                    if in_rect(lr) {
                        if self.preview_open && self.snap.preview_side == "left" {
                            self.preview_scroll = self.preview_scroll.saturating_sub(3);
                        } else if !self.snap.history.is_empty() {
                            let next = self
                                .history_state
                                .selected()
                                .map(|i| i.saturating_sub(1))
                                .unwrap_or(0);
                            self.history_state.select(Some(next));
                        }
                        return;
                    }
                }
                if let Some(rr) = self.last_right_rect {
                    if in_rect(rr) {
                        if self.preview_open && self.snap.preview_side != "left" {
                            self.preview_scroll = self.preview_scroll.saturating_sub(3);
                        } else if !self.snap.models.is_empty() {
                            let next = self
                                .inspector_model_state
                                .selected()
                                .map(|i| i.saturating_sub(1))
                                .unwrap_or(0);
                            self.inspector_model_state.select(Some(next));
                        }
                        return;
                    }
                }
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                if let Some(lr) = self.last_left_rect {
                    if in_rect(lr) {
                        if self.preview_open && self.snap.preview_side == "left" {
                            self.preview_scroll = self.preview_scroll.saturating_add(3);
                        } else if !self.snap.history.is_empty() {
                            let next = self
                                .history_state
                                .selected()
                                .map(|i| (i + 1).min(self.snap.history.len() - 1))
                                .unwrap_or(0);
                            self.history_state.select(Some(next));
                        }
                        return;
                    }
                }
                if let Some(rr) = self.last_right_rect {
                    if in_rect(rr) {
                        if self.preview_open && self.snap.preview_side != "left" {
                            self.preview_scroll = self.preview_scroll.saturating_add(3);
                        } else if !self.snap.models.is_empty() {
                            let next = self
                                .inspector_model_state
                                .selected()
                                .map(|i| (i + 1).min(self.snap.models.len() - 1))
                                .unwrap_or(0);
                            self.inspector_model_state.select(Some(next));
                        }
                        return;
                    }
                }
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_add(3);
            }
            _ => {}
        }
    }

    fn open_copy_modal(&mut self) {
        let Some(last) = self
            .snap
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| m.content.clone())
        else {
            self.local_notice = Some("No assistant reply to copy yet".into());
            return;
        };
        let sections = extract_copy_sections(&last);
        if sections.is_empty() {
            self.local_notice = Some("Assistant reply is empty".into());
            return;
        }
        let mut st = ListState::default();
        let default_idx = sections.iter().position(|s| s.kind == "CODE").unwrap_or(0);
        st.select(Some(default_idx));
        self.modal = Modal::CopyPicker {
            sections,
            list_state: st,
            raw_reply: last,
        };
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if !matches!(self.modal, Modal::None) {
            self.handle_modal_key(code);
            return;
        }

        // Ctrl shortcuts work across Insert & Normal modes
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Backspace | KeyCode::Char('h') if self.vim_mode == VimMode::Insert => {
                    self.delete_prev_word();
                }
                KeyCode::Char('h') if self.vim_mode == VimMode::Normal => {
                    self.focus = FocusPane::History;
                }
                KeyCode::Char('l') if self.vim_mode == VimMode::Normal && self.pending_w => {
                    self.pending_w = false;
                    self.focus = FocusPane::Inspector;
                }
                KeyCode::Char('w') if self.vim_mode == VimMode::Insert => {
                    self.delete_prev_word();
                }
                KeyCode::Char('w') if self.vim_mode == VimMode::Normal => {
                    if self.pending_w {
                        self.pending_w = false;
                        self.focus = match self.focus {
                            FocusPane::History => FocusPane::Chat,
                            FocusPane::Chat => FocusPane::Inspector,
                            FocusPane::Inspector => FocusPane::History,
                        };
                    } else {
                        self.pending_w = true;
                    }
                }
                KeyCode::Char('d') => {
                    self.auto_scroll = false;
                    if self.focus == FocusPane::Inspector && self.preview_open {
                        self.preview_scroll = self.preview_scroll.saturating_add(12);
                    } else {
                        self.scroll_offset = self.scroll_offset.saturating_add(12);
                    }
                }
                KeyCode::Char('u') if self.vim_mode == VimMode::Insert => self.input.clear(),
                KeyCode::Char('u') => {
                    self.auto_scroll = false;
                    if self.focus == FocusPane::Inspector && self.preview_open {
                        self.preview_scroll = self.preview_scroll.saturating_sub(12);
                    } else {
                        self.scroll_offset = self.scroll_offset.saturating_sub(12);
                    }
                }
                KeyCode::Char('q') | KeyCode::Char('c') => self.should_quit = true,
                KeyCode::Char('p') => self.open_provider_modal(),
                KeyCode::Char('m') => self.open_model_modal(),
                KeyCode::Char('g') => self.open_history_modal(),
                KeyCode::Char('o') => self.open_markdown_picker(),
                KeyCode::Char('e') => self.toggle_artifact_preview(),
                KeyCode::Char('r') => self.send_cmd(&ClientCmd::FetchModels),
                KeyCode::Char('n') => {
                    self.scroll_offset = 0;
                    self.preview_open = false;
                    self.send_cmd(&ClientCmd::NewChat);
                }
                KeyCode::Char('b') => {
                    self.force_panes = Some(!self.force_panes.unwrap_or(false));
                    self.chat_zoomed = false;
                }
                KeyCode::Char('z') => {
                    self.chat_zoomed = !self.chat_zoomed;
                }
                KeyCode::Left => {
                    self.vim_mode = VimMode::Normal;
                    self.focus = match self.focus {
                        FocusPane::Inspector => FocusPane::Chat,
                        FocusPane::Chat => FocusPane::History,
                        FocusPane::History => FocusPane::History,
                    };
                }
                KeyCode::Right => {
                    self.vim_mode = VimMode::Normal;
                    self.focus = match self.focus {
                        FocusPane::History => FocusPane::Chat,
                        FocusPane::Chat => FocusPane::Inspector,
                        FocusPane::Inspector => FocusPane::Inspector,
                    };
                }
                KeyCode::Char('y') => self.open_copy_modal(),
                KeyCode::Char('l') => self.send_cmd(&ClientCmd::ToggleLinuxQuick),
                _ => {}
            }
            return;
        }

        if mods.contains(KeyModifiers::ALT) {
            match code {
                KeyCode::Char('1') => {
                    self.vim_mode = VimMode::Normal;
                    self.focus = FocusPane::History;
                }
                KeyCode::Char('2') => {
                    self.focus = FocusPane::Chat;
                }
                KeyCode::Char('3') => {
                    self.vim_mode = VimMode::Normal;
                    self.focus = FocusPane::Inspector;
                }
                KeyCode::Char('p') => self.open_provider_modal(),
                KeyCode::Char('m') => self.open_model_modal(),
                KeyCode::Char('h') => self.open_history_modal(),
                KeyCode::Backspace => self.delete_prev_word(),
                _ => {}
            }
            return;
        }

        match self.vim_mode {
            VimMode::Search => self.handle_search_key(code),
            VimMode::Command => self.handle_command_key(code),
            VimMode::Normal => self.handle_normal_key(code),
            VimMode::Insert => self.handle_insert_key(code),
        }
    }

    fn toggle_artifact_preview(&mut self) {
        if self.preview_open {
            self.preview_open = false;
            if self.focus == FocusPane::Inspector {
                self.focus = FocusPane::Chat;
            }
            return;
        }
        self.preview_artifacts = extract_artifacts(&self.snap.messages);
        if self.preview_artifacts.is_empty() {
            self.local_notice = Some("No assistant reply or artifact to preview yet".into());
            return;
        }
        self.preview_idx = 0;
        self.preview_scroll = 0;
        self.preview_open = true;
    }

    fn handle_insert_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.vim_mode = VimMode::Normal;
                self.pending_g = false;
                self.pending_w = false;
            }
            KeyCode::F(1) => {
                self.modal = Modal::Help {
                    tab: 0,
                    selected: 0,
                    scroll: 0,
                    awaiting_key: false,
                }
            }
            KeyCode::F(2) => self.open_provider_modal(),
            KeyCode::F(3) => self.open_model_modal(),
            KeyCode::F(4) => self.open_history_modal(),
            KeyCode::F(5) => self.send_cmd(&ClientCmd::FetchModels),
            KeyCode::F(6) => {
                self.vim_mode = VimMode::Normal;
                self.focus = match self.focus {
                    FocusPane::Chat => FocusPane::Inspector,
                    FocusPane::Inspector => FocusPane::History,
                    FocusPane::History => FocusPane::Chat,
                };
            }
            KeyCode::Tab => {
                self.vim_mode = VimMode::Normal;
                self.focus = match self.focus {
                    FocusPane::Chat => FocusPane::Inspector,
                    FocusPane::Inspector => FocusPane::History,
                    FocusPane::History => FocusPane::Chat,
                };
            }
            KeyCode::Enter => {
                let prompt = self.input.trim().to_string();
                if !prompt.is_empty() && !self.snap.streaming {
                    self.input.clear();
                    self.auto_scroll = true;
                    self.send_cmd(&ClientCmd::SendPrompt { prompt });
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Up => {
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_sub(2);
            }
            KeyCode::Down => {
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_add(2);
            }
            KeyCode::PageUp => {
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, code: KeyCode) {
        if self.pending_w {
            self.pending_w = false;
            match code {
                KeyCode::Char('h') | KeyCode::Left => {
                    self.focus = FocusPane::History;
                    return;
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    self.focus = FocusPane::Inspector;
                    return;
                }
                KeyCode::Char('c') | KeyCode::Char('j') | KeyCode::Char('k') => {
                    self.focus = FocusPane::Chat;
                    return;
                }
                KeyCode::Char('w') => {
                    self.focus = match self.focus {
                        FocusPane::History => FocusPane::Chat,
                        FocusPane::Chat => FocusPane::Inspector,
                        FocusPane::Inspector => FocusPane::History,
                    };
                    return;
                }
                _ => {}
            }
        }

        let was_g = self.pending_g;
        self.pending_g = false;

        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('i') | KeyCode::Char('a') | KeyCode::Char('o') => {
                self.focus = FocusPane::Chat;
                self.vim_mode = VimMode::Insert;
            }
            KeyCode::Char('/') => {
                self.search_query.clear();
                self.search_matches.clear();
                self.search_match_idx = 0;
                self.vim_mode = VimMode::Search;
            }
            KeyCode::Char(':') => {
                self.command_buf.clear();
                self.vim_mode = VimMode::Command;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.modal = Modal::Help {
                    tab: 0,
                    selected: 0,
                    scroll: 0,
                    awaiting_key: false,
                };
            }
            KeyCode::F(6) => {
                self.focus = match self.focus {
                    FocusPane::Chat => FocusPane::Inspector,
                    FocusPane::Inspector => FocusPane::History,
                    FocusPane::History => FocusPane::Chat,
                };
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    FocusPane::Chat => FocusPane::Inspector,
                    FocusPane::Inspector => FocusPane::History,
                    FocusPane::History => FocusPane::Chat,
                };
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.focus = match self.focus {
                    FocusPane::Inspector => FocusPane::Chat,
                    FocusPane::Chat => FocusPane::History,
                    FocusPane::History => FocusPane::History,
                };
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.focus = match self.focus {
                    FocusPane::History => FocusPane::Chat,
                    FocusPane::Chat => FocusPane::Inspector,
                    FocusPane::Inspector => FocusPane::Inspector,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                FocusPane::Chat => {
                    self.auto_scroll = false;
                    self.scroll_offset = self.scroll_offset.saturating_add(2);
                }
                FocusPane::History => {
                    if !self.snap.history.is_empty() {
                        let next = self
                            .history_state
                            .selected()
                            .map(|i| (i + 1).min(self.snap.history.len() - 1))
                            .unwrap_or(0);
                        self.history_state.select(Some(next));
                    }
                }
                FocusPane::Inspector => {
                    if self.preview_open {
                        self.preview_scroll = self.preview_scroll.saturating_add(2);
                    } else if !self.snap.models.is_empty() {
                        let next = self
                            .inspector_model_state
                            .selected()
                            .map(|i| (i + 1).min(self.snap.models.len() - 1))
                            .unwrap_or(0);
                        self.inspector_model_state.select(Some(next));
                    }
                }
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                FocusPane::Chat => {
                    self.auto_scroll = false;
                    self.scroll_offset = self.scroll_offset.saturating_sub(2);
                }
                FocusPane::History => {
                    if !self.snap.history.is_empty() {
                        let next = self
                            .history_state
                            .selected()
                            .map(|i| i.saturating_sub(1))
                            .unwrap_or(0);
                        self.history_state.select(Some(next));
                    }
                }
                FocusPane::Inspector => {
                    if self.preview_open {
                        self.preview_scroll = self.preview_scroll.saturating_sub(2);
                    } else if !self.snap.models.is_empty() {
                        let next = self
                            .inspector_model_state
                            .selected()
                            .map(|i| i.saturating_sub(1))
                            .unwrap_or(0);
                        self.inspector_model_state.select(Some(next));
                    }
                }
            },
            KeyCode::Enter => match self.focus {
                FocusPane::History => {
                    if let Some(idx) = self.history_state.selected() {
                        if let Some(conv) = self.snap.history.get(idx) {
                            let id = conv.id;
                            self.auto_scroll = true;
                            self.focus = FocusPane::Chat;
                            self.send_cmd(&ClientCmd::LoadConversation { id });
                        }
                    }
                }
                FocusPane::Inspector if !self.preview_open => {
                    if let Some(idx) = self.inspector_model_state.selected() {
                        if let Some(model) = self.snap.models.get(idx).cloned() {
                            self.send_cmd(&ClientCmd::SelectModel { model });
                        }
                    }
                }
                _ => {
                    self.focus = FocusPane::Chat;
                    self.vim_mode = VimMode::Insert;
                }
            },
            KeyCode::Char('g') => {
                if was_g {
                    self.auto_scroll = false;
                    if self.focus == FocusPane::Inspector && self.preview_open {
                        self.preview_scroll = 0;
                    } else {
                        self.scroll_offset = 0;
                    }
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                if self.focus == FocusPane::Inspector && self.preview_open {
                    self.preview_scroll = u16::MAX / 2;
                } else {
                    self.auto_scroll = true;
                }
            }
            KeyCode::Char('n') => self.jump_search_match(true),
            KeyCode::Char('N') => self.jump_search_match(false),
            KeyCode::Char('p') => self.toggle_artifact_preview(),
            KeyCode::Char('x') if self.preview_open => {
                self.preview_open = false;
                if self.focus == FocusPane::Inspector {
                    self.focus = FocusPane::Chat;
                }
            }
            KeyCode::Char('s') => {
                let next_side = if self.snap.preview_side == "left" {
                    "right"
                } else {
                    "left"
                };
                self.send_cmd(&ClientCmd::SetPreviewSide {
                    side: next_side.to_string(),
                });
            }
            KeyCode::Char('[') if self.preview_open && !self.preview_artifacts.is_empty() => {
                self.preview_idx = self.preview_idx.saturating_sub(1);
                self.preview_scroll = 0;
            }
            KeyCode::Char(']') if self.preview_open && !self.preview_artifacts.is_empty() => {
                self.preview_idx = (self.preview_idx + 1).min(self.preview_artifacts.len() - 1);
                self.preview_scroll = 0;
            }
            KeyCode::Char('y') => {
                if self.focus == FocusPane::Inspector && self.preview_open {
                    if let Some(art) = self.preview_artifacts.get(self.preview_idx).cloned() {
                        self.copy_text_to_clipboard(
                            &art.content,
                            &format!("Copied {} to clipboard", art.title),
                        );
                        return;
                    }
                }
                self.open_copy_modal();
            }
            KeyCode::Char('m') => self.open_model_modal(),
            KeyCode::Char('z') => self.chat_zoomed = !self.chat_zoomed,
            KeyCode::Char('r') => self.send_cmd(&ClientCmd::FetchModels),
            KeyCode::F(2) => self.open_provider_modal(),
            KeyCode::F(3) => self.open_model_modal(),
            KeyCode::F(4) => self.open_history_modal(),
            KeyCode::F(5) => self.send_cmd(&ClientCmd::FetchModels),
            KeyCode::PageUp => {
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.auto_scroll = false;
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.vim_mode = VimMode::Normal;
                self.search_query.clear();
                self.search_matches.clear();
            }
            KeyCode::Enter => {
                self.vim_mode = VimMode::Normal;
                if !self.search_matches.is_empty() {
                    self.jump_search_match(true);
                }
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.update_search_matches();
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.update_search_matches();
            }
            _ => {}
        }
    }

    fn update_search_matches(&mut self) {
        self.search_matches.clear();
        self.search_match_idx = 0;
        let q = self.search_query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return;
        }
        let mut row = 0usize;
        for msg in &self.snap.messages {
            row += 1; // header line
            for line in msg.content.lines() {
                if line.to_ascii_lowercase().contains(&q) {
                    self.search_matches.push(row);
                }
                row += 1;
            }
            row += 2; // footer + spacer
        }
    }

    fn jump_search_match(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            if !self.search_query.trim().is_empty() {
                self.local_notice = Some(format!("Pattern not found: {}", self.search_query));
            }
            return;
        }
        if forward {
            self.search_match_idx = (self.search_match_idx + 1) % self.search_matches.len();
        } else {
            self.search_match_idx = if self.search_match_idx == 0 {
                self.search_matches.len() - 1
            } else {
                self.search_match_idx - 1
            };
        }
        let target_row = self.search_matches[self.search_match_idx];
        self.auto_scroll = false;
        self.scroll_offset = (target_row as u16).saturating_sub(3);
        self.local_notice = Some(format!(
            "Match {}/{} for '{}'",
            self.search_match_idx + 1,
            self.search_matches.len(),
            self.search_query
        ));
    }

    fn handle_command_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.vim_mode = VimMode::Normal;
                self.command_buf.clear();
            }
            KeyCode::Backspace => {
                self.command_buf.pop();
            }
            KeyCode::Enter => {
                let cmd = self.command_buf.trim().to_string();
                self.command_buf.clear();
                self.vim_mode = VimMode::Normal;
                self.execute_vim_command(&cmd);
            }
            KeyCode::Char(c) => self.command_buf.push(c),
            _ => {}
        }
    }

    fn execute_vim_command(&mut self, raw: &str) {
        let mut parts = raw.split_whitespace();
        let Some(cmd) = parts.next() else {
            return;
        };
        let arg = parts.next().unwrap_or("");
        match cmd {
            "q" | "quit" | "wq" => self.should_quit = true,
            "new" | "clear" => {
                self.scroll_offset = 0;
                self.preview_open = false;
                self.send_cmd(&ClientCmd::NewChat);
            }
            "p" | "preview" => self.toggle_artifact_preview(),
            "close" | "pc" => {
                self.preview_open = false;
                self.focus = FocusPane::Chat;
            }
            "side" => {
                if arg.eq_ignore_ascii_case("left") || arg.eq_ignore_ascii_case("right") {
                    self.send_cmd(&ClientCmd::SetPreviewSide {
                        side: arg.to_ascii_lowercase(),
                    });
                } else {
                    self.local_notice = Some("Usage: :side left | :side right".into());
                }
            }
            "layout" => {
                let nums: Vec<u16> = raw
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|s| s.parse::<u16>().ok())
                    .collect();
                if nums.len() == 3 && nums.iter().sum::<u16>() == 100 && nums.iter().all(|&n| n >= 10)
                {
                    self.send_cmd(&ClientCmd::SetPaneRatios {
                        ratios: [nums[0], nums[1], nums[2]],
                    });
                } else {
                    self.local_notice =
                        Some("Usage: :layout <left> <center> <right> (sum=100, e.g. :layout 20 60 20)".into());
                }
            }
            "panes" => {
                self.force_panes = Some(!self.force_panes.unwrap_or(false));
                self.chat_zoomed = false;
            }
            "z" | "zoom" => {
                self.chat_zoomed = !self.chat_zoomed;
            }
            "m" | "model" => self.open_model_modal(),
            "prov" | "provider" => self.open_provider_modal(),
            "hist" | "history" => self.open_history_modal(),
            "y" | "yank" | "copy" => self.open_copy_modal(),
            "r" | "refresh" => self.send_cmd(&ClientCmd::FetchModels),
            "o" | "open" => self.open_markdown_picker(),
            "c" | "config" | "cfg" => {
                self.modal = Modal::Help {
                    tab: 4,
                    selected: 0,
                    scroll: 0,
                    awaiting_key: false,
                }
            }
            "edit-config" | "tui.toml" => {
                let p = tui_config_path().to_string_lossy().into_owned();
                self.open_markdown_viewer(&p);
            }
            "h" | "help" => {
                self.modal = Modal::Help {
                    tab: 0,
                    selected: 0,
                    scroll: 0,
                    awaiting_key: false,
                }
            }
            _ => {
                self.local_notice = Some(format!("Unknown command: :{cmd} (try :help)"));
            }
        }
    }

    fn open_provider_modal(&mut self) {
        let idx = self
            .snap
            .providers
            .iter()
            .position(|(id, _, _)| id == &self.snap.provider_id)
            .unwrap_or(0);
        let mut st = ListState::default();
        if !self.snap.providers.is_empty() {
            st.select(Some(idx));
        }
        self.modal = Modal::Provider(st);
    }

    fn open_model_modal(&mut self) {
        let idx = self
            .snap
            .models
            .iter()
            .position(|m| m == &self.snap.model)
            .unwrap_or(0);
        let mut st = ListState::default();
        if !self.snap.models.is_empty() {
            st.select(Some(idx));
        }
        self.modal = Modal::Model(st);
    }

    fn open_history_modal(&mut self) {
        self.send_cmd(&ClientCmd::GetState);
        let mut st = ListState::default();
        if !self.snap.history.is_empty() {
            st.select(Some(0));
        }
        self.modal = Modal::History(st);
    }

    fn open_markdown_picker(&mut self) {
        let mut files = discover_previewable_files();
        files.sort();
        let mut list_state = ListState::default();
        if !files.is_empty() {
            list_state.select(Some(0));
        }
        self.modal = Modal::MarkdownPicker {
            files,
            list_state,
            path_input: String::new(),
        };
    }

    fn open_markdown_viewer(&mut self, path_str: &str) {
        let expanded = expand_user_path(path_str);
        let width = crossterm::terminal::size()
            .map(|(w, _)| w.saturating_sub(8) as usize)
            .unwrap_or(90);
        match load_preview_lines_for_tui(&expanded, width) {
            Ok(lines) => {
                self.modal = Modal::MarkdownViewer {
                    title: format!(" {} (Up/Down/PgUp/PgDn, Esc: Back) ", expanded.display()),
                    lines,
                    scroll: 0,
                };
            }
            Err(e) => {
                self.modal = Modal::None;
                self.local_notice = Some(format!("Preview failed for {}: {e:#}", expanded.display()));
            }
        }
    }

    fn handle_modal_key(&mut self, code: KeyCode) {
        if let Modal::MarkdownViewer { lines, scroll, .. } = &mut self.modal {
            let max_s = (lines.len() as u16).saturating_sub(10);
            match code {
                KeyCode::Esc | KeyCode::Char('q') => self.modal = Modal::None,
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(2),
                KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 2).min(max_s),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(15),
                KeyCode::PageDown => *scroll = (*scroll + 15).min(max_s),
                KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                KeyCode::End | KeyCode::Char('G') => *scroll = max_s,
                _ => {}
            }
            return;
        }

        if let Modal::MarkdownPicker {
            files,
            list_state,
            path_input,
        } = &mut self.modal
        {
            match code {
                KeyCode::Esc => {
                    self.modal = Modal::None;
                    return;
                }
                KeyCode::Up if path_input.is_empty() => {
                    if !files.is_empty() {
                        let next = list_state.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
                        list_state.select(Some(next));
                    }
                    return;
                }
                KeyCode::Down if path_input.is_empty() => {
                    if !files.is_empty() {
                        let next = list_state
                            .selected()
                            .map(|i| (i + 1).min(files.len() - 1))
                            .unwrap_or(0);
                        list_state.select(Some(next));
                    }
                    return;
                }
                KeyCode::Backspace => {
                    path_input.pop();
                    return;
                }
                KeyCode::Enter => {
                    let chosen = if !path_input.trim().is_empty() {
                        path_input.trim().to_string()
                    } else if let Some(idx) = list_state.selected() {
                        files.get(idx).cloned().unwrap_or_default()
                    } else {
                        String::new()
                    };
                    if !chosen.is_empty() {
                        self.open_markdown_viewer(&chosen);
                    }
                    return;
                }
                KeyCode::Char(c) => {
                    path_input.push(c);
                    return;
                }
                _ => return,
            }
        }

        if let Modal::CopyPicker {
            sections,
            list_state,
            raw_reply,
        } = &mut self.modal
        {
            match code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.modal = Modal::None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if !sections.is_empty() {
                        let next = list_state.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
                        list_state.select(Some(next));
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !sections.is_empty() {
                        let next = list_state
                            .selected()
                            .map(|i| (i + 1).min(sections.len() - 1))
                            .unwrap_or(0);
                        list_state.select(Some(next));
                    }
                }
                KeyCode::Char('a') => {
                    let text = raw_reply.trim().to_string();
                    self.modal = Modal::None;
                    self.copy_text_to_clipboard(&text, "Copied full reply (clean) to clipboard");
                }
                KeyCode::Char('v') => {
                    let clean_lines: Vec<String> =
                        raw_reply.lines().map(|l| l.to_string()).collect();
                    if !clean_lines.is_empty() {
                        self.modal = Modal::CopyLines {
                            lines: clean_lines,
                            cursor: 0,
                            anchor: None,
                            scroll: 0,
                        };
                    }
                }
                KeyCode::Char(d @ '1'..='9') => {
                    let idx = (d as u8 - b'1') as usize;
                    if let Some(sec) = sections.get(idx).cloned() {
                        self.modal = Modal::None;
                        self.copy_text_to_clipboard(
                            &sec.content,
                            &format!("Copied {} section to clipboard", sec.kind),
                        );
                    }
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    if let Some(idx) = list_state.selected() {
                        if let Some(sec) = sections.get(idx).cloned() {
                            self.modal = Modal::None;
                            self.copy_text_to_clipboard(
                                &sec.content,
                                &format!("Copied {} section to clipboard", sec.kind),
                            );
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        if let Modal::CopyLines {
            lines,
            cursor,
            anchor,
            scroll,
        } = &mut self.modal
        {
            let max_idx = lines.len().saturating_sub(1);
            match code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.modal = Modal::None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *cursor = cursor.saturating_sub(1);
                    if (*cursor as u16) < *scroll {
                        *scroll = *cursor as u16;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *cursor = (*cursor + 1).min(max_idx);
                    if (*cursor as u16) >= scroll.saturating_add(16) {
                        *scroll = (*cursor as u16).saturating_sub(15);
                    }
                }
                KeyCode::Char(' ') | KeyCode::Char('v') => {
                    *anchor = if anchor.is_some() { None } else { Some(*cursor) };
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    let (lo, hi) = if let Some(a) = *anchor {
                        (a.min(*cursor), a.max(*cursor))
                    } else {
                        (*cursor, *cursor)
                    };
                    let selected = lines[lo..=hi.min(max_idx)].join("\n");
                    let count = hi.saturating_sub(lo) + 1;
                    self.modal = Modal::None;
                    self.copy_text_to_clipboard(
                        &selected,
                        &format!("Copied {count} clean line(s) to clipboard"),
                    );
                }
                _ => {}
            }
            return;
        }

        if let Modal::Help {
            tab,
            selected,
            scroll,
            awaiting_key,
        } = &mut self.modal
        {
            const HELP_TABS: usize = 5;
            const CONFIG_ITEMS: usize = 13;
            if *awaiting_key {
                let new_key = match code {
                    KeyCode::Esc => {
                        *awaiting_key = false;
                        return;
                    }
                    KeyCode::Char(c) => c.to_string(),
                    KeyCode::F(n) => format!("F{n}"),
                    _ => return,
                };
                let action = match *selected {
                    6 => "toggle_preview",
                    7 => "swap_side",
                    8 => "copy_section",
                    9 => "search",
                    10 => "command",
                    11 => "focus_left",
                    12 => "focus_right",
                    _ => {
                        *awaiting_key = false;
                        return;
                    }
                };
                *awaiting_key = false;
                self.send_cmd(&ClientCmd::UpdateKeybind {
                    action: action.to_string(),
                    key: new_key,
                });
                return;
            }

            match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                    self.modal = Modal::None;
                }
                KeyCode::Char(d @ '1'..='5') => {
                    *tab = (d as u8 - b'1') as usize;
                    *scroll = 0;
                }
                KeyCode::Tab | KeyCode::Right => {
                    *tab = (*tab + 1) % HELP_TABS;
                    *scroll = 0;
                }
                KeyCode::BackTab | KeyCode::Left => {
                    *tab = if *tab == 0 { HELP_TABS - 1 } else { *tab - 1 };
                    *scroll = 0;
                }
                KeyCode::Char('l') if *tab < 4 => {
                    *tab = (*tab + 1) % HELP_TABS;
                    *scroll = 0;
                }
                KeyCode::Char('h') if *tab < 4 => {
                    *tab = if *tab == 0 { HELP_TABS - 1 } else { *tab - 1 };
                    *scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if *tab == 4 {
                        *selected = selected.saturating_sub(1);
                    } else {
                        *scroll = scroll.saturating_sub(2);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if *tab == 4 {
                        *selected = (*selected + 1).min(CONFIG_ITEMS - 1);
                    } else {
                        *scroll = scroll.saturating_add(2);
                    }
                }
                KeyCode::Char('g') if *tab < 4 => *scroll = 0,
                KeyCode::Char('G') if *tab < 4 => *scroll = 30,
                KeyCode::Char('c') => {
                    *tab = 4;
                    *scroll = 0;
                }
                KeyCode::Char('e') => {
                    let p = tui_config_path().to_string_lossy().into_owned();
                    self.open_markdown_viewer(&p);
                }
                KeyCode::Char('R') => {
                    self.send_cmd(&ClientCmd::ResetKeybinds);
                }
                KeyCode::Enter | KeyCode::Char(' ') if *tab == 4 => match *selected {
                    0 => {
                        let presets = [[20, 60, 20], [18, 64, 18], [25, 50, 25], [15, 70, 15]];
                        let cur = self.snap.pane_ratios;
                        let next_idx = presets
                            .iter()
                            .position(|&p| p == cur)
                            .map(|i| (i + 1) % presets.len())
                            .unwrap_or(0);
                        self.send_cmd(&ClientCmd::SetPaneRatios {
                            ratios: presets[next_idx],
                        });
                    }
                    1 => {
                        let next_side = if self.snap.preview_side == "left" {
                            "right"
                        } else {
                            "left"
                        };
                        self.send_cmd(&ClientCmd::SetPreviewSide {
                            side: next_side.to_string(),
                        });
                    }
                    2 => self.send_cmd(&ClientCmd::ToggleAutoPreview),
                    3 => self.send_cmd(&ClientCmd::ToggleStartMode),
                    4 => self.send_cmd(&ClientCmd::ToggleShowTips),
                    5 => self.send_cmd(&ClientCmd::ToggleLinuxQuick),
                    6..=12 => {
                        *awaiting_key = true;
                    }
                    _ => {}
                },
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Esc => {
                self.modal = Modal::None;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let len = self.modal_len();
                if let Some(st) = self.modal_state_mut() {
                    if len > 0 {
                        let next = st.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
                        st.select(Some(next));
                    }
                }
                return;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.modal_len();
                if let Some(st) = self.modal_state_mut() {
                    if len > 0 {
                        let next = st.selected().map(|i| (i + 1).min(len - 1)).unwrap_or(0);
                        st.select(Some(next));
                    }
                }
                return;
            }
            KeyCode::Enter => {}
            _ => return,
        }

        let modal = std::mem::replace(&mut self.modal, Modal::None);
        match modal {
            Modal::Provider(st) => {
                if let Some(idx) = st.selected() {
                    if let Some((id, _, _)) = self.snap.providers.get(idx).cloned() {
                        self.send_cmd(&ClientCmd::SelectProvider { id });
                    }
                }
            }
            Modal::Model(st) => {
                if let Some(idx) = st.selected() {
                    if let Some(model) = self.snap.models.get(idx).cloned() {
                        self.send_cmd(&ClientCmd::SelectModel { model });
                    }
                }
            }
            Modal::History(st) => {
                if let Some(idx) = st.selected() {
                    if let Some(conv) = self.snap.history.get(idx) {
                        let id = conv.id;
                        self.auto_scroll = true;
                        self.send_cmd(&ClientCmd::LoadConversation { id });
                    }
                }
            }
            Modal::None
            | Modal::Help { .. }
            | Modal::MarkdownPicker { .. }
            | Modal::MarkdownViewer { .. }
            | Modal::CopyPicker { .. }
            | Modal::CopyLines { .. } => {}
        }
    }

    fn modal_len(&self) -> usize {
        match &self.modal {
            Modal::None
            | Modal::Help { .. }
            | Modal::MarkdownViewer { .. }
            | Modal::CopyLines { .. } => 0,
            Modal::Provider(_) => self.snap.providers.len(),
            Modal::Model(_) => self.snap.models.len(),
            Modal::History(_) => self.snap.history.len(),
            Modal::MarkdownPicker { files, .. } => files.len(),
            Modal::CopyPicker { sections, .. } => sections.len(),
        }
    }

    fn modal_state_mut(&mut self) -> Option<&mut ListState> {
        match &mut self.modal {
            Modal::None
            | Modal::Help { .. }
            | Modal::MarkdownViewer { .. }
            | Modal::CopyLines { .. } => None,
            Modal::Provider(s)
            | Modal::Model(s)
            | Modal::History(s)
            | Modal::MarkdownPicker { list_state: s, .. }
            | Modal::CopyPicker { list_state: s, .. } => Some(s),
        }
    }

    fn render_history_sidebar(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let focused = self.focus == FocusPane::History;
        let border_color = if focused {
            Color::LightCyan
        } else {
            Color::Rgb(65, 85, 115)
        };
        let items: Vec<ListItem<'_>> = if self.snap.history.is_empty() {
            vec![ListItem::new(Span::styled(
                "No saved chats yet",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            self.snap
                .history
                .iter()
                .map(|c| {
                    let is_active = self.snap.active_conversation_id == Some(c.id);
                    let marker = if is_active { "● " } else { "  " };
                    let title_style = if is_active {
                        Style::default()
                            .fg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::styled(marker, Style::default().fg(Color::LightGreen)),
                            Span::styled(c.title.clone(), title_style),
                        ]),
                        Line::from(Span::styled(
                            format!("  {}", c.model),
                            Style::default().fg(Color::DarkGray),
                        )),
                    ])
                })
                .collect()
        };
        let title = if focused {
            " Recent Chats (j/k:Move Enter:Load) "
        } else {
            " Recent Chats (Ctrl+W h) "
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_color))
                    .title(Span::styled(
                        title,
                        Style::default()
                            .fg(if focused {
                                Color::LightCyan
                            } else {
                                Color::Gray
                            })
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(36, 62, 102))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, area, &mut self.history_state);
    }

    fn render_preview_panel(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let focused = self.focus == FocusPane::Inspector;
        let border_color = if focused {
            Color::LightGreen
        } else {
            Color::Rgb(75, 130, 100)
        };
        let inner_w = area.width.saturating_sub(4).max(20) as usize;
        let (title, body_lines): (String, Vec<Line<'static>>) =
            if let Some(art) = self.preview_artifacts.get(self.preview_idx) {
                let t = format!(
                    " Preview [{}/{}] {} (s:Side x:Close) ",
                    self.preview_idx + 1,
                    self.preview_artifacts.len().max(1),
                    art.title
                );
                let mut out = Vec::new();
                match art.kind.as_str() {
                    "markdown" => out.extend(render_markdown_lines(&art.content, None, inner_w)),
                    "html" => {
                        let md = html_to_readable_markdown(&art.content);
                        out.extend(render_markdown_lines(&md, None, inner_w));
                    }
                    _ => {
                        let fenced = format!("```{}\n{}\n```", art.kind, art.content);
                        out.extend(render_markdown_lines(&fenced, None, inner_w));
                    }
                }
                (t, out)
            } else {
                (
                    " Preview (Empty) ".to_string(),
                    vec![Line::from(Span::styled(
                        "No artifact generated yet.",
                        Style::default().fg(Color::DarkGray),
                    ))],
                )
            };

        let p = Paragraph::new(body_lines)
            .wrap(Wrap { trim: false })
            .scroll((self.preview_scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_color))
                    .title(Span::styled(
                        title,
                        Style::default()
                            .fg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        frame.render_widget(p, area);
    }

    fn render_inspector_panel(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let focused = self.focus == FocusPane::Inspector;
        let border_color = if focused {
            Color::LightYellow
        } else {
            Color::Rgb(85, 80, 120)
        };
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9),
                Constraint::Min(5),
                Constraint::Length(9),
            ])
            .split(area);

        let (ctx_win, tier, api_proto) =
            infer_model_specs(&self.snap.model, &self.snap.provider_kind);
        let msg_count = self.snap.messages.len();
        let specs_lines = vec![
            Line::from(vec![
                Span::styled("Provider: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.snap.provider_name.clone(),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Model:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.snap.model.clone(),
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("Context:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(ctx_win, Style::default().fg(Color::LightGreen)),
            ]),
            Line::from(vec![
                Span::styled("Tier:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(tier, Style::default().fg(Color::LightMagenta)),
            ]),
            Line::from(vec![
                Span::styled("API:      ", Style::default().fg(Color::DarkGray)),
                Span::styled(api_proto, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("Auth:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(self.snap.provider_auth.clone(), Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled("Session:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{msg_count} msgs | Side:{}", self.snap.preview_side), Style::default().fg(Color::Gray)),
            ]),
        ];
        let specs_box = Paragraph::new(specs_lines)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_color))
                    .title(Span::styled(
                        " Model Specs ",
                        Style::default()
                            .fg(Color::LightYellow)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        self.last_specs_rect = Some(sections[0]);
        self.last_models_rect = Some(sections[1]);
        self.last_tips_rect = Some(sections[2]);
        frame.render_widget(specs_box, sections[0]);

        let model_items: Vec<ListItem<'_>> = if self.snap.models.is_empty() {
            vec![ListItem::new(Span::styled(
                "Press r to fetch models",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            self.snap
                .models
                .iter()
                .map(|m| {
                    let active = m == &self.snap.model;
                    let prefix = if active { "★ " } else { "  " };
                    ListItem::new(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(Color::LightYellow)),
                        Span::styled(
                            m.clone(),
                            if active {
                                Style::default()
                                    .fg(Color::LightYellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            },
                        ),
                    ]))
                })
                .collect()
        };
        let switcher_title = if focused {
            " Switch Model (j/k Enter | r:Fetch) "
        } else {
            " Models (Ctrl+W l to switch) "
        };
        let model_list = List::new(model_items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_color))
                    .title(Span::styled(
                        switcher_title,
                        Style::default()
                            .fg(if focused {
                                Color::LightYellow
                            } else {
                                Color::Gray
                            })
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(85, 65, 25))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(model_list, sections[1], &mut self.inspector_model_state);

        // Lower-third: Recommended TUI Tips & Help Launcher
        let kb = &self.snap.keybinds;
        let tips_lines = vec![
            Line::from(vec![
                Span::styled(" Esc/i  ", Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)),
                Span::styled("Normal / Insert mode", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled(" Alt+1/2/3 ", Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)),
                Span::styled("or F6: Switch panes", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled(format!(" {}text  ", kb.search), Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
                Span::styled("Search (n/N next/prev)", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled(format!(" Ctrl+Y/{} ", kb.copy_section), Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
                Span::styled("Clean section copy", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled(format!(" {}/{}    ", kb.toggle_preview, kb.swap_side), Style::default().fg(Color::LightMagenta).add_modifier(Modifier::BOLD)),
                Span::styled("Preview / Swap side", Style::default().fg(Color::Gray)),
            ]),
            Line::from(vec![
                Span::styled(format!(" {}/F1   ", kb.help), Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)),
                Span::styled("Help & Keybind Config", Style::default().fg(Color::White)),
            ]),
        ];
        let tips_box = Paragraph::new(tips_lines)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Rgb(70, 110, 145)))
                    .title(Span::styled(
                        " Tips & Shortcuts (? Help) ",
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        frame.render_widget(tips_box, sections[2]);
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let full_area = frame.area();
        // Only enable 3-pane layout in true fullscreen/wide terminals (>= 150 cols),
        // keeping the initial ~50% Alt+Space popup focused on Chat only (unless preview is open or forced).
        let wide_mode = !self.chat_zoomed && self.force_panes.unwrap_or(full_area.width >= 150);
        let [r_left, r_center, r_right] = self.snap.pane_ratios;

        let (left_area, center_area, right_area): (Option<Rect>, Rect, Option<Rect>) = if self.chat_zoomed {
            (None, full_area, None)
        } else if wide_mode {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(r_left),
                    Constraint::Percentage(r_center),
                    Constraint::Percentage(r_right),
                ])
                .split(full_area);
            (Some(cols[0]), cols[1], Some(cols[2]))
        } else if self.preview_open {
            if self.snap.preview_side == "left" {
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                    .split(full_area);
                (Some(cols[0]), cols[1], None)
            } else {
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(54), Constraint::Percentage(46)])
                    .split(full_area);
                (None, cols[0], Some(cols[1]))
            }
        } else {
            (None, full_area, None)
        };

        self.last_left_rect = left_area;
        self.last_right_rect = right_area;
        self.last_specs_rect = None;
        self.last_models_rect = None;
        self.last_tips_rect = None;
        self.last_modal_rect = None;
        self.header_hitboxes.clear();
        self.footer_hitboxes.clear();
        self.help_tab_hitboxes.clear();

        if let Some(la) = left_area {
            if self.preview_open && self.snap.preview_side == "left" {
                self.render_preview_panel(frame, la);
            } else {
                self.render_history_sidebar(frame, la);
            }
        }
        if let Some(ra) = right_area {
            if self.preview_open && self.snap.preview_side != "left" {
                self.render_preview_panel(frame, ra);
            } else if wide_mode {
                self.render_inspector_panel(frame, ra);
            }
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(center_area);

        // Colorful Antigravity-inspired header pills + Vim Mode indicator
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][self.tick % 10];
        let mode_badge = match self.vim_mode {
            VimMode::Insert => Span::styled(
                " INSERT ",
                Style::default()
                    .bg(Color::Rgb(22, 115, 70))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            VimMode::Normal => Span::styled(
                " NORMAL ",
                Style::default()
                    .bg(Color::Rgb(30, 95, 155))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            VimMode::Search => Span::styled(
                " SEARCH ",
                Style::default()
                    .bg(Color::Rgb(150, 110, 20))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            VimMode::Command => Span::styled(
                " CMD ",
                Style::default()
                    .bg(Color::Rgb(135, 50, 145))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        };
        let linux_badge = if self.snap.linux_quick_mode {
            Span::styled(
                " Linux:ON ",
                Style::default()
                    .bg(Color::Rgb(20, 90, 75))
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " Linux:OFF ",
                Style::default()
                    .bg(Color::Rgb(50, 50, 60))
                    .fg(Color::Gray),
            )
        };
        let status_span = if self.snap.streaming {
            Span::styled(
                format!(" {spinner} Streaming "),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            let msg = self
                .local_notice
                .as_deref()
                .unwrap_or(self.snap.status.as_str());
            Span::styled(format!(" {msg} "), Style::default().fg(Color::Gray))
        };

        let prov_text = format!(" {} ", self.snap.provider_name);
        let model_text = format!(" {} ", self.snap.model);
        let linux_text = if self.snap.linux_quick_mode {
            " Linux:ON "
        } else {
            " Linux:OFF "
        };
        let mode_text = match self.vim_mode {
            VimMode::Insert => " INSERT ",
            VimMode::Normal => " NORMAL ",
            VimMode::Search => " SEARCH ",
            VimMode::Command => " CMD ",
        };
        let mut hx = chunks[0].x + 1;
        let mw = mode_text.chars().count() as u16;
        self.header_hitboxes.push((hx, hx + mw, MouseAction::ToggleMode));
        hx += mw + 1 + " ✦ HyprPilot ".chars().count() as u16 + 1;
        let pw = prov_text.chars().count() as u16;
        self.header_hitboxes.push((hx, hx + pw, MouseAction::OpenProvider));
        hx += pw + 1;
        let mdw = model_text.chars().count() as u16;
        self.header_hitboxes.push((hx, hx + mdw, MouseAction::OpenModel));
        hx += mdw + 1;
        let lw = linux_text.chars().count() as u16;
        self.header_hitboxes.push((hx, hx + lw, MouseAction::ToggleLinux));
        self.last_header_rect = chunks[0];

        let header = Paragraph::new(Line::from(vec![
            mode_badge,
            Span::raw(" "),
            Span::styled(
                " ✦ HyprPilot ",
                Style::default()
                    .bg(Color::Rgb(105, 45, 165))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                prov_text,
                Style::default()
                    .bg(Color::Rgb(24, 78, 120))
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                model_text,
                Style::default()
                    .bg(Color::Rgb(95, 68, 22))
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            linux_badge,
            status_span,
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(110, 90, 190))),
        );
        frame.render_widget(header, chunks[0]);

        // Colorful Chat Transcript (pre-wrapped so prefixes & span styles never break across lines)
        let inner_width = chunks[1].width.saturating_sub(2).max(24) as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.snap.messages.is_empty() && !self.snap.streaming {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  ✦ ", Style::default().fg(Color::LightMagenta)),
                Span::styled(
                    "Ready with your last-used model.",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                "    Esc: Normal mode (Vim motions, / search, : commands) | p: Preview | ?: Help",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for m in &self.snap.messages {
                render_styled_message(
                    &mut lines,
                    &m.role,
                    &m.content,
                    &self.snap.model,
                    false,
                    inner_width,
                );
            }
            if self.snap.streaming {
                let text = if self.snap.streaming_reply.is_empty() {
                    format!("{spinner} Thinking...")
                } else {
                    self.snap.streaming_reply.clone()
                };
                render_styled_message(
                    &mut lines,
                    "assistant",
                    &text,
                    &self.snap.model,
                    true,
                    inner_width,
                );
            }
        }

        if !self.search_query.trim().is_empty() {
            let q = self.search_query.trim().to_ascii_lowercase();
            for line in &mut lines {
                let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                if plain.to_ascii_lowercase().contains(&q) {
                    for sp in &mut line.spans {
                        sp.style = sp.style.bg(Color::Rgb(75, 62, 18));
                    }
                }
            }
        }

        // Record plain text rows for pane-clamped selection and highlight active drag range
        self.rendered_transcript_lines = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        self.last_transcript_rect = chunks[1];

        if let (Some(a), Some(b)) = (self.drag_anchor, self.drag_cursor) {
            let (r_lo, r_hi) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
            for (idx, line) in lines.iter_mut().enumerate() {
                if idx >= r_lo && idx <= r_hi {
                    for sp in &mut line.spans {
                        sp.style = sp.style.bg(Color::Rgb(42, 72, 118));
                    }
                }
            }
        }

        let inner_height = chunks[1].height.saturating_sub(2);
        let total_wrapped_rows = lines.len();
        let max_scroll = (total_wrapped_rows as u16).saturating_sub(inner_height);
        if self.auto_scroll {
            self.scroll_offset = max_scroll;
        } else {
            self.scroll_offset = self.scroll_offset.min(max_scroll);
        }

        let chat_focused = self.focus == FocusPane::Chat;
        let transcript = Paragraph::new(lines)
            .scroll((self.scroll_offset, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(if chat_focused {
                        Color::Rgb(80, 145, 205)
                    } else {
                        Color::Rgb(55, 80, 110)
                    }))
                    .title(Span::styled(
                        if self.search_matches.is_empty() {
                            " Conversation ".to_string()
                        } else {
                            format!(
                                " Conversation [Match {}/{}] ",
                                self.search_match_idx + 1,
                                self.search_matches.len()
                            )
                        },
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        frame.render_widget(transcript, chunks[1]);

        // Input Box
        let cursor_span = if self.vim_mode == VimMode::Insert {
            Span::styled("█", Style::default().fg(Color::LightCyan))
        } else {
            Span::styled(" ", Style::default())
        };
        let input_title = match self.vim_mode {
            VimMode::Insert => " Ask (Enter: Send | Esc: Normal Mode | Ctrl+Bksp: Del Word) ",
            VimMode::Normal => " Normal Mode (i: Insert | j/k: Scroll | /: Search | :: Cmd | ?: Help) ",
            VimMode::Search => " Search Mode (Enter: Jump | Esc: Cancel) ",
            VimMode::Command => " Command Mode (Enter: Run | Esc: Cancel) ",
        };
        let input_box = Paragraph::new(Line::from(vec![
            Span::styled(
                " ❯ ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.input.clone(), Style::default().fg(Color::White)),
            cursor_span,
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if self.snap.streaming {
                    Color::Yellow
                } else if self.vim_mode == VimMode::Normal {
                    Color::Rgb(95, 135, 195)
                } else {
                    Color::LightCyan
                }))
                .title(Span::styled(
                    input_title,
                    Style::default().fg(Color::LightBlue),
                )),
        );
        self.last_input_rect = chunks[2];
        frame.render_widget(input_box, chunks[2]);

        // Bottom Bar: Vim Search / Command prompt or clickable shortcut chips
        self.last_footer_rect = chunks[3];
        let mut fx = chunks[3].x;
        let mut build_chips = |defs: &[(&str, &str, MouseAction)]| -> Vec<Span<'static>> {
            let mut spans = Vec::with_capacity(defs.len());
            for &(k, l, act) in defs {
                let txt = format!(" {k}:{l} ");
                let w = txt.chars().count() as u16;
                self.footer_hitboxes.push((fx, fx + w, act));
                fx += w;
                spans.push(key_chip(k, l));
            }
            spans
        };
        let footer = match self.vim_mode {
            VimMode::Search => Paragraph::new(Line::from(vec![
                Span::styled(
                    " /",
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.search_query.clone(), Style::default().fg(Color::White)),
                Span::styled("█  ", Style::default().fg(Color::LightYellow)),
                Span::styled(
                    format!("({} matches)", self.search_matches.len()),
                    Style::default().fg(Color::DarkGray),
                ),
            ])),
            VimMode::Command => Paragraph::new(Line::from(vec![
                Span::styled(
                    " :",
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.command_buf.clone(), Style::default().fg(Color::White)),
                Span::styled("█", Style::default().fg(Color::LightMagenta)),
            ])),
            VimMode::Normal => Paragraph::new(Line::from(build_chips(&[
                ("i", "Insert", MouseAction::EnterInsert),
                ("/", "Search", MouseAction::EnterSearch),
                (":", "Cmd", MouseAction::EnterCommand),
                ("p", "Preview", MouseAction::TogglePreview),
                ("s", "Side", MouseAction::SwapSide),
                ("y", "Copy", MouseAction::OpenCopy),
                ("Ctrl+W", "Pane", MouseAction::CycleFocus),
                ("?", "Help", MouseAction::OpenHelp),
                ("q", "Hide", MouseAction::Quit),
            ]))),
            VimMode::Insert => Paragraph::new(Line::from(build_chips(&[
                ("Esc", "Normal", MouseAction::ToggleMode),
                ("Ctrl+P", "Provider", MouseAction::OpenProvider),
                ("Ctrl+M", "Model", MouseAction::OpenModel),
                ("Ctrl+E", "Preview", MouseAction::TogglePreview),
                ("Ctrl+G", "History", MouseAction::OpenHistory),
                ("Ctrl+Y", "Copy", MouseAction::OpenCopy),
                ("?", "Help", MouseAction::OpenHelp),
                ("Ctrl+Q", "Quit", MouseAction::Quit),
            ]))),
        };
        frame.render_widget(footer, chunks[3]);

        // Modals
        match &mut self.modal {
            Modal::None => {}
            Modal::Help {
                tab,
                selected,
                scroll,
                awaiting_key,
            } => {
                let area = centered_rect(88, 86, frame.area());
                self.last_modal_rect = Some(area);
                frame.render_widget(Clear, area);
                let parts = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Min(12),
                        Constraint::Length(2),
                    ])
                    .split(area);

                let tab_names = [
                    "1:Overview & Modes",
                    "2:Motions & Search",
                    "3:Panes & Preview",
                    "4:Commands (:ex)",
                    "5:TUI Config (tui.toml)",
                ];
                let mut tab_spans = vec![Span::raw(" ")];
                let mut tx = parts[0].x + 2;
                let ty = parts[0].y + 1;
                for (i, name) in tab_names.iter().enumerate() {
                    let label = format!(" {name} ");
                    let tw = label.chars().count() as u16;
                    self.help_tab_hitboxes.push((ty, tx, tx + tw, i));
                    tx += tw + 1;
                    if i == *tab {
                        tab_spans.push(Span::styled(
                            label,
                            Style::default()
                                .bg(Color::Rgb(35, 90, 155))
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        tab_spans.push(Span::styled(
                            label,
                            Style::default().fg(Color::Gray),
                        ));
                    }
                    tab_spans.push(Span::raw(" "));
                }
                let tab_bar = Paragraph::new(Line::from(tab_spans)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::LightCyan))
                        .title(Span::styled(
                            " HyprPilot TUI Manual & Configurator (1-5 / Tab / ←→) ",
                            Style::default()
                                .fg(Color::LightCyan)
                                .add_modifier(Modifier::BOLD),
                        )),
                );
                frame.render_widget(tab_bar, parts[0]);

                if *tab == 4 {
                    let kb = &self.snap.keybinds;
                    let cfg_path = tui_config_path().display().to_string();
                    let rows: [(&str, String); 13] = [
                        (
                            "Layout Proportions (L/C/R %)",
                            format!(
                                "{}% / {}% / {}%  (Enter: cycle, or :layout L C R)",
                                self.snap.pane_ratios[0],
                                self.snap.pane_ratios[1],
                                self.snap.pane_ratios[2]
                            ),
                        ),
                        (
                            "Artifact Preview Side",
                            format!("{}  (Enter: left/right)", self.snap.preview_side),
                        ),
                        (
                            "Auto-Preview MD Artifacts",
                            if self.snap.auto_preview_md { "ON" } else { "OFF" }.into(),
                        ),
                        (
                            "Startup Vim Mode",
                            format!("{}  (insert / normal)", self.snap.start_mode),
                        ),
                        (
                            "Show Inspector Tips Panel",
                            if self.snap.show_tips { "ON" } else { "OFF" }.into(),
                        ),
                        (
                            "Linux Quick Prompt Mode",
                            if self.snap.linux_quick_mode { "ON" } else { "OFF" }.into(),
                        ),
                        ("Key: Toggle Preview", kb.toggle_preview.clone()),
                        ("Key: Swap Preview Side", kb.swap_side.clone()),
                        ("Key: Copy Clean Section", kb.copy_section.clone()),
                        ("Key: Search Transcript", kb.search.clone()),
                        ("Key: Command Bar", kb.command.clone()),
                        ("Key: Focus Left Pane", kb.focus_left.clone()),
                        ("Key: Focus Right Pane", kb.focus_right.clone()),
                    ];

                    let list_items: Vec<ListItem<'_>> = rows
                        .iter()
                        .enumerate()
                        .map(|(i, (label, val))| {
                            let is_sel = i == *selected;
                            let display_val = if is_sel && *awaiting_key {
                                "Press new key (Esc to cancel)...".to_string()
                            } else {
                                val.clone()
                            };
                            let bg = if is_sel {
                                Color::Rgb(42, 72, 122)
                            } else {
                                Color::Reset
                            };
                            ListItem::new(Line::from(vec![
                                Span::styled(
                                    if is_sel { " ▶ " } else { "   " },
                                    Style::default().fg(Color::LightCyan).bg(bg),
                                ),
                                Span::styled(
                                    format!("{label:<30} "),
                                    Style::default()
                                        .fg(Color::White)
                                        .bg(bg)
                                        .add_modifier(if is_sel {
                                            Modifier::BOLD
                                        } else {
                                            Modifier::empty()
                                        }),
                                ),
                                Span::styled(
                                    display_val,
                                    Style::default()
                                        .fg(if is_sel && *awaiting_key {
                                            Color::LightYellow
                                        } else {
                                            Color::LightGreen
                                        })
                                        .bg(bg),
                                ),
                            ]))
                        })
                        .collect();

                    let cfg_list = List::new(list_items).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::LightGreen))
                            .title(Span::styled(
                                format!(" TUI-Only Config: {cfg_path} (Separated from GUI config.toml) "),
                                Style::default()
                                    .fg(Color::LightGreen)
                                    .add_modifier(Modifier::BOLD),
                            )),
                    );
                    frame.render_widget(cfg_list, parts[1]);
                } else {
                    let doc_md = match *tab {
                        0 => "# HyprPilot TUI — Overview & Modal Architecture\n\n\
                              HyprPilot TUI combines **Neovim-style modal editing** with **Emacs-friendly Ctrl/Alt chords**.\n\n\
                              ## Modes\n\
                              - **INSERT** (`i` / `a` / `o` or startup default): Type prompts directly, `Enter` sends, `Ctrl+Backspace`/`Ctrl+W` deletes previous word, `Ctrl+U` clears line.\n\
                              - **NORMAL** (`Esc` from Insert): Zero-chord navigation (`j`/`k`, `gg`/`G`, `/`, `:`, `p`, `y`, `s`). Press `q` or `Esc` again to hide.\n\
                              - **SEARCH** (`/` in Normal): Incremental transcript search with highlighted matches (`n` next, `N` previous).\n\
                              - **COMMAND** (`:` in Normal): Ex-style command line (`:new`, `:p`, `:layout 20 60 20`, `:config`, `:q`).\n\n\
                              ## Adaptive Window Geometry\n\
                              - **50% Floating Popup** (`Alt+Shift+Space`): Shows **Chat only** for distraction-free speed.\n\
                              - **Fullscreen / Wide** (`width >= 150`): Expands to **20% Recent Chats | 60% Center Chat | 20% Model Specs & Tips** (configurable).",
                        1 => "# Neovim & Emacs Motions, Editing & Search\n\n\
                              ## Scrolling & Jumping (Normal Mode)\n\
                              - `j` / `Down` : Scroll down 2 lines (or move list cursor in sidebars)\n\
                              - `k` / `Up`   : Scroll up 2 lines\n\
                              - `Ctrl+D` / `Ctrl+U` : Half-page down / up (works in both Chat & Preview)\n\
                              - `gg` / `G`   : Jump to top / bottom of conversation or preview\n\n\
                              ## Insert Mode Editing (Emacs / Readline Style)\n\
                              - `Ctrl+W` / `Ctrl+Backspace` / `Alt+Backspace` : Kill previous word\n\
                              - `Ctrl+U`     : Clear prompt input buffer\n\
                              - `Ctrl+P` / `F2` : Switch AI Provider\n\
                              - `Ctrl+M` / `F3` : Switch Model\n\
                              - `Ctrl+G` / `F4` : Browse SQLite Chat History\n\n\
                              ## Incremental Search\n\
                              - `/`          : Enter Search mode and type pattern\n\
                              - `Enter`      : Lock pattern and jump to first match\n\
                              - `n` / `N`    : Jump to next / previous match with `[i/N]` counter",
                        2 => "# 3-Pane Workspace, Artifact Preview & Clean Copy\n\n\
                              ## Switching Panes (Works in Insert & Normal)\n\
                              - `Alt+1` / `Alt+2` / `Alt+3` : Focus Left (History), Center (Chat), or Right (Specs/Preview)\n\
                              - `F6` / `Ctrl+Left` / `Ctrl+Right` : Cycle pane focus\n\
                              - `Ctrl+W h` / `Ctrl+W l` / `Ctrl+W w` : Neovim window navigation\n\
                              - `Ctrl+B`     : Force-toggle 3-pane layout at any width\n\
                              - `Enter`      : In Left pane loads selected chat; in Right pane switches to highlighted model\n\n\
                              ## Live Markdown / HTML / SVG Artifact Preview\n\
                              - Auto-opens on your configured side (`right` by default) when a reply contains ```markdown / ```md / ```html / ```svg.\n\
                              - `p` / `Ctrl+E` : Toggle Artifact Preview manually\n\
                              - `s`            : Swap Preview side (`left` <-> `right`, saved to `tui.toml`)\n\
                              - `[` / `]`      : Cycle between artifacts in current reply; `x` closes preview\n\n\
                              ## Clean Clipboard Copy (Zero TUI Borders)\n\
                              - `Ctrl+Y` / `y` : Open Section Picker (`1-9` quick copy, `a` full reply, `v` Visual Line-Range mode)",
                        _ => "# `:ex` Command-Line Reference\n\n\
                              Press `:` in Normal mode to run any command:\n\n\
                              - `:new` / `:clear`        Start a fresh conversation\n\
                              - `:p` / `:preview`        Toggle live Artifact Preview pane\n\
                              - `:close` / `:pc`         Close Artifact Preview pane\n\
                              - `:side left | right`     Move Artifact Preview side (saved to `tui.toml`)\n\
                              - `:layout <L> <C> <R>`    Set pane percentages (sum=100, e.g. `:layout 20 60 20`)\n\
                              - `:panes`                 Force-toggle 3-pane mode\n\
                              - `:model` / `:provider`   Open Model or Provider selector\n\
                              - `:history` / `:open`     Open History or `.md`/`.html`/`.pdf` file picker\n\
                              - `:copy` / `:yank`        Open Clean Section Copy & Visual Line-Range modal\n\
                              - `:refresh`               Fetch latest models from active provider API\n\
                              - `:config` / `:cfg`       Open TUI-only Configurator (`tui.toml`)\n\
                              - `:edit-config`           View raw `~/.config/hyprpilot/tui.toml`\n\
                              - `:q` / `:quit`           Hide / Exit HyprPilot TUI",
                    };
                    let doc_w = parts[1].width.saturating_sub(4).max(40) as usize;
                    let doc_lines = render_markdown_lines(doc_md, None, doc_w);
                    let doc_view = Paragraph::new(doc_lines)
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_type(BorderType::Rounded)
                                .border_style(Style::default().fg(Color::Rgb(90, 125, 175)))
                                .title(Span::styled(
                                    format!(" {} (j/k:Scroll | c:TUI Config | e:View tui.toml) ", tab_names[*tab]),
                                    Style::default().fg(Color::LightYellow),
                                )),
                        );
                    frame.render_widget(doc_view, parts[1]);
                }

                let help_footer = Paragraph::new(Line::from(vec![
                    key_chip("1-5/Tab", "Switch Tab"),
                    key_chip("j/k", if *tab == 4 { "Select" } else { "Scroll" }),
                    key_chip("Enter", if *tab == 4 { "Edit/Rebind" } else { "—" }),
                    key_chip("e", "View tui.toml"),
                    key_chip("R", "Reset tui.toml"),
                    key_chip("Esc/q", "Close"),
                ]));
                frame.render_widget(help_footer, parts[2]);
            }
            Modal::Provider(st) => {
                self.last_modal_rect = Some(centered_rect(72, 65, frame.area()));
                let items: Vec<ListItem<'_>> = self
                    .snap
                    .providers
                    .iter()
                    .map(|(_, name, model)| {
                        ListItem::new(Line::from(vec![
                            Span::styled(name.clone(), Style::default().fg(Color::LightCyan)),
                            Span::styled(" — ", Style::default().fg(Color::DarkGray)),
                            Span::styled(model.clone(), Style::default().fg(Color::LightYellow)),
                        ]))
                    })
                    .collect();
                render_modal_list(frame, " Select Provider (Enter / Esc) ", items, st);
            }
            Modal::Model(st) => {
                self.last_modal_rect = Some(centered_rect(72, 65, frame.area()));
                let items: Vec<ListItem<'_>> = self
                    .snap
                    .models
                    .iter()
                    .map(|m| {
                        let color = if m.contains("nano")
                            || m.contains("lite")
                            || m.contains("mini")
                            || m.ends_with(":free")
                        {
                            Color::LightGreen
                        } else {
                            Color::White
                        };
                        ListItem::new(Span::styled(m.clone(), Style::default().fg(color)))
                    })
                    .collect();
                render_modal_list(frame, " Select Model (Enter / Esc) ", items, st);
            }
            Modal::History(st) => {
                self.last_modal_rect = Some(centered_rect(72, 65, frame.area()));
                let items: Vec<ListItem<'_>> = self
                    .snap
                    .history
                    .iter()
                    .map(|c| {
                        ListItem::new(Line::from(vec![
                            Span::styled(c.title.clone(), Style::default().fg(Color::White)),
                            Span::styled(
                                format!("  [{}]", c.model),
                                Style::default().fg(Color::LightYellow),
                            ),
                        ]))
                    })
                    .collect();
                render_modal_list(frame, " Chat History (Enter / Esc) ", items, st);
            }
            Modal::MarkdownPicker {
                files,
                list_state,
                path_input,
            } => {
                let area = centered_rect(76, 65, frame.area());
                frame.render_widget(Clear, area);
                let parts = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(3), Constraint::Min(4)])
                    .split(area);
                let path_box = Paragraph::new(format!(" Path: {}█", path_input)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::LightCyan))
                        .title(" Open .md File (Type path or pick below, Enter / Esc) "),
                );
                frame.render_widget(path_box, parts[0]);
                let items: Vec<ListItem<'_>> = if files.is_empty() {
                    vec![ListItem::new(Span::styled(
                        "No .md files in current directory — type a file path above",
                        Style::default().fg(Color::DarkGray),
                    ))]
                } else {
                    files
                        .iter()
                        .map(|f| ListItem::new(Span::styled(f.clone(), Style::default().fg(Color::LightYellow))))
                        .collect()
                };
                let list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::LightMagenta)),
                    )
                    .highlight_style(
                        Style::default()
                            .bg(Color::Rgb(46, 76, 130))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("▶ ");
                frame.render_stateful_widget(list, parts[1], list_state);
            }
            Modal::MarkdownViewer {
                title,
                lines,
                scroll,
            } => {
                let area = centered_rect(92, 88, frame.area());
                frame.render_widget(Clear, area);
                let viewer = Paragraph::new(lines.clone())
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::LightMagenta))
                            .title(Span::styled(
                                title.as_str(),
                                Style::default()
                                    .fg(Color::LightCyan)
                                    .add_modifier(Modifier::BOLD),
                            )),
                    );
                frame.render_widget(viewer, area);
            }
            Modal::CopyPicker {
                sections,
                list_state,
                ..
            } => {
                let area = centered_rect(88, 80, frame.area());
                self.last_modal_rect = Some(area);
                frame.render_widget(Clear, area);
                let parts = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
                    .split(area);

                let items: Vec<ListItem<'_>> = sections
                    .iter()
                    .enumerate()
                    .map(|(i, sec)| {
                        let badge_color = match sec.kind {
                            "CODE" => Color::LightGreen,
                            "HEAD" => Color::LightMagenta,
                            "ALL" => Color::LightCyan,
                            _ => Color::LightYellow,
                        };
                        let num = if i < 9 { format!("[{}] ", i + 1) } else { "    ".into() };
                        ListItem::new(Line::from(vec![
                            Span::styled(num, Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                format!("[{}] ", sec.kind),
                                Style::default().fg(badge_color).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(sec.label.clone(), Style::default().fg(Color::White)),
                        ]))
                    })
                    .collect();

                let list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::LightCyan))
                            .title(Span::styled(
                                " Copy Clean Section (Enter/y: Copy | 1-9: Quick | v: Visual Lines | a: All) ",
                                Style::default()
                                    .fg(Color::LightCyan)
                                    .add_modifier(Modifier::BOLD),
                            )),
                    )
                    .highlight_style(
                        Style::default()
                            .bg(Color::Rgb(46, 76, 130))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("▶ ");
                frame.render_stateful_widget(list, parts[0], list_state);

                let preview_text = list_state
                    .selected()
                    .and_then(|idx| sections.get(idx))
                    .map(|s| s.content.as_str())
                    .unwrap_or("");
                let preview = Paragraph::new(preview_text)
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::Rgb(90, 120, 150)))
                            .title(Span::styled(
                                " Clean Clipboard Preview (Zero TUI Borders) ",
                                Style::default().fg(Color::LightGreen),
                            )),
                    );
                frame.render_widget(preview, parts[1]);
            }
            Modal::CopyLines {
                lines,
                cursor,
                anchor,
                scroll,
            } => {
                let area = centered_rect(90, 84, frame.area());
                frame.render_widget(Clear, area);
                let (sel_lo, sel_hi) = if let Some(a) = *anchor {
                    (a.min(*cursor), a.max(*cursor))
                } else {
                    (*cursor, *cursor)
                };
                let rendered: Vec<Line<'static>> = lines
                    .iter()
                    .enumerate()
                    .map(|(i, text)| {
                        let in_range = i >= sel_lo && i <= sel_hi;
                        let is_cursor = i == *cursor;
                        let bg = if is_cursor {
                            Color::Rgb(58, 92, 148)
                        } else if in_range && anchor.is_some() {
                            Color::Rgb(42, 62, 92)
                        } else {
                            Color::Reset
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{:>3} │ ", i + 1),
                                Style::default().fg(Color::DarkGray).bg(bg),
                            ),
                            Span::styled(
                                text.clone(),
                                Style::default().fg(Color::White).bg(bg),
                            ),
                        ])
                    })
                    .collect();

                let mode_hint = if anchor.is_some() {
                    "VISUAL RANGE — j/k: Extend | Space/v: Clear | y/Enter: Copy Range"
                } else {
                    "CURSOR — j/k: Move | Space/v: Start Range | y/Enter: Copy Line"
                };
                let viewer = Paragraph::new(rendered)
                    .scroll((*scroll, 0))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(Color::LightGreen))
                            .title(Span::styled(
                                format!(" {mode_hint} "),
                                Style::default()
                                    .fg(Color::LightGreen)
                                    .add_modifier(Modifier::BOLD),
                            )),
                    );
                frame.render_widget(viewer, area);
            }
        }
    }
}

fn key_chip(key: &str, label: &str) -> Span<'static> {
    Span::styled(
        format!(" {key}:{label} "),
        Style::default().fg(Color::Rgb(160, 180, 210)),
    )
}

fn wrap_styled_line_preserving_prefix(
    line: Line<'static>,
    prefix_spans_count: usize,
    max_width: usize,
) -> Vec<Line<'static>> {
    let prefix: Vec<Span<'static>> = line.spans.iter().take(prefix_spans_count).cloned().collect();
    let prefix_w: usize = prefix.iter().map(|s| s.content.chars().count()).sum();
    let avail = max_width.saturating_sub(prefix_w).max(12);

    let body: Vec<Span<'static>> = line.spans.into_iter().skip(prefix_spans_count).collect();
    let body_w: usize = body.iter().map(|s| s.content.chars().count()).sum();
    if body_w <= avail {
        let mut all = prefix;
        all.extend(body);
        return vec![Line::from(all)];
    }

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cur_spans: Vec<Span<'static>> = prefix.clone();
    let mut cur_w = 0usize;

    for sp in body {
        let style = sp.style;
        let mut chunk = String::new();
        // Split preserving spaces so words wrap cleanly without breaking style
        for token in sp.content.split_inclusive(char::is_whitespace) {
            let tok_len = token.chars().count();
            if cur_w + tok_len <= avail {
                chunk.push_str(token);
                cur_w += tok_len;
            } else {
                if !chunk.is_empty() {
                    cur_spans.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                if cur_w > 0 && tok_len <= avail {
                    rows.push(Line::from(std::mem::replace(&mut cur_spans, prefix.clone())));
                    cur_w = 0;
                    let trimmed = token.trim_start();
                    let tlen = trimmed.chars().count();
                    chunk.push_str(trimmed);
                    cur_w += tlen;
                } else {
                    // Long unbroken token (e.g., URL or code identifier): char-wrap cleanly
                    for ch in token.chars() {
                        if cur_w >= avail {
                            if !chunk.is_empty() {
                                cur_spans.push(Span::styled(std::mem::take(&mut chunk), style));
                            }
                            rows.push(Line::from(std::mem::replace(&mut cur_spans, prefix.clone())));
                            cur_w = 0;
                        }
                        chunk.push(ch);
                        cur_w += 1;
                    }
                }
            }
        }
        if !chunk.is_empty() {
            cur_spans.push(Span::styled(chunk, style));
        }
    }
    if cur_spans.len() > prefix.len() || rows.is_empty() {
        rows.push(Line::from(cur_spans));
    }
    rows
}

fn strip_transcript_row_prefix(row: &str) -> &str {
    if let Some(rest) = row.strip_prefix(" │ ") {
        rest
    } else if row.starts_with(" ╭─ ") || row.starts_with(" ╰──") {
        ""
    } else {
        row
    }
}

fn extract_pane_selection(
    rows: &[String],
    anchor: (usize, usize),
    cursor: (usize, usize),
) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let ((r0, c0), (r1, c1)) = if anchor <= cursor {
        (anchor, cursor)
    } else {
        (cursor, anchor)
    };
    let r_start = r0.min(rows.len() - 1);
    let r_end = r1.min(rows.len() - 1);

    let mut out = Vec::new();
    for (idx, row) in rows.iter().enumerate().take(r_end + 1).skip(r_start) {
        let chars: Vec<char> = row.chars().collect();
        let lo = if idx == r_start { c0.min(chars.len()) } else { 0 };
        let hi = if idx == r_end {
            c1.saturating_add(1).min(chars.len())
        } else {
            chars.len()
        };
        let slice: String = if lo < hi {
            chars[lo..hi].iter().collect()
        } else {
            String::new()
        };
        let cleaned = if lo == 0 {
            strip_transcript_row_prefix(&slice)
        } else {
            slice.as_str()
        };
        if !cleaned.trim().is_empty() {
            out.push(cleaned.trim_end().to_string());
        }
    }
    out.join("\n")
}

fn render_styled_message(
    out: &mut Vec<Line<'static>>,
    role: &str,
    content: &str,
    model_name: &str,
    is_streaming: bool,
    max_width: usize,
) {
    if role == "user" {
        out.push(Line::from(Span::styled(
            " ╭─ ❯ You ",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )));
        for line in content.lines() {
            let raw = Line::from(vec![
                Span::styled(" │ ", Style::default().fg(Color::Cyan)),
                Span::styled(line.to_string(), Style::default().fg(Color::White)),
            ]);
            out.extend(wrap_styled_line_preserving_prefix(raw, 1, max_width));
        }
        if content.trim().is_empty() {
            out.push(Line::from(Span::styled(" │ ", Style::default().fg(Color::Cyan))));
        }
        out.push(Line::from(Span::styled(
            " ╰──",
            Style::default().fg(Color::Cyan),
        )));
        out.push(Line::from(""));
        return;
    }

    let badge = if is_streaming {
        format!(" ╭─ ✦ HyprPilot ({model_name}) • streaming ")
    } else {
        format!(" ╭─ ✦ HyprPilot ({model_name}) ")
    };
    out.push(Line::from(Span::styled(
        badge,
        Style::default()
            .fg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD),
    )));

    let bar = Span::styled(" │ ", Style::default().fg(Color::Magenta));
    let md_lines = render_markdown_lines(content, Some(bar), max_width.saturating_sub(4).max(30));
    for line in md_lines {
        out.extend(wrap_styled_line_preserving_prefix(line, 1, max_width));
    }

    out.push(Line::from(Span::styled(
        " ╰──",
        Style::default().fg(Color::Magenta),
    )));
    out.push(Line::from(""));
}

fn render_modal_list(
    frame: &mut ratatui::Frame<'_>,
    title: &str,
    items: Vec<ListItem<'_>>,
    state: &mut ListState,
) {
    let area = centered_rect(72, 65, frame.area());
    frame.render_widget(Clear, area);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::LightMagenta))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(46, 76, 130))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, area, state);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn run_oneshot(prompt: String) -> Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let Some(provider) = config.active_provider().cloned() else {
        anyhow::bail!("No provider configured. Run hyprpilot GUI Settings to add an API key.");
    };
    let db = HistoryDb::open_default().ok();
    let conv_id = db.as_ref().and_then(|d| {
        let title = derive_title_from_prompt(&prompt);
        d.create_conversation(&title, &provider.id, &provider.model).ok()
    });
    if let (Some(d), Some(cid)) = (&db, conv_id) {
        let _ = d.add_message(cid, "user", &prompt);
    }

    let rt = Runtime::new()?;
    let client = reqwest::Client::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sys = config.effective_system_prompt();
    let msgs = vec![AiMessage {
        role: "user".into(),
        content: prompt,
    }];
    rt.spawn(stream_chat(client, provider, sys, msgs, tx));

    let mut full = String::new();
    let mut stdout = io::stdout();
    rt.block_on(async {
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Delta(chunk) => {
                    full.push_str(&chunk);
                    let _ = write!(stdout, "{chunk}");
                    let _ = stdout.flush();
                }
                StreamEvent::Done => break,
                StreamEvent::Error(e) => {
                    eprintln!("\n[Error: {e}]");
                    break;
                }
            }
        }
    });
    println!();
    if let (Some(d), Some(cid)) = (&db, conv_id) {
        if !full.is_empty() {
            let _ = d.add_message(cid, "assistant", &full);
        }
    }
    Ok(())
}

fn discover_previewable_files() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let e = ext.to_ascii_lowercase();
                    if matches!(
                        e.as_str(),
                        "md" | "markdown" | "html" | "htm" | "pdf" | "svg" | "txt" | "json" | "toml"
                    ) {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            out.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

fn load_preview_lines_for_tui(path: &std::path::Path, width: usize) -> Result<Vec<Line<'static>>> {
    let kind = detect_preview_kind(path);
    let md_content = match kind {
        PreviewKind::Markdown => fs::read_to_string(path)?,
        PreviewKind::Html => html_to_readable_markdown(&fs::read_to_string(path)?),
        PreviewKind::Pdf => {
            let info = inspect_pdf(path)?;
            let text = extract_pdf_text(path, None)?;
            let title = info.title.unwrap_or_else(|| path.display().to_string());
            format!("# {title}\n\n*PDF — {} page(s)*\n\n---\n\n{text}", info.pages)
        }
        PreviewKind::Image => {
            format!("# Image Preview\n\nPath: `{}`\n\nRun `hyprpilot --preview {}` for GUI or `hyprpilot-tui --preview {}` in kitty for inline graphics.", path.display(), path.display(), path.display())
        }
        PreviewKind::Svg | PreviewKind::CodeOrText => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
            let body = fs::read_to_string(path)?;
            format!("```{ext}\n{body}\n```")
        }
    };
    Ok(render_markdown_lines(&md_content, None, width))
}

fn read_stdin_or_path_markdown(target: &str) -> Result<String> {
    if target == "-" {
        use std::io::Read;
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        return Ok(buf);
    }
    let path = expand_user_path(target);
    let kind = detect_preview_kind(&path);
    match kind {
        PreviewKind::Html => Ok(html_to_readable_markdown(&fs::read_to_string(&path)?)),
        PreviewKind::Pdf => {
            let info = inspect_pdf(&path)?;
            let text = extract_pdf_text(&path, None)?;
            let title = info.title.unwrap_or_else(|| path.display().to_string());
            Ok(format!("# {title}\n\n*PDF — {} page(s)*\n\n---\n\n{text}", info.pages))
        }
        PreviewKind::Markdown => Ok(fs::read_to_string(&path)?),
        PreviewKind::Image => Ok(format!("Image: `{}` (use `--preview` in Kitty or GUI)", path.display())),
        PreviewKind::Svg | PreviewKind::CodeOrText => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
            let body = fs::read_to_string(&path)?;
            Ok(format!("```{ext}\n{body}\n```"))
        }
    }
}

fn render_file_stdout(target: &str) -> Result<()> {
    let content = read_stdin_or_path_markdown(target)?;
    let width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100);
    let rendered = render_markdown_ansi(&content, width);
    print!("{rendered}");
    Ok(())
}

fn preview_file_terminal(target: &str) -> Result<()> {
    let path = expand_user_path(target);
    let kind = detect_preview_kind(&path);
    let is_kitty = std::env::var("TERM").is_ok_and(|t| t.contains("kitty"));
    if is_kitty && kind == PreviewKind::Image {
        let status = Command::new("kitty")
            .args(["+kitten", "icat"])
            .arg(&path)
            .status();
        if status.is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }
    if is_kitty && kind == PreviewKind::Pdf {
        let info = inspect_pdf(&path)?;
        println!("PDF: {} ({} page(s))", path.display(), info.pages);
        let cache = state_dir().join("pdf-preview");
        if let Ok(png) = render_pdf_page_png(&path, 1, 140, &cache) {
            let status = Command::new("kitty")
                .args(["+kitten", "icat"])
                .arg(&png)
                .status();
            if status.is_ok_and(|s| s.success()) {
                return Ok(());
            }
        }
    }
    render_file_stdout(target)
}

fn run_file_viewer(target: &str) -> Result<()> {
    let content = read_stdin_or_path_markdown(target)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let width = terminal.size()?.width.saturating_sub(4) as usize;
    let lines = render_markdown_lines(&content, None, width);
    let mut scroll: u16 = 0;

    loop {
        let max_s = (lines.len() as u16).saturating_sub(10);
        terminal.draw(|frame| {
            let area = frame.area();
            let widget = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::LightMagenta))
                        .title(Span::styled(
                            format!(" {target} (Up/Down/PgUp/PgDn, q/Esc: Exit) "),
                            Style::default()
                                .fg(Color::LightCyan)
                                .add_modifier(Modifier::BOLD),
                        )),
                );
            frame.render_widget(widget, area);
        })?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => break,
                        KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(2),
                        KeyCode::Down | KeyCode::Char('j') => scroll = (scroll + 2).min(max_s),
                        KeyCode::PageUp => scroll = scroll.saturating_sub(15),
                        KeyCode::PageDown => scroll = (scroll + 15).min(max_s),
                        KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                        KeyCode::End | KeyCode::Char('G') => scroll = max_s,
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = args.first() {
        match first.as_str() {
            "--daemon" => return run_daemon(),
            "--quit" => {
                if let Ok(mut s) = UnixStream::connect(tui_socket_path()) {
                    let line = serde_json::to_string(&ClientCmd::QuitDaemon)? + "\n";
                    let _ = s.write_all(line.as_bytes());
                }
                return Ok(());
            }
            "--render" => {
                let target = args.get(1).context("usage: hyprpilot-tui --render <file | ->")?;
                return render_file_stdout(target);
            }
            "--preview" => {
                let target = args.get(1).context("usage: hyprpilot-tui --preview <file>")?;
                return preview_file_terminal(target);
            }
            "--view" => {
                let target = args.get(1).context("usage: hyprpilot-tui --view <file | ->")?;
                return run_file_viewer(target);
            }
            "--help" | "-h" => {
                println!(
                    "HyprPilot TUI — Persistent Background Terminal AI Assistant & Preview Engine\n\n\
                     USAGE:\n  \
                       hyprpilot-tui                   Attach to persistent TUI session (auto-starts daemon)\n  \
                       hyprpilot-tui --render <file>   Render Markdown/HTML/PDF text with ANSI colors to stdout\n  \
                       hyprpilot-tui --preview <file>  Preview file in terminal (Kitty icat for PDF/images)\n  \
                       hyprpilot-tui --view <file>     Open .md/.html/.pdf/.txt in scrollable TUI pager\n  \
                       hyprpilot-tui <file.md>         Render existing .md file to stdout\n  \
                       hyprpilot-tui <prompt...>       Stream a single AI answer to stdout\n  \
                       hyprpilot-tui --daemon          Run background TUI session daemon\n  \
                       hyprpilot-tui --quit            Stop the background TUI daemon\n  \
                       hyprpilot-toggle --tui          Toggle floating TUI window on Hyprland"
                );
                return Ok(());
            }
            path if args.len() == 1
                && (path.ends_with(".md") || path.ends_with(".markdown"))
                && expand_user_path(path).is_file() =>
            {
                return render_file_stdout(path);
            }
            _ => return run_oneshot(args.join(" ")),
        }
    }

    let mut client = TuiClient::connect()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    while !client.should_quit {
        client.poll_server();
        client.tick = client.tick.wrapping_add(1);
        terminal.draw(|f| client.render(f))?;

        if event::poll(Duration::from_millis(45))? {
            match event::read()? {
                Event::Key(key) => {
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        client.handle_key(key.code, key.modifiers);
                    }
                }
                Event::Mouse(me) => client.handle_mouse(me),
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_delete_removes_last_word_cleanly() {
        let mut s = "how to find a directory".to_string();
        while s.ends_with(char::is_whitespace) {
            s.pop();
        }
        while let Some(c) = s.chars().next_back() {
            if c.is_whitespace() {
                break;
            }
            s.pop();
        }
        assert_eq!(s, "how to find a ");
    }

    #[test]
    fn markdown_and_code_render_without_panicking() {
        let mut lines = Vec::new();
        render_styled_message(
            &mut lines,
            "assistant",
            "## Find Dir\nUse `find`:\n```bash\nfind / -type d -name foo # comment\n```",
            "gemini-2.5-flash-lite",
            false,
            90,
        );
        assert!(lines.len() >= 6);
    }

    #[test]
    fn extracts_clean_copy_sections_without_tui_borders() {
        let md = "## Overview\nRun this command:\n```bash\nls -la /tmp\n```\nDone.";
        let secs = extract_copy_sections(md);
        assert_eq!(secs.len(), 4);
        assert_eq!(secs[0].kind, "ALL");
        assert_eq!(secs[1].kind, "HEAD");
        assert_eq!(secs[2].kind, "CODE");
        assert_eq!(secs[2].content, "ls -la /tmp");
        assert!(!secs[2].content.contains("│"));
        assert_eq!(secs[3].kind, "TEXT");
        assert_eq!(secs[3].content, "Done.");
    }

    #[test]
    fn extracts_markdown_artifacts_and_infers_model_specs() {
        let msgs = vec![WireMessage {
            role: "assistant".into(),
            content: "Here is your doc:\n```markdown\n# Plan\n- Step 1\n```\n".into(),
        }];
        assert!(has_rich_md_artifact(&msgs));
        let arts = extract_artifacts(&msgs);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].kind, "markdown");
        assert!(arts[0].content.contains("# Plan"));

        let (ctx, tier, api) = infer_model_specs("gemini-2.5-flash-lite", "Gemini");
        assert_eq!(ctx, "1M tokens");
        assert_eq!(tier, "Fast / Low-Token");
        assert_eq!(api, "Gemini GenerateContent");
    }

    #[test]
    fn wrap_preserves_border_prefix_and_styles() {
        let mut lines = Vec::new();
        render_styled_message(
            &mut lines,
            "user",
            "This is a very long user prompt that must wrap across multiple rows while keeping the cyan vertical border on every single line",
            "gemini",
            false,
            40,
        );
        // Middle lines (skip header, footer, trailing blank) must all start with " │ " in Cyan
        let body_lines = &lines[1..lines.len() - 2];
        assert!(body_lines.len() >= 3);
        for l in body_lines {
            assert_eq!(l.spans[0].content.as_ref(), " │ ");
            assert_eq!(l.spans[0].style.fg, Some(Color::Cyan));
            assert!(l.width() <= 40);
        }
    }

    #[test]
    fn pane_selection_strips_borders_and_clamps_to_chat() {
        let rows = vec![
            " ╭─ ✦ HyprPilot ".to_string(),
            " │ Hello world".to_string(),
            " │ Second line here".to_string(),
            " ╰──".to_string(),
        ];
        let selected = extract_pane_selection(&rows, (0, 0), (3, 99));
        assert_eq!(selected, "Hello world\nSecond line here");
        assert!(!selected.contains("│"));
        assert!(!selected.contains("╭"));
    }
}
