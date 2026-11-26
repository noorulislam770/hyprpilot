use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Html,
    Svg,
    Markdown,
    Pdf,
    Image,
    CodeOrText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfInfo {
    pub pages: u32,
    pub title: Option<String>,
}

pub fn detect_preview_kind(path: &Path) -> PreviewKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" | "xhtml" => PreviewKind::Html,
        "svg" => PreviewKind::Svg,
        "md" | "markdown" | "mdx" => PreviewKind::Markdown,
        "pdf" => PreviewKind::Pdf,
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico" | "avif" => PreviewKind::Image,
        _ => PreviewKind::CodeOrText,
    }
}

pub fn expand_user_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

pub fn inspect_pdf(path: &Path) -> Result<PdfInfo> {
    let out = Command::new("pdfinfo")
        .arg(path)
        .output()
        .context("failed to run pdfinfo (install poppler)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("pdfinfo failed: {}", err.trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_pdfinfo_output(&text))
}

pub fn parse_pdfinfo_output(text: &str) -> PdfInfo {
    let mut pages = 1u32;
    let mut title = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                pages = n.max(1);
            }
        } else if let Some(rest) = line.strip_prefix("Title:") {
            let t = rest.trim();
            if !t.is_empty() {
                title = Some(t.to_string());
            }
        }
    }
    PdfInfo { pages, title }
}

pub fn render_pdf_page_png(
    pdf_path: &Path,
    page: u32,
    dpi: u32,
    out_dir: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(out_dir)?;
    let prefix = out_dir.join(format!("page_{page}_{dpi}"));
    let page_str = page.to_string();
    let dpi_str = dpi.to_string();
    let out = Command::new("pdftoppm")
        .args([
            "-png",
            "-r",
            &dpi_str,
            "-f",
            &page_str,
            "-l",
            &page_str,
            "-singlefile",
        ])
        .arg(pdf_path)
        .arg(&prefix)
        .output()
        .context("failed to run pdftoppm (install poppler)")?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("pdftoppm failed: {}", err.trim());
    }
    let png_path = prefix.with_extension("png");
    if !png_path.exists() {
        anyhow::bail!("pdftoppm did not produce {}", png_path.display());
    }
    Ok(png_path)
}

pub fn extract_pdf_text(pdf_path: &Path, page: Option<u32>) -> Result<String> {
    let mut cmd = Command::new("pdftotext");
    if let Some(p) = page {
        let ps = p.to_string();
        cmd.args(["-f", &ps, "-l", &ps]);
    }
    cmd.arg("-layout").arg(pdf_path).arg("-");
    let out = cmd
        .output()
        .context("failed to run pdftotext (install poppler)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("pdftotext failed: {}", err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Wraps raw SVG markup inside a clean centered HTML document for crisp WebKit rendering.
pub fn wrap_svg_as_html(svg: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <style>html,body{{margin:0;padding:24px;background:#1d1c1a;display:flex;\
         align-items:center;justify-content:center;min-height:100vh;box-sizing:border-box;}}\
         svg{{max-width:100%;height:auto;}}</style></head>\
         <body>{svg}</body></html>"
    )
}

/// Lightweight HTML-to-text/Markdown helper for terminal preview fallback.
pub fn html_to_readable_markdown(html: &str) -> String {
    let mut cleaned = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut skip_depth = 0usize;

    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            tag_buf.clear();
            continue;
        }
        if ch == '>' && in_tag {
            in_tag = false;
            let tag = tag_buf.trim().to_ascii_lowercase();
            let name = tag.split_whitespace().next().unwrap_or("");
            match name {
                "script" | "style" => skip_depth += 1,
                "/script" | "/style" => skip_depth = skip_depth.saturating_sub(1),
                "h1" => cleaned.push_str("\n# "),
                "h2" => cleaned.push_str("\n## "),
                "h3" | "h4" => cleaned.push_str("\n### "),
                "/h1" | "/h2" | "/h3" | "/h4" | "/p" | "/div" | "br" | "br/" => {
                    cleaned.push('\n');
                }
                "li" => cleaned.push_str("\n- "),
                "pre" => cleaned.push_str("\n```\n"),
                "/pre" => cleaned.push_str("\n```\n"),
                "code" => cleaned.push('`'),
                "/code" => cleaned.push('`'),
                _ => {}
            }
            continue;
        }
        if in_tag {
            tag_buf.push(ch);
        } else if skip_depth == 0 {
            cleaned.push(ch);
        }
    }

    cleaned
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_all_supported_preview_extensions() {
        assert_eq!(detect_preview_kind(Path::new("index.html")), PreviewKind::Html);
        assert_eq!(detect_preview_kind(Path::new("diagram.SVG")), PreviewKind::Svg);
        assert_eq!(detect_preview_kind(Path::new("README.md")), PreviewKind::Markdown);
        assert_eq!(detect_preview_kind(Path::new("paper.pdf")), PreviewKind::Pdf);
        assert_eq!(detect_preview_kind(Path::new("shot.png")), PreviewKind::Image);
        assert_eq!(detect_preview_kind(Path::new("main.rs")), PreviewKind::CodeOrText);
    }

    #[test]
    fn parses_pdfinfo_metadata() {
        let sample = "Title: Arch Linux Guide\nAuthor: Noor\nPages: 14\nPage size: 612 x 792 pts\n";
        let info = parse_pdfinfo_output(sample);
        assert_eq!(info.pages, 14);
        assert_eq!(info.title.as_deref(), Some("Arch Linux Guide"));
    }

    #[test]
    fn strips_scripts_and_converts_html_headings_for_terminal() {
        let html = "<html><head><style>body{color:red}</style></head><body><h1>Title</h1><p>Hello &amp; <code>world</code></p><ul><li>One</li></ul></body></html>";
        let md = html_to_readable_markdown(html);
        assert!(md.contains("# Title"));
        assert!(md.contains("Hello & `world`"));
        assert!(md.contains("- One"));
        assert!(!md.contains("color:red"));
    }
}
