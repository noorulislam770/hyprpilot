use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::fmt::Write as _;

/// Renders CommonMark / GFM Markdown into colorful Ratatui `Line`s.
/// `line_prefix` (if provided) is prepended to every output line (e.g. a colored quote bar).
pub fn render_markdown_lines(
    input: &str,
    line_prefix: Option<Span<'static>>,
    max_width: usize,
) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(input, opts);
    let mut r = Renderer {
        out: Vec::new(),
        prefix: line_prefix,
        max_width: max_width.max(40),
        cur_spans: Vec::new(),
        bold: false,
        italic: false,
        strike: false,
        heading: None,
        blockquote_depth: 0,
        list_stack: Vec::new(),
        pending_list_marker: None,
        in_code_block: false,
        code_buf: String::new(),
        in_table: false,
        table_header: Vec::new(),
        table_rows: Vec::new(),
        cur_row: Vec::new(),
        cur_cell: String::new(),
        in_table_head: false,
        link_url: None,
    };

    for ev in parser {
        r.handle_event(ev);
    }
    r.flush_line();
    // Trim trailing empty blank lines
    while r.out.last().is_some_and(|l| l.spans.is_empty() || l.width() == 0) {
        r.out.pop();
    }
    r.out
}

/// Renders Markdown to a 24-bit/ANSI colored string suitable for printing to stdout.
pub fn render_markdown_ansi(input: &str, max_width: usize) -> String {
    let lines = render_markdown_lines(input, None, max_width);
    lines_to_ansi(&lines)
}

pub fn lines_to_ansi(lines: &[Line<'_>]) -> String {
    let mut out = String::new();
    for line in lines {
        for span in &line.spans {
            let has_style = span.style.fg.is_some()
                || span.style.bg.is_some()
                || !span.style.add_modifier.is_empty();
            if has_style {
                write_ansi_prefix(&mut out, span.style);
                out.push_str(&span.content);
                out.push_str("\x1b[0m");
            } else {
                out.push_str(&span.content);
            }
        }
        out.push('\n');
    }
    out
}

fn write_ansi_prefix(out: &mut String, style: Style) {
    out.push_str("\x1b[");
    let mut first = true;
    let mut push_code = |code: &str| {
        if !first {
            out.push(';');
        }
        first = false;
        out.push_str(code);
    };

    if style.add_modifier.contains(Modifier::BOLD) {
        push_code("1");
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        push_code("3");
    }
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        push_code("4");
    }
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        push_code("9");
    }
    if let Some(fg) = style.fg {
        let code = color_to_ansi(fg, false);
        push_code(&code);
    }
    if let Some(bg) = style.bg {
        let code = color_to_ansi(bg, true);
        push_code(&code);
    }
    out.push('m');
}

fn color_to_ansi(c: Color, is_bg: bool) -> String {
    let base = if is_bg { 40 } else { 30 };
    let bright = if is_bg { 100 } else { 90 };
    match c {
        Color::Black => format!("{base}"),
        Color::Red => format!("{}", base + 1),
        Color::Green => format!("{}", base + 2),
        Color::Yellow => format!("{}", base + 3),
        Color::Blue => format!("{}", base + 4),
        Color::Magenta => format!("{}", base + 5),
        Color::Cyan => format!("{}", base + 6),
        Color::Gray => format!("{}", base + 7),
        Color::DarkGray => format!("{bright}"),
        Color::LightRed => format!("{}", bright + 1),
        Color::LightGreen => format!("{}", bright + 2),
        Color::LightYellow => format!("{}", bright + 3),
        Color::LightBlue => format!("{}", bright + 4),
        Color::LightMagenta => format!("{}", bright + 5),
        Color::LightCyan => format!("{}", bright + 6),
        Color::White => format!("{}", bright + 7),
        Color::Rgb(r, g, b) => {
            let prefix = if is_bg { 48 } else { 38 };
            format!("{prefix};2;{r};{g};{b}")
        }
        _ => format!("{}", base + 7),
    }
}

enum ListKind {
    Unordered,
    Ordered(u64),
}

struct Renderer {
    out: Vec<Line<'static>>,
    prefix: Option<Span<'static>>,
    max_width: usize,
    cur_spans: Vec<Span<'static>>,
    bold: bool,
    italic: bool,
    strike: bool,
    heading: Option<HeadingLevel>,
    blockquote_depth: usize,
    list_stack: Vec<ListKind>,
    pending_list_marker: Option<Vec<Span<'static>>>,
    in_code_block: bool,
    code_buf: String,
    in_table: bool,
    table_header: Vec<String>,
    table_rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
    cur_cell: String,
    in_table_head: bool,
    link_url: Option<String>,
}

impl Renderer {
    fn handle_event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => self.push_inline_code(&code),
            Event::SoftBreak => {
                if self.in_table {
                    self.cur_cell.push(' ');
                } else {
                    self.push_text(" ");
                }
            }
            Event::HardBreak => {
                if self.in_table {
                    self.cur_cell.push(' ');
                } else {
                    self.flush_line();
                }
            }
            Event::Rule => {
                self.flush_line();
                let w = self.max_width.saturating_sub(6).clamp(16, 64);
                self.emit_line(vec![Span::styled(
                    "─".repeat(w),
                    Style::default().fg(Color::DarkGray),
                )]);
                self.emit_blank();
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked {
                    Span::styled(
                        "[✓] ",
                        Style::default()
                            .fg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("[ ] ", Style::default().fg(Color::DarkGray))
                };
                if let Some(ref mut pending) = self.pending_list_marker {
                    // Replace bullet with task checkbox
                    if pending.len() >= 2 {
                        pending.pop();
                    }
                    pending.push(marker);
                } else {
                    self.ensure_line_prefix();
                    self.cur_spans.push(marker);
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let t = html.trim();
                if !t.is_empty() {
                    self.push_text(t);
                }
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.heading = Some(level);
            }
            Tag::Paragraph => {
                // Keep list item paragraph inline with its bullet
                if self.pending_list_marker.is_none() {
                    self.flush_line();
                }
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                self.code_buf.clear();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.trim().to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let label = if lang.is_empty() { "code" } else { lang.as_str() };
                self.emit_line(vec![Span::styled(
                    format!("┌─ {label} ─────────────"),
                    Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::BOLD),
                )]);
            }
            Tag::List(start) => {
                self.flush_line();
                match start {
                    Some(n) => self.list_stack.push(ListKind::Ordered(n)),
                    None => self.list_stack.push(ListKind::Unordered),
                }
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(ListKind::Ordered(n)) => {
                        let s = format!("{n}. ");
                        *n += 1;
                        Span::styled(
                            s,
                            Style::default()
                                .fg(Color::LightYellow)
                                .add_modifier(Modifier::BOLD),
                        )
                    }
                    _ => {
                        let sym = match depth % 3 {
                            0 => "• ",
                            1 => "◦ ",
                            _ => "▪ ",
                        };
                        Span::styled(sym, Style::default().fg(Color::LightMagenta))
                    }
                };
                self.pending_list_marker = Some(vec![Span::raw(indent), marker]);
            }
            Tag::Emphasis => self.italic = true,
            Tag::Strong => self.bold = true,
            Tag::Strikethrough => self.strike = true,
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
            }
            Tag::Table(_) => {
                self.flush_line();
                self.in_table = true;
                self.table_header.clear();
                self.table_rows.clear();
            }
            Tag::TableHead => {
                self.in_table_head = true;
                self.cur_row.clear();
            }
            Tag::TableRow => {
                self.cur_row.clear();
            }
            Tag::TableCell => {
                self.cur_cell.clear();
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(level) => {
                let text_len: usize = self.cur_spans.iter().map(|s| s.width()).sum();
                self.flush_line();
                if matches!(level, HeadingLevel::H1) {
                    let rule_w = text_len.clamp(8, 56);
                    self.emit_line(vec![Span::styled(
                        "═".repeat(rule_w),
                        Style::default().fg(Color::LightMagenta),
                    )]);
                }
                self.heading = None;
                self.emit_blank();
            }
            TagEnd::Paragraph => {
                self.flush_line();
                if self.list_stack.is_empty() {
                    self.emit_blank();
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                if self.blockquote_depth == 0 {
                    self.emit_blank();
                }
            }
            TagEnd::CodeBlock => {
                let code = std::mem::take(&mut self.code_buf);
                for line in code.lines() {
                    self.emit_code_line(line);
                }
                self.emit_line(vec![Span::styled(
                    "└───────────────────────",
                    Style::default().fg(Color::LightBlue),
                )]);
                self.in_code_block = false;
                self.emit_blank();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.emit_blank();
                }
            }
            TagEnd::Item => {
                self.flush_line();
                self.pending_list_marker = None;
            }
            TagEnd::Emphasis => self.italic = false,
            TagEnd::Strong => self.bold = false,
            TagEnd::Strikethrough => self.strike = false,
            TagEnd::Link => {
                if let Some(url) = self.link_url.take() {
                    if !url.is_empty() && !self.in_table {
                        self.cur_spans.push(Span::styled(
                            format!(" ({url})"),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.cur_cell).trim().to_string();
                self.cur_row.push(cell);
            }
            TagEnd::TableHead => {
                self.in_table_head = false;
                self.table_header = std::mem::take(&mut self.cur_row);
            }
            TagEnd::TableRow => {
                if !self.in_table_head {
                    let row = std::mem::take(&mut self.cur_row);
                    self.table_rows.push(row);
                }
            }
            TagEnd::Table => {
                self.in_table = false;
                self.render_table();
                self.emit_blank();
            }
            _ => {}
        }
    }

    fn current_style(&self) -> Style {
        let mut style = if let Some(level) = self.heading {
            let color = match level {
                HeadingLevel::H1 => Color::LightMagenta,
                HeadingLevel::H2 => Color::LightCyan,
                HeadingLevel::H3 => Color::LightBlue,
                _ => Color::LightYellow,
            };
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else if self.link_url.is_some() {
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::UNDERLINED)
        } else if self.blockquote_depth > 0 {
            Style::default()
                .fg(Color::Rgb(175, 195, 220))
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(Color::Rgb(230, 235, 245))
        };

        if self.bold {
            style = style.fg(Color::White).add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            style = style
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }

    fn ensure_line_prefix(&mut self) {
        if self.cur_spans.is_empty() {
            if self.blockquote_depth > 0 {
                let bars = "▎ ".repeat(self.blockquote_depth);
                self.cur_spans
                    .push(Span::styled(bars, Style::default().fg(Color::LightBlue)));
            }
            if let Some(marker) = self.pending_list_marker.take() {
                self.cur_spans.extend(marker);
            } else if self.heading.is_some() {
                // Keep headings clean without list indentation
            }
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_buf.push_str(text);
            return;
        }
        if self.in_table {
            self.cur_cell.push_str(text);
            return;
        }
        self.ensure_line_prefix();
        let style = self.current_style();
        self.cur_spans.push(Span::styled(text.to_string(), style));
    }

    fn push_inline_code(&mut self, code: &str) {
        if self.in_table {
            self.cur_cell.push_str(code);
            return;
        }
        self.ensure_line_prefix();
        self.cur_spans.push(Span::styled(
            format!(" {code} "),
            Style::default()
                .bg(Color::Rgb(42, 48, 62))
                .fg(Color::LightYellow),
        ));
    }

    fn flush_line(&mut self) {
        if !self.cur_spans.is_empty() {
            let spans = std::mem::take(&mut self.cur_spans);
            self.emit_line(spans);
        }
    }

    fn emit_line(&mut self, mut spans: Vec<Span<'static>>) {
        if let Some(ref pfx) = self.prefix {
            let mut full = Vec::with_capacity(spans.len() + 1);
            full.push(pfx.clone());
            full.extend(spans);
            spans = full;
        }
        self.out.push(Line::from(spans));
    }

    fn emit_blank(&mut self) {
        let already_blank = self.out.last().is_some_and(|l| {
            let prefix_only = self.prefix.as_ref().map(|p| p.width()).unwrap_or(0);
            l.width() <= prefix_only
        });
        if !self.out.is_empty() && !already_blank {
            self.emit_line(Vec::new());
        }
    }

    fn emit_code_line(&mut self, line: &str) {
        let mut spans = vec![Span::styled("│ ", Style::default().fg(Color::LightBlue))];
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            spans.push(Span::styled(
                line.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
        } else {
            for (idx, tok) in line.split_inclusive(' ').enumerate() {
                let color = if idx == 0 && !tok.trim().starts_with('-') {
                    Color::LightGreen
                } else if tok.trim_start().starts_with('-') {
                    Color::LightCyan
                } else if tok.contains('"') || tok.contains('\'') {
                    Color::LightYellow
                } else {
                    Color::Rgb(220, 230, 245)
                };
                spans.push(Span::styled(tok.to_string(), Style::default().fg(color)));
            }
        }
        self.emit_line(spans);
    }

    fn render_table(&mut self) {
        let cols = self
            .table_header
            .len()
            .max(self.table_rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if cols == 0 {
            return;
        }
        let mut widths = vec![3usize; cols];
        for (i, h) in self.table_header.iter().enumerate() {
            widths[i] = widths[i].max(h.chars().count());
        }
        for row in &self.table_rows {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
        // Clamp columns cleanly if table is wider than viewport
        let budget = self.max_width.saturating_sub(cols * 3 + 4).max(cols * 4);
        let total: usize = widths.iter().sum();
        if total > budget {
            for w in &mut widths {
                *w = ((*w * budget) / total).max(6);
            }
        }

        let border_line = |left: &str, mid: &str, right: &str| -> Vec<Span<'static>> {
            let mut s = String::from(left);
            for (i, w) in widths.iter().enumerate() {
                let _ = write!(s, "{}", "─".repeat(w + 2));
                if i + 1 < widths.len() {
                    s.push_str(mid);
                }
            }
            s.push_str(right);
            vec![Span::styled(s, Style::default().fg(Color::Rgb(80, 110, 150)))]
        };

        self.emit_line(border_line("┌", "┬", "┐"));
        if !self.table_header.is_empty() {
            self.emit_table_row(&self.table_header.clone(), &widths, true);
            self.emit_line(border_line("├", "┼", "┤"));
        }
        let rows = self.table_rows.clone();
        for row in &rows {
            self.emit_table_row(row, &widths, false);
        }
        self.emit_line(border_line("└", "┴", "┘"));
    }

    fn emit_table_row(&mut self, row: &[String], widths: &[usize], is_header: bool) {
        let border_style = Style::default().fg(Color::Rgb(80, 110, 150));
        let cell_style = if is_header {
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(230, 235, 245))
        };
        let mut spans = vec![Span::styled("│", border_style)];
        for (i, w) in widths.iter().enumerate() {
            let val = row.get(i).map(|s| s.as_str()).unwrap_or("");
            let padded = pad_or_truncate(val, *w);
            spans.push(Span::styled(format!(" {padded} "), cell_style));
            spans.push(Span::styled("│", border_style));
        }
        self.emit_line(spans);
    }
}

fn pad_or_truncate(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count > width {
        if width <= 1 {
            return s.chars().take(width).collect();
        }
        let mut truncated: String = s.chars().take(width - 1).collect();
        truncated.push('…');
        truncated
    } else {
        format!("{s}{}", " ".repeat(width - count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_gfm_tables_code_lists_and_quotes() {
        let md = "# Title\n\n> A helpful quote\n\n- [x] Done task\n- Item with `inline` and **bold**\n\n| Key | Action |\n| --- | --- |\n| Ctrl+O | Open MD |\n\n```rust\nfn main() {}\n```\n";
        let lines = render_markdown_lines(md, None, 100);
        let plain: String = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("Title"));
        assert!(plain.contains("▎ A helpful quote"));
        assert!(plain.contains("[✓] Done task"));
        assert!(plain.contains("┌─"));
        assert!(plain.contains("Ctrl+O"));
        assert!(plain.contains("┌─ rust"));
    }

    #[test]
    fn ansi_renderer_emits_escape_codes() {
        let ansi = render_markdown_ansi("# Hello\n\n**World** and `code`", 80);
        assert!(ansi.contains("\x1b["));
        assert!(ansi.contains("Hello"));
        assert!(ansi.contains("World"));
    }
}
