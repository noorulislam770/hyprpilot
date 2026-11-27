use std::collections::BTreeMap;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use hyprpilot::ai::models::fetch_available_models;
use hyprpilot::ai::{AiMessage, StreamEvent, stream_chat};
use hyprpilot::hyprland::{render_hyprland_conf, render_hyprland_lua};
use hyprpilot::markdown::render_markdown_ansi;
use hyprpilot::storage::config::{AuthMode, ProviderKind, ProviderProfile};
use hyprpilot::storage::db::HistoryDb;
use hyprpilot::ui::markdown_gtk::{MdBlock, parse_markdown_blocks};

async fn spawn_mock_server(routes: Vec<(&'static str, &'static str, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let mut matched = None;
                for (method_path, content_type, body) in &routes {
                    if first_line.starts_with(method_path) {
                        matched = Some((*content_type, body.clone()));
                        break;
                    }
                }
                let (ct, body) = matched.unwrap_or((
                    "application/json",
                    r#"{"error":{"message":"not found"}}"#.to_string(),
                ));
                let status = if first_line.is_empty() {
                    "400 Bad Request"
                } else {
                    "200 OK"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn openai_models_discovery_and_sse_streaming_end_to_end() {
    let models_json = r#"{
        "data": [
            {"id": "gpt-4o"},
            {"id": "o3-pro"},
            {"id": "whisper-1"},
            {"id": "gpt-4.1-nano"}
        ]
    }"#
    .to_string();
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" HyprPilot\"}}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_string();

    let base = spawn_mock_server(vec![
        ("GET /v1/models", "application/json", models_json),
        ("POST /v1/chat/completions", "text/event-stream", sse_body),
    ])
    .await;

    let provider = ProviderProfile {
        id: "openai".into(),
        name: "OpenAI".into(),
        kind: ProviderKind::OpenAiCompatible,
        auth_mode: AuthMode::ApiKey,
        base_url: format!("{base}/v1"),
        model: "gpt-4o".into(),
        available_models: Vec::new(),
        token: "test-token".into(),
        extra_headers: BTreeMap::new(),
    };

    let client = reqwest::Client::new();
    let fetched = fetch_available_models(&client, &provider).await.unwrap();
    assert!(!fetched.models.contains(&"whisper-1".to_string()));
    assert_eq!(fetched.recommended.as_deref(), Some("gpt-4.1-nano"));

    let (tx, mut rx) = mpsc::unbounded_channel();
    stream_chat(
        client,
        provider,
        Some("Be brief".into()),
        vec![AiMessage {
            role: "user".into(),
            content: "hi".into(),
        }],
        tx,
    )
    .await;

    let mut collected = String::new();
    let mut saw_done = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Delta(d) => collected.push_str(&d),
            StreamEvent::Stats(_) => {}
            StreamEvent::Done => {
                saw_done = true;
                break;
            }
            StreamEvent::Error(e) => panic!("unexpected stream error: {e}"),
        }
    }
    assert!(saw_done);
    assert_eq!(collected, "Hello HyprPilot");
}

#[tokio::test]
async fn gemini_and_anthropic_sse_streaming_end_to_end() {
    let gemini_sse = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"find .\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" -type d\"}]}}]}\n\n"
    )
    .to_string();
    let anthropic_sse = concat!(
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Arch\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" Linux\"}}\n\n"
    )
    .to_string();

    let base = spawn_mock_server(vec![
        (
            "POST /v1beta/models/gemini-2.5-flash-lite:streamGenerateContent",
            "text/event-stream",
            gemini_sse,
        ),
        ("POST /v1/messages", "text/event-stream", anthropic_sse),
    ])
    .await;

    let client = reqwest::Client::new();

    // Test Gemini (including accidental models/ prefix stripping)
    let gemini = ProviderProfile {
        id: "gemini".into(),
        name: "Google Gemini".into(),
        kind: ProviderKind::Gemini,
        auth_mode: AuthMode::ApiKey,
        base_url: format!("{base}/v1beta"),
        model: "models/gemini-2.5-flash-lite".into(),
        available_models: Vec::new(),
        token: "gem-key".into(),
        extra_headers: BTreeMap::new(),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    stream_chat(
        client.clone(),
        gemini,
        None,
        vec![AiMessage {
            role: "user".into(),
            content: "dir".into(),
        }],
        tx,
    )
    .await;
    let mut gem_out = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::Delta(d) => gem_out.push_str(&d),
            StreamEvent::Stats(_) => {}
            StreamEvent::Done => break,
            StreamEvent::Error(e) => panic!("gemini stream error: {e}"),
        }
    }
    assert_eq!(gem_out, "find . -type d");

    // Test Anthropic
    let claude = ProviderProfile {
        id: "anthropic".into(),
        name: "Claude".into(),
        kind: ProviderKind::Anthropic,
        auth_mode: AuthMode::ApiKey,
        base_url: format!("{base}/v1"),
        model: "claude-3-5-haiku-latest".into(),
        available_models: Vec::new(),
        token: "ant-key".into(),
        extra_headers: BTreeMap::new(),
    };
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    stream_chat(
        client,
        claude,
        Some("Quick".into()),
        vec![AiMessage {
            role: "user".into(),
            content: "os".into(),
        }],
        tx2,
    )
    .await;
    let mut claude_out = String::new();
    while let Some(ev) = rx2.recv().await {
        match ev {
            StreamEvent::Delta(d) => claude_out.push_str(&d),
            StreamEvent::Stats(_) => {}
            StreamEvent::Done => break,
            StreamEvent::Error(e) => panic!("anthropic stream error: {e}"),
        }
    }
    assert_eq!(claude_out, "Arch Linux");
}

#[test]
fn chat_history_regenerate_and_markdown_pipeline() {
    let db = HistoryDb::open(Path::new(":memory:")).unwrap();
    let cid = db
        .create_conversation("Find Dir", "gemini", "gemini-2.5-flash-lite")
        .unwrap();
    db.add_message(cid, "user", "how to find dir").unwrap();
    db.add_message(cid, "assistant", "First reply").unwrap();
    assert!(db.delete_last_message_if_role(cid, "assistant").unwrap());
    db.add_message(
        cid,
        "assistant",
        "# Result\nUse `find / -type d`:\n```bash\nfind . -type d\n```",
    )
    .unwrap();

    let msgs = db.get_messages(cid).unwrap();
    assert_eq!(msgs.len(), 2);
    let blocks = parse_markdown_blocks(&msgs[1].content);
    assert!(matches!(blocks[0], MdBlock::Heading { level: 1, .. }));
    assert!(matches!(blocks[2], MdBlock::Code { ref lang, .. } if lang == "bash"));

    let ansi = render_markdown_ansi(&msgs[1].content, 80);
    assert!(ansi.contains("\x1b["));
    let stripped = strip_ansi(&ansi);
    assert!(stripped.contains("Result"));
    assert!(stripped.contains("find . -type d"));
}

#[test]
fn preview_engine_detects_formats_and_renders_html_and_svg() {
    use hyprpilot::preview::{
        detect_preview_kind, html_to_readable_markdown, parse_pdfinfo_output, wrap_svg_as_html,
        PreviewKind,
    };

    assert_eq!(detect_preview_kind(Path::new("index.html")), PreviewKind::Html);
    assert_eq!(detect_preview_kind(Path::new("diagram.svg")), PreviewKind::Svg);
    assert_eq!(detect_preview_kind(Path::new("notes.md")), PreviewKind::Markdown);
    assert_eq!(detect_preview_kind(Path::new("paper.pdf")), PreviewKind::Pdf);
    assert_eq!(detect_preview_kind(Path::new("shot.png")), PreviewKind::Image);
    assert_eq!(detect_preview_kind(Path::new("src/main.rs")), PreviewKind::CodeOrText);

    let wrapped_svg = wrap_svg_as_html("<svg><circle cx=\"5\" cy=\"5\" r=\"4\"/></svg>");
    assert!(wrapped_svg.contains("<!DOCTYPE html>"));
    assert!(wrapped_svg.contains("<circle cx=\"5\" cy=\"5\" r=\"4\"/>"));

    let md = html_to_readable_markdown("<h1>Hello</h1><p>World &amp; <code>rust</code></p>");
    assert!(md.contains("# Hello"));
    assert!(md.contains("World & `rust`"));

    let ansi = render_markdown_ansi(&md, 80);
    let stripped = strip_ansi(&ansi);
    assert!(stripped.contains("Hello"));
    assert!(stripped.contains("World &"));

    let info = parse_pdfinfo_output("Title: Sample Report\nPages: 12\n");
    assert_eq!(info.pages, 12);
    assert_eq!(info.title.as_deref(), Some("Sample Report"));
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn swapped_hotkeys_render_distinct_gui_and_tui_binds() {
    let lua = render_hyprland_lua("ALT + SHIFT + SPACE", "ALT + SPACE", "hyprpilot-toggle");
    assert!(lua.contains("hl.bind(\"ALT + SHIFT + SPACE\", hl.dsp.exec_cmd(\"hyprpilot-toggle\")"));
    assert!(lua.contains("hl.bind(\"ALT + SPACE\", hl.dsp.exec_cmd(\"hyprpilot-toggle --tui\")"));

    let conf = render_hyprland_conf("ALT + SHIFT + SPACE", "ALT + SPACE", "hyprpilot-toggle");
    assert!(conf.contains("bind = ALT SHIFT, SPACE, exec, hyprpilot-toggle\n"));
    assert!(conf.contains("bind = ALT, SPACE, exec, hyprpilot-toggle --tui\n"));
}
