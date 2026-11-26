use gtk4::gdk;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::rc::Rc;
use tokio::sync::mpsc;

use super::native_chat::NativeChatPane;
use super::settings::show_settings_dialog;
use super::web_portal::WebPortalPane;
use crate::APP_NAME;
use crate::hyprland::snapshot_and_save_geometry;
use crate::storage::config::{AppConfig, socket_path};

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
        .hide_on_close(true)
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
    hide_btn.set_tooltip_text(Some("Hide to Background (Esc or Toggle Hotkey)"));

    let quit_btn = gtk4::Button::from_icon_name("application-exit-symbolic");
    quit_btn.set_tooltip_text(Some("Quit HyprPilot Completely (Ctrl+Q)"));

    let settings_btn = gtk4::Button::from_icon_name("emblem-system-symbolic");
    settings_btn.set_tooltip_text(Some("Settings (Ctrl+,)"));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));
    header.pack_start(&settings_btn);
    header.pack_end(&quit_btn);
    header.pack_end(&hide_btn);
    header.pack_end(&fs_btn);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&view_stack));
    window.set_content(Some(&toolbar_view));

    start_ipc_server(window.clone(), app.clone());

    // Clicking window close button hides to background rather than exiting
    {
        let win = window.clone();
        window.connect_close_request(move |_| {
            hide_to_background(&win);
            gtk4::glib::Propagation::Stop
        });
    }

    // Lazy-load WebKit only when the user switches to Web Portals
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
    {
        let win = window.clone();
        hide_btn.connect_clicked(move |_| hide_to_background(&win));
    }

    // Quit button
    {
        let app_ref = app.clone();
        quit_btn.connect_clicked(move |_| quit_app(&app_ref));
    }

    // Settings window (separate modal window)
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

    // Keyboard shortcuts (Bubble phase so modal dialogs & popovers handle Esc first)
    let key_ctrl = gtk4::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Bubble);
    {
        let win = window.clone();
        let app_ref = app.clone();
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
                hide_to_background(&win);
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
                    gdk::Key::q | gdk::Key::Q => {
                        quit_app(&app_ref);
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

fn hide_to_background(win: &adw::ApplicationWindow) {
    let _ = snapshot_and_save_geometry();
    if win.is_fullscreen() {
        win.unfullscreen();
    }
    win.set_visible(false);
}

fn quit_app(app: &adw::Application) {
    let _ = fs::remove_file(socket_path());
    app.quit();
}

fn start_ipc_server(win: adw::ApplicationWindow, app: adw::Application) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let sock = socket_path();
    if let Some(parent) = sock.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::remove_file(&sock);

    std::thread::spawn(move || {
        let Ok(listener) = UnixListener::bind(&sock) else {
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&sock, fs::Permissions::from_mode(0o600));
        }
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let cmd = line.trim().to_uppercase();
                if !cmd.is_empty() {
                    let _ = tx.send(cmd);
                }
            }
        }
    });

    gtk4::glib::MainContext::default().spawn_local(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd.as_str() {
                "SHOW" => {
                    win.set_visible(true);
                    win.present();
                }
                "HIDE" => {
                    hide_to_background(&win);
                }
                "QUIT" => {
                    quit_app(&app);
                    break;
                }
                _ => {}
            }
        }
    });
}

fn toggle_fullscreen(win: &adw::ApplicationWindow) {
    if win.is_fullscreen() {
        win.unfullscreen();
    } else {
        win.fullscreen();
    }
}
