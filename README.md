# HyprPilot

A lightweight, Windows Copilot-style AI assistant built specifically for **Arch Linux & Hyprland** in **Rust + GTK4 / Libadwaita + WebKitGTK 6.0**.

## Features

- **Dual UI Modes (GUI + TUI)**:
  - **Lightweight Native GUI (`hyprpilot`)**: `Alt + Space` (configurable) toggles a GTK4/Libadwaita window with Native Chat and persistent WebKitGTK portals.
  - **Terminal TUI (`hyprpilot-tui`)**: `Alt + Shift + Space` (configurable) pops open a floating terminal running the fast Ratatui TUI, automatically selecting your last-used provider & model and sharing SQLite history.
  - **Built-in Preview Engine (`Ctrl+3`)**:
    - GUI Preview tab renders **HTML**, **SVG**, **PDF** (page-by-page + zoom + `pdftotext` extraction), **Markdown**, **Images** (`png`, `jpg`, `webp`, `gif`), and source/text files.
    - **Claude-Style Artifact Previews**: Any `html`, `svg`, or `markdown` fenced code block in AI replies includes a **`Preview ↗`** button to render it immediately in the Preview tab.
    - Terminal CLI preview: `hyprpilot --preview <file>` (GUI), `hyprpilot-tui --preview <file>` (Kitty inline graphics / ANSI), `hyprpilot-tui --view <file>` (scrollable pager).
- **Instant Hyprland Toggle (`hyprpilot-toggle`)**:
  - Defaults to `Alt + Space` (GUI) and `Alt + Shift + Space` (TUI), both configurable in Settings or `~/.config/hyprpilot/config.toml`.
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
  poppler sqlite wl-clipboard hyprland
```

> **Note**: If you want to build **Native Fast Chat only** (without embedded WebKitGTK portals), `webkitgtk-6.0` can be omitted by passing `--no-default-features` to Cargo.

---

## Build & Install

```bash
git clone git@github.com:noorulislam770/hyprpilot.git
cd hyprpilot

# Full build (GUI + TUI + Toggle)
cargo build --release

# Install binaries into ~/.local/bin (make sure ~/.local/bin is in your PATH)
install -Dm755 target/release/hyprpilot ~/.local/bin/hyprpilot
install -Dm755 target/release/hyprpilot-toggle ~/.local/bin/hyprpilot-toggle
install -Dm755 target/release/hyprpilot-tui ~/.local/bin/hyprpilot-tui
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
| `Alt + Space` *(default, configurable)* | Toggle HyprPilot GUI show/hide via Hyprland |
| `Alt + Shift + Space` *(default, configurable)* | Toggle HyprPilot TUI terminal (`hyprpilot-toggle --tui`) |
| `F11` | Toggle Fullscreen |
| `Esc` | Hide window (configurable in Settings) |
| `Ctrl + 1` / `Ctrl + 2` / `Ctrl + 3` | Switch between Native Chat, Web Portals & Preview Engine |
| `Ctrl + B` or `Ctrl + \` | Toggle Claude-style Recents sidebar |
| `Ctrl + K` | Open sidebar & search chats |
| `Ctrl + N` | Start a new chat |
| `Ctrl + ,` | Open Settings |
| `Ctrl + Q` | Quit background GUI process |
| `Enter` / `Shift + Enter` | *(GUI)* Send prompt / Insert newline |
| `Ctrl + P` / `Ctrl + M` / `Ctrl + R` | *(TUI)* Switch Provider / Switch Model / Fetch Models |
| `Ctrl + G` or `F4` | *(TUI)* Browse & load SQLite chat history |
| `Ctrl + O` | *(TUI)* Open & render a local `.md` file |
| `Ctrl + Backspace` / `Ctrl + W` | *(TUI)* Delete previous word |
| `Ctrl + Y` / `Ctrl + L` | *(TUI)* Copy last reply / Toggle Linux Quick mode |

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
