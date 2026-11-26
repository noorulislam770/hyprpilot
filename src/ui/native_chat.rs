use gtk4::gdk;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};

use super::copy_to_clipboard;
use super::markdown_gtk::{PreviewCallback, render_markdown_widgets_with_preview};
use crate::ai::models::fetch_available_models;
use crate::ai::{AiMessage, StreamEvent, stream_chat};
use crate::preview::PreviewKind;
use crate::storage::config::{AppConfig, AuthMode};
use crate::storage::db::{HistoryDb, derive_title_from_prompt};

const QUICK_SUGGESTIONS: &[(&str, &str)] = &[
    ("Find a directory", "How to find a directory inside Linux by name?"),
    ("Explain a command", "Explain `tar -xzvf archive.tar.gz -C /target` simply"),
    ("Debug systemd", "How do I check logs for a failed systemd service with journalctl?"),
    ("Hyprland tip", "How do I list all active Hyprland windows and workspaces from CLI?"),
];

#[derive(Clone)]
pub struct NativeChatPane {
    pub root: gtk4::Box,
    pub sidebar: gtk4::Box,
    state: Rc<RefCell<ChatState>>,
    config: Rc<RefCell<AppConfig>>,
    runtime: Rc<Runtime>,
    http: reqwest::Client,
    on_title_change: Rc<RefCell<Option<Rc<dyn Fn(&str)>>>>,
    on_preview: Rc<RefCell<Option<PreviewCallback>>>,
}

struct ChatState {
    db: Option<HistoryDb>,
    active_conversation_id: Option<i64>,
    messages: Vec<AiMessage>,
    streaming: bool,
    syncing: bool,
    abort_tx: Option<oneshot::Sender<()>>,
    history_list: gtk4::ListBox,
    search_entry: gtk4::SearchEntry,
    content_stack: gtk4::Stack,
    msg_box: gtk4::Box,
    scrolled: gtk4::ScrolledWindow,
    composer: gtk4::TextView,
    send_btn: gtk4::Button,
    stop_btn: gtk4::Button,
    provider_dropdown: gtk4::DropDown,
    provider_ids: Vec<String>,
    model_dropdown: gtk4::DropDown,
    model_ids: Vec<String>,
    fetch_models_btn: gtk4::Button,
    linux_btn: gtk4::ToggleButton,
    status_lbl: gtk4::Label,
}

impl NativeChatPane {
    pub fn new(config: Rc<RefCell<AppConfig>>, runtime: Rc<Runtime>) -> Self {
        // ------------------------------------------------------------------
        // Claude-style Left Sidebar
        // ------------------------------------------------------------------
        let sidebar = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        sidebar.add_css_class("claude-sidebar");
        sidebar.set_margin_top(12);
        sidebar.set_margin_bottom(10);
        sidebar.set_margin_start(10);
        sidebar.set_margin_end(10);

        let brand_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let spark = gtk4::Label::new(Some("✦"));
        spark.add_css_class("claude-brand-spark");
        let brand = gtk4::Label::new(Some("HyprPilot"));
        brand.add_css_class("claude-brand");
        brand.set_hexpand(true);
        brand.set_halign(gtk4::Align::Start);
        brand_row.append(&spark);
        brand_row.append(&brand);

        let new_chat_btn = gtk4::Button::with_label("+  New chat");
        new_chat_btn.add_css_class("claude-new-chat-btn");

        let search_entry = gtk4::SearchEntry::new();
        search_entry.add_css_class("claude-sidebar-search");
        search_entry.set_placeholder_text(Some("Search chats (Ctrl+K)..."));

        let recents_lbl = gtk4::Label::new(Some("RECENTS"));
        recents_lbl.add_css_class("claude-section-label");
        recents_lbl.set_halign(gtk4::Align::Start);
        recents_lbl.set_margin_top(4);
        recents_lbl.set_margin_start(4);

        let history_list = gtk4::ListBox::new();
        history_list.set_selection_mode(gtk4::SelectionMode::None);
        history_list.add_css_class("navigation-sidebar");

        let hist_scroll = gtk4::ScrolledWindow::new();
        hist_scroll.set_vexpand(true);
        hist_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        hist_scroll.set_child(Some(&history_list));

        sidebar.append(&brand_row);
        sidebar.append(&new_chat_btn);
        sidebar.append(&search_entry);
        sidebar.append(&recents_lbl);
        sidebar.append(&hist_scroll);

        // ------------------------------------------------------------------
        // Main Claude-style Centered Reading Column
        // ------------------------------------------------------------------
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.set_vexpand(true);
        root.set_hexpand(true);

        let content_stack = gtk4::Stack::new();
        content_stack.set_vexpand(true);
        content_stack.set_hexpand(true);
        content_stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
        content_stack.set_transition_duration(150);

        // Welcome Hero Page
        let welcome_clamp = adw::Clamp::new();
        welcome_clamp.set_maximum_size(680);
        welcome_clamp.set_tightening_threshold(520);

        let welcome_box = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        welcome_box.set_valign(gtk4::Align::Center);
        welcome_box.set_halign(gtk4::Align::Center);
        welcome_box.set_vexpand(true);
        welcome_box.set_margin_start(20);
        welcome_box.set_margin_end(20);

        let w_spark = gtk4::Label::new(Some("✦"));
        w_spark.add_css_class("claude-welcome-spark");
        let w_title = gtk4::Label::new(Some("How can I help you today?"));
        w_title.add_css_class("claude-welcome-title");
        w_title.set_wrap(true);
        w_title.set_justify(gtk4::Justification::Center);

        let w_sub = gtk4::Label::new(Some(
            "Ask quick Linux & dev questions, or switch to Web Portals for ChatGPT / Claude web.",
        ));
        w_sub.add_css_class("claude-welcome-sub");
        w_sub.set_wrap(true);
        w_sub.set_justify(gtk4::Justification::Center);

        let chips_flow = gtk4::FlowBox::new();
        chips_flow.set_selection_mode(gtk4::SelectionMode::None);
        chips_flow.set_max_children_per_line(2);
        chips_flow.set_min_children_per_line(1);
        chips_flow.set_row_spacing(8);
        chips_flow.set_column_spacing(8);
        chips_flow.set_halign(gtk4::Align::Center);
        chips_flow.set_margin_top(6);

        let mut chip_buttons: Vec<(gtk4::Button, &'static str)> = Vec::new();
        for (title, prompt) in QUICK_SUGGESTIONS {
            let btn = gtk4::Button::with_label(title);
            btn.add_css_class("suggestion-chip");
            btn.set_tooltip_text(Some(prompt));
            chips_flow.insert(&btn, -1);
            chip_buttons.push((btn, *prompt));
        }

        welcome_box.append(&w_spark);
        welcome_box.append(&w_title);
        welcome_box.append(&w_sub);
        welcome_box.append(&chips_flow);
        welcome_clamp.set_child(Some(&welcome_box));

        // Active Thread Scrollable Column
        let msg_box = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        msg_box.set_margin_top(14);
        msg_box.set_margin_bottom(14);
        msg_box.set_margin_start(16);
        msg_box.set_margin_end(16);

        let thread_clamp = adw::Clamp::new();
        thread_clamp.set_maximum_size(760);
        thread_clamp.set_tightening_threshold(580);
        thread_clamp.set_child(Some(&msg_box));

        let scrolled = gtk4::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_hexpand(true);
        scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scrolled.set_child(Some(&thread_clamp));

        content_stack.add_named(&welcome_clamp, Some("welcome"));
        content_stack.add_named(&scrolled, Some("thread"));
        content_stack.set_visible_child_name("welcome");

        // Floating Claude-style Composer Card (anchored in Clamp at bottom)
        let composer_clamp = adw::Clamp::new();
        composer_clamp.set_maximum_size(760);
        composer_clamp.set_tightening_threshold(580);

        let composer_outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        composer_outer.set_margin_start(14);
        composer_outer.set_margin_end(14);
        composer_outer.set_margin_bottom(14);
        composer_outer.set_margin_top(4);

        let status_lbl = gtk4::Label::new(None);
        status_lbl.add_css_class("dim-label");
        status_lbl.set_halign(gtk4::Align::Start);
        status_lbl.set_margin_start(8);
        status_lbl.set_visible(false);

        let composer_card = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        composer_card.add_css_class("composer-card");

        let composer = gtk4::TextView::new();
        composer.add_css_class("composer-textview");
        composer.set_wrap_mode(gtk4::WrapMode::WordChar);
        composer.set_hexpand(true);
        composer.set_accepts_tab(false);

        let comp_scroll = gtk4::ScrolledWindow::new();
        comp_scroll.set_hexpand(true);
        comp_scroll.set_min_content_height(44);
        comp_scroll.set_max_content_height(150);
        comp_scroll.set_propagate_natural_height(true);
        comp_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        comp_scroll.set_child(Some(&composer));

        // Bottom controls bar inside composer card
        let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        controls.add_css_class("composer-controls");

        let provider_dropdown = gtk4::DropDown::from_strings(&[]);
        let model_dropdown = gtk4::DropDown::from_strings(&[]);
        model_dropdown.set_hexpand(true);

        let fetch_models_btn = gtk4::Button::from_icon_name("view-refresh-symbolic");
        fetch_models_btn.set_tooltip_text(Some(
            "Fetch available models & pick lightweight model",
        ));
        fetch_models_btn.add_css_class("flat");

        let linux_btn = gtk4::ToggleButton::with_label("Linux Quick");
        linux_btn.add_css_class("claude-pill-toggle");
        linux_btn.set_tooltip_text(Some("Concise Arch/Linux CLI answers"));
        linux_btn.set_active(config.borrow().linux_quick_mode);

        let stop_btn = gtk4::Button::with_label("■ Stop");
        stop_btn.add_css_class("claude-stop-btn");
        stop_btn.set_visible(false);

        let send_btn = gtk4::Button::with_label("↑");
        send_btn.add_css_class("claude-send-btn");
        send_btn.set_tooltip_text(Some("Send (Enter)"));

        controls.append(&provider_dropdown);
        controls.append(&model_dropdown);
        controls.append(&fetch_models_btn);
        controls.append(&linux_btn);
        controls.append(&stop_btn);
        controls.append(&send_btn);

        composer_card.append(&comp_scroll);
        composer_card.append(&controls);

        composer_outer.append(&status_lbl);
        composer_outer.append(&composer_card);
        composer_clamp.set_child(Some(&composer_outer));

        root.append(&content_stack);
        root.append(&composer_clamp);

        let db = HistoryDb::open_default().ok();
        let state = Rc::new(RefCell::new(ChatState {
            db,
            active_conversation_id: None,
            messages: Vec::new(),
            streaming: false,
            syncing: false,
            abort_tx: None,
            history_list,
            search_entry: search_entry.clone(),
            content_stack,
            msg_box,
            scrolled,
            composer: composer.clone(),
            send_btn: send_btn.clone(),
            stop_btn: stop_btn.clone(),
            provider_dropdown: provider_dropdown.clone(),
            provider_ids: Vec::new(),
            model_dropdown: model_dropdown.clone(),
            model_ids: Vec::new(),
            fetch_models_btn: fetch_models_btn.clone(),
            linux_btn: linux_btn.clone(),
            status_lbl,
        }));

        let pane = Self {
            root,
            sidebar,
            state,
            config,
            runtime,
            http: reqwest::Client::new(),
            on_title_change: Rc::new(RefCell::new(None)),
            on_preview: Rc::new(RefCell::new(None)),
        };

        pane.sync_providers_from_config();
        pane.refresh_history();
        pane.wire_signals(
            new_chat_btn,
            search_entry,
            send_btn,
            stop_btn,
            composer,
            linux_btn,
            provider_dropdown,
            model_dropdown,
            fetch_models_btn,
            chip_buttons,
        );
        pane.maybe_auto_fetch_active();

        pane
    }

    pub fn set_on_title_change<F: Fn(&str) + 'static>(&self, cb: F) {
        *self.on_title_change.borrow_mut() = Some(Rc::new(cb));
    }

    pub fn set_on_preview<F: Fn(PreviewKind, String, String) + 'static>(&self, cb: F) {
        *self.on_preview.borrow_mut() = Some(Rc::new(cb));
    }

    fn emit_title(&self, title: &str) {
        if let Some(cb) = self.on_title_change.borrow().as_ref() {
            cb(title);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn wire_signals(
        &self,
        new_chat_btn: gtk4::Button,
        search_entry: gtk4::SearchEntry,
        send_btn: gtk4::Button,
        stop_btn: gtk4::Button,
        composer: gtk4::TextView,
        linux_btn: gtk4::ToggleButton,
        provider_dropdown: gtk4::DropDown,
        model_dropdown: gtk4::DropDown,
        fetch_models_btn: gtk4::Button,
        chip_buttons: Vec<(gtk4::Button, &'static str)>,
    ) {
        {
            let pane = self.clone();
            new_chat_btn.connect_clicked(move |_| pane.new_chat());
        }
        {
            let pane = self.clone();
            search_entry.connect_search_changed(move |_| pane.refresh_history());
        }
        {
            let pane = self.clone();
            linux_btn.connect_toggled(move |b| {
                if pane.state.borrow().syncing {
                    return;
                }
                let mut c = pane.config.borrow_mut();
                c.linux_quick_mode = b.is_active();
                let _ = c.save();
            });
        }
        {
            let pane = self.clone();
            provider_dropdown.connect_selected_notify(move |dd| {
                if pane.state.borrow().syncing {
                    return;
                }
                let idx = dd.selected() as usize;
                let id_opt = pane.state.borrow().provider_ids.get(idx).cloned();
                if let Some(id) = id_opt {
                    {
                        let mut c = pane.config.borrow_mut();
                        c.active_provider_id = id;
                        let _ = c.save();
                    }
                    pane.sync_models_from_active_provider();
                    pane.maybe_auto_fetch_active();
                }
            });
        }
        {
            let pane = self.clone();
            model_dropdown.connect_selected_notify(move |dd| {
                if pane.state.borrow().syncing {
                    return;
                }
                let idx = dd.selected() as usize;
                let model_opt = pane.state.borrow().model_ids.get(idx).cloned();
                if let Some(model_id) = model_opt {
                    let mut c = pane.config.borrow_mut();
                    let active_id = c.active_provider_id.clone();
                    if let Some(p) = c.providers.iter_mut().find(|p| p.id == active_id) {
                        p.model = model_id;
                    }
                    let _ = c.save();
                }
            });
        }
        {
            let pane = self.clone();
            fetch_models_btn.connect_clicked(move |_| pane.fetch_models_for_active());
        }
        {
            let pane = self.clone();
            send_btn.connect_clicked(move |_| pane.send_current_prompt());
        }
        {
            let pane = self.clone();
            stop_btn.connect_clicked(move |_| pane.stop_streaming());
        }
        for (btn, prompt) in chip_buttons {
            let pane = self.clone();
            btn.connect_clicked(move |_| pane.send_prompt_text(prompt));
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

    pub fn focus_search(&self) {
        self.state.borrow().search_entry.grab_focus();
    }

    pub fn sync_providers_from_config(&self) {
        let (labels, ids, active_idx, linux_quick) = {
            let cfg = self.config.borrow();
            let labels: Vec<String> = cfg.providers.iter().map(|p| p.name.clone()).collect();
            let ids: Vec<String> = cfg.providers.iter().map(|p| p.id.clone()).collect();
            let active_idx = ids
                .iter()
                .position(|id| id == &cfg.active_provider_id)
                .unwrap_or(0);
            (labels, ids, active_idx, cfg.linux_quick_mode)
        };
        let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        let prov_list = gtk4::StringList::new(&refs);

        let (dropdown, linux_btn) = {
            let mut st = self.state.borrow_mut();
            st.syncing = true;
            st.provider_ids = ids;
            (st.provider_dropdown.clone(), st.linux_btn.clone())
        };

        dropdown.set_model(Some(&prov_list));
        dropdown.set_selected(active_idx as u32);
        linux_btn.set_active(linux_quick);

        self.state.borrow_mut().syncing = false;
        self.sync_models_from_active_provider();
    }

    fn sync_models_from_active_provider(&self) {
        let (models, active_model) = {
            let cfg = self.config.borrow();
            if let Some(p) = cfg.active_provider() {
                let mut list = p.available_models.clone();
                if list.is_empty() && !p.model.trim().is_empty() {
                    list.push(p.model.trim().to_string());
                } else if !p.model.trim().is_empty() && !list.iter().any(|m| m == p.model.trim()) {
                    list.insert(0, p.model.trim().to_string());
                }
                (list, p.model.clone())
            } else {
                (Vec::new(), String::new())
            }
        };

        let selected_idx = models
            .iter()
            .position(|m| m == active_model.trim())
            .unwrap_or(0);
        let refs: Vec<&str> = models.iter().map(|s| s.as_str()).collect();
        let has_models = !refs.is_empty();
        let model_list = gtk4::StringList::new(&refs);

        let model_dd = {
            let mut st = self.state.borrow_mut();
            st.syncing = true;
            st.model_ids = models;
            st.model_dropdown.clone()
        };

        model_dd.set_model(Some(&model_list));
        if has_models {
            model_dd.set_selected(selected_idx as u32);
        }

        self.state.borrow_mut().syncing = false;
    }

    fn maybe_auto_fetch_active(&self) {
        let should_fetch = {
            let cfg = self.config.borrow();
            cfg.active_provider().is_some_and(|p| {
                p.available_models.is_empty()
                    && (!p.token.trim().is_empty() || p.auth_mode == AuthMode::None)
            })
        };
        if should_fetch {
            self.fetch_models_for_active();
        }
    }

    pub fn fetch_models_for_active(&self) {
        let Some(provider) = self.config.borrow().active_provider().cloned() else {
            return;
        };
        let target_id = provider.id.clone();

        let (btn, status_lbl) = {
            let st = self.state.borrow();
            (st.fetch_models_btn.clone(), st.status_lbl.clone())
        };
        btn.set_sensitive(false);
        status_lbl.set_text(&format!("Fetching models for {}...", provider.name));
        status_lbl.set_visible(true);

        let (tx, rx) = oneshot::channel();
        let client = self.http.clone();
        self.runtime.spawn(async move {
            let res = fetch_available_models(&client, &provider).await;
            let _ = tx.send(res);
        });

        let pane = self.clone();
        gtk4::glib::MainContext::default().spawn_local(async move {
            if let Ok(res) = rx.await {
                match res {
                    Ok(fetched) => {
                        let count = fetched.models.len();
                        let chosen = fetched.recommended.clone().unwrap_or_default();
                        let still_active = {
                            let mut cfg = pane.config.borrow_mut();
                            if let Some(p) = cfg.providers.iter_mut().find(|p| p.id == target_id) {
                                p.available_models = fetched.models;
                                if !chosen.is_empty() {
                                    p.model = chosen.clone();
                                }
                            }
                            let _ = cfg.save();
                            cfg.active_provider_id == target_id
                        };
                        if still_active {
                            pane.sync_models_from_active_provider();
                            let lbl = pane.state.borrow().status_lbl.clone();
                            lbl.set_text(&format!(
                                "Fetched {count} models — selected lightweight: {chosen}"
                            ));
                            lbl.set_visible(true);
                        }
                    }
                    Err(e) => {
                        let lbl = pane.state.borrow().status_lbl.clone();
                        lbl.set_text(&format!("Model fetch failed: {e:#}"));
                        lbl.set_visible(true);
                    }
                }
            }
            pane.state.borrow().fetch_models_btn.set_sensitive(true);
        });
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
            st.content_stack.set_visible_child_name("welcome");
            st.status_lbl.set_visible(false);
        }
        self.emit_title("New chat");
        self.refresh_history();
        self.focus_input();
    }

    pub fn stop_streaming(&self) {
        let tx_opt = self.state.borrow_mut().abort_tx.take();
        if let Some(tx) = tx_opt {
            let _ = tx.send(());
        }
    }

    fn refresh_history(&self) {
        let (query, active_cid, list) = {
            let st = self.state.borrow();
            (
                st.search_entry.text().to_string(),
                st.active_conversation_id,
                st.history_list.clone(),
            )
        };
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
            let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
            row_box.add_css_class("claude-history-row");
            if active_cid == Some(conv.id) {
                row_box.add_css_class("claude-history-row-active");
            }

            let text_btn = gtk4::Button::new();
            text_btn.add_css_class("flat");
            text_btn.set_hexpand(true);

            let text_col = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
            let title_lbl = gtk4::Label::new(Some(&conv.title));
            title_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            title_lbl.set_xalign(0.0);
            let sub_lbl = gtk4::Label::new(Some(&conv.model));
            sub_lbl.add_css_class("claude-history-subtitle");
            sub_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            sub_lbl.set_xalign(0.0);
            text_col.append(&title_lbl);
            text_col.append(&sub_lbl);
            text_btn.set_child(Some(&text_col));

            let del_btn = gtk4::Button::from_icon_name("user-trash-symbolic");
            del_btn.add_css_class("flat");
            del_btn.set_valign(gtk4::Align::Center);
            del_btn.set_tooltip_text(Some("Delete chat"));

            let pane_load = self.clone();
            let cid = conv.id;
            let title_clone = conv.title.clone();
            text_btn.connect_clicked(move |_| pane_load.load_conversation(cid, &title_clone));

            let pane_del = self.clone();
            del_btn.connect_clicked(move |_| pane_del.delete_conversation(cid));

            row_box.append(&text_btn);
            row_box.append(&del_btn);
            list.append(&row_box);
        }
    }

    fn load_conversation(&self, id: i64, title: &str) {
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
            st.content_stack.set_visible_child_name("thread");
            loaded
        };

        let last_idx = msgs.len().saturating_sub(1);
        for (i, m) in msgs.iter().enumerate() {
            let show_regen = m.role == "assistant" && i == last_idx;
            self.append_rendered_message(&m.role, &m.content, None, show_regen);
        }
        self.emit_title(title);
        self.refresh_history();
        self.scroll_to_bottom();
        self.focus_input();
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
                st.content_stack.set_visible_child_name("welcome");
                true
            } else {
                false
            }
        };
        if was_active {
            self.emit_title("New chat");
        }
        self.refresh_history();
    }

    fn send_current_prompt(&self) {
        let text = {
            let st = self.state.borrow();
            let buf = st.composer.buffer();
            buf.text(&buf.start_iter(), &buf.end_iter(), false)
                .trim()
                .to_string()
        };
        if text.is_empty() {
            return;
        }
        {
            let st = self.state.borrow();
            st.composer.buffer().set_text("");
        }
        self.send_prompt_text(&text);
    }

    fn send_prompt_text(&self, text: &str) {
        let prompt = text.trim().to_string();
        if prompt.is_empty() {
            return;
        }

        let (provider, system_prompt, history_snapshot, conv_id, title_for_bar) = {
            let mut st = self.state.borrow_mut();
            if st.streaming {
                return;
            }
            let Some(provider) = self.config.borrow().active_provider().cloned() else {
                return;
            };

            st.content_stack.set_visible_child_name("thread");

            let mut title_bar = None;
            let conv_id = if let Some(cid) = st.active_conversation_id {
                if let Some(db) = &st.db {
                    let _ = db.update_conversation_meta(cid, None, &provider.id, &provider.model);
                }
                Some(cid)
            } else if let Some(db) = &st.db {
                let title = derive_title_from_prompt(&prompt);
                title_bar = Some(title.clone());
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
                let _ = db.add_message(cid, "user", &prompt);
            }

            st.messages.push(AiMessage {
                role: "user".into(),
                content: prompt.clone(),
            });
            let sys = self.config.borrow().effective_system_prompt();
            (provider, sys, st.messages.clone(), conv_id, title_bar)
        };

        if let Some(t) = title_for_bar {
            self.emit_title(&t);
        }
        self.append_rendered_message("user", &prompt, None, false);
        self.refresh_history();
        self.start_stream_for_history(provider, system_prompt, history_snapshot, conv_id);
    }

    fn regenerate_last(&self) {
        let (provider, system_prompt, history_snapshot, conv_id) = {
            let mut st = self.state.borrow_mut();
            if st.streaming {
                return;
            }
            if st.messages.last().is_some_and(|m| m.role == "assistant") {
                st.messages.pop();
                if let Some(last_widget) = st.msg_box.last_child() {
                    st.msg_box.remove(&last_widget);
                }
                if let (Some(db), Some(cid)) = (&st.db, st.active_conversation_id) {
                    let _ = db.delete_last_message_if_role(cid, "assistant");
                }
            }
            if st.messages.is_empty() {
                return;
            }
            let Some(provider) = self.config.borrow().active_provider().cloned() else {
                return;
            };
            let sys = self.config.borrow().effective_system_prompt();
            (provider, sys, st.messages.clone(), st.active_conversation_id)
        };
        self.start_stream_for_history(provider, system_prompt, history_snapshot, conv_id);
    }

    fn start_stream_for_history(
        &self,
        provider: crate::storage::config::ProviderProfile,
        system_prompt: Option<String>,
        history_snapshot: Vec<AiMessage>,
        conv_id: Option<i64>,
    ) {
        let (abort_tx, mut abort_rx) = oneshot::channel::<()>();
        {
            let mut st = self.state.borrow_mut();
            st.streaming = true;
            st.abort_tx = Some(abort_tx);
            st.send_btn.set_visible(false);
            st.stop_btn.set_visible(true);
            st.status_lbl.set_text(&format!(
                "Streaming from {} ({})...",
                provider.name, provider.model
            ));
            st.status_lbl.set_visible(true);
        }

        let model_label = provider.model.clone();
        let live_label = self.append_streaming_placeholder(&provider.name, &model_label);
        self.scroll_to_bottom();

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
            let mut stopped_early = false;

            loop {
                tokio::select! {
                    biased;
                    _ = &mut abort_rx => {
                        stopped_early = true;
                        break;
                    }
                    ev = rx.recv() => {
                        match ev {
                            Some(StreamEvent::Delta(chunk)) => {
                                full.push_str(&chunk);
                                live_label.set_text(&full);
                                pane.scroll_to_bottom();
                            }
                            Some(StreamEvent::Done) | None => break,
                            Some(StreamEvent::Error(e)) => {
                                err_msg = Some(e);
                                break;
                            }
                        }
                    }
                }
            }

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
                let final_text = if stopped_early && full.is_empty() {
                    "[Stopped]".to_string()
                } else {
                    full
                };
                {
                    let mut st = pane.state.borrow_mut();
                    if let (Some(db), Some(cid)) = (&st.db, conv_id) {
                        let _ = db.add_message(cid, "assistant", &final_text);
                    }
                    st.messages.push(AiMessage {
                        role: "assistant".into(),
                        content: final_text.clone(),
                    });
                }
                pane.append_rendered_message("assistant", &final_text, Some(&model_label), true);
                pane.refresh_history();
            }

            {
                let mut st = pane.state.borrow_mut();
                st.streaming = false;
                st.abort_tx = None;
                st.stop_btn.set_visible(false);
                st.send_btn.set_visible(true);
                st.status_lbl.set_visible(false);
            }
            pane.scroll_to_bottom();
        });
    }

    fn append_streaming_placeholder(&self, provider_name: &str, model: &str) -> gtk4::Label {
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        card.add_css_class("chat-reply-claude");

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let spark = gtk4::Label::new(Some("✦"));
        spark.add_css_class("claude-brand-spark");
        let role = gtk4::Label::new(Some(provider_name));
        role.add_css_class("role-label-claude");
        let badge = gtk4::Label::new(Some(model));
        badge.add_css_class("model-badge");
        header.append(&spark);
        header.append(&role);
        header.append(&badge);

        let body = gtk4::Label::new(Some("Thinking..."));
        body.set_wrap(true);
        body.set_selectable(true);
        body.set_xalign(0.0);
        body.add_css_class("md-paragraph");

        card.append(&header);
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

    fn append_rendered_message(
        &self,
        role: &str,
        content: &str,
        model_badge: Option<&str>,
        allow_regenerate: bool,
    ) {
        if role == "user" {
            let outer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            outer.set_halign(gtk4::Align::End);
            let card = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            card.add_css_class("chat-bubble-user");

            let lbl = gtk4::Label::new(Some(content));
            lbl.set_wrap(true);
            lbl.set_selectable(true);
            lbl.set_xalign(0.0);
            lbl.set_max_width_chars(62);
            card.append(&lbl);
            outer.append(&card);
            self.state.borrow().msg_box.append(&outer);
            return;
        }

        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        card.add_css_class("chat-reply-claude");

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let spark = gtk4::Label::new(Some("✦"));
        spark.add_css_class("claude-brand-spark");
        let role_lbl = gtk4::Label::new(Some("HyprPilot"));
        role_lbl.add_css_class("role-label-claude");
        header.append(&spark);
        header.append(&role_lbl);

        if let Some(m) = model_badge {
            let badge = gtk4::Label::new(Some(m));
            badge.add_css_class("model-badge");
            header.append(&badge);
        }

        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        let copy_all = gtk4::Button::from_icon_name("edit-copy-symbolic");
        copy_all.add_css_class("flat");
        copy_all.set_tooltip_text(Some("Copy full reply"));
        let full_text = content.to_string();
        copy_all.connect_clicked(move |_| copy_to_clipboard(&full_text));
        header.append(&copy_all);

        if allow_regenerate {
            let regen = gtk4::Button::from_icon_name("view-refresh-symbolic");
            regen.add_css_class("flat");
            regen.set_tooltip_text(Some("Regenerate reply"));
            let pane_regen = self.clone();
            regen.connect_clicked(move |_| pane_regen.regenerate_last());
            header.append(&regen);
        }

        card.append(&header);

        let body_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        let preview_cb = self.on_preview.borrow().clone();
        render_markdown_widgets_with_preview(&body_box, content, preview_cb);
        card.append(&body_box);

        self.state.borrow().msg_box.append(&card);
    }

    pub fn scroll_to_bottom(&self) {
        let scrolled = self.state.borrow().scrolled.clone();
        gtk4::glib::idle_add_local_once(move || {
            let adj = scrolled.vadjustment();
            adj.set_value(adj.upper() - adj.page_size());
        });
    }

    pub fn self_test_render_and_reset(&self) {
        self.state
            .borrow()
            .content_stack
            .set_visible_child_name("thread");
        self.append_rendered_message("user", "how to find dir", None, false);
        self.append_rendered_message(
            "assistant",
            "# Directory Search\n\nUse **find** with `type <d>`:\n\n| Flag | Meaning |\n| --- | --- |\n| -type d | Directories |\n\n```bash\nfind . -type d -name foo\n```",
            Some("test-model"),
            true,
        );
        let msg_box = self.state.borrow().msg_box.clone();
        assert!(msg_box.first_child().is_some(), "expected rendered user bubble");
        assert!(
            msg_box.first_child() != msg_box.last_child(),
            "expected rendered assistant bubble"
        );
        self.sync_providers_from_config();
        self.new_chat();
        assert_eq!(
            self.state
                .borrow()
                .content_stack
                .visible_child_name()
                .as_deref(),
            Some("welcome")
        );
        assert!(msg_box.first_child().is_none(), "expected new_chat to clear bubbles");
    }
}

fn clear_box(container: &gtk4::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}
