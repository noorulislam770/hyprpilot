use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::hyprland::sync_hyprland_config;
use crate::storage::config::{AppConfig, AuthMode, PortalEntry, ProviderKind, ProviderProfile};

pub fn show_settings_dialog<F>(
    parent: &adw::ApplicationWindow,
    config: Rc<RefCell<AppConfig>>,
    on_saved: F,
) where
    F: Fn(&AppConfig) + 'static,
{
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("HyprPilot Settings");
    dialog.set_content_width(640);
    dialog.set_content_height(580);

    let general_page = build_general_page(config.clone());
    let providers_page = build_providers_page(config.clone());
    let portals_page = build_portals_page(config.clone());

    dialog.add(&general_page);
    dialog.add(&providers_page);
    dialog.add(&portals_page);

    let on_saved = Rc::new(on_saved);
    let cfg_close = config.clone();
    dialog.connect_closed(move |_| {
        let cfg = cfg_close.borrow().clone();
        let _ = cfg.save();
        on_saved(&cfg);
    });

    dialog.present(Some(parent));
}

fn build_general_page(config: Rc<RefCell<AppConfig>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("General & Hyprland");
    page.set_icon_name(Some("preferences-system-symbolic"));

    // Hyprland Hotkey Group
    let hypr_group = adw::PreferencesGroup::new();
    hypr_group.set_title("Hyprland Toggle & Hotkey");
    hypr_group.set_description(Some(
        "Default is ALT + SPACE. Changing it updates ~/.config/hypr/hyprpilot.lua & .conf.",
    ));

    let hotkey_row = adw::EntryRow::new();
    hotkey_row.set_title("Toggle Hotkey (e.g. ALT + SPACE, SUPER + A)");
    hotkey_row.set_text(&config.borrow().toggle_hotkey);

    let status_label = gtk4::Label::new(None);
    status_label.add_css_class("dim-label");
    status_label.set_halign(gtk4::Align::Start);

    let apply_btn = gtk4::Button::with_label("Apply & Reload Hyprland");
    apply_btn.add_css_class("suggested-action");
    apply_btn.set_valign(gtk4::Align::Center);

    let apply_row = adw::ActionRow::new();
    apply_row.set_title("Write Hyprland config & reload");
    apply_row.set_subtitle("Adds 50% centered floating rule + configurable toggle bind");
    apply_row.add_suffix(&apply_btn);

    let cfg_apply = config.clone();
    let hk_ref = hotkey_row.clone();
    let st_ref = status_label.clone();
    apply_btn.connect_clicked(move |_| {
        let new_hotkey = hk_ref.text().to_string();
        {
            let mut cfg = cfg_apply.borrow_mut();
            cfg.toggle_hotkey = if new_hotkey.trim().is_empty() {
                "ALT + SPACE".to_string()
            } else {
                new_hotkey.trim().to_string()
            };
            let _ = cfg.save();
        }
        let cfg = cfg_apply.borrow().clone();
        match sync_hyprland_config(&cfg, true) {
            Ok(path) => st_ref.set_text(&format!("Saved {} and reloaded Hyprland", path.display())),
            Err(e) => st_ref.set_text(&format!("Error: {e:#}")),
        }
    });

    hypr_group.add(&hotkey_row);
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
    esc_row.set_subtitle("Pressing Esc hides HyprPilot back to the special workspace");
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

    behavior_group.add(&view_row);
    behavior_group.add(&linux_row);
    behavior_group.add(&esc_row);
    behavior_group.add(&sys_row);

    page.add(&hypr_group);
    page.add(&behavior_group);
    page
}

fn build_providers_page(config: Rc<RefCell<AppConfig>>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    page.set_title("Native AI & Tokens");
    page.set_icon_name(Some("network-server-symbolic"));

    let group = adw::PreferencesGroup::new();
    group.set_title("API Keys & Bearer Token Profiles");
    group.set_description(Some(
        "Configure API keys, Ollama local endpoints, or custom Bearer tokens & headers.",
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

    let model_row = adw::EntryRow::new();
    model_row.set_title("Model ID");

    let headers_row = adw::EntryRow::new();
    headers_row.set_title("Extra Headers (comma-sep Key:Value)");

    let save_btn = gtk4::Button::with_label("Save Profile");
    save_btn.add_css_class("suggested-action");
    save_btn.set_valign(gtk4::Align::Center);
    let save_row = adw::ActionRow::new();
    save_row.set_title("Save selected provider credentials");
    save_row.add_suffix(&save_btn);

    let saved_lbl = gtk4::Label::new(None);
    saved_lbl.add_css_class("dim-label");
    saved_lbl.set_halign(gtk4::Align::Start);

    let populate = {
        let cfg = config.clone();
        let auth_row = auth_row.clone();
        let token_row = token_row.clone();
        let base_url_row = base_url_row.clone();
        let model_row = model_row.clone();
        let headers_row = headers_row.clone();
        move |idx: usize| {
            let guard = cfg.borrow();
            if let Some(p) = guard.providers.get(idx) {
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

    let pop_clone = populate.clone();
    selector.connect_selected_notify(move |s| {
        pop_clone(s.selected() as usize);
    });

    let cfg_save = config.clone();
    let sel_ref = selector.clone();
    let auth_ref = auth_row.clone();
    let tok_ref = token_row.clone();
    let url_ref = base_url_row.clone();
    let mod_ref = model_row.clone();
    let hdr_ref = headers_row.clone();
    let lbl_ref = saved_lbl.clone();
    save_btn.connect_clicked(move |_| {
        let idx = sel_ref.selected() as usize;
        let mut cfg = cfg_save.borrow_mut();
        let saved_info = if let Some(p) = cfg.providers.get_mut(idx) {
            p.auth_mode = idx_to_auth(auth_ref.selected());
            p.token = tok_ref.text().trim().to_string();
            p.base_url = url_ref.text().trim().to_string();
            p.model = mod_ref.text().trim().to_string();
            p.extra_headers = parse_headers(&hdr_ref.text());
            Some((p.id.clone(), p.name.clone()))
        } else {
            None
        };
        if let Some((id, name)) = saved_info {
            cfg.active_provider_id = id;
            let _ = cfg.save();
            lbl_ref.set_text(&format!("Saved credentials for {name}"));
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
    group.add(&model_row);
    group.add(&headers_row);
    group.add(&save_row);
    group.add(&saved_lbl);

    add_group.add(&custom_name);
    add_group.add(&custom_url);
    add_group.add(&custom_model);
    add_group.add(&add_row);

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
