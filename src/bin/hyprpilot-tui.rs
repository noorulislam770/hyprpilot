use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use hyprpilot::storage::config::{AppConfig, AuthMode, state_dir, tui_socket_path};
use hyprpilot::storage::db::{ConversationSummary, HistoryDb, derive_title_from_prompt};

// ---------------------------------------------------------------------------
// Daemon <-> TUI Client Protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireMessage {
    role: String,
    content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionSnapshot {
    provider_id: String,
    provider_name: String,
    model: String,
    linux_quick_mode: bool,
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
    LoadConversation { id: i64 },
    QuitDaemon,
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
        let (provider_id, provider_name, model, models) = match self.config.active_provider() {
            Some(p) => {
                let mut list = p.available_models.clone();
                if list.is_empty() && !p.model.trim().is_empty() {
                    list.push(p.model.trim().to_string());
                } else if !p.model.trim().is_empty() && !list.iter().any(|m| m == p.model.trim()) {
                    list.insert(0, p.model.trim().to_string());
                }
                (p.id.clone(), p.name.clone(), p.model.clone(), list)
            }
            None => (
                String::new(),
                "None".to_string(),
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
            model,
            linux_quick_mode: self.config.linux_quick_mode,
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
    let db = HistoryDb::open_default().ok();
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );
    let http = reqwest::Client::new();

    let state = Arc::new(Mutex::new(DaemonState {
        config,
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

enum Modal {
    None,
    Provider(ListState),
    Model(ListState),
    History(ListState),
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
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags, LeaveAlternateScreen);
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
}

impl TuiClient {
    fn connect() -> Result<Self> {
        let (tx_stream, rx) = connect_or_spawn_daemon()?;
        let empty = SessionSnapshot {
            provider_id: String::new(),
            provider_name: "Connecting...".into(),
            model: "-".into(),
            linux_quick_mode: true,
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
                    self.snap = s;
                    self.local_notice = None;
                    if self.snap.streaming {
                        self.auto_scroll = true;
                    }
                }
                ServerEvent::Delta { text } => {
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

        // Ctrl+Backspace (often sent as Ctrl+H by terminals!) & Ctrl+W delete previous word
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Char('w') => {
                    self.delete_prev_word();
                }
                KeyCode::Char('u') => self.input.clear(),
                KeyCode::Char('q') | KeyCode::Char('c') => self.should_quit = true,
                KeyCode::Char('p') => self.open_provider_modal(),
                KeyCode::Char('m') => self.open_model_modal(),
                KeyCode::Char('g') => self.open_history_modal(),
                KeyCode::Char('o') => self.open_markdown_picker(),
                KeyCode::Char('r') => self.send_cmd(&ClientCmd::FetchModels),
                KeyCode::Char('n') => {
                    self.scroll_offset = 0;
                    self.send_cmd(&ClientCmd::NewChat);
                }
                KeyCode::Char('y') => self.open_copy_modal(),
                KeyCode::Char('l') => self.send_cmd(&ClientCmd::ToggleLinuxQuick),
                _ => {}
            }
            return;
        }

        if mods.contains(KeyModifiers::ALT) {
            match code {
                KeyCode::Char('p') => self.open_provider_modal(),
                KeyCode::Char('m') => self.open_model_modal(),
                KeyCode::Char('h') => self.open_history_modal(),
                KeyCode::Backspace => self.delete_prev_word(),
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::F(2) => self.open_provider_modal(),
            KeyCode::F(3) => self.open_model_modal(),
            KeyCode::F(4) => self.open_history_modal(),
            KeyCode::F(5) => self.send_cmd(&ClientCmd::FetchModels),
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
            | Modal::MarkdownPicker { .. }
            | Modal::MarkdownViewer { .. }
            | Modal::CopyPicker { .. }
            | Modal::CopyLines { .. } => {}
        }
    }

    fn modal_len(&self) -> usize {
        match &self.modal {
            Modal::None | Modal::MarkdownViewer { .. } | Modal::CopyLines { .. } => 0,
            Modal::Provider(_) => self.snap.providers.len(),
            Modal::Model(_) => self.snap.models.len(),
            Modal::History(_) => self.snap.history.len(),
            Modal::MarkdownPicker { files, .. } => files.len(),
            Modal::CopyPicker { sections, .. } => sections.len(),
        }
    }

    fn modal_state_mut(&mut self) -> Option<&mut ListState> {
        match &mut self.modal {
            Modal::None | Modal::MarkdownViewer { .. } | Modal::CopyLines { .. } => None,
            Modal::Provider(s)
            | Modal::Model(s)
            | Modal::History(s)
            | Modal::MarkdownPicker { list_state: s, .. }
            | Modal::CopyPicker { list_state: s, .. } => Some(s),
        }
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(frame.area());

        // Colorful Antigravity-inspired header pills
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][self.tick % 10];
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

        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                " ✦ HyprPilot ",
                Style::default()
                    .bg(Color::Rgb(105, 45, 165))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!(" {} ", self.snap.provider_name),
                Style::default()
                    .bg(Color::Rgb(24, 78, 120))
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!(" {} ", self.snap.model),
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

        // Colorful Chat Transcript
        let inner_width = chunks[1].width.saturating_sub(6).max(30) as usize;
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
                "    Try: \"how to find a directory inside linux\"  |  Ctrl+O: Open .md  |  Ctrl+G: History",
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

        let inner_width = chunks[1].width.saturating_sub(2).max(1) as usize;
        let inner_height = chunks[1].height.saturating_sub(2);
        let total_wrapped_rows: usize = lines
            .iter()
            .map(|l| (l.width().max(1) + inner_width - 1) / inner_width)
            .sum();
        let max_scroll = (total_wrapped_rows as u16).saturating_sub(inner_height);
        if self.auto_scroll {
            self.scroll_offset = max_scroll;
        } else {
            self.scroll_offset = self.scroll_offset.min(max_scroll);
        }

        let transcript = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_offset, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Rgb(70, 120, 165)))
                    .title(Span::styled(
                        " Conversation ",
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            );
        frame.render_widget(transcript, chunks[1]);

        // Input Box
        let input_box = Paragraph::new(Line::from(vec![
            Span::styled(
                " ❯ ",
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.input.clone(), Style::default().fg(Color::White)),
            Span::styled("█", Style::default().fg(Color::LightCyan)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if self.snap.streaming {
                    Color::Yellow
                } else {
                    Color::LightCyan
                }))
                .title(Span::styled(
                    " Ask (Enter: Send | Ctrl+Bksp: Del Word) ",
                    Style::default().fg(Color::LightBlue),
                )),
        );
        frame.render_widget(input_box, chunks[2]);

        // Colorful Shortcut Footer
        let footer = Paragraph::new(Line::from(vec![
            key_chip("F2/Ctrl+P", "Provider"),
            key_chip("F3/Ctrl+M", "Model"),
            key_chip("Ctrl+O", "Open.md"),
            key_chip("F4/Ctrl+G", "History"),
            key_chip("Ctrl+N", "New"),
            key_chip("Ctrl+Y", "Copy Section"),
            key_chip("Esc", "Hide"),
        ]));
        frame.render_widget(footer, chunks[3]);

        // Modals
        match &mut self.modal {
            Modal::None => {}
            Modal::Provider(st) => {
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
            out.push(Line::from(vec![
                Span::styled(" │ ", Style::default().fg(Color::Cyan)),
                Span::styled(line.to_string(), Style::default().fg(Color::White)),
            ]));
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
    out.extend(render_markdown_lines(content, Some(bar), max_width));

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
    execute!(stdout, EnterAlternateScreen)?;
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
            if let Event::Key(key) = event::read()? {
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    client.handle_key(key.code, key.modifiers);
                }
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
}
