# HyprPilot

A lightweight, Windows Copilot-style AI assistant built specifically for **Arch Linux & Hyprland** in **Rust + GTK4 / Libadwaita + WebKitGTK 6.0**.

## Features

- **Instant Hyprland Toggle (`hyprpilot-toggle`)**:
  - Defaults to `Alt + Space` (fully configurable in Settings or `~/.config/hyprpilot/config.toml` — never hardcoded).
  - Launches centered at **50% × 50%** of your screen on first open.
  - Remembers your **exact last floating position, size, and fullscreen state** (`~/.local/state/hyprpilot/window.json`) when toggled away and back.
  - Hides silently into `special:hyprpilot` so chats and logged-in web sessions stay warm in memory with zero reload.
  - Supports fullscreen (`F11` or headerbar button) and `Esc` to hide.
- **Dual-Mode AI Support**:
  - **Native Fast Chat**: Pure-Rust SSE streaming for **OpenAI**, **Claude (Anthropic)**, **Google Gemini**, **DeepSeek**, **Groq**, **OpenRouter**, **local Ollama**, and **Custom Bearer Token / Custom Header** endpoints. Includes local SQLite chat history (`~/.local/share/hyprpilot/history.db`), full-text search, "Linux Quick Mode", and one-click code block copy (`wl-copy`).
  - **Embedded Web Portals (WebKitGTK 6.0)**: Persistent, hardware-accelerated Wayland web tabs for **ChatGPT**, **Claude**, **Microsoft Copilot**, **Gemini**, **DeepSeek**, and custom URLs. Sign in once — cookies and sessions persist in `~/.local/share/hyprpilot/webdata/`.

---

## System Dependencies (Arch Linux)

Install the required system libraries and tools:

```bash
sudo pacman -S --needed \
  rust cargo pkgconf base-devel \
  gtk4 libadwaita webkitgtk-6.0 \
  sqlite wl-clipboard hyprland
```

> **Note**: If you want to build **Native Fast Chat only** (without embedded WebKitGTK portals), `webkitgtk-6.0` can be omitted by passing `--no-default-features` to Cargo.

---

## Build & Install

```bash
git clone git@github.com:noorulislam770/hyprpilot.git
cd hyprpilot

# Full build (Native Chat + WebKitGTK Portals)
cargo build --release

# Install binaries into ~/.local/bin (make sure ~/.local/bin is in your PATH)
install -Dm755 target/release/hyprpilot ~/.local/bin/hyprpilot
install -Dm755 target/release/hyprpilot-toggle ~/.local/bin/hyprpilot-toggle
```

### Optional: Native-Only Build (without `webkitgtk-6.0`)

```bash
cargo build --release --no-default-features
```

---

## Hyprland Setup (Configurable Hotkey)

Run the built-in setup command once:

```bash
hyprpilot --install-hyprland
```

This does two things automatically:
1. Generates **`~/.config/hypr/hyprpilot.lua`** (for ML4W / Lua Hyprland configs) and **`~/.config/hypr/hyprpilot.conf`** (for standard `hyprland.conf`) with:
   - Floating window rule (`50% 50%`, centered by default)
   - Your configured toggle hotkey (default: `ALT + SPACE` -> `hyprpilot-toggle`)
2. Ensures `~/.config/hypr/custom.lua` loads `pcall(require, "hyprpilot")` (or `hyprland.conf` sources `hyprpilot.conf`), then reloads Hyprland.

### Changing the Toggle Hotkey
- **Via GUI**: Open **Settings (`Ctrl+,`) → General & Hyprland**, type a new hotkey (e.g. `SUPER + A` or `ALT + Q`), and click **Apply & Reload Hyprland**.
- **Via Config**: Edit `toggle_hotkey` in `~/.config/hyprpilot/config.toml` and run `hyprpilot --install-hyprland`.

---

## Getting Tokens & Using Existing ChatGPT / Claude Logins

HyprPilot gives you two ways to connect to AI models:

1. **Using Your Existing ChatGPT / Claude / Copilot Account (No API Key Needed)**
   - Your Chrome login at `chatgpt.com` or `claude.ai` uses browser web sessions protected by Cloudflare/anti-bot checks, not a developer API key.
   - Switch to **Web Portals (`Ctrl+2`)** inside HyprPilot, select **ChatGPT** (or Claude / Copilot / Gemini / DeepSeek), and sign in **once**. Cookies stay saved in `~/.local/share/hyprpilot/webdata/` across toggles and reboots.

2. **Using Native Fast Chat (`Ctrl+1`) with API Keys or Local Ollama**
   - Open **Settings (`Ctrl+,`) → Native AI & Tokens**, choose a provider, and paste its API key:
     - **Google Gemini (Free tier)**: `https://aistudio.google.com/apikey`
     - **Groq (Free tier, fast Llama)**: `https://console.groq.com/keys`
     - **OpenRouter (includes `:free` models)**: `https://openrouter.ai/keys`
     - **OpenAI API**: `https://platform.openai.com/api-keys` *(separate from `chatgpt.com` Plus/Free web login)*
     - **Claude (Anthropic)**: `https://console.anthropic.com/settings/keys`
     - **DeepSeek**: `https://platform.deepseek.com/api_keys`
     - **Ollama (Local, no key)**: Install & run `ollama serve`, then pick **Ollama (Local)**.
     - **Custom Bearer / Token Endpoint**: For custom/self-hosted gateways accepting a Bearer token or custom headers.

---

## Keyboard Shortcuts

| Shortcut | Action |
| --- | --- |
| `Alt + Space` *(default, configurable)* | Toggle HyprPilot show/hide via Hyprland |
| `F11` | Toggle Fullscreen |
| `Esc` | Hide window (configurable in Settings) |
| `Ctrl + 1` / `Ctrl + 2` | Switch between Native Chat & Web Portals |
| `Ctrl + N` | Start a new Native Chat |
| `Ctrl + ,` | Open Settings |
| `Enter` / `Shift + Enter` | Send prompt / Insert newline |

---

## Data & Config Paths

- **Config (mode `0600`)**: `~/.config/hyprpilot/config.toml`
- **Chat History (SQLite)**: `~/.local/share/hyprpilot/history.db`
- **WebKit Portal Cookies/Cache**: `~/.local/share/hyprpilot/webdata/`
- **Window Geometry State**: `~/.local/state/hyprpilot/window.json`

---

## Running Tests

```bash
cargo test --no-default-features
# or with webkitgtk-6.0 installed:
cargo test
```
