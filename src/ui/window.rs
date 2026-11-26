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
use super::preview::PreviewPane;
use super::settings::show_settings_dialog;
use super::web_portal::WebPortalPane;
use crate::APP_NAME;
use crate::hyprland::snapshot_and_save_geometry;
use crate::preview::expand_user_path;
use crate::storage::config::{AppConfig, socket_path};

pub fn build_main_window(app: &adw::Application) -> adw::ApplicationWindow {
    build_main_window_inner(app, true, None)
}

pub fn build_main_window_with_preview(
    app: &adw::Application,
    initial_preview: Option<&str>,
) -> adw::ApplicationWindow {
    build_main_window_inner(app, true, initial_preview)
}

fn build_main_window_inner(
    app: &adw::Application,
    enable_ipc: bool,
    initial_preview: Option<&str>,
) -> adw::ApplicationWindow {
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
        .default_width(980)
        .default_height(640)
        .hide_on_close(true)
        .build();
    window.add_css_class("hyprpilot-window");

    let native_pane = NativeChatPane::new(config.clone(), runtime.clone());
    let portal_pane = Rc::new(WebPortalPane::new(config.clone()));
    let preview_pane = Rc::new(PreviewPane::new(Some(window.clone().upcast())));

    let view_stack = adw::ViewStack::new();
    let native_page = view_stack.add_titled(&native_pane.root, Some("native"), "Chat");
    native_page.set_icon_name(Some("chat-message-new-symbolic"));
    let portal_page = view_stack.add_titled(&portal_pane.root, Some("portal"), "Web Portals");
    portal_page.set_icon_name(Some("web-browser-symbolic"));
    let preview_page = view_stack.add_titled(&preview_pane.root, Some("preview"), "Preview");
    preview_page.set_icon_name(Some("document-open-symbolic"));

    // Claude-style OverlaySplitView: collapsible sidebar on left, view_stack on right
    let split = adw::OverlaySplitView::new();
    split.set_min_sidebar_width(235.0);
    split.set_max_sidebar_width(290.0);
    split.set_sidebar_width_fraction(0.27);
    split.set_show_sidebar(false);

    // Sidebar footer: Chat / Web Portals / Settings pills
    let sidebar_box = native_pane.sidebar.clone();
    let footer_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    footer_box.add_css_class("claude-sidebar-footer");

    let nav_chat_btn = gtk4::ToggleButton::with_label("Native Chat  (Ctrl+1)");
    nav_chat_btn.add_css_class("flat");
    nav_chat_btn.add_css_class("claude-nav-pill");

    let nav_portal_btn = gtk4::ToggleButton::with_label("Web Portals  (Ctrl+2)");
    nav_portal_btn.add_css_class("flat");
    nav_portal_btn.add_css_class("claude-nav-pill");

    let nav_preview_btn = gtk4::ToggleButton::with_label("Preview Engine  (Ctrl+3)");
    nav_preview_btn.add_css_class("flat");
    nav_preview_btn.add_css_class("claude-nav-pill");

    let sidebar_settings_btn = gtk4::Button::with_label("Settings  (Ctrl+,)");
    sidebar_settings_btn.add_css_class("flat");
    sidebar_settings_btn.add_css_class("claude-nav-pill");

    footer_box.append(&nav_chat_btn);
    footer_box.append(&nav_portal_btn);
    footer_box.append(&nav_preview_btn);
    footer_box.append(&sidebar_settings_btn);
    sidebar_box.append(&footer_box);

    split.set_sidebar(Some(&sidebar_box));

    // Minimal Claude-style top headerbar
    let sidebar_btn = gtk4::ToggleButton::new();
    sidebar_btn.set_icon_name("sidebar-show-symbolic");
    sidebar_btn.set_tooltip_text(Some("Toggle Sidebar (Ctrl+B)"));
    sidebar_btn.set_active(false);

    let chat_title_lbl = gtk4::Label::new(Some("New chat"));
    chat_title_lbl.add_css_class("claude-header-title");
    chat_title_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    chat_title_lbl.set_max_width_chars(34);

    let switcher = adw::ViewSwitcher::new();
    switcher.set_stack(Some(&view_stack));
    switcher.set_policy(adw::ViewSwitcherPolicy::Narrow);

    let fs_btn = gtk4::Button::from_icon_name("view-fullscreen-symbolic");
    fs_btn.set_tooltip_text(Some("Toggle Fullscreen (F11)"));
    let hide_btn = gtk4::Button::from_icon_name("window-minimize-symbolic");
    hide_btn.set_tooltip_text(Some("Hide to Background (Esc)"));
    let quit_btn = gtk4::Button::from_icon_name("application-exit-symbolic");
    quit_btn.set_tooltip_text(Some("Quit HyprPilot Completely (Ctrl+Q)"));
    let settings_btn = gtk4::Button::from_icon_name("emblem-system-symbolic");
    settings_btn.set_tooltip_text(Some("Settings (Ctrl+,)"));

    let left_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    left_box.append(&sidebar_btn);
    left_box.append(&chat_title_lbl);

    let header = adw::HeaderBar::new();
    header.add_css_class("claude-header");
    header.pack_start(&left_box);
    header.set_title_widget(Some(&switcher));
    header.pack_end(&quit_btn);
    header.pack_end(&hide_btn);
    header.pack_end(&fs_btn);
    header.pack_end(&settings_btn);

    let main_toolbar = adw::ToolbarView::new();
    main_toolbar.add_top_bar(&header);
    main_toolbar.set_content(Some(&view_stack));
    split.set_content(Some(&main_toolbar));
    window.set_content(Some(&split));

    if let Some(path_str) = initial_preview {
        preview_pane.open_file(&expand_user_path(path_str));
        view_stack.set_visible_child_name("preview");
        nav_preview_btn.set_active(true);
        chat_title_lbl.set_text("Preview");
    } else if config.borrow().default_view == "portal" {
        view_stack.set_visible_child_name("portal");
        nav_portal_btn.set_active(true);
        portal_pane.ensure_active_loaded();
    } else {
        view_stack.set_visible_child_name("native");
        nav_chat_btn.set_active(true);
    }

    // Wire Claude-style Artifact "Preview ↗" button inside chat code blocks
    {
        let pp = preview_pane.clone();
        let vs = view_stack.clone();
        native_pane.set_on_preview(move |kind, title, code| {
            pp.preview_inline(kind, &title, &code);
            vs.set_visible_child_name("preview");
        });
    }

    // Update header title when active chat changes
    {
        let lbl = chat_title_lbl.clone();
        native_pane.set_on_title_change(move |t| lbl.set_text(t));
    }

    // Sync sidebar toggle button <-> OverlaySplitView
    {
        let sp = split.clone();
        sidebar_btn.connect_toggled(move |b| sp.set_show_sidebar(b.is_active()));
    }
    {
        let sb = sidebar_btn.clone();
        split.connect_show_sidebar_notify(move |sp| {
            if sb.is_active() != sp.shows_sidebar() {
                sb.set_active(sp.shows_sidebar());
            }
        });
    }

    // Sidebar nav pills <-> ViewStack
    {
        let vs = view_stack.clone();
        nav_chat_btn.connect_clicked(move |b| {
            if b.is_active() {
                vs.set_visible_child_name("native");
            }
        });
    }
    {
        let vs = view_stack.clone();
        nav_portal_btn.connect_clicked(move |b| {
            if b.is_active() {
                vs.set_visible_child_name("portal");
            }
        });
    }
    {
        let vs = view_stack.clone();
        nav_preview_btn.connect_clicked(move |b| {
            if b.is_active() {
                vs.set_visible_child_name("preview");
            }
        });
    }

    // Lazy-load WebKit only when switching to Web Portals & sync sidebar pills
    {
        let pp = portal_pane.clone();
        let np = native_pane.clone();
        let nc = nav_chat_btn.clone();
        let npb = nav_portal_btn.clone();
        let npr = nav_preview_btn.clone();
        let title_lbl = chat_title_lbl.clone();
        view_stack.connect_visible_child_name_notify(move |vs| {
            let name = vs.visible_child_name();
            let active = name.as_deref().unwrap_or("native");
            nc.set_active(active == "native");
            npb.set_active(active == "portal");
            npr.set_active(active == "preview");
            match active {
                "portal" => {
                    title_lbl.set_text("Web Portals");
                    pp.ensure_active_loaded();
                }
                "preview" => {
                    title_lbl.set_text("Preview Engine");
                }
                _ => {
                    np.focus_input();
                }
            }
        });
    }

    if enable_ipc {
        start_ipc_server(
            window.clone(),
            app.clone(),
            preview_pane.clone(),
            view_stack.clone(),
        );
    }

    // Clicking window close button hides to background rather than exiting
    {
        let win = window.clone();
        let app_ref = app.clone();
        window.connect_close_request(move |_| {
            hide_to_background(&win, &app_ref);
            gtk4::glib::Propagation::Stop
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
        let app_ref = app.clone();
        hide_btn.connect_clicked(move |_| hide_to_background(&win, &app_ref));
    }

    // Quit button
    {
        let app_ref = app.clone();
        quit_btn.connect_clicked(move |_| quit_app(&app_ref));
    }

    // Settings launchers (headerbar + sidebar)
    let open_settings: Rc<dyn Fn()> = Rc::new({
        let win = window.clone();
        let cfg = config.clone();
        let np = native_pane.clone();
        let pp = portal_pane.clone();
        let rt = runtime.clone();
        move || {
            let np2 = np.clone();
            let pp2 = pp.clone();
            show_settings_dialog(&win, cfg.clone(), rt.clone(), move |_| {
                np2.sync_providers_from_config();
                pp2.rebuild_tabs();
            });
        }
    });
    {
        let open = open_settings.clone();
        settings_btn.connect_clicked(move |_| open());
    }
    {
        let open = open_settings.clone();
        sidebar_settings_btn.connect_clicked(move |_| open());
    }

    // Keyboard shortcuts (Bubble phase so modal dialogs & popovers handle Esc first)
    let key_ctrl = gtk4::EventControllerKey::new();
    key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Bubble);
    {
        let win = window.clone();
        let app_ref = app.clone();
        let vs = view_stack.clone();
        let np = native_pane.clone();
        let sp = split.clone();
        let cfg = config.clone();
        let open_set = open_settings.clone();
        key_ctrl.connect_key_pressed(move |_, key, _, mods| {
            if key == gdk::Key::F11 {
                toggle_fullscreen(&win);
                return gtk4::glib::Propagation::Stop;
            }
            if key == gdk::Key::Escape && cfg.borrow().hide_on_escape {
                hide_to_background(&win, &app_ref);
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
                    gdk::Key::_3 => {
                        vs.set_visible_child_name("preview");
                        return gtk4::glib::Propagation::Stop;
                    }
                    gdk::Key::b | gdk::Key::B | gdk::Key::backslash => {
                        sp.set_show_sidebar(!sp.shows_sidebar());
                        return gtk4::glib::Propagation::Stop;
                    }
                    gdk::Key::k | gdk::Key::K => {
                        sp.set_show_sidebar(true);
                        vs.set_visible_child_name("native");
                        np.focus_search();
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
                        open_set();
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

fn hide_to_background(win: &adw::ApplicationWindow, app: &adw::Application) {
    let main_win = win.upcast_ref::<gtk4::Window>();
    for sub in app.windows() {
        if &sub != main_win {
            sub.close();
        }
    }
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

fn start_ipc_server(
    win: adw::ApplicationWindow,
    app: adw::Application,
    preview_pane: Rc<PreviewPane>,
    view_stack: adw::ViewStack,
) {
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
                let cmd = line.trim().to_string();
                if !cmd.is_empty() {
                    let _ = tx.send(cmd);
                }
            }
        }
    });

    gtk4::glib::MainContext::default().spawn_local(async move {
        while let Some(cmd) = rx.recv().await {
            if let Some(raw_path) = cmd.strip_prefix("PREVIEW:") {
                preview_pane.open_file(&expand_user_path(raw_path));
                view_stack.set_visible_child_name("preview");
                win.set_visible(true);
                win.present();
                continue;
            }
            match cmd.to_ascii_uppercase().as_str() {
                "SHOW" => {
                    win.set_visible(true);
                    win.present();
                }
                "HIDE" => {
                    hide_to_background(&win, &app);
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

pub fn run_gui_self_test(app: &adw::Application) {
    let config = Rc::new(RefCell::new(AppConfig::load().unwrap_or_default()));
    let runtime = Rc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );

    let win = build_main_window_inner(app, false, None);
    assert_eq!(win.title().as_deref(), Some(APP_NAME));

    let preview = PreviewPane::new(Some(win.clone().upcast()));
    preview.self_test_preview();

    // Exercise SplitView sidebar toggle & NativeChatPane Markdown rendering + reset
    let split = win
        .content()
        .and_then(|w| w.downcast::<adw::OverlaySplitView>().ok())
        .expect("OverlaySplitView root");
    assert!(!split.shows_sidebar());
    split.set_show_sidebar(true);
    assert!(split.shows_sidebar());
    split.set_show_sidebar(false);
    assert!(!split.shows_sidebar());

    let pane = NativeChatPane::new(config.clone(), runtime.clone());
    pane.self_test_render_and_reset();

    // Exercise Settings modal open + close callback without RefCell re-entrancy panic
    let portal = Rc::new(WebPortalPane::new(config.clone()));
    let pane2 = pane.clone();
    let portal2 = portal.clone();
    show_settings_dialog(&win, config, runtime, move |_| {
        pane2.sync_providers_from_config();
        portal2.rebuild_tabs();
    });
    for w in app.windows() {
        if w.upcast_ref::<gtk4::Window>() != win.upcast_ref::<gtk4::Window>() {
            w.close();
        }
    }

    app.quit();
}
