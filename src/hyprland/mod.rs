use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::{APP_ID, APP_NAME, TUI_APP_ID};
use crate::storage::config::{AppConfig, format_hyprland_hotkey, socket_path, state_dir};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    #[serde(default)]
    pub fullscreen: bool,
}

#[derive(Debug, Deserialize)]
struct HyprClient {
    address: String,
    #[serde(default)]
    class: String,
    #[serde(rename = "initialClass", default)]
    initial_class: String,
    #[serde(default)]
    title: String,
    at: [i32; 2],
    size: [i32; 2],
    #[serde(default)]
    fullscreen: i32,
    #[allow(dead_code)]
    workspace: HyprWorkspaceRef,
}

#[derive(Debug, Deserialize)]
struct HyprWorkspaceRef {
    #[allow(dead_code)]
    #[serde(default)]
    id: i64,
    #[allow(dead_code)]
    name: String,
}

fn geometry_path(filename: &str) -> PathBuf {
    state_dir().join(filename)
}

pub fn load_geometry() -> Option<SavedGeometry> {
    load_geometry_file("window.json")
}

pub fn load_geometry_file(filename: &str) -> Option<SavedGeometry> {
    let raw = fs::read_to_string(geometry_path(filename)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_geometry(geom: &SavedGeometry) -> Result<()> {
    save_geometry_file("window.json", geom)
}

pub fn save_geometry_file(filename: &str, geom: &SavedGeometry) -> Result<()> {
    let path = geometry_path(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(geom)?)?;
    Ok(())
}

fn run_hyprctl(args: &[&str]) -> Result<String> {
    let out = Command::new("hyprctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to invoke hyprctl {:?}", args))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success()
        || stdout.trim_start().starts_with("error:")
        || stdout.trim_start().starts_with("Invalid")
    {
        anyhow::bail!(
            "hyprctl {:?} failed: {} {}",
            args,
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(stdout)
}

fn is_lua_config() -> bool {
    hypr_config_dir().join("hyprland.lua").exists()
}

fn dispatch_focus_address(address: &str) {
    if is_lua_config() {
        let cmd = format!("hl.dsp.focus({{ window = \"address:{address}\" }})");
        let _ = run_hyprctl(&["dispatch", &cmd]);
    } else {
        let arg = format!("address:{address}");
        let _ = run_hyprctl(&["dispatch", "focuswindow", &arg]);
    }
}

fn dispatch_toggle_fullscreen() {
    if is_lua_config() {
        let _ = run_hyprctl(&[
            "dispatch",
            "hl.dsp.window.fullscreen({ mode = \"fullscreen\", action = \"toggle\" })",
        ]);
    } else {
        let _ = run_hyprctl(&["dispatch", "fullscreen", "0"]);
    }
}

fn dispatch_close_address(address: &str) {
    if is_lua_config() {
        let cmd = format!("hl.dsp.window.close({{ window = \"address:{address}\" }})");
        let _ = run_hyprctl(&["dispatch", &cmd]);
    } else {
        let arg = format!("address:{address}");
        let _ = run_hyprctl(&["dispatch", "closewindow", &arg]);
    }
}

fn find_client_by_class(class_id: &str, preferred_title: Option<&str>) -> Result<Option<HyprClient>> {
    let raw = run_hyprctl(&["-j", "clients"])?;
    let clients: Vec<HyprClient> = serde_json::from_str(&raw)?;
    let matching: Vec<HyprClient> = clients
        .into_iter()
        .filter(|c| c.class == class_id || c.initial_class == class_id)
        .collect();
    if let Some(title) = preferred_title {
        if let Some(pos) = matching.iter().position(|c| c.title == title) {
            return Ok(Some(matching.into_iter().nth(pos).unwrap()));
        }
    }
    Ok(matching.into_iter().next())
}

fn find_hyprpilot_client() -> Result<Option<HyprClient>> {
    find_client_by_class(APP_ID, Some(APP_NAME))
}

fn apply_geometry_to_address(address: &str, geom: &SavedGeometry) {
    if is_lua_config() {
        let resize_cmd = format!(
            "hl.dsp.window.resize({{ x = {}, y = {}, relative = false, window = \"address:{address}\" }})",
            geom.width, geom.height
        );
        let move_cmd = format!(
            "hl.dsp.window.move({{ x = {}, y = {}, relative = false, window = \"address:{address}\" }})",
            geom.x, geom.y
        );
        let _ = run_hyprctl(&["dispatch", &resize_cmd]);
        let _ = run_hyprctl(&["dispatch", &move_cmd]);
    } else {
        let size_arg = format!("exact {} {},address:{}", geom.width, geom.height, address);
        let pos_arg = format!("exact {} {},address:{}", geom.x, geom.y, address);
        let _ = run_hyprctl(&["dispatch", "resizewindowpixel", &size_arg]);
        let _ = run_hyprctl(&["dispatch", "movewindowpixel", &pos_arg]);
    }
}

fn snapshot_client_geometry(client: &HyprClient) -> SavedGeometry {
    let is_fs = client.fullscreen > 0;
    // Preserve last non-fullscreen floating bounds when toggling from fullscreen.
    if is_fs {
        if let Some(mut prev) = load_geometry() {
            prev.fullscreen = true;
            return prev;
        }
    }
    SavedGeometry {
        x: client.at[0],
        y: client.at[1],
        width: client.size[0].max(360),
        height: client.size[1].max(300),
        fullscreen: is_fs,
    }
}

pub fn send_ipc_command(cmd: &str) -> bool {
    let path = socket_path();
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return false;
    };
    let line = format!("{cmd}\n");
    stream.write_all(line.as_bytes()).is_ok()
}

pub fn snapshot_and_save_geometry() -> Result<Option<HyprClientSnapshot>> {
    let Some(client) = find_hyprpilot_client()? else {
        return Ok(None);
    };
    let geom = snapshot_client_geometry(&client);
    save_geometry(&geom)?;
    Ok(Some(HyprClientSnapshot {
        address: client.address,
        fullscreen: client.fullscreen > 0,
    }))
}

pub struct HyprClientSnapshot {
    pub address: String,
    pub fullscreen: bool,
}

pub fn hide_window() -> Result<()> {
    let _ = snapshot_and_save_geometry();
    if send_ipc_command("HIDE") {
        return Ok(());
    }
    Ok(())
}

pub fn restore_geometry_when_mapped() {
    restore_geometry_for(APP_ID, Some(APP_NAME), "window.json");
}

fn restore_geometry_for(class_id: &str, preferred_title: Option<&str>, geom_file: &str) {
    let saved = load_geometry_file(geom_file);
    for _ in 0..80 {
        if let Ok(Some(client)) = find_client_by_class(class_id, preferred_title) {
            if let Some(ref geom) = saved {
                apply_geometry_to_address(&client.address, geom);
            }
            dispatch_focus_address(&client.address);
            if saved.as_ref().is_some_and(|g| g.fullscreen) && client.fullscreen == 0 {
                dispatch_toggle_fullscreen();
            }
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn toggle_or_launch() -> Result<()> {
    if find_hyprpilot_client()?.is_some() {
        let _ = snapshot_and_save_geometry();
        if send_ipc_command("HIDE") {
            return Ok(());
        }
    }

    if !send_ipc_command("SHOW") {
        launch_hyprpilot_detached()?;
    }

    restore_geometry_when_mapped();
    Ok(())
}

pub fn toggle_tui_or_launch() -> Result<()> {
    if let Some(client) = find_client_by_class(TUI_APP_ID, None)? {
        let geom = snapshot_client_geometry(&client);
        let _ = save_geometry_file("tui-window.json", &geom);
        dispatch_close_address(&client.address);
        return Ok(());
    }

    launch_tui_terminal_detached()?;
    restore_geometry_for(TUI_APP_ID, None, "tui-window.json");
    Ok(())
}

fn resolve_sibling_bin(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn launch_hyprpilot_detached() -> Result<()> {
    let bin = resolve_sibling_bin("hyprpilot");
    Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to launch hyprpilot binary")?;
    Ok(())
}

fn command_exists(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn launch_tui_terminal_detached() -> Result<()> {
    let tui_bin = resolve_sibling_bin("hyprpilot-tui");
    let tui_str = tui_bin.to_string_lossy().into_owned();

    let mut cmd = if command_exists("kitty") {
        let mut c = Command::new("kitty");
        c.args(["--class", TUI_APP_ID, "--title", "HyprPilot TUI", &tui_str]);
        c
    } else if command_exists("foot") {
        let mut c = Command::new("foot");
        c.args([
            &format!("--app-id={TUI_APP_ID}"),
            "--title=HyprPilot TUI",
            &tui_str,
        ]);
        c
    } else if command_exists("alacritty") {
        let mut c = Command::new("alacritty");
        c.args(["--class", TUI_APP_ID, "-t", "HyprPilot TUI", "-e", &tui_str]);
        c
    } else if command_exists("wezterm") {
        let mut c = Command::new("wezterm");
        c.args(["start", "--class", TUI_APP_ID, "--", &tui_str]);
        c
    } else {
        anyhow::bail!("No supported terminal emulator found (tried kitty, foot, alacritty, wezterm)");
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to launch hyprpilot-tui in terminal")?;
    Ok(())
}

pub fn hypr_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hypr")
}

pub fn render_hyprland_lua(toggle_hotkey: &str, tui_hotkey: &str, toggle_cmd: &str) -> String {
    let (lua_hotkey, _) = format_hyprland_hotkey(toggle_hotkey);
    let (lua_tui_hotkey, _) = format_hyprland_hotkey(tui_hotkey);
    format!(
        "-- Auto-generated by HyprPilot. Edit hotkeys in HyprPilot Settings or ~/.config/hyprpilot/config.toml\n\
hl.window_rule({{\n\
    name = \"hyprpilot\",\n\
    match = {{ class = \"^{APP_ID}$\" }},\n\
    float = true,\n\
    center = true,\n\
    size = \"(monitor_w*0.5) (monitor_h*0.5)\",\n\
}})\n\n\
hl.window_rule({{\n\
    name = \"hyprpilot-tui\",\n\
    match = {{ class = \"^{TUI_APP_ID}$\" }},\n\
    float = true,\n\
    center = true,\n\
    size = \"(monitor_w*0.5) (monitor_h*0.5)\",\n\
}})\n\n\
hl.bind(\"{lua_hotkey}\", hl.dsp.exec_cmd(\"{toggle_cmd}\"), {{ description = \"Toggle HyprPilot AI Assistant\" }})\n\
hl.bind(\"{lua_tui_hotkey}\", hl.dsp.exec_cmd(\"{toggle_cmd} --tui\"), {{ description = \"Toggle HyprPilot TUI Assistant\" }})\n"
    )
}

pub fn render_hyprland_conf(toggle_hotkey: &str, tui_hotkey: &str, toggle_cmd: &str) -> String {
    let (_, conf_hotkey) = format_hyprland_hotkey(toggle_hotkey);
    let (_, conf_tui_hotkey) = format_hyprland_hotkey(tui_hotkey);
    format!(
        "# Auto-generated by HyprPilot. Edit hotkeys in HyprPilot Settings or ~/.config/hyprpilot/config.toml\n\
windowrulev2 = float, class:^({APP_ID})$\n\
windowrulev2 = size 50% 50%, class:^({APP_ID})$\n\
windowrulev2 = center, class:^({APP_ID})$\n\n\
windowrulev2 = float, class:^({TUI_APP_ID})$\n\
windowrulev2 = size 50% 50%, class:^({TUI_APP_ID})$\n\
windowrulev2 = center, class:^({TUI_APP_ID})$\n\n\
bind = {conf_hotkey}, exec, {toggle_cmd}\n\
bind = {conf_tui_hotkey}, exec, {toggle_cmd} --tui\n"
    )
}

fn resolve_toggle_command() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("hyprpilot-toggle");
            if candidate.exists() {
                return candidate.display().to_string();
            }
        }
    }
    "hyprpilot-toggle".to_string()
}

/// Writes ~/.config/hypr/hyprpilot.lua & hyprpilot.conf, ensures ML4W custom.lua
/// (or hyprland.conf fallback) sources it, and optionally reloads Hyprland.
pub fn sync_hyprland_config(cfg: &AppConfig, reload_hyprland: bool) -> Result<PathBuf> {
    let hypr_dir = hypr_config_dir();
    fs::create_dir_all(&hypr_dir)?;

    let toggle_cmd = resolve_toggle_command();
    let lua_path = hypr_dir.join("hyprpilot.lua");
    let conf_path = hypr_dir.join("hyprpilot.conf");

    fs::write(
        &lua_path,
        render_hyprland_lua(&cfg.toggle_hotkey, &cfg.tui_toggle_hotkey, &toggle_cmd),
    )?;
    fs::write(
        &conf_path,
        render_hyprland_conf(&cfg.toggle_hotkey, &cfg.tui_toggle_hotkey, &toggle_cmd),
    )?;

    let hyprland_lua = hypr_dir.join("hyprland.lua");
    let custom_lua = hypr_dir.join("custom.lua");
    if hyprland_lua.exists() {
        ensure_line_in_file(&custom_lua, "pcall(require, \"hyprpilot\")")?;
    } else {
        let hyprland_conf = hypr_dir.join("hyprland.conf");
        if hyprland_conf.exists() {
            ensure_line_in_file(&hyprland_conf, "source = ~/.config/hypr/hyprpilot.conf")?;
        }
    }

    if reload_hyprland {
        let _ = run_hyprctl(&["reload"]);
    }
    Ok(lua_path)
}

fn ensure_line_in_file(path: &Path, line: &str) -> Result<()> {
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    if existing.lines().any(|l| l.trim() == line.trim()) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(line);
    updated.push('\n');
    fs::write(path, updated)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_configs_use_custom_hotkey() {
        let lua = render_hyprland_lua("SUPER + A", "ALT + SHIFT + SPACE", "hyprpilot-toggle");
        assert!(lua.contains("hl.bind(\"SUPER + A\""));
        assert!(lua.contains("hl.bind(\"ALT + SHIFT + SPACE\""));
        assert!(lua.contains("hyprpilot-toggle --tui"));
        assert!(lua.contains("size = \"(monitor_w*0.5) (monitor_h*0.5)\""));

        let conf = render_hyprland_conf("SUPER + A", "ALT + SHIFT + SPACE", "hyprpilot-toggle");
        assert!(conf.contains("bind = SUPER, A, exec, hyprpilot-toggle"));
        assert!(conf.contains("bind = ALT SHIFT, SPACE, exec, hyprpilot-toggle --tui"));
        assert!(conf.contains("windowrulev2 = size 50% 50%"));
    }

    #[test]
    fn parses_special_workspace_without_id() {
        let json = r#"[{
            "address": "0x123",
            "class": "dev.noor.HyprPilot",
            "initialClass": "dev.noor.HyprPilot",
            "at": [100, 100],
            "size": [900, 500],
            "fullscreen": 0,
            "workspace": { "address": "special", "type": "special", "name": "special:hyprpilot" }
        }]"#;
        let clients: Vec<HyprClient> = serde_json::from_str(json).unwrap();
        assert_eq!(clients[0].workspace.name, "special:hyprpilot");
    }
}
