use gtk4::gdk;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use super::native_chat::NativeChatPane;
use super::settings::show_settings_dialog;
use super::web_portal::WebPortalPane;
use crate::APP_NAME;
use crate::hyprland::hide_window;
use crate::storage::config::AppConfig;

pub fn build_main_window(app: &adw::Application) -> adw::ApplicationWindow {
    let config = Rc::new(RefCell::new(AppConfig::load().unwrap_or_default()));
    let runtime = Rc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(APP_NAME)
        .default_width(920)
        .default_height(620)
        .build();
    window.add_css_class("hyprpilot-window");

    let view_stack = adw::ViewStack::new();
    let native_pane = NativeChatPane::new(config.clone(), runtime);
    let portal_pane = Rc::new(WebPortalPane::new(config.clone()));

    let native_page = view_stack.add_titled(&native_pane.root, Some("native"), "Native Chat");
    native_page.set_icon_name(Some("chat-message-new-symbolic"));

    let portal_page = view_stack.add_titled(&portal_pane.root, Some("portal"), "Web Portals");
    portal_page.set_icon_name(Some("web-browser-symbolic"));

    if config.borrow().default_view == "portal" {
        view_stack.set_visible_child_name("portal");
        portal_pane.ensure_active_loaded();
    } else {
        view_stack.set_visible_child_name("native");
    }

    let switcher = adw::ViewSwitcher::new();
    switcher.set_stack(Some(&view_stack));
    switcher.set_policy(adw::ViewSwitcherPolicy::Wide);

    let fs_btn = gtk4::Button::from_icon_name("view-fullscreen-symbolic");
    fs_btn.set_tooltip_text(Some("Toggle Fullscreen (F11)"));

    let hide_btn = gtk4::Button::from_icon_name("window-minimize-symbolic");
    hide_btn.set_tooltip_text(Some("Hide (Esc or Toggle Hotkey)"));

    let settings_btn = gtk4::Button::from_icon_name("emblem-system-symbolic");
    settings_btn.set_tooltip_text(Some("Settings (Ctrl+,)"));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));
    header.pack_start(&settings_btn);
    header.pack_end(&hide_btn);
    header.pack_end(&fs_btn);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&view_stack));
    window.set_content(Some(&toolbar_view));

    // Lazy-load WebKit only when the user actually switches to Web Portals
    {
        let pp = portal_pane.clone();
        let np = native_pane.clone();
        view_stack.connect_visible_child_name_notify(move |vs| {
            if vs.visible_child_name().as_deref() == Some("portal") {
                pp.ensure_active_loaded();
            } else {
                np.focus_input();
            }
        });
    }

    // Fullscreen toggle
    {
        let win = window.clone();
        fs_btn.connect_clicked(move |_| toggle_fullscreen(&win));
    }

    // Hide button
    hide_btn.connect_clicked(|_| {
        let _ = hide_window();
    });

    // Settings dialog
    {
        let win = window.clone();
        let cfg = config.clone();
        let np = native_pane.clone();
        let pp = portal_pane.clone();
        settings_btn.connect_clicked(move |_| {
            let np2 = np.clone();
            let pp2 = pp.clone();
            show_settings_dialog(&win, cfg.clone(), move |_| {
                np2.sync_providers_from_config();
                pp2.rebuild_tabs();
            });
        });
    }

    // Global keyboard shortcuts
    let key_ctrl = gtk4::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let win = window.clone();
        let vs = view_stack.clone();
        let np = native_pane.clone();
        let pp = portal_pane.clone();
        let cfg = config.clone();
        key_ctrl.connect_key_pressed(move |_, key, _, mods| {
            if key == gdk::Key::F11 {
                toggle_fullscreen(&win);
                return gtk4::glib::Propagation::Stop;
            }
            if key == gdk::Key::Escape && cfg.borrow().hide_on_escape {
                let _ = hide_window();
                return gtk4::glib::Propagation::Stop;
            }
            if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                match key {
                    gdk::Key::_1 => {
                        vs.set_visible_child_name("native");
                        return gtk4::glib::Propagation::Stop;
                    }
                    gdk::Key::_2 => {
                        vs.set_visible_child_name("portal");
                        return gtk4::glib::Propagation::Stop;
                    }
                    gdk::Key::n | gdk::Key::N => {
                        vs.set_visible_child_name("native");
                        np.new_chat();
                        return gtk4::glib::Propagation::Stop;
                    }
                    gdk::Key::comma => {
                        let np2 = np.clone();
                        let pp2 = pp.clone();
                        show_settings_dialog(&win, cfg.clone(), move |_| {
                            np2.sync_providers_from_config();
                            pp2.rebuild_tabs();
                        });
                        return gtk4::glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            gtk4::glib::Propagation::Proceed
        });
    }
    window.add_controller(key_ctrl);

    // Focus composer when window becomes active
    {
        let np = native_pane.clone();
        let vs = view_stack.clone();
        window.connect_is_active_notify(move |w| {
            if w.is_active() && vs.visible_child_name().as_deref() == Some("native") {
                np.focus_input();
            }
        });
    }

    native_pane.focus_input();
    window
}

fn toggle_fullscreen(win: &adw::ApplicationWindow) {
    if win.is_fullscreen() {
        win.unfullscreen();
    } else {
        win.fullscreen();
    }
}
