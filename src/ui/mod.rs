pub mod markdown_gtk;
pub mod native_chat;
pub mod preview;
pub mod settings;
pub mod web_portal;
pub mod window;

use gtk4::gdk;
use gtk4::prelude::*;
use std::process::{Command, Stdio};

pub fn load_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(include_str!("../../assets/style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
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
