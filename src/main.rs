use anyhow::Result;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

use hyprpilot::APP_ID;
use hyprpilot::hyprland::{
    hide_window, restore_geometry_when_mapped, send_ipc_command, sync_hyprland_config,
    toggle_or_launch,
};
use hyprpilot::storage::config::{AppConfig, socket_path};
use hyprpilot::ui::{load_css, window::build_main_window};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--install-hyprland") => {
            let cfg = AppConfig::load()?;
            let path = sync_hyprland_config(&cfg, true)?;
            println!("Installed Hyprland integration at {}", path.display());
            return Ok(());
        }
        Some("--toggle") => return toggle_or_launch(),
        Some("--hide") => return hide_window(),
        Some("--quit") => {
            let _ = send_ipc_command("QUIT");
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            println!(
                "HyprPilot — Lightweight Copilot-style AI Assistant for Arch & Hyprland\n\n\
                 USAGE:\n  \
                   hyprpilot                     Launch or show the GUI window\n  \
                   hyprpilot-toggle              Toggle show/hide on Hyprland (bind to hotkey)\n  \
                   hyprpilot --install-hyprland  Write ~/.config/hypr/hyprpilot.lua & .conf and reload\n  \
                   hyprpilot --hide              Hide window to background\n  \
                   hyprpilot --quit              Exit the background HyprPilot process"
            );
            return Ok(());
        }
        _ => {}
    }

    // If a background instance is already alive, show & focus it immediately.
    if send_ipc_command("SHOW") {
        restore_geometry_when_mapped();
        return Ok(());
    }
    let _ = std::fs::remove_file(socket_path());

    let app = adw::Application::builder().application_id(APP_ID).build();
    let hold_guard: Rc<RefCell<Option<gtk4::gio::ApplicationHoldGuard>>> =
        Rc::new(RefCell::new(None));
    let win_slot: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));

    let hold_startup = hold_guard.clone();
    app.connect_startup(move |app| {
        *hold_startup.borrow_mut() = Some(app.hold());
        load_css();
    });

    app.connect_activate(move |app| {
        if let Some(win) = win_slot.borrow().as_ref() {
            win.set_visible(true);
            win.present();
            return;
        }
        let win = build_main_window(app);
        *win_slot.borrow_mut() = Some(win.clone());
        win.present();
    });

    let code = app.run_with_args::<String>(&[]);
    if code != gtk4::glib::ExitCode::SUCCESS {
        let exit_num: u8 = code.into();
        anyhow::bail!("GTK application exited with code {exit_num}");
    }
    Ok(())
}
