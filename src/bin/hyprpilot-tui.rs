use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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
use hyprpilot::storage::config::{AppConfig, AuthMode, tui_socket_path};
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

enum Modal {
    None,
    Provider(ListState),
    Model(ListState),
    History(ListState),
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
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

    fn copy_last_reply(&mut self) {
        let Some(last) = self
            .snap
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| m.content.as_str())
        else {
            self.local_notice = Some("No assistant reply to copy yet".into());
            return;
        };
        if let Ok(mut child) = Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(last.as_bytes());
            }
            self.local_notice = Some("Copied last reply to clipboard".into());
        } else {
            self.local_notice = Some("wl-copy not available".into());
        }
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
                KeyCode::Char('r') => self.send_cmd(&ClientCmd::FetchModels),
                KeyCode::Char('n') => {
                    self.scroll_offset = 0;
                    self.send_cmd(&ClientCmd::NewChat);
                }
                KeyCode::Char('y') => self.copy_last_reply(),
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

    fn handle_modal_key(&mut self, code: KeyCode) {
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
            Modal::None => {}
        }
    }

    fn modal_len(&self) -> usize {
        match &self.modal {
            Modal::None => 0,
            Modal::Provider(_) => self.snap.providers.len(),
            Modal::Model(_) => self.snap.models.len(),
            Modal::History(_) => self.snap.history.len(),
        }
    }

    fn modal_state_mut(&mut self) -> Option<&mut ListState> {
        match &mut self.modal {
            Modal::None => None,
            Modal::Provider(s) | Modal::Model(s) | Modal::History(s) => Some(s),
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
                "    Try: \"how to find a directory inside linux\"   (or Ctrl+G for past chats)",
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
                );
            }
            if self.snap.streaming {
                let text = if self.snap.streaming_reply.is_empty() {
                    format!("{spinner} Thinking...")
                } else {
                    self.snap.streaming_reply.clone()
                };
                render_styled_message(&mut lines, "assistant", &text, &self.snap.model, true);
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
            key_chip("Ctrl+P", "Provider"),
            key_chip("Ctrl+M", "Model"),
            key_chip("Ctrl+R", "Fetch"),
            key_chip("Ctrl+G/F4", "History"),
            key_chip("Ctrl+N", "New"),
            key_chip("Ctrl+Y", "Copy"),
            key_chip("Ctrl+L", "Linux"),
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
) {
    if role == "user" {
        out.push(Line::from(vec![
            Span::styled(
                " ╭─ ❯ You ",
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
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

    let mut in_code = false;
    for raw_line in content.lines() {
        let trimmed = raw_line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if !in_code {
                in_code = true;
                let code_lang = rest.trim();
                let lang_tag = if code_lang.is_empty() { "code" } else { code_lang };
                out.push(Line::from(vec![
                    Span::styled(" │ ", Style::default().fg(Color::Magenta)),
                    Span::styled(
                        format!("┌─ {lang_tag} ────────────"),
                        Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            } else {
                in_code = false;
                out.push(Line::from(vec![
                    Span::styled(" │ ", Style::default().fg(Color::Magenta)),
                    Span::styled("└────────────────────", Style::default().fg(Color::LightBlue)),
                ]));
            }
            continue;
        }

        if in_code {
            out.push(highlight_code_line(raw_line));
        } else {
            out.push(highlight_markdown_line(raw_line));
        }
    }

    if in_code {
        out.push(Line::from(vec![
            Span::styled(" │ ", Style::default().fg(Color::Magenta)),
            Span::styled("└────────────────────", Style::default().fg(Color::LightBlue)),
        ]));
    }

    out.push(Line::from(Span::styled(
        " ╰──",
        Style::default().fg(Color::Magenta),
    )));
    out.push(Line::from(""));
}

fn highlight_code_line(line: &str) -> Line<'static> {
    let mut spans = vec![
        Span::styled(" │ ", Style::default().fg(Color::Magenta)),
        Span::styled("│ ", Style::default().fg(Color::LightBlue)),
    ];
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with("//") {
        spans.push(Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    } else {
        // Simple token coloring for shell/code readability
        for (idx, tok) in line.split_inclusive(' ').enumerate() {
            let color = if idx == 0 && !tok.trim().starts_with('-') {
                Color::LightGreen
            } else if tok.trim_start().starts_with('-') {
                Color::LightCyan
            } else if tok.contains('"') || tok.contains('\'') {
                Color::LightYellow
            } else {
                Color::Rgb(220, 230, 245)
            };
            spans.push(Span::styled(tok.to_string(), Style::default().fg(color)));
        }
    }
    Line::from(spans)
}

fn highlight_markdown_line(line: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(" │ ", Style::default().fg(Color::Magenta))];
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        spans.push(Span::styled(
            trimmed.trim_start_matches('#').trim().to_string(),
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    let body = if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* "))
    {
        spans.push(Span::styled("• ", Style::default().fg(Color::LightMagenta)));
        rest
    } else {
        line
    };

    // Highlight inline `code` and **bold** spans
    let mut i = 0;
    let bytes = body.as_bytes();
    let mut normal_start = 0;
    while i < bytes.len() {
        if body[i..].starts_with('`') {
            if let Some(end) = body[i + 1..].find('`') {
                if i > normal_start {
                    spans.push(Span::styled(
                        body[normal_start..i].to_string(),
                        Style::default().fg(Color::Rgb(230, 235, 245)),
                    ));
                }
                let code_txt = &body[i + 1..i + 1 + end];
                spans.push(Span::styled(
                    format!(" {code_txt} "),
                    Style::default()
                        .bg(Color::Rgb(42, 48, 62))
                        .fg(Color::LightYellow),
                ));
                i = i + 1 + end + 1;
                normal_start = i;
                continue;
            }
        } else if body[i..].starts_with("**") {
            if let Some(end) = body[i + 2..].find("**") {
                if i > normal_start {
                    spans.push(Span::styled(
                        body[normal_start..i].to_string(),
                        Style::default().fg(Color::Rgb(230, 235, 245)),
                    ));
                }
                spans.push(Span::styled(
                    body[i + 2..i + 2 + end].to_string(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                i = i + 2 + end + 2;
                normal_start = i;
                continue;
            }
        }
        i += body[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    }
    if normal_start < body.len() {
        spans.push(Span::styled(
            body[normal_start..].to_string(),
            Style::default().fg(Color::Rgb(230, 235, 245)),
        ));
    }
    Line::from(spans)
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
            "--help" | "-h" => {
                println!(
                    "HyprPilot TUI — Persistent Background Terminal AI Assistant\n\n\
                     USAGE:\n  \
                       hyprpilot-tui                 Attach to persistent TUI session (auto-starts daemon)\n  \
                       hyprpilot-tui --daemon        Run background TUI session daemon\n  \
                       hyprpilot-tui --quit          Stop the background TUI daemon\n  \
                       hyprpilot-tui <prompt...>     Stream a single answer to stdout\n  \
                       hyprpilot-toggle --tui        Toggle floating TUI window on Hyprland"
                );
                return Ok(());
            }
            _ => return run_oneshot(args.join(" ")),
        }
    }

    let mut client = TuiClient::connect()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    while !client.should_quit {
        client.poll_server();
        client.tick = client.tick.wrapping_add(1);
        terminal.draw(|f| client.render(f))?;

        if event::poll(Duration::from_millis(45))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
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
        );
        assert!(lines.len() >= 6);
    }
}
