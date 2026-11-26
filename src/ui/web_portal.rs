use gtk4::prelude::*;
use std::cell::RefCell;
#[cfg(feature = "web-portal")]
use std::collections::HashMap;
use std::rc::Rc;

use crate::storage::config::{AppConfig, PortalEntry};

pub struct WebPortalPane {
    pub root: gtk4::Box,
    config: Rc<RefCell<AppConfig>>,
    #[cfg(feature = "web-portal")]
    inner: Rc<RefCell<PortalState>>,
}

#[cfg(feature = "web-portal")]
struct PortalState {
    session: webkit6::NetworkSession,
    stack: gtk4::Stack,
    views: HashMap<String, webkit6::WebView>,
    urls: HashMap<String, String>,
    buttons: HashMap<String, gtk4::ToggleButton>,
    tab_bar: gtk4::Box,
    active_id: String,
    url_label: gtk4::Label,
    syncing: bool,
}

impl WebPortalPane {
    pub fn new(config: Rc<RefCell<AppConfig>>) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.set_vexpand(true);
        root.set_hexpand(true);

        #[cfg(feature = "web-portal")]
        {
            let web_dir = crate::storage::config::data_dir().join("webdata");
            let cache_dir = web_dir.join("cache");
            let _ = std::fs::create_dir_all(&web_dir);
            let _ = std::fs::create_dir_all(&cache_dir);

            let session = webkit6::NetworkSession::new(
                Some(&web_dir.to_string_lossy()),
                Some(&cache_dir.to_string_lossy()),
            );

            let top_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            top_bar.add_css_class("portal-tab-bar");

            let tab_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
            let scroller = gtk4::ScrolledWindow::new();
            scroller.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
            scroller.set_hexpand(true);
            scroller.set_child(Some(&tab_bar));

            let back_btn = gtk4::Button::from_icon_name("go-previous-symbolic");
            back_btn.set_tooltip_text(Some("Back"));
            back_btn.add_css_class("flat");

            let fwd_btn = gtk4::Button::from_icon_name("go-next-symbolic");
            fwd_btn.set_tooltip_text(Some("Forward"));
            fwd_btn.add_css_class("flat");

            let reload_btn = gtk4::Button::from_icon_name("view-refresh-symbolic");
            reload_btn.set_tooltip_text(Some("Reload Portal"));
            reload_btn.add_css_class("flat");

            let open_ext_btn = gtk4::Button::from_icon_name("external-link-symbolic");
            open_ext_btn.set_tooltip_text(Some("Open current URL in external browser"));
            open_ext_btn.add_css_class("flat");

            let url_label = gtk4::Label::new(None);
            url_label.add_css_class("dim-label");
            url_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            url_label.set_max_width_chars(30);

            top_bar.append(&scroller);
            top_bar.append(&url_label);
            top_bar.append(&back_btn);
            top_bar.append(&fwd_btn);
            top_bar.append(&reload_btn);
            top_bar.append(&open_ext_btn);

            let stack = gtk4::Stack::new();
            stack.set_vexpand(true);
            stack.set_hexpand(true);

            root.append(&top_bar);
            root.append(&stack);

            let active_id = config.borrow().active_portal_id.clone();
            let inner = Rc::new(RefCell::new(PortalState {
                session,
                stack,
                views: HashMap::new(),
                urls: HashMap::new(),
                buttons: HashMap::new(),
                tab_bar,
                active_id,
                url_label,
                syncing: false,
            }));

            let pane = Self {
                root,
                config: config.clone(),
                inner: inner.clone(),
            };
            pane.rebuild_tabs();

            // Navigation button handlers
            {
                let st = inner.clone();
                back_btn.connect_clicked(move |_| {
                    use webkit6::prelude::*;
                    if let Some(view) = st.borrow().current_view() {
                        view.go_back();
                    }
                });
            }
            {
                let st = inner.clone();
                fwd_btn.connect_clicked(move |_| {
                    use webkit6::prelude::*;
                    if let Some(view) = st.borrow().current_view() {
                        view.go_forward();
                    }
                });
            }
            {
                let st = inner.clone();
                reload_btn.connect_clicked(move |_| {
                    use webkit6::prelude::*;
                    if let Some(view) = st.borrow().current_view() {
                        view.reload();
                    }
                });
            }
            {
                let st = inner.clone();
                open_ext_btn.connect_clicked(move |_| {
                    use webkit6::prelude::*;
                    if let Some(view) = st.borrow().current_view() {
                        if let Some(uri) = view.uri() {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(uri.as_str())
                                .spawn();
                        }
                    }
                });
            }

            pane
        }

        #[cfg(not(feature = "web-portal"))]
        {
            let status = libadwaita::StatusPage::new();
            status.set_icon_name(Some("web-browser-symbolic"));
            status.set_title("WebKitGTK Portals Disabled");
            status.set_description(Some(
                "This binary was compiled without the 'web-portal' feature.\nInstall webkitgtk-6.0 on Arch (`sudo pacman -S webkitgtk-6.0`) and rebuild with default features.",
            ));
            status.set_vexpand(true);
            root.append(&status);
            Self { root, config }
        }
    }

    pub fn ensure_active_loaded(&self) {
        #[cfg(feature = "web-portal")]
        {
            let active = self.inner.borrow().active_id.clone();
            Self::activate_portal(&self.inner, &self.config, &active);
        }
    }

    pub fn rebuild_tabs(&self) {
        #[cfg(feature = "web-portal")]
        {
            let portals = self.config.borrow().portals.clone();
            {
                let mut st = self.inner.borrow_mut();
                while let Some(child) = st.tab_bar.first_child() {
                    st.tab_bar.remove(&child);
                }
                st.buttons.clear();
                st.urls.clear();
                for p in &portals {
                    st.urls.insert(p.id.clone(), p.url.clone());
                }
            }

            for portal in portals {
                let btn = gtk4::ToggleButton::with_label(&portal.name);
                btn.add_css_class("flat");
                let pid = portal.id.clone();
                let st_ref = self.inner.clone();
                let cfg_ref = self.config.clone();
                btn.connect_clicked(move |b| {
                    if st_ref.borrow().syncing {
                        return;
                    }
                    if b.is_active() {
                        Self::activate_portal(&st_ref, &cfg_ref, &pid);
                    } else if st_ref.borrow().active_id == pid {
                        b.set_active(true);
                    }
                });
                let mut st = self.inner.borrow_mut();
                st.tab_bar.append(&btn);
                st.buttons.insert(portal.id, btn);
            }

            let (target, had_views) = {
                let st = self.inner.borrow();
                let t = if st.urls.contains_key(&st.active_id) {
                    st.active_id.clone()
                } else {
                    st.urls.keys().next().cloned().unwrap_or_default()
                };
                (t, !st.views.is_empty())
            };
            if had_views && !target.is_empty() {
                Self::activate_portal(&self.inner, &self.config, &target);
            }
        }
        #[cfg(not(feature = "web-portal"))]
        {
            let _ = &self.config;
        }
    }

    #[cfg(feature = "web-portal")]
    fn activate_portal(
        inner: &Rc<RefCell<PortalState>>,
        config: &Rc<RefCell<AppConfig>>,
        id: &str,
    ) {
        use webkit6::prelude::*;

        let need_create = {
            let st = inner.borrow();
            !st.views.contains_key(id)
        };

        if need_create {
            let (session, url, stack, url_lbl) = {
                let st = inner.borrow();
                (
                    st.session.clone(),
                    st.urls.get(id).cloned().unwrap_or_default(),
                    st.stack.clone(),
                    st.url_label.clone(),
                )
            };
            let settings = webkit6::Settings::new();
            settings.set_enable_javascript(true);
            settings.set_enable_webgl(true);
            settings.set_enable_developer_extras(false);

            let webview = webkit6::WebView::builder()
                .network_session(&session)
                .settings(&settings)
                .build();
            webview.set_vexpand(true);
            webview.set_hexpand(true);

            let lbl_clone = url_lbl.clone();
            webview.connect_uri_notify(move |wv| {
                if let Some(uri) = wv.uri() {
                    lbl_clone.set_text(uri.as_str());
                }
            });

            if !url.is_empty() {
                webview.load_uri(&url);
            }
            stack.add_named(&webview, Some(id));
            inner.borrow_mut().views.insert(id.to_string(), webview);
        }

        let button_updates: Vec<(gtk4::ToggleButton, bool)> = {
            let mut st = inner.borrow_mut();
            st.syncing = true;
            st.active_id = id.to_string();
            st.stack.set_visible_child_name(id);
            if let Some(view) = st.views.get(id) {
                if let Some(uri) = view.uri() {
                    st.url_label.set_text(uri.as_str());
                } else if let Some(u) = st.urls.get(id) {
                    st.url_label.set_text(u);
                }
            }
            st.buttons
                .iter()
                .map(|(btn_id, btn)| (btn.clone(), btn_id == id))
                .collect()
        };

        for (btn, should_be_active) in button_updates {
            if btn.is_active() != should_be_active {
                btn.set_active(should_be_active);
            }
        }
        inner.borrow_mut().syncing = false;
        config.borrow_mut().active_portal_id = id.to_string();
    }
}

#[cfg(feature = "web-portal")]
impl PortalState {
    fn current_view(&self) -> Option<&webkit6::WebView> {
        self.views.get(&self.active_id)
    }
}

#[allow(dead_code)]
fn _keep_portal_entry_used(_: PortalEntry) {}
