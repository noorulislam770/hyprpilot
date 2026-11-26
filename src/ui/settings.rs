use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use crate::ai::models::fetch_available_models;
use crate::hyprland::sync_hyprland_config;
use crate::storage::config::{
    AppConfig, AuthMode, PortalEntry, ProviderKind, ProviderProfile, format_hyprland_hotkey,
};

const HOTKEY_PRESETS: &[&str] = &[
    "ALT + SPACE",
    "ALT + SHIFT + SPACE",
    "SUPER + SPACE",
    "SUPER + SHIFT + SPACE",
    "SUPER + A",
    "SUPER + SHIFT + A",
    "CTRL + ALT + SPACE",
    "ALT + Q",
    "SUPER + GRAVE",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordTarget {
    None,
    Gui,
    Tui,
}

pub fn show_settings_dialog<F>(
    parent: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    runtime: Rc<Runtime>,
    on_saved: F,
) where
    F: Fn(&AppConfig) + 'static,
{
    let settings_win = adw::Window::builder()
        .title("HyprPilot Settings")
        .transient_for(parent)
        .modal(true)
        .default_width(680)
        .default_height(640)
        .build();
    settings_win.set_application(parent.application().as_ref());

    let recording = Rc::new(Cell::new(RecordTarget::None));
    let (general_page, apply_recorded) =
        build_general_page(config.clone(), recording.clone());
    let providers_page = build_providers_page(config.clone(), runtime);
    let portals_page = build_portals_page(config.clone());

    let view_stack = adw::ViewStack::new();
    let p1 = view_stack.add_titled(&general_page, Some("general"), "General & Hyprland");
    p1.set_icon_name(Some("preferences-system-symbolic"));
    let p2 = view_stack.add_titled(&providers_page, Some("providers"), "Native AI & Tokens");
    p2.set_icon_name(Some("network-server-symbolic"));
    let p3 = view_stack.add_titled(&portals_page, Some("portals"), "Web Portals");
    p3.set_icon_name(Some("web-browser-symbolic"));

    let switcher = adw::ViewSwitcher::new();
    switcher.set_stack(Some(&view_stack));
    switcher.set_policy(adw::ViewSwitcherPolicy::Wide);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&view_stack));
    settings_win.set_content(Some(&toolbar));

    let key_ctrl = gtk4::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let win_esc = settings_win.clone();
    let rec_ctrl = recording.clone();
    key_ctrl.connect_key_pressed(move |_, key, _, mods| {
        let target = rec_ctrl.get();
        if target != RecordTarget::None {
            if key == gtk4::gdk::Key::Escape {
                rec_ctrl.set(RecordTarget::None);
                apply_recorded(target, None);
                return gtk4::glib::Propagation::Stop;
            }
            if let Some(combo) = gdk_event_to_hyprland_combo(key, mods) {
                rec_ctrl.set(RecordTarget::None);
                apply_recorded(target, Some(combo));
            }
            return gtk4::glib::Propagation::Stop;
        }
        if key == gtk4::gdk::Key::Escape {
            win_esc.close();
            return gtk4::glib::Propagation::Stop;
        }
        gtk4::glib::Propagation::Proceed
    });
    settings_win.add_controller(key_ctrl);

    let on_saved = Rc::new(on_saved);
    let cfg_close = config.clone();
    settings_win.connect_close_request(move |_| {
        let cfg = cfg_close.borrow().clone();
        let _ = cfg.save();
        on_saved(&cfg);
        gtk4::glib::Propagation::Proceed
    });

    settings_win.present();
}

type RecordedCb = Rc<dyn Fn(RecordTarget, Option<String>)>;

fn build_general_page(
    config: Rc<RefCell<AppConfig>>,
    recording: Rc<Cell<RecordTarget>>,
) -> (adw::PreferencesPage, RecordedCb) {
    let page = adw::PreferencesPage::new();
    page.set_title("General & Hyprland");
    page.set_icon_name(Some("preferences-system-symbolic"));

    // Hyprland Hotkey Group
    let hypr_group = adw::PreferencesGroup::new();
    hypr_group.set_title("Hyprland Toggle &amp; Hotkeys");
    hypr_group.set_description(Some(
        "Pick a preset from the dropdown or click Record to press your key combo. Changes apply to Hyprland immediately.",
    ));

    let status_label = gtk4::Label::new(None);
    status_label.add_css_class("dim-label");
    status_label.set_halign(gtk4::Align::Start);
    status_label.set_wrap(true);

    let syncing = Rc::new(Cell::new(false));
    let (gui_row, gui_dd, gui_rec_btn) =
        build_hotkey_selector_row("GUI Toggle Hotkey", &config.borrow().toggle_hotkey);
    let (tui_row, tui_dd, tui_rec_btn) =
        build_hotkey_selector_row("TUI Toggle Hotkey", &config.borrow().tui_toggle_hotkey);

    let sync_ui_rows = {
        let cfg = config.clone();
        let gui_row = gui_row.clone();
        let gui_dd = gui_dd.clone();
        let gui_rec_btn = gui_rec_btn.clone();
        let tui_row = tui_row.clone();
        let tui_dd = tui_dd.clone();
        let tui_rec_btn = tui_rec_btn.clone();
        let syncing = syncing.clone();
        let rec_state = recording.clone();
        move || {
            syncing.set(true);
            let (gui_hk, tui_hk) = {
                let c = cfg.borrow();
                (
                    format_hyprland_hotkey(&c.toggle_hotkey).0,
                    format_hyprland_hotkey(&c.tui_toggle_hotkey).0,
                )
            };
            update_hotkey_row_widgets(&gui_row, &gui_dd, &gui_hk);
            update_hotkey_row_widgets(&tui_row, &tui_dd, &tui_hk);
            let cur_rec = rec_state.get();
            gui_rec_btn.set_active(cur_rec == RecordTarget::Gui);
            gui_rec_btn.set_label(if cur_rec == RecordTarget::Gui {
                "Press keys..."
            } else {
                "Record..."
            });
            tui_rec_btn.set_active(cur_rec == RecordTarget::Tui);
            tui_rec_btn.set_label(if cur_rec == RecordTarget::Tui {
                "Press keys..."
            } else {
                "Record..."
            });
            syncing.set(false);
        }
    };

    let commit_hotkey = {
        let cfg = config.clone();
        let st_ref = status_label.clone();
        let sync_rows = sync_ui_rows.clone();
        move |target: RecordTarget, raw_combo: &str| {
            let (norm, _) = format_hyprland_hotkey(raw_combo);
            let swapped = {
                let mut c = cfg.borrow_mut();
                let mut did_swap = false;
                match target {
                    RecordTarget::Gui => {
                        let old_gui = format_hyprland_hotkey(&c.toggle_hotkey).0;
                        let cur_tui = format_hyprland_hotkey(&c.tui_toggle_hotkey).0;
                        if norm == cur_tui {
                            c.tui_toggle_hotkey = old_gui;
                            did_swap = true;
                        }
                        c.toggle_hotkey = norm.clone();
                    }
                    RecordTarget::Tui => {
                        let old_tui = format_hyprland_hotkey(&c.tui_toggle_hotkey).0;
                        let cur_gui = format_hyprland_hotkey(&c.toggle_hotkey).0;
                        if norm == cur_gui {
                            c.toggle_hotkey = old_tui;
                            did_swap = true;
                        }
                        c.tui_toggle_hotkey = norm.clone();
                    }
                    RecordTarget::None => {}
                }
                let _ = c.save();
                did_swap
            };
            sync_rows();
            let snapshot = cfg.borrow().clone();
            match sync_hyprland_config(&snapshot, true) {
                Ok(path) => {
                    let note = if swapped {
                        format!(
                            "Swapped hotkeys (GUI: {}, TUI: {}) -> {}",
                            snapshot.toggle_hotkey,
                            snapshot.tui_toggle_hotkey,
                            path.display()
                        )
                    } else {
                        format!(
                            "Applied (GUI: {}, TUI: {}) -> {}",
                            snapshot.toggle_hotkey,
                            snapshot.tui_toggle_hotkey,
                            path.display()
                        )
                    };
                    st_ref.set_text(&note);
                }
                Err(e) => st_ref.set_text(&format!("Error: {e:#}")),
            }
        }
    };

    // Preset dropdown handlers
    {
        let syncing = syncing.clone();
        let commit = commit_hotkey.clone();
        gui_dd.connect_selected_notify(move |dd| {
            if syncing.get() {
                return;
            }
            if let Some(preset) = HOTKEY_PRESETS.get(dd.selected() as usize) {
                commit(RecordTarget::Gui, preset);
            }
        });
    }
    {
        let syncing = syncing.clone();
        let commit = commit_hotkey.clone();
        tui_dd.connect_selected_notify(move |dd| {
            if syncing.get() {
                return;
            }
            if let Some(preset) = HOTKEY_PRESETS.get(dd.selected() as usize) {
                commit(RecordTarget::Tui, preset);
            }
        });
    }

    // Record button handlers
    {
        let syncing = syncing.clone();
        let rec = recording.clone();
        let sync_rows = sync_ui_rows.clone();
        gui_rec_btn.connect_toggled(move |b| {
            if syncing.get() {
                return;
            }
            rec.set(if b.is_active() {
                RecordTarget::Gui
            } else {
                RecordTarget::None
            });
            sync_rows();
        });
    }
    {
        let syncing = syncing.clone();
        let rec = recording.clone();
        let sync_rows = sync_ui_rows.clone();
        tui_rec_btn.connect_toggled(move |b| {
            if syncing.get() {
                return;
            }
            rec.set(if b.is_active() {
                RecordTarget::Tui
            } else {
                RecordTarget::None
            });
            sync_rows();
        });
    }

    let swap_btn = gtk4::Button::with_label("Swap GUI ↔ TUI");
    swap_btn.set_valign(gtk4::Align::Center);
    let apply_btn = gtk4::Button::with_label("Reload Hyprland");
    apply_btn.add_css_class("suggested-action");
    apply_btn.set_valign(gtk4::Align::Center);

    let actions_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    actions_box.append(&swap_btn);
    actions_box.append(&apply_btn);

    let apply_row = adw::ActionRow::new();
    apply_row.set_title("Quick actions");
    apply_row.set_subtitle("Swap GUI/TUI shortcuts in one click or reload Hyprland");
    apply_row.add_suffix(&actions_box);

    {
        let cfg_swap = config.clone();
        let sync_rows = sync_ui_rows.clone();
        let st_ref = status_label.clone();
        swap_btn.connect_clicked(move |_| {
            {
                let mut c = cfg_swap.borrow_mut();
                let c_ref = &mut *c;
                std::mem::swap(&mut c_ref.toggle_hotkey, &mut c_ref.tui_toggle_hotkey);
                let _ = c.save();
            }
            sync_rows();
            let snapshot = cfg_swap.borrow().clone();
            match sync_hyprland_config(&snapshot, true) {
                Ok(_) => st_ref.set_text(&format!(
                    "Swapped! GUI: {} | TUI: {}",
                    snapshot.toggle_hotkey, snapshot.tui_toggle_hotkey
                )),
                Err(e) => st_ref.set_text(&format!("Error: {e:#}")),
            }
        });
    }

    {
        let cfg_apply = config.clone();
        let st_ref = status_label.clone();
        apply_btn.connect_clicked(move |_| {
            let snapshot = cfg_apply.borrow().clone();
            let _ = snapshot.save();
            match sync_hyprland_config(&snapshot, true) {
                Ok(path) => st_ref.set_text(&format!(
                    "Reloaded {} (GUI: {}, TUI: {})",
                    path.display(),
                    snapshot.toggle_hotkey,
                    snapshot.tui_toggle_hotkey
                )),
                Err(e) => st_ref.set_text(&format!("Error: {e:#}")),
            }
        });
    }

    let recorded_cb: RecordedCb = Rc::new({
        let sync_rows = sync_ui_rows.clone();
        let commit = commit_hotkey;
        move |target, maybe_combo| match maybe_combo {
            Some(combo) => commit(target, &combo),
            None => sync_rows(),
        }
    });

    hypr_group.add(&gui_row);
    hypr_group.add(&tui_row);
    hypr_group.add(&apply_row);
    hypr_group.add(&status_label);

    // Behavior Group
    let behavior_group = adw::PreferencesGroup::new();
    behavior_group.set_title("Assistant Behavior");

    let view_row = adw::ComboRow::new();
    view_row.set_title("Default View on Launch");
    let view_model = gtk4::StringList::new(&["Native Fast Chat", "Embedded Web Portals"]);
    view_row.set_model(Some(&view_model));
    view_row.set_selected(if config.borrow().default_view == "portal" {
        1
    } else {
        0
    });
    let cfg_v = config.clone();
    view_row.connect_selected_notify(move |r| {
        cfg_v.borrow_mut().default_view = if r.selected() == 1 {
            "portal".to_string()
        } else {
            "native".to_string()
        };
    });

    let linux_row = adw::SwitchRow::new();
    linux_row.set_title("Linux / Arch Quick Mode");
    linux_row.set_subtitle("Prefer concise answers with copyable shell commands");
    linux_row.set_active(config.borrow().linux_quick_mode);
    let cfg_l = config.clone();
    linux_row.connect_active_notify(move |r| {
        cfg_l.borrow_mut().linux_quick_mode = r.is_active();
    });

    let esc_row = adw::SwitchRow::new();
    esc_row.set_title("Hide on Escape");
    esc_row.set_subtitle("Pressing Esc hides HyprPilot to the background");
    esc_row.set_active(config.borrow().hide_on_escape);
    let cfg_e = config.clone();
    esc_row.connect_active_notify(move |r| {
        cfg_e.borrow_mut().hide_on_escape = r.is_active();
    });

    let sys_row = adw::EntryRow::new();
    sys_row.set_title("Custom System Prompt (optional override)");
    sys_row.set_text(&config.borrow().custom_system_prompt);
    let cfg_s = config.clone();
    sys_row.connect_changed(move |r| {
        cfg_s.borrow_mut().custom_system_prompt = r.text().to_string();
    });

    let theme_row = adw::ComboRow::new();
    theme_row.set_title("Appearance Theme");
    theme_row.set_subtitle("Follow system dark/light preference or force a mode");
    theme_row.set_model(Some(&gtk4::StringList::new(&[
        "Follow System",
        "Light",
        "Dark",
    ])));
    theme_row.set_selected(match config.borrow().theme_mode.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    });
    let cfg_t = config.clone();
    theme_row.connect_selected_notify(move |r| {
        let mode = match r.selected() {
            1 => "light",
            2 => "dark",
            _ => "system",
        };
        cfg_t.borrow_mut().theme_mode = mode.to_string();
        super::apply_theme_mode(mode);
    });

    behavior_group.add(&view_row);
    behavior_group.add(&theme_row);
    behavior_group.add(&linux_row);
    behavior_group.add(&esc_row);
    behavior_group.add(&sys_row);

    page.add(&hypr_group);
    page.add(&behavior_group);
    (page, recorded_cb)
}

fn build_providers_page(config: Rc<RefCell<AppConfig>>, runtime: Rc<Runtime>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Native AI & Tokens");
    page.set_icon_name(Some("network-server-symbolic"));

    let guide_group = adw::PreferencesGroup::new();
    guide_group.set_title("Where to Get Keys vs Existing Chrome Login");
    guide_group.set_description(Some(
        "Your Chrome ChatGPT/Claude login uses web session cookies behind Cloudflare, not an API key. To use that account without paying for an API key, switch to the Web Portals tab (Ctrl+2) and sign in once.\n\
         For Native Fast Chat (Ctrl+1), generate a free or paid API key from:\n\
         • Google Gemini (Free tier): https://aistudio.google.com/apikey\n\
         • Groq (Free tier): https://console.groq.com/keys\n\
         • OpenRouter (Free models): https://openrouter.ai/keys\n\
         • OpenAI: https://platform.openai.com/api-keys  |  Claude: https://console.anthropic.com\n\
         • DeepSeek: https://platform.deepseek.com/api_keys  |  Local Ollama: no key required",
    ));

    let group = adw::PreferencesGroup::new();
    group.set_title("API Keys &amp; Bearer Token Profiles");
    group.set_description(Some(
        "Configure API keys, Ollama local endpoints, or custom Bearer tokens &amp; headers.",
    ));

    let provider_names: Vec<String> = config
        .borrow()
        .providers
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let name_refs: Vec<&str> = provider_names.iter().map(|s| s.as_str()).collect();

    let selector = adw::ComboRow::new();
    selector.set_title("Provider Profile");
    selector.set_model(Some(&gtk4::StringList::new(&name_refs)));

    let auth_row = adw::ComboRow::new();
    auth_row.set_title("Authentication Mode");
    auth_row.set_model(Some(&gtk4::StringList::new(&[
        "API Key",
        "Bearer Token",
        "Custom Headers Only",
        "None (Local)",
    ])));

    let token_row = adw::PasswordEntryRow::new();
    token_row.set_title("API Key / Pasted Token");

    let base_url_row = adw::EntryRow::new();
    base_url_row.set_title("Base URL");

    let model_combo = adw::ComboRow::new();
    model_combo.set_title("Available Models (lightweight first)");

    let fetch_btn = gtk4::Button::with_label("Fetch Models");
    fetch_btn.set_valign(gtk4::Align::Center);
    model_combo.add_suffix(&fetch_btn);

    let model_row = adw::EntryRow::new();
    model_row.set_title("Selected / Custom Model ID");

    let headers_row = adw::EntryRow::new();
    headers_row.set_title("Extra Headers (comma-sep Key:Value)");

    let save_btn = gtk4::Button::with_label("Save Profile");
    save_btn.add_css_class("suggested-action");
    save_btn.set_valign(gtk4::Align::Center);
    let save_row = adw::ActionRow::new();
    save_row.set_title("Save credentials &amp; auto-fetch models");
    save_row.add_suffix(&save_btn);

    let saved_lbl = gtk4::Label::new(None);
    saved_lbl.add_css_class("dim-label");
    saved_lbl.set_halign(gtk4::Align::Start);
    saved_lbl.set_wrap(true);

    let syncing = Rc::new(Cell::new(false));
    let fetched_models: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let update_model_combo = {
        let model_combo = model_combo.clone();
        let fetched_models = fetched_models.clone();
        let syncing = syncing.clone();
        move |models: &[String], current_model: &str| {
            let mut list: Vec<String> = models.to_vec();
            if list.is_empty() && !current_model.trim().is_empty() {
                list.push(current_model.trim().to_string());
            } else if !current_model.trim().is_empty() && !list.iter().any(|m| m == current_model.trim()) {
                list.push(current_model.trim().to_string());
            }
            let selected = list
                .iter()
                .position(|m| m == current_model.trim())
                .unwrap_or(0);
            let refs: Vec<&str> = list.iter().map(|s| s.as_str()).collect();
            *fetched_models.borrow_mut() = list.clone();
            syncing.set(true);
            model_combo.set_model(Some(&gtk4::StringList::new(&refs)));
            if !refs.is_empty() {
                model_combo.set_selected(selected as u32);
            }
            syncing.set(false);
        }
    };

    let populate = {
        let cfg = config.clone();
        let auth_row = auth_row.clone();
        let token_row = token_row.clone();
        let base_url_row = base_url_row.clone();
        let model_row = model_row.clone();
        let headers_row = headers_row.clone();
        let update_model_combo = update_model_combo.clone();
        move |idx: usize| {
            let snapshot = cfg.borrow().providers.get(idx).cloned();
            if let Some(p) = snapshot {
                auth_row.set_selected(auth_to_idx(&p.auth_mode));
                token_row.set_text(&p.token);
                base_url_row.set_text(&p.base_url);
                model_row.set_text(&p.model);
                let hdrs = p
                    .extra_headers
                    .iter()
                    .map(|(k, v)| format!("{k}:{v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                headers_row.set_text(&hdrs);
                update_model_combo(&p.available_models, &p.model);
            }
        }
    };

    let active_idx = config
        .borrow()
        .providers
        .iter()
        .position(|p| p.id == config.borrow().active_provider_id)
        .unwrap_or(0);
    selector.set_selected(active_idx as u32);
    populate(active_idx);

    {
        let syncing_sel = syncing.clone();
        let fetched_sel = fetched_models.clone();
        let mod_row_sel = model_row.clone();
        model_combo.connect_selected_notify(move |c| {
            if syncing_sel.get() {
                return;
            }
            let idx = c.selected() as usize;
            if let Some(m) = fetched_sel.borrow().get(idx).cloned() {
                mod_row_sel.set_text(&m);
            }
        });
    }

    let pop_clone = populate.clone();
    selector.connect_selected_notify(move |s| {
        pop_clone(s.selected() as usize);
    });

    let trigger_fetch = {
        let cfg_fetch = config.clone();
        let sel_ref = selector.clone();
        let auth_ref = auth_row.clone();
        let tok_ref = token_row.clone();
        let url_ref = base_url_row.clone();
        let hdr_ref = headers_row.clone();
        let mod_ref = model_row.clone();
        let lbl_ref = saved_lbl.clone();
        let fetch_btn_ref = fetch_btn.clone();
        let update_combo = update_model_combo.clone();
        let rt = runtime.clone();
        let http = reqwest::Client::new();
        move || {
            let idx = sel_ref.selected() as usize;
            let profile_opt = {
                let mut cfg = cfg_fetch.borrow_mut();
                if let Some(p) = cfg.providers.get_mut(idx) {
                    p.auth_mode = idx_to_auth(auth_ref.selected());
                    p.token = tok_ref.text().trim().to_string();
                    p.base_url = url_ref.text().trim().to_string();
                    p.extra_headers = parse_headers(&hdr_ref.text());
                    Some(p.clone())
                } else {
                    None
                }
            };
            let Some(profile) = profile_opt else { return };
            fetch_btn_ref.set_sensitive(false);
            lbl_ref.set_text(&format!("Fetching available models from {}...", profile.name));

            let (tx, rx) = oneshot::channel();
            let client = http.clone();
            let prof_for_req = profile.clone();
            rt.spawn(async move {
                let res = fetch_available_models(&client, &prof_for_req).await;
                let _ = tx.send(res);
            });

            let cfg_done = cfg_fetch.clone();
            let lbl_done = lbl_ref.clone();
            let mod_done = mod_ref.clone();
            let btn_done = fetch_btn_ref.clone();
            let upd_done = update_combo.clone();
            let sel_done = sel_ref.clone();
            gtk4::glib::MainContext::default().spawn_local(async move {
                if let Ok(res) = rx.await {
                    match res {
                        Ok(fetched) => {
                            let count = fetched.models.len();
                            let chosen = fetched.recommended.clone().unwrap_or_default();
                            {
                                let mut cfg = cfg_done.borrow_mut();
                                if let Some(p) = cfg.providers.get_mut(idx) {
                                    p.available_models = fetched.models.clone();
                                    if !chosen.is_empty() {
                                        p.model = chosen.clone();
                                    }
                                    cfg.active_provider_id = p.id.clone();
                                    let _ = cfg.save();
                                }
                            }
                            if sel_done.selected() as usize == idx {
                                if !chosen.is_empty() {
                                    mod_done.set_text(&chosen);
                                }
                                upd_done(&fetched.models, &chosen);
                            }
                            lbl_done.set_text(&format!(
                                "Fetched {count} models — selected lightweight model: {chosen}"
                            ));
                        }
                        Err(e) => {
                            lbl_done.set_text(&format!("Model fetch failed: {e:#}"));
                        }
                    }
                }
                btn_done.set_sensitive(true);
            });
        }
    };

    {
        let tf = trigger_fetch.clone();
        fetch_btn.connect_clicked(move |_| tf());
    }

    let cfg_save = config.clone();
    let sel_ref = selector.clone();
    let auth_ref = auth_row.clone();
    let tok_ref = token_row.clone();
    let url_ref = base_url_row.clone();
    let mod_ref = model_row.clone();
    let hdr_ref = headers_row.clone();
    let lbl_ref = saved_lbl.clone();
    let tf_on_save = trigger_fetch;
    save_btn.connect_clicked(move |_| {
        let idx = sel_ref.selected() as usize;
        let saved_info = {
            let mut cfg = cfg_save.borrow_mut();
            let info = if let Some(p) = cfg.providers.get_mut(idx) {
                p.auth_mode = idx_to_auth(auth_ref.selected());
                p.token = tok_ref.text().trim().to_string();
                p.base_url = url_ref.text().trim().to_string();
                p.model = mod_ref.text().trim().to_string();
                p.extra_headers = parse_headers(&hdr_ref.text());
                let should_fetch = p.available_models.is_empty()
                    && (!p.token.is_empty() || p.auth_mode == AuthMode::None);
                Some((p.id.clone(), p.name.clone(), should_fetch))
            } else {
                None
            };
            if let Some((ref id, _, _)) = info {
                cfg.active_provider_id = id.clone();
                let _ = cfg.save();
            }
            info
        };
        if let Some((_, name, should_fetch)) = saved_info {
            lbl_ref.set_text(&format!("Saved credentials for {name}"));
            if should_fetch {
                tf_on_save();
            }
        }
    });

    // Quick add custom OpenAI-compatible profile
    let add_group = adw::PreferencesGroup::new();
    add_group.set_title("Add Custom Provider");
    let custom_name = adw::EntryRow::new();
    custom_name.set_title("Display Name");
    let custom_url = adw::EntryRow::new();
    custom_url.set_title("Base URL (OpenAI-compatible)");
    let custom_model = adw::EntryRow::new();
    custom_model.set_title("Model ID");
    let add_btn = gtk4::Button::with_label("Add Provider");
    add_btn.set_valign(gtk4::Align::Center);
    let add_row = adw::ActionRow::new();
    add_row.set_title("Create custom OpenAI-compatible provider");
    add_row.add_suffix(&add_btn);

    let cfg_add = config.clone();
    let cn = custom_name.clone();
    let cu = custom_url.clone();
    let cm = custom_model.clone();
    let lbl_add = saved_lbl.clone();
    add_btn.connect_clicked(move |_| {
        let name = cn.text().trim().to_string();
        let url = cu.text().trim().to_string();
        let model = cm.text().trim().to_string();
        if name.is_empty() || url.is_empty() {
            lbl_add.set_text("Enter both a Name and Base URL.");
            return;
        }
        let id = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>();
        let mut cfg = cfg_add.borrow_mut();
        cfg.providers.push(ProviderProfile {
            id,
            name: name.clone(),
            kind: ProviderKind::OpenAiCompatible,
            auth_mode: AuthMode::BearerToken,
            base_url: url,
            model: if model.is_empty() { "default".into() } else { model },
            available_models: Vec::new(),
            token: String::new(),
            extra_headers: BTreeMap::new(),
        });
        let _ = cfg.save();
        cn.set_text("");
        cu.set_text("");
        cm.set_text("");
        lbl_add.set_text(&format!("Added '{name}'. Reopen Settings to configure its token."));
    });

    group.add(&selector);
    group.add(&auth_row);
    group.add(&token_row);
    group.add(&base_url_row);
    group.add(&model_combo);
    group.add(&model_row);
    group.add(&headers_row);
    group.add(&save_row);
    group.add(&saved_lbl);

    add_group.add(&custom_name);
    add_group.add(&custom_url);
    add_group.add(&custom_model);
    add_group.add(&add_row);

    page.add(&guide_group);
    page.add(&group);
    page.add(&add_group);
    page
}

fn build_portals_page(config: Rc<RefCell<AppConfig>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Web Portals");
    page.set_icon_name(Some("web-browser-symbolic"));

    let list_group = adw::PreferencesGroup::new();
    list_group.set_title("Configured AI Web Portals");
    list_group.set_description(Some(
        "Sign in once inside each Web Portal tab; cookies stay saved in ~/.local/share/hyprpilot/webdata.",
    ));

    let portals = config.borrow().portals.clone();
    for portal in portals {
        let row = adw::ActionRow::new();
        row.set_title(&portal.name);
        row.set_subtitle(&portal.url);
        let rm_btn = gtk4::Button::from_icon_name("user-trash-symbolic");
        rm_btn.set_valign(gtk4::Align::Center);
        rm_btn.add_css_class("flat");
        let pid = portal.id.clone();
        let cfg_rm = config.clone();
        let row_ref = row.clone();
        let grp_ref = list_group.clone();
        rm_btn.connect_clicked(move |_| {
            let mut cfg = cfg_rm.borrow_mut();
            if cfg.portals.len() > 1 {
                cfg.portals.retain(|p| p.id != pid);
                let _ = cfg.save();
                grp_ref.remove(&row_ref);
            }
        });
        row.add_suffix(&rm_btn);
        list_group.add(&row);
    }

    let add_group = adw::PreferencesGroup::new();
    add_group.set_title("Add Custom AI Web Portal");
    let p_name = adw::EntryRow::new();
    p_name.set_title("Portal Name (e.g. Perplexity)");
    let p_url = adw::EntryRow::new();
    p_url.set_title("URL (e.g. https://www.perplexity.ai)");

    let add_btn = gtk4::Button::with_label("Add Portal");
    add_btn.add_css_class("suggested-action");
    add_btn.set_valign(gtk4::Align::Center);
    let add_row = adw::ActionRow::new();
    add_row.set_title("Add to Web Portals bar");
    add_row.add_suffix(&add_btn);

    let cfg_add = config.clone();
    let pn = p_name.clone();
    let pu = p_url.clone();
    let grp_add = list_group.clone();
    add_btn.connect_clicked(move |_| {
        let name = pn.text().trim().to_string();
        let url = pu.text().trim().to_string();
        if name.is_empty() || url.is_empty() {
            return;
        }
        let id = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>();
        let entry = PortalEntry {
            id,
            name: name.clone(),
            url: url.clone(),
        };
        {
            let mut cfg = cfg_add.borrow_mut();
            cfg.portals.push(entry);
            let _ = cfg.save();
        }
        let row = adw::ActionRow::new();
        row.set_title(&name);
        row.set_subtitle(&url);
        grp_add.add(&row);
        pn.set_text("");
        pu.set_text("");
    });

    add_group.add(&p_name);
    add_group.add(&p_url);
    add_group.add(&add_row);

    page.add(&list_group);
    page.add(&add_group);
    page
}

fn auth_to_idx(mode: &AuthMode) -> u32 {
    match mode {
        AuthMode::ApiKey => 0,
        AuthMode::BearerToken => 1,
        AuthMode::CustomHeaders => 2,
        AuthMode::None => 3,
    }
}

fn idx_to_auth(idx: u32) -> AuthMode {
    match idx {
        0 => AuthMode::ApiKey,
        1 => AuthMode::BearerToken,
        2 => AuthMode::CustomHeaders,
        _ => AuthMode::None,
    }
}

fn parse_headers(raw: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in raw.split(',') {
        if let Some((k, v)) = pair.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if !k.is_empty() {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

fn build_hotkey_selector_row(
    title: &str,
    initial_hotkey: &str,
) -> (adw::ActionRow, gtk4::DropDown, gtk4::ToggleButton) {
    let row = adw::ActionRow::new();
    row.set_title(title);

    let mut items: Vec<&str> = HOTKEY_PRESETS.to_vec();
    items.push("Custom (use Record...)");
    let dd = gtk4::DropDown::from_strings(&items);
    dd.set_valign(gtk4::Align::Center);

    let rec_btn = gtk4::ToggleButton::with_label("Record...");
    rec_btn.set_valign(gtk4::Align::Center);

    let suffix = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    suffix.append(&dd);
    suffix.append(&rec_btn);
    row.add_suffix(&suffix);

    let (norm, _) = format_hyprland_hotkey(initial_hotkey);
    update_hotkey_row_widgets(&row, &dd, &norm);

    (row, dd, rec_btn)
}

fn update_hotkey_row_widgets(row: &adw::ActionRow, dd: &gtk4::DropDown, norm_hotkey: &str) {
    row.set_subtitle(&format!("Active: {norm_hotkey}"));
    let idx = HOTKEY_PRESETS
        .iter()
        .position(|&p| p == norm_hotkey)
        .unwrap_or(HOTKEY_PRESETS.len());
    dd.set_selected(idx as u32);
}

fn gdk_event_to_hyprland_combo(
    key: gtk4::gdk::Key,
    mods: gtk4::gdk::ModifierType,
) -> Option<String> {
    use gtk4::gdk::{Key, ModifierType};

    if matches!(
        key,
        Key::Shift_L
            | Key::Shift_R
            | Key::Control_L
            | Key::Control_R
            | Key::Alt_L
            | Key::Alt_R
            | Key::Meta_L
            | Key::Meta_R
            | Key::Super_L
            | Key::Super_R
            | Key::ISO_Level3_Shift
    ) {
        return None;
    }

    let mut parts = Vec::new();
    if mods.contains(ModifierType::SUPER_MASK) || mods.contains(ModifierType::META_MASK) {
        parts.push("SUPER".to_string());
    }
    if mods.contains(ModifierType::ALT_MASK) {
        parts.push("ALT".to_string());
    }
    if mods.contains(ModifierType::CONTROL_MASK) {
        parts.push("CTRL".to_string());
    }
    if mods.contains(ModifierType::SHIFT_MASK) {
        parts.push("SHIFT".to_string());
    }

    let key_name = match key {
        Key::space => "SPACE".to_string(),
        Key::Return | Key::KP_Enter => "RETURN".to_string(),
        Key::Tab | Key::ISO_Left_Tab => "TAB".to_string(),
        Key::grave | Key::asciitilde => "GRAVE".to_string(),
        Key::BackSpace => "BACKSPACE".to_string(),
        _ => {
            let raw = key.name()?.to_string();
            if raw.len() == 1 {
                raw.to_uppercase()
            } else {
                raw.to_uppercase()
            }
        }
    };

    parts.push(key_name);
    Some(parts.join(" + "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk4::gdk::{Key, ModifierType};

    #[test]
    fn converts_gdk_combo_to_hyprland_format() {
        assert_eq!(
            gdk_event_to_hyprland_combo(Key::Shift_L, ModifierType::SHIFT_MASK),
            None
        );
        assert_eq!(
            gdk_event_to_hyprland_combo(
                Key::space,
                ModifierType::ALT_MASK | ModifierType::SHIFT_MASK
            ),
            Some("ALT + SHIFT + SPACE".to_string())
        );
        assert_eq!(
            gdk_event_to_hyprland_combo(Key::a, ModifierType::SUPER_MASK),
            Some("SUPER + A".to_string())
        );
    }
}
