use anyhow::Result;
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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};

use hyprpilot::ai::models::{FetchedModels, fetch_available_models};
use hyprpilot::ai::{AiMessage, StreamEvent, stream_chat};
use hyprpilot::storage::config::{AppConfig, AuthMode};
use hyprpilot::storage::db::{ConversationSummary, HistoryDb, derive_title_from_prompt};

enum Modal {
    None,
    Provider(ListState),
    Model(ListState),
    History {
        items: Vec<ConversationSummary>,
        list_state: ListState,
    },
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

struct TuiApp {
    config: AppConfig,
    db: Option<HistoryDb>,
    runtime: Runtime,
    http: reqwest::Client,
    active_conversation_id: Option<i64>,
    messages: Vec<AiMessage>,
    input: String,
    streaming_reply: String,
    streaming: bool,
    stream_rx: Option<mpsc::UnboundedReceiver<StreamEvent>>,
    fetch_rx: Option<(String, oneshot::Receiver<Result<FetchedModels>>)>,
    status: String,
    scroll_offset: u16,
    auto_scroll: bool,
    modal: Modal,
    should_quit: bool,
}

impl TuiApp {
    fn new() -> Result<Self> {
        let config = AppConfig::load().unwrap_or_default();
        let db = HistoryDb::open_default().ok();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let mut app = Self {
            config,
            db,
            runtime,
            http: reqwest::Client::new(),
            active_conversation_id: None,
            messages: Vec::new(),
            input: String::new(),
            streaming_reply: String::new(),
            streaming: false,
            stream_rx: None,
            fetch_rx: None,
            status: "Ready — ask anything or press Ctrl+P / Ctrl+M to switch model".to_string(),
            scroll_offset: 0,
            auto_scroll: true,
            modal: Modal::None,
            should_quit: false,
        };
        app.maybe_auto_fetch_models();
        Ok(app)
    }

    fn maybe_auto_fetch_models(&mut self) {
        let should = self.config.active_provider().is_some_and(|p| {
            p.available_models.is_empty()
                && (!p.token.trim().is_empty() || p.auth_mode == AuthMode::None)
        });
        if should {
            self.trigger_fetch_models();
        }
    }

    fn trigger_fetch_models(&mut self) {
        if self.fetch_rx.is_some() {
            return;
        }
        let Some(provider) = self.config.active_provider().cloned() else {
            return;
        };
        let target_id = provider.id.clone();
        self.status = format!("Fetching models for {}...", provider.name);
        let (tx, rx) = oneshot::channel();
        let client = self.http.clone();
        self.runtime.spawn(async move {
            let res = fetch_available_models(&client, &provider).await;
            let _ = tx.send(res);
        });
        self.fetch_rx = Some((target_id, rx));
    }

    fn poll_async(&mut self) {
        if let Some((target_id, mut rx)) = self.fetch_rx.take() {
            match rx.try_recv() {
                Ok(Ok(fetched)) => {
                    let count = fetched.models.len();
                    let chosen = fetched.recommended.clone().unwrap_or_default();
                    if let Some(p) = self
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
                    let _ = self.config.save();
                    self.status = format!("Fetched {count} models — selected {chosen}");
                }
                Ok(Err(e)) => {
                    self.status = format!("Model fetch failed: {e:#}");
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.fetch_rx = Some((target_id, rx));
                }
                Err(_) => {}
            }
        }

        let mut finished = false;
        let mut error_text: Option<String> = None;

        if let Some(rx) = &mut self.stream_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    StreamEvent::Delta(chunk) => {
                        self.streaming_reply.push_str(&chunk);
                        self.auto_scroll = true;
                    }
                    StreamEvent::Done => {
                        finished = true;
                        break;
                    }
                    StreamEvent::Error(e) => {
                        error_text = Some(e);
                        finished = true;
                        break;
                    }
                }
            }
        }

        if finished {
            self.stream_rx = None;
            self.streaming = false;
            if let Some(err) = error_text {
                let msg = if self.streaming_reply.is_empty() {
                    format!("[Error: {err}]")
                } else {
                    format!("{}\n\n[Error: {err}]", self.streaming_reply)
                };
                self.messages.push(AiMessage {
                    role: "assistant".into(),
                    content: msg,
                });
                self.status = "Request failed".to_string();
            } else {
                let reply = std::mem::take(&mut self.streaming_reply);
                if let (Some(db), Some(cid)) = (&self.db, self.active_conversation_id) {
                    let _ = db.add_message(cid, "assistant", &reply);
                }
                self.messages.push(AiMessage {
                    role: "assistant".into(),
                    content: reply,
                });
                self.status = "Done (Ctrl+Y to copy last reply)".to_string();
            }
            self.streaming_reply.clear();
            self.auto_scroll = true;
        }
    }

    fn send_prompt(&mut self) {
        if self.streaming {
            return;
        }
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let Some(provider) = self.config.active_provider().cloned() else {
            self.status = "No provider configured".to_string();
            return;
        };

        self.input.clear();
        let conv_id = if let Some(cid) = self.active_conversation_id {
            if let Some(db) = &self.db {
                let _ = db.update_conversation_meta(cid, None, &provider.id, &provider.model);
            }
            Some(cid)
        } else if let Some(db) = &self.db {
            let title = derive_title_from_prompt(&prompt);
            match db.create_conversation(&title, &provider.id, &provider.model) {
                Ok(cid) => {
                    self.active_conversation_id = Some(cid);
                    Some(cid)
                }
                Err(_) => None,
            }
        } else {
            None
        };

        if let (Some(db), Some(cid)) = (&self.db, conv_id) {
            let _ = db.add_message(cid, "user", &prompt);
        }

        self.messages.push(AiMessage {
            role: "user".into(),
            content: prompt,
        });
        self.streaming = true;
        self.streaming_reply.clear();
        self.auto_scroll = true;
        self.status = format!("Streaming from {} ({})...", provider.name, provider.model);

        let (tx, rx) = mpsc::unbounded_channel();
        self.stream_rx = Some(rx);
        let client = self.http.clone();
        let sys = self.config.effective_system_prompt();
        let history = self.messages.clone();
        self.runtime
            .spawn(stream_chat(client, provider, sys, history, tx));
    }

    fn new_chat(&mut self) {
        if self.streaming {
            return;
        }
        self.active_conversation_id = None;
        self.messages.clear();
        self.streaming_reply.clear();
        self.scroll_offset = 0;
        self.status = "Started new chat".to_string();
    }

    fn copy_last_reply(&mut self) {
        let Some(last) = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| m.content.as_str())
        else {
            self.status = "No assistant reply to copy".to_string();
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
            self.status = "Copied last reply to clipboard (wl-copy)".to_string();
        } else {
            self.status = "wl-copy not available".to_string();
        }
    }

    fn open_provider_modal(&mut self) {
        let idx = self
            .config
            .providers
            .iter()
            .position(|p| p.id == self.config.active_provider_id)
            .unwrap_or(0);
        let mut state = ListState::default();
        state.select(Some(idx));
        self.modal = Modal::Provider(state);
    }

    fn open_model_modal(&mut self) {
        let models = self.current_model_list();
        let active = self
            .config
            .active_provider()
            .map(|p| p.model.as_str())
            .unwrap_or("");
        let idx = models.iter().position(|m| m == active).unwrap_or(0);
        let mut state = ListState::default();
        if !models.is_empty() {
            state.select(Some(idx));
        }
        self.modal = Modal::Model(state);
    }

    fn open_history_modal(&mut self) {
        let items = self
            .db
            .as_ref()
            .and_then(|db| db.list_conversations("").ok())
            .unwrap_or_default();
        let mut list_state = ListState::default();
        if !items.is_empty() {
            list_state.select(Some(0));
        }
        self.modal = Modal::History { items, list_state };
    }

    fn current_model_list(&self) -> Vec<String> {
        let Some(p) = self.config.active_provider() else {
            return Vec::new();
        };
        let mut list = p.available_models.clone();
        if list.is_empty() && !p.model.trim().is_empty() {
            list.push(p.model.trim().to_string());
        } else if !p.model.trim().is_empty() && !list.iter().any(|m| m == p.model.trim()) {
            list.insert(0, p.model.trim().to_string());
        }
        list
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if !matches!(self.modal, Modal::None) {
            self.handle_modal_key(code);
            return;
        }

        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('q') | KeyCode::Char('c') => self.should_quit = true,
                KeyCode::Char('p') => self.open_provider_modal(),
                KeyCode::Char('m') => self.open_model_modal(),
                KeyCode::Char('r') => self.trigger_fetch_models(),
                KeyCode::Char('h') => self.open_history_modal(),
                KeyCode::Char('n') => self.new_chat(),
                KeyCode::Char('y') => self.copy_last_reply(),
                KeyCode::Char('l') => {
                    self.config.linux_quick_mode = !self.config.linux_quick_mode;
                    let _ = self.config.save();
                    self.status = format!(
                        "Linux Quick Mode: {}",
                        if self.config.linux_quick_mode { "ON" } else { "OFF" }
                    );
                }
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Enter => self.send_prompt(),
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
                        let next = st
                            .selected()
                            .map(|i| (i + 1).min(len - 1))
                            .unwrap_or(0);
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
                    if let Some(p) = self.config.providers.get(idx) {
                        self.config.active_provider_id = p.id.clone();
                        let label = format!("{} ({})", p.name, p.model);
                        let _ = self.config.save();
                        self.status = format!("Switched to {label}");
                        self.maybe_auto_fetch_models();
                    }
                }
            }
            Modal::Model(st) => {
                let models = self.current_model_list();
                if let Some(idx) = st.selected() {
                    if let Some(m) = models.get(idx).cloned() {
                        let active_id = self.config.active_provider_id.clone();
                        if let Some(p) = self
                            .config
                            .providers
                            .iter_mut()
                            .find(|p| p.id == active_id)
                        {
                            p.model = m.clone();
                        }
                        let _ = self.config.save();
                        self.status = format!("Selected model: {m}");
                    }
                }
            }
            Modal::History { items, list_state } => {
                if let Some(idx) = list_state.selected() {
                    if let Some(conv) = items.get(idx) {
                        if let Some(db) = &self.db {
                            let loaded = db.get_messages(conv.id).unwrap_or_default();
                            self.active_conversation_id = Some(conv.id);
                            self.messages = loaded
                                .into_iter()
                                .map(|m| AiMessage {
                                    role: m.role,
                                    content: m.content,
                                })
                                .collect();
                            self.auto_scroll = true;
                            self.status = format!("Loaded chat: {}", conv.title);
                        }
                    }
                }
            }
            Modal::None => {}
        }
    }

    fn modal_len(&self) -> usize {
        match &self.modal {
            Modal::None => 0,
            Modal::Provider(_) => self.config.providers.len(),
            Modal::Model(_) => self.current_model_list().len(),
            Modal::History { items, .. } => items.len(),
        }
    }

    fn modal_state_mut(&mut self) -> Option<&mut ListState> {
        match &mut self.modal {
            Modal::None => None,
            Modal::Provider(s) | Modal::Model(s) | Modal::History { list_state: s, .. } => Some(s),
        }
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(frame.area());

        // Header
        let (prov_name, model_name) = self
            .config
            .active_provider()
            .map(|p| (p.name.as_str(), p.model.as_str()))
            .unwrap_or(("None", "-"));
        let linux_tag = if self.config.linux_quick_mode {
            "ON"
        } else {
            "OFF"
        };
        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                " HyprPilot TUI ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" | "),
            Span::styled(prov_name, Style::default().fg(Color::Green)),
            Span::raw(" : "),
            Span::styled(model_name, Style::default().fg(Color::Yellow)),
            Span::raw(format!(" | Linux Quick: {linux_tag}")),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(self.status.as_str()),
        );
        frame.render_widget(header, chunks[0]);

        // Chat transcript
        let mut lines: Vec<Line<'_>> = Vec::new();
        if self.messages.is_empty() && !self.streaming {
            lines.push(Line::from(Span::styled(
                "Ask anything using your last-used model. Example: how to find a directory inside linux",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for m in &self.messages {
                let (label, color) = if m.role == "user" {
                    ("You", Color::Cyan)
                } else {
                    ("HyprPilot", Color::Green)
                };
                lines.push(Line::from(Span::styled(
                    format!("{label}:"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )));
                for line in m.content.lines() {
                    lines.push(Line::from(format!("  {line}")));
                }
                lines.push(Line::from(""));
            }
            if self.streaming {
                lines.push(Line::from(Span::styled(
                    "HyprPilot:",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                let text = if self.streaming_reply.is_empty() {
                    "Thinking..."
                } else {
                    self.streaming_reply.as_str()
                };
                for line in text.lines() {
                    lines.push(Line::from(format!("  {line}")));
                }
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
            .block(Block::default().borders(Borders::ALL).title("Chat"));
        frame.render_widget(transcript, chunks[1]);

        // Input box
        let input_box = Paragraph::new(format!("> {}█", self.input)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("Ask (Enter to send)"),
        );
        frame.render_widget(input_box, chunks[2]);

        // Footer
        let footer = Paragraph::new(
            "Enter: Send | Ctrl+P: Provider | Ctrl+M: Model | Ctrl+R: Fetch Models | Ctrl+H: History | Ctrl+Y: Copy | Ctrl+N: New | Esc: Close",
        )
        .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, chunks[3]);

        // Modals
        let model_items = if matches!(self.modal, Modal::Model(_)) {
            self.current_model_list()
        } else {
            Vec::new()
        };
        match &mut self.modal {
            Modal::None => {}
            Modal::Provider(st) => {
                let items: Vec<ListItem<'_>> = self
                    .config
                    .providers
                    .iter()
                    .map(|p| ListItem::new(format!("{} — {}", p.name, p.model)))
                    .collect();
                render_modal_list(frame, "Select Provider (Enter / Esc)", items, st);
            }
            Modal::Model(st) => {
                let items: Vec<ListItem<'_>> =
                    model_items.into_iter().map(ListItem::new).collect();
                render_modal_list(frame, "Select Model (Enter / Esc)", items, st);
            }
            Modal::History { items, list_state } => {
                let rows: Vec<ListItem<'_>> = items
                    .iter()
                    .map(|c| ListItem::new(format!("{} [{}]", c.title, c.model)))
                    .collect();
                render_modal_list(frame, "Chat History (Enter / Esc)", rows, list_state);
            }
        }
    }
}

fn render_modal_list(
    frame: &mut ratatui::Frame<'_>,
    title: &str,
    items: Vec<ListItem<'_>>,
    state: &mut ListState,
) {
    let area = centered_rect(70, 65, frame.area());
    frame.render_widget(Clear, area);
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
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
        if first == "--help" || first == "-h" {
            println!(
                "HyprPilot TUI — Lightweight Terminal AI Assistant\n\n\
                 USAGE:\n  \
                   hyprpilot-tui                 Launch interactive TUI using last-used model\n  \
                   hyprpilot-tui <prompt...>     Stream a single answer to stdout\n  \
                   hyprpilot-toggle --tui        Toggle floating TUI terminal on Hyprland"
            );
            return Ok(());
        }
        return run_oneshot(args.join(" "));
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = TuiApp::new()?;

    while !app.should_quit {
        app.poll_async();
        terminal.draw(|f| app.render(f))?;

        if event::poll(Duration::from_millis(40))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code, key.modifiers);
                }
            }
        }
    }
    Ok(())
}
