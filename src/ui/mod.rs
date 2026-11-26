pub mod markdown_gtk;
pub mod native_chat;
pub mod preview;
pub mod settings;
pub mod web_portal;
pub mod window;

use gtk4::gdk;
use gtk4::prelude::*;
use libadwaita as adw;
use std::process::{Command, Stdio};

const DARK_CSS: &str = include_str!("../../assets/style.css");
const LIGHT_CSS: &str = include_str!("../../assets/style_light.css");

pub fn apply_theme_mode(mode: &str) {
    let style_mgr = adw::StyleManager::default();
    let scheme = match mode {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    };
    style_mgr.set_color_scheme(scheme);
}

pub fn load_css() {
    let provider = gtk4::CssProvider::new();
    let style_mgr = adw::StyleManager::default();
    let apply_palette = |prov: &gtk4::CssProvider, is_dark: bool| {
        prov.load_from_string(if is_dark { DARK_CSS } else { LIGHT_CSS });
    };
    apply_palette(&provider, style_mgr.is_dark());
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    let prov_clone = provider.clone();
    style_mgr.connect_dark_notify(move |mgr| {
        prov_clone.load_from_string(if mgr.is_dark() { DARK_CSS } else { LIGHT_CSS });
    });
}

pub fn copy_to_clipboard(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
    // Also mirror to wl-copy so the clipboard persists even if the window hides.
    if let Ok(mut child) = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(text.as_bytes());
        }
    }
}
