use anyhow::Result;

fn main() -> Result<()> {
    let is_tui = std::env::args().any(|a| a == "--tui");
    if is_tui {
        hyprpilot::hyprland::toggle_tui_or_launch()
    } else {
        hyprpilot::hyprland::toggle_or_launch()
    }
}
