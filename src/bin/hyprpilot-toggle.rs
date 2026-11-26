use anyhow::Result;

fn main() -> Result<()> {
    hyprpilot::hyprland::toggle_or_launch()
}
