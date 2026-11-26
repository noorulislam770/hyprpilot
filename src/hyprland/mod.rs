use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::storage::config::{AppConfig, format_hyprland_hotkey, state_dir};
use crate::{APP_ID, SPECIAL_WORKSPACE};

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
    at: [i32; 2],
    size: [i32; 2],
    #[serde(default)]
    fullscreen: i32,
    workspace: HyprWorkspaceRef,
}

#[derive(Debug, Deserialize)]
struct HyprWorkspaceRef {
    #[allow(dead_code)]
    #[serde(default)]
    id: i64,
    name: String,
}

#[derive(Debug, Deserialize)]
struct HyprActiveWorkspace {
    #[serde(default)]
    id: i64,
    #[allow(dead_code)]
    name: String,
}

fn geometry_path() -> PathBuf {
    state_dir().join("window.json")
}

pub fn load_geometry() -> Option<SavedGeometry> {
    let raw = fs::read_to_string(geometry_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_geometry(geom: &SavedGeometry) -> Result<()> {
    let path = geometry_path();
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

fn dispatch_move_workspace(address: &str, workspace: &str, silent: bool) -> Result<()> {
    if is_lua_config() {
        let silent_arg = if silent { ", silent = true" } else { "" };
        let cmd = format!(
            "hl.dsp.window.move({{ workspace = \"{workspace}\"{silent_arg}, window = \"address:{address}\" }})"
        );
        run_hyprctl(&["dispatch", &cmd])?;
    } else {
        let dispatcher = if silent {
            "movetoworkspacesilent"
        } else {
            "movetoworkspace"
        };
        let arg = format!("{workspace},address:{address}");
        run_hyprctl(&["dispatch", dispatcher, &arg])?;
    }
    Ok(())
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

fn find_hyprpilot_client() -> Result<Option<HyprClient>> {
    let raw = run_hyprctl(&["-j", "clients"])?;
    let clients: Vec<HyprClient> = serde_json::from_str(&raw)?;
    Ok(clients
        .into_iter()
        .find(|c| c.class == APP_ID || c.initial_class == APP_ID))
}

fn active_workspace_id() -> Result<i64> {
    let raw = run_hyprctl(&["-j", "activeworkspace"])?;
    let ws: HyprActiveWorkspace = serde_json::from_str(&raw)?;
    Ok(ws.id)
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

pub fn hide_window() -> Result<()> {
    let Some(client) = find_hyprpilot_client()? else {
        return Ok(());
    };
    if client.workspace.name == SPECIAL_WORKSPACE {
        return Ok(());
    }
    let geom = snapshot_client_geometry(&client);
    let _ = save_geometry(&geom);

    if client.fullscreen > 0 {
        dispatch_focus_address(&client.address);
        dispatch_toggle_fullscreen();
    }
    dispatch_move_workspace(&client.address, SPECIAL_WORKSPACE, true)?;
    Ok(())
}

fn show_window(client: &HyprClient) -> Result<()> {
    let ws_id = active_workspace_id()?.to_string();
    dispatch_move_workspace(&client.address, &ws_id, false)?;

    let saved = load_geometry();
    if let Some(ref geom) = saved {
        apply_geometry_to_address(&client.address, geom);
    }

    dispatch_focus_address(&client.address);

    if saved.as_ref().is_some_and(|g| g.fullscreen) && client.fullscreen == 0 {
        dispatch_toggle_fullscreen();
    }
    Ok(())
}

pub fn toggle_or_launch() -> Result<()> {
    if let Some(client) = find_hyprpilot_client()? {
        if client.workspace.name == SPECIAL_WORKSPACE {
            show_window(&client)?;
        } else {
            hide_window()?;
        }
        return Ok(());
    }

    launch_hyprpilot_detached()?;

    // If the user has saved window geometry from an earlier session, restore it once mapped.
    if let Some(geom) = load_geometry() {
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(50));
            if let Ok(Some(client)) = find_hyprpilot_client() {
                apply_geometry_to_address(&client.address, &geom);
                dispatch_focus_address(&client.address);
                if geom.fullscreen {
                    dispatch_toggle_fullscreen();
                }
                break;
            }
        }
    }
    Ok(())
}

fn launch_hyprpilot_detached() -> Result<()> {
    let exe = std::env::current_exe().ok();
    let sibling = exe
        .as_ref()
        .and_then(|p| p.parent())
        .map(|dir| dir.join("hyprpilot"))
        .filter(|p| p.exists());

    let bin = sibling.unwrap_or_else(|| PathBuf::from("hyprpilot"));
    Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to launch hyprpilot binary")?;
    Ok(())
}

pub fn hypr_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hypr")
}

pub fn render_hyprland_lua(toggle_hotkey: &str, toggle_cmd: &str) -> String {
    let (lua_hotkey, _) = format_hyprland_hotkey(toggle_hotkey);
    format!(
        "-- Auto-generated by HyprPilot. Edit hotkey in HyprPilot Settings or ~/.config/hyprpilot/config.toml\n\
hl.window_rule({{\n\
    name = \"hyprpilot\",\n\
    match = {{ class = \"^{APP_ID}$\" }},\n\
    float = true,\n\
    center = true,\n\
    size = \"(monitor_w*0.5) (monitor_h*0.5)\",\n\
}})\n\n\
hl.bind(\"{lua_hotkey}\", hl.dsp.exec_cmd(\"{toggle_cmd}\"), {{ description = \"Toggle HyprPilot AI Assistant\" }})\n"
    )
}

pub fn render_hyprland_conf(toggle_hotkey: &str, toggle_cmd: &str) -> String {
    let (_, conf_hotkey) = format_hyprland_hotkey(toggle_hotkey);
    format!(
        "# Auto-generated by HyprPilot. Edit hotkey in HyprPilot Settings or ~/.config/hyprpilot/config.toml\n\
windowrulev2 = float, class:^({APP_ID})$\n\
windowrulev2 = size 50% 50%, class:^({APP_ID})$\n\
windowrulev2 = center, class:^({APP_ID})$\n\n\
bind = {conf_hotkey}, exec, {toggle_cmd}\n"
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

    fs::write(&lua_path, render_hyprland_lua(&cfg.toggle_hotkey, &toggle_cmd))?;
    fs::write(&conf_path, render_hyprland_conf(&cfg.toggle_hotkey, &toggle_cmd))?;

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
        let lua = render_hyprland_lua("SUPER + A", "hyprpilot-toggle");
        assert!(lua.contains("hl.bind(\"SUPER + A\""));
        assert!(lua.contains("size = \"(monitor_w*0.5) (monitor_h*0.5)\""));

        let conf = render_hyprland_conf("SUPER + A", "hyprpilot-toggle");
        assert!(conf.contains("bind = SUPER, A, exec, hyprpilot-toggle"));
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
