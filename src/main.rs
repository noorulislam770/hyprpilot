use anyhow::Result;
use gtk4::prelude::*;
use libadwaita as adw;

use hyprpilot::APP_ID;
use hyprpilot::hyprland::{hide_window, sync_hyprland_config, toggle_or_launch};
use hyprpilot::storage::config::AppConfig;
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
        Some("--help") | Some("-h") => {
            println!(
                "HyprPilot — Lightweight Copilot-style AI Assistant for Arch & Hyprland\n\n\
                 USAGE:\n  \
                   hyprpilot                     Launch or focus the GUI window\n  \
                   hyprpilot-toggle              Toggle show/hide on Hyprland (bind to hotkey)\n  \
                   hyprpilot --install-hyprland  Write ~/.config/hypr/hyprpilot.lua & .conf and reload\n  \
                   hyprpilot --hide              Hide window to special:hyprpilot"
            );
            return Ok(());
        }
        _ => {}
    }

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        load_css();
    });

    app.connect_activate(|app| {
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        let win = build_main_window(app);
        win.present();
    });

    let code = app.run_with_args::<String>(&[]);
    if code != gtk4::glib::ExitCode::SUCCESS {
        let exit_num: u8 = code.into();
        anyhow::bail!("GTK application exited with code {exit_num}");
    }
    Ok(())
}
