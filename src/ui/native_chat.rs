use gtk4::gdk;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use super::copy_to_clipboard;
use crate::ai::{AiMessage, StreamEvent, stream_chat};
use crate::storage::config::AppConfig;
use crate::storage::db::{HistoryDb, derive_title_from_prompt};

#[derive(Clone)]
pub struct NativeChatPane {
    pub root: gtk4::Box,
    state: Rc<RefCell<ChatState>>,
    config: Rc<RefCell<AppConfig>>,
    runtime: Rc<Runtime>,
    http: reqwest::Client,
}

struct ChatState {
    db: Option<HistoryDb>,
    active_conversation_id: Option<i64>,
    messages: Vec<AiMessage>,
    streaming: bool,
    syncing: bool,
    sidebar_box: gtk4::Box,
    history_list: gtk4::ListBox,
    search_entry: gtk4::SearchEntry,
    msg_box: gtk4::Box,
    scrolled: gtk4::ScrolledWindow,
    composer: gtk4::TextView,
    send_btn: gtk4::Button,
    provider_dropdown: gtk4::DropDown,
    provider_ids: Vec<String>,
    linux_btn: gtk4::ToggleButton,
    status_lbl: gtk4::Label,
}

impl NativeChatPane {
    pub fn new(config: Rc<RefCell<AppConfig>>, runtime: Rc<Runtime>) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.set_vexpand(true);
        root.set_hexpand(true);

        let db = HistoryDb::open_default().ok();

        // Sidebar: history & search
        let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        sidebar_box.add_css_class("history-sidebar");
        sidebar_box.set_size_request(230, -1);
        sidebar_box.set_margin_top(6);
        sidebar_box.set_margin_bottom(6);
        sidebar_box.set_margin_start(6);
        sidebar_box.set_margin_end(6);

        let new_chat_btn = gtk4::Button::with_label("+ New Chat");
        new_chat_btn.add_css_class("suggested-action");

        let search_entry = gtk4::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search chats..."));

        let history_list = gtk4::ListBox::new();
        history_list.set_selection_mode(gtk4::SelectionMode::Single);
        history_list.add_css_class("navigation-sidebar");

        let hist_scroll = gtk4::ScrolledWindow::new();
        hist_scroll.set_vexpand(true);
        hist_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        hist_scroll.set_child(Some(&history_list));

        sidebar_box.append(&new_chat_btn);
        sidebar_box.append(&search_entry);
        sidebar_box.append(&hist_scroll);

        // Main chat column
        let chat_col = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        chat_col.set_hexpand(true);
        chat_col.set_vexpand(true);
        chat_col.set_margin_top(6);
        chat_col.set_margin_bottom(8);
        chat_col.set_margin_start(10);
        chat_col.set_margin_end(10);

        // Top toolbar
        let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let hist_toggle = gtk4::ToggleButton::new();
        hist_toggle.set_icon_name("sidebar-show-symbolic");
        hist_toggle.set_tooltip_text(Some("Toggle History Sidebar"));
        hist_toggle.set_active(false);
        sidebar_box.set_visible(false);

        let provider_dropdown = gtk4::DropDown::from_strings(&[]);
        provider_dropdown.set_hexpand(true);

        let linux_btn = gtk4::ToggleButton::with_label("Linux Quick");
        linux_btn.set_tooltip_text(Some("Concise Arch/Linux CLI answers"));
        linux_btn.set_active(config.borrow().linux_quick_mode);

        let clear_btn = gtk4::Button::from_icon_name("edit-clear-all-symbolic");
        clear_btn.set_tooltip_text(Some("New Chat (Ctrl+N)"));
        clear_btn.add_css_class("flat");

        toolbar.append(&hist_toggle);
        toolbar.append(&provider_dropdown);
        toolbar.append(&linux_btn);
        toolbar.append(&clear_btn);

        // Messages scroll area
        let msg_box = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        msg_box.set_margin_top(6);
        msg_box.set_margin_bottom(6);
        msg_box.set_margin_start(4);
        msg_box.set_margin_end(4);

        let scrolled = gtk4::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_hexpand(true);
        scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scrolled.set_child(Some(&msg_box));

        let status_lbl = gtk4::Label::new(None);
        status_lbl.add_css_class("dim-label");
        status_lbl.set_halign(gtk4::Align::Start);
        status_lbl.set_visible(false);

        // Composer
        let composer_frame = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        composer_frame.add_css_class("composer-frame");

        let composer = gtk4::TextView::new();
        composer.add_css_class("composer-textview");
        composer.set_wrap_mode(gtk4::WrapMode::WordChar);
        composer.set_hexpand(true);
        composer.set_accepts_tab(false);

        let comp_scroll = gtk4::ScrolledWindow::new();
        comp_scroll.set_hexpand(true);
        comp_scroll.set_min_content_height(38);
        comp_scroll.set_max_content_height(130);
        comp_scroll.set_propagate_natural_height(true);
        comp_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        comp_scroll.set_child(Some(&composer));

        let send_btn = gtk4::Button::with_label("Ask");
        send_btn.add_css_class("suggested-action");
        send_btn.set_valign(gtk4::Align::End);

        composer_frame.append(&comp_scroll);
        composer_frame.append(&send_btn);

        chat_col.append(&toolbar);
        chat_col.append(&scrolled);
        chat_col.append(&status_lbl);
        chat_col.append(&composer_frame);

        root.append(&sidebar_box);
        root.append(&chat_col);

        let state = Rc::new(RefCell::new(ChatState {
            db,
            active_conversation_id: None,
            messages: Vec::new(),
            streaming: false,
            syncing: false,
            sidebar_box: sidebar_box.clone(),
            history_list: history_list.clone(),
            search_entry: search_entry.clone(),
            msg_box: msg_box.clone(),
            scrolled: scrolled.clone(),
            composer: composer.clone(),
            send_btn: send_btn.clone(),
            provider_dropdown: provider_dropdown.clone(),
            provider_ids: Vec::new(),
            linux_btn: linux_btn.clone(),
            status_lbl: status_lbl.clone(),
        }));

        let pane = Self {
            root,
            state,
            config,
            runtime,
            http: reqwest::Client::new(),
        };

        pane.sync_providers_from_config();
        pane.render_welcome();
        pane.refresh_history();
        pane.wire_signals(hist_toggle, new_chat_btn, clear_btn, search_entry, send_btn, composer, linux_btn, provider_dropdown);

        pane
    }

    fn wire_signals(
        &self,
        hist_toggle: gtk4::ToggleButton,
        new_chat_btn: gtk4::Button,
        clear_btn: gtk4::Button,
        search_entry: gtk4::SearchEntry,
        send_btn: gtk4::Button,
        composer: gtk4::TextView,
        linux_btn: gtk4::ToggleButton,
        provider_dropdown: gtk4::DropDown,
    ) {
        {
            let st = self.state.clone();
            hist_toggle.connect_toggled(move |b| {
                st.borrow().sidebar_box.set_visible(b.is_active());
            });
        }

        {
            let pane = self.clone();
            new_chat_btn.connect_clicked(move |_| pane.new_chat());
        }
        {
            let pane = self.clone();
            clear_btn.connect_clicked(move |_| pane.new_chat());
        }

        {
            let pane = self.clone();
            search_entry.connect_search_changed(move |_| pane.refresh_history());
        }

        {
            let st = self.state.clone();
            let cfg = self.config.clone();
            linux_btn.connect_toggled(move |b| {
                if st.borrow().syncing {
                    return;
                }
                let mut c = cfg.borrow_mut();
                c.linux_quick_mode = b.is_active();
                let _ = c.save();
            });
        }

        {
            let st = self.state.clone();
            let cfg = self.config.clone();
            provider_dropdown.connect_selected_notify(move |dd| {
                if st.borrow().syncing {
                    return;
                }
                let idx = dd.selected() as usize;
                if let Some(id) = st.borrow().provider_ids.get(idx).cloned() {
                    let mut c = cfg.borrow_mut();
                    c.active_provider_id = id;
                    let _ = c.save();
                }
            });
        }

        {
            let pane = self.clone();
            send_btn.connect_clicked(move |_| pane.send_current_prompt());
        }

        // Enter to send, Shift+Enter for newline
        let key_ctrl = gtk4::EventControllerKey::new();
        let pane = self.clone();
        key_ctrl.connect_key_pressed(move |_, key, _, mods| {
            if (key == gdk::Key::Return || key == gdk::Key::KP_Enter)
                && !mods.contains(gdk::ModifierType::SHIFT_MASK)
            {
                pane.send_current_prompt();
                return gtk4::glib::Propagation::Stop;
            }
            gtk4::glib::Propagation::Proceed
        });
        composer.add_controller(key_ctrl);
    }

    pub fn focus_input(&self) {
        self.state.borrow().composer.grab_focus();
    }

    pub fn sync_providers_from_config(&self) {
        let (labels, ids, active_idx, linux_quick) = {
            let cfg = self.config.borrow();
            let labels: Vec<String> = cfg
                .providers
                .iter()
                .map(|p| format!("{} — {}", p.name, p.model))
                .collect();
            let ids: Vec<String> = cfg.providers.iter().map(|p| p.id.clone()).collect();
            let active_idx = ids
                .iter()
                .position(|id| id == &cfg.active_provider_id)
                .unwrap_or(0);
            (labels, ids, active_idx, cfg.linux_quick_mode)
        };
        let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();

        let (dropdown, linux_btn) = {
            let mut st = self.state.borrow_mut();
            st.syncing = true;
            st.provider_ids = ids;
            (st.provider_dropdown.clone(), st.linux_btn.clone())
        };

        dropdown.set_model(Some(&gtk4::StringList::new(&refs)));
        dropdown.set_selected(active_idx as u32);
        linux_btn.set_active(linux_quick);

        self.state.borrow_mut().syncing = false;
    }

    pub fn new_chat(&self) {
        {
            let mut st = self.state.borrow_mut();
            if st.streaming {
                return;
            }
            st.active_conversation_id = None;
            st.messages.clear();
            clear_box(&st.msg_box);
        }
        self.render_welcome();
        self.focus_input();
    }

    fn render_welcome(&self) {
        let st = self.state.borrow();
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        card.add_css_class("chat-bubble-assistant");
        let title = gtk4::Label::new(Some("Ask HyprPilot anything"));
        title.add_css_class("title-3");
        title.set_halign(gtk4::Align::Start);
        let hint = gtk4::Label::new(Some(
            "Example: \"how to find a directory inside linux\" or switch to Web Portals above to sign into ChatGPT / Claude / Copilot directly.",
        ));
        hint.set_wrap(true);
        hint.set_xalign(0.0);
        hint.add_css_class("dim-label");
        card.append(&title);
        card.append(&hint);
        st.msg_box.append(&card);
    }

    fn refresh_history(&self) {
        let query = self.state.borrow().search_entry.text().to_string();
        let list = self.state.borrow().history_list.clone();
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let summaries = {
            let st = self.state.borrow();
            match &st.db {
                Some(db) => db.list_conversations(&query).unwrap_or_default(),
                None => Vec::new(),
            }
        };

        for conv in summaries {
            let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            row_box.set_margin_top(4);
            row_box.set_margin_bottom(4);
            row_box.set_margin_start(6);
            row_box.set_margin_end(4);

            let text_btn = gtk4::Button::new();
            text_btn.add_css_class("flat");
            text_btn.set_hexpand(true);
            let lbl = gtk4::Label::new(Some(&conv.title));
            lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            lbl.set_xalign(0.0);
            text_btn.set_child(Some(&lbl));

            let del_btn = gtk4::Button::from_icon_name("user-trash-symbolic");
            del_btn.add_css_class("flat");

            let pane_load = self.clone();
            let cid = conv.id;
            text_btn.connect_clicked(move |_| pane_load.load_conversation(cid));

            let pane_del = self.clone();
            del_btn.connect_clicked(move |_| pane_del.delete_conversation(cid));

            row_box.append(&text_btn);
            row_box.append(&del_btn);
            list.append(&row_box);
        }
    }

    fn load_conversation(&self, id: i64) {
        let msgs = {
            let mut st = self.state.borrow_mut();
            if st.streaming {
                return;
            }
            let Some(db) = &st.db else { return };
            let loaded = db.get_messages(id).unwrap_or_default();
            st.active_conversation_id = Some(id);
            st.messages = loaded
                .iter()
                .map(|m| AiMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                })
                .collect();
            clear_box(&st.msg_box);
            loaded
        };

        for m in msgs {
            self.append_rendered_message(&m.role, &m.content);
        }
        self.scroll_to_bottom();
    }

    fn delete_conversation(&self, id: i64) {
        let was_active = {
            let mut st = self.state.borrow_mut();
            if let Some(db) = &st.db {
                let _ = db.delete_conversation(id);
            }
            if st.active_conversation_id == Some(id) {
                st.active_conversation_id = None;
                st.messages.clear();
                clear_box(&st.msg_box);
                true
            } else {
                false
            }
        };
        if was_active {
            self.render_welcome();
        }
        self.refresh_history();
    }

    fn send_current_prompt(&self) {
        let (prompt, provider, system_prompt, history_snapshot, conv_id) = {
            let mut st = self.state.borrow_mut();
            if st.streaming {
                return;
            }
            let buf = st.composer.buffer();
            let text = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .trim()
                .to_string();
            if text.is_empty() {
                return;
            }

            let Some(provider) = self.config.borrow().active_provider().cloned() else {
                return;
            };

            // Clear welcome card on first message
            if st.messages.is_empty() {
                clear_box(&st.msg_box);
            }
            buf.set_text("");

            let conv_id = if let Some(cid) = st.active_conversation_id {
                if let Some(db) = &st.db {
                    let _ = db.update_conversation_meta(cid, None, &provider.id, &provider.model);
                }
                Some(cid)
            } else if let Some(db) = &st.db {
                let title = derive_title_from_prompt(&text);
                match db.create_conversation(&title, &provider.id, &provider.model) {
                    Ok(cid) => {
                        st.active_conversation_id = Some(cid);
                        Some(cid)
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

            if let (Some(db), Some(cid)) = (&st.db, conv_id) {
                let _ = db.add_message(cid, "user", &text);
            }

            st.messages.push(AiMessage {
                role: "user".into(),
                content: text.clone(),
            });
            st.streaming = true;
            st.send_btn.set_sensitive(false);
            st.status_lbl
                .set_text(&format!("Streaming from {} ({})...", provider.name, provider.model));
            st.status_lbl.set_visible(true);

            let sys = self.config.borrow().effective_system_prompt();
            (text, provider, sys, st.messages.clone(), conv_id)
        };

        self.append_rendered_message("user", &prompt);
        let live_label = self.append_streaming_placeholder(&provider.name);
        self.scroll_to_bottom();
        self.refresh_history();

        let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();
        let client = self.http.clone();
        self.runtime.spawn(stream_chat(
            client,
            provider,
            system_prompt,
            history_snapshot,
            tx,
        ));

        let pane = self.clone();
        gtk4::glib::MainContext::default().spawn_local(async move {
            let mut full = String::new();
            let mut err_msg: Option<String> = None;

            while let Some(ev) = rx.recv().await {
                match ev {
                    StreamEvent::Delta(chunk) => {
                        full.push_str(&chunk);
                        live_label.set_text(&full);
                        pane.scroll_to_bottom();
                    }
                    StreamEvent::Done => break,
                    StreamEvent::Error(e) => {
                        err_msg = Some(e);
                        break;
                    }
                }
            }

            // Replace streaming placeholder with formatted code-card bubble
            if let Some(bubble_parent) = live_label.parent() {
                let msg_box = pane.state.borrow().msg_box.clone();
                msg_box.remove(&bubble_parent);
            }

            if let Some(err) = err_msg {
                let display = if full.is_empty() {
                    err
                } else {
                    format!("{full}\n\n[Error: {err}]")
                };
                pane.append_error_bubble(&display);
            } else {
                {
                    let mut st = pane.state.borrow_mut();
                    if let (Some(db), Some(cid)) = (&st.db, conv_id) {
                        let _ = db.add_message(cid, "assistant", &full);
                    }
                    st.messages.push(AiMessage {
                        role: "assistant".into(),
                        content: full.clone(),
                    });
                }
                pane.append_rendered_message("assistant", &full);
                pane.refresh_history();
            }

            {
                let st = pane.state.borrow_mut();
                drop(st);
                let mut st = pane.state.borrow_mut();
                st.streaming = false;
                st.send_btn.set_sensitive(true);
                st.status_lbl.set_visible(false);
            }
            pane.scroll_to_bottom();
        });
    }

    fn append_streaming_placeholder(&self, provider_name: &str) -> gtk4::Label {
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        card.add_css_class("chat-bubble-assistant");

        let role = gtk4::Label::new(Some(provider_name));
        role.add_css_class("role-label");
        role.set_halign(gtk4::Align::Start);

        let body = gtk4::Label::new(Some("Thinking..."));
        body.set_wrap(true);
        body.set_selectable(true);
        body.set_xalign(0.0);

        card.append(&role);
        card.append(&body);
        self.state.borrow().msg_box.append(&card);
        body
    }

    fn append_error_bubble(&self, msg: &str) {
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        card.add_css_class("chat-bubble-error");
        let lbl = gtk4::Label::new(Some(msg));
        lbl.set_wrap(true);
        lbl.set_selectable(true);
        lbl.set_xalign(0.0);
        card.append(&lbl);
        self.state.borrow().msg_box.append(&card);
    }

    fn append_rendered_message(&self, role: &str, content: &str) {
        let is_user = role == "user";
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        card.add_css_class(if is_user {
            "chat-bubble-user"
        } else {
            "chat-bubble-assistant"
        });

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let role_lbl = gtk4::Label::new(Some(if is_user { "You" } else { "HyprPilot" }));
        role_lbl.add_css_class("role-label");
        role_lbl.set_hexpand(true);
        role_lbl.set_halign(gtk4::Align::Start);
        header.append(&role_lbl);

        if !is_user {
            let copy_all = gtk4::Button::from_icon_name("edit-copy-symbolic");
            copy_all.add_css_class("flat");
            copy_all.set_tooltip_text(Some("Copy full reply"));
            let full_text = content.to_string();
            copy_all.connect_clicked(move |_| copy_to_clipboard(&full_text));
            header.append(&copy_all);
        }
        card.append(&header);

        for segment in split_fenced_blocks(content) {
            match segment {
                Segment::Text(t) if !t.trim().is_empty() => {
                    let lbl = gtk4::Label::new(Some(t.trim()));
                    lbl.set_wrap(true);
                    lbl.set_selectable(true);
                    lbl.set_xalign(0.0);
                    card.append(&lbl);
                }
                Segment::Code { lang, code } => {
                    card.append(&build_code_card(&lang, &code));
                }
                _ => {}
            }
        }

        self.state.borrow().msg_box.append(&card);
    }

    fn scroll_to_bottom(&self) {
        let scrolled = self.state.borrow().scrolled.clone();
        gtk4::glib::idle_add_local_once(move || {
            let adj = scrolled.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        });
    }
}

fn build_code_card(lang: &str, code: &str) -> gtk4::Box {
    let box_card = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    box_card.add_css_class("code-card");

    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let lang_lbl = gtk4::Label::new(Some(if lang.is_empty() { "code" } else { lang }));
    lang_lbl.add_css_class("code-card-lang");
    lang_lbl.set_hexpand(true);
    lang_lbl.set_halign(gtk4::Align::Start);

    let copy_btn = gtk4::Button::with_label("Copy");
    copy_btn.add_css_class("flat");
    let code_owned = code.to_string();
    copy_btn.connect_clicked(move |btn| {
        copy_to_clipboard(&code_owned);
        btn.set_label("Copied");
    });

    top.append(&lang_lbl);
    top.append(&copy_btn);

    let code_lbl = gtk4::Label::new(Some(code));
    code_lbl.add_css_class("code-card-text");
    code_lbl.set_selectable(true);
    code_lbl.set_wrap(true);
    code_lbl.set_xalign(0.0);

    box_card.append(&top);
    box_card.append(&code_lbl);
    box_card
}

fn clear_box(container: &gtk4::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

enum Segment {
    Text(String),
    Code { lang: String, code: String },
}

fn split_fenced_blocks(input: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut current_text = String::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();

    for line in input.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            if !in_code {
                if !current_text.is_empty() {
                    out.push(Segment::Text(std::mem::take(&mut current_text)));
                }
                in_code = true;
                code_lang = rest.trim().to_string();
                code_buf.clear();
            } else {
                in_code = false;
                out.push(Segment::Code {
                    lang: std::mem::take(&mut code_lang),
                    code: std::mem::take(&mut code_buf).trim_end().to_string(),
                });
            }
            continue;
        }
        if in_code {
            code_buf.push_str(line);
            code_buf.push('\n');
        } else {
            current_text.push_str(line);
            current_text.push('\n');
        }
    }

    if in_code {
        out.push(Segment::Code {
            lang: code_lang,
            code: code_buf.trim_end().to_string(),
        });
    } else if !current_text.is_empty() {
        out.push(Segment::Text(current_text));
    }

    out
}
