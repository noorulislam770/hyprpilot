use gtk4::glib::markup_escape_text;
use gtk4::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::copy_to_clipboard;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdBlock {
    Heading { level: u8, pango: String },
    Paragraph { pango: String },
    ListItem { bullet: String, indent: usize, pango: String },
    Blockquote { pango: String },
    Code { lang: String, code: String },
    Table { headers: Vec<String>, rows: Vec<Vec<String>> },
    Rule,
}

enum ListState {
    Unordered,
    Ordered(u64),
}

pub fn parse_markdown_blocks(input: &str) -> Vec<MdBlock> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(input, opts);
    let mut blocks = Vec::new();
    let mut cur_pango = String::new();
    let mut heading: Option<HeadingLevel> = None;
    let mut blockquote_depth: usize = 0;
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut pending_item: Option<(String, usize)> = None;
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut in_table = false;
    let mut in_table_head = false;
    let mut table_headers = Vec::new();
    let mut table_rows = Vec::new();
    let mut cur_row = Vec::new();
    let mut cur_cell = String::new();
    let mut link_open = false;

    let flush_inline = |cur_pango: &mut String,
                        heading: Option<HeadingLevel>,
                        blockquote_depth: usize,
                        pending_item: &mut Option<(String, usize)>,
                        blocks: &mut Vec<MdBlock>| {
        let trimmed = cur_pango.trim();
        if trimmed.is_empty() {
            cur_pango.clear();
            return;
        }
        let text = trimmed.to_string();
        cur_pango.clear();
        if let Some(level) = heading {
            let lvl = match level {
                HeadingLevel::H1 => 1,
                HeadingLevel::H2 => 2,
                HeadingLevel::H3 => 3,
                HeadingLevel::H4 => 4,
                HeadingLevel::H5 => 5,
                HeadingLevel::H6 => 6,
            };
            blocks.push(MdBlock::Heading { level: lvl, pango: text });
        } else if let Some((bullet, indent)) = pending_item.take() {
            blocks.push(MdBlock::ListItem {
                bullet,
                indent,
                pango: text,
            });
        } else if blockquote_depth > 0 {
            blocks.push(MdBlock::Blockquote { pango: text });
        } else {
            blocks.push(MdBlock::Paragraph { pango: text });
        }
    };

    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    heading = Some(level);
                }
                Tag::Paragraph => {
                    if pending_item.is_none() {
                        flush_inline(
                            &mut cur_pango,
                            heading,
                            blockquote_depth,
                            &mut pending_item,
                            &mut blocks,
                        );
                    }
                }
                Tag::BlockQuote(_) => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    blockquote_depth += 1;
                }
                Tag::CodeBlock(kind) => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    in_code_block = true;
                    code_buf.clear();
                    code_lang = match kind {
                        CodeBlockKind::Fenced(l) => l.trim().to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                }
                Tag::List(start) => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    match start {
                        Some(n) => list_stack.push(ListState::Ordered(n)),
                        None => list_stack.push(ListState::Unordered),
                    }
                }
                Tag::Item => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    let indent = list_stack.len().saturating_sub(1);
                    let bullet = match list_stack.last_mut() {
                        Some(ListState::Ordered(n)) => {
                            let s = format!("{n}.");
                            *n += 1;
                            s
                        }
                        _ => "•".to_string(),
                    };
                    pending_item = Some((bullet, indent));
                }
                Tag::Strong => cur_pango.push_str("<b>"),
                Tag::Emphasis => cur_pango.push_str("<i>"),
                Tag::Strikethrough => cur_pango.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    let url = markup_escape_text(&dest_url);
                    if url.starts_with("http://") || url.starts_with("https://") {
                        cur_pango.push_str(&format!("<a href=\"{url}\">"));
                        link_open = true;
                    } else {
                        cur_pango.push_str("<u>");
                        link_open = true;
                    }
                }
                Tag::Table(_) => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    in_table = true;
                    table_headers.clear();
                    table_rows.clear();
                }
                Tag::TableHead => {
                    in_table_head = true;
                    cur_row.clear();
                }
                Tag::TableRow => cur_row.clear(),
                Tag::TableCell => cur_cell.clear(),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    heading = None;
                }
                TagEnd::Paragraph => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                }
                TagEnd::BlockQuote(_) => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    blocks.push(MdBlock::Code {
                        lang: std::mem::take(&mut code_lang),
                        code: std::mem::take(&mut code_buf).trim_end().to_string(),
                    });
                    in_code_block = false;
                }
                TagEnd::List(_) => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    list_stack.pop();
                }
                TagEnd::Item => {
                    flush_inline(
                        &mut cur_pango,
                        heading,
                        blockquote_depth,
                        &mut pending_item,
                        &mut blocks,
                    );
                    pending_item = None;
                }
                TagEnd::Strong => cur_pango.push_str("</b>"),
                TagEnd::Emphasis => cur_pango.push_str("</i>"),
                TagEnd::Strikethrough => cur_pango.push_str("</s>"),
                TagEnd::Link => {
                    if link_open {
                        // Close whichever tag was opened (<a> or <u>)
                        if cur_pango.rfind("<a ").is_some_and(|a_pos| {
                            cur_pango.rfind("<u>").is_none_or(|u_pos| a_pos > u_pos)
                        }) {
                            cur_pango.push_str("</a>");
                        } else {
                            cur_pango.push_str("</u>");
                        }
                        link_open = false;
                    }
                }
                TagEnd::TableCell => {
                    cur_row.push(std::mem::take(&mut cur_cell).trim().to_string());
                }
                TagEnd::TableHead => {
                    in_table_head = false;
                    table_headers = std::mem::take(&mut cur_row);
                }
                TagEnd::TableRow => {
                    if !in_table_head {
                        table_rows.push(std::mem::take(&mut cur_row));
                    }
                }
                TagEnd::Table => {
                    in_table = false;
                    blocks.push(MdBlock::Table {
                        headers: std::mem::take(&mut table_headers),
                        rows: std::mem::take(&mut table_rows),
                    });
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else if in_table {
                    cur_cell.push_str(&text);
                } else {
                    cur_pango.push_str(&markup_escape_text(&text));
                }
            }
            Event::Code(code) => {
                if in_table {
                    cur_cell.push_str(&code);
                } else {
                    let esc = markup_escape_text(&code);
                    cur_pango.push_str(&format!(
                        "<tt><span background=\"#171614\" foreground=\"#E8A87C\"> {esc} </span></tt>"
                    ));
                }
            }
            Event::SoftBreak => {
                if in_table {
                    cur_cell.push(' ');
                } else {
                    cur_pango.push(' ');
                }
            }
            Event::HardBreak => {
                if in_table {
                    cur_cell.push(' ');
                } else {
                    cur_pango.push('\n');
                }
            }
            Event::Rule => {
                flush_inline(
                    &mut cur_pango,
                    heading,
                    blockquote_depth,
                    &mut pending_item,
                    &mut blocks,
                );
                blocks.push(MdBlock::Rule);
            }
            Event::TaskListMarker(checked) => {
                if let Some((ref mut bullet, _)) = pending_item {
                    *bullet = if checked { "☑" } else { "☐" }.to_string();
                }
            }
            _ => {}
        }
    }

    flush_inline(
        &mut cur_pango,
        heading,
        blockquote_depth,
        &mut pending_item,
        &mut blocks,
    );
    if in_code_block && !code_buf.trim().is_empty() {
        blocks.push(MdBlock::Code {
            lang: code_lang,
            code: code_buf.trim_end().to_string(),
        });
    }
    blocks
}

pub fn render_markdown_widgets(container: &gtk4::Box, content: &str) {
    for block in parse_markdown_blocks(content) {
        match block {
            MdBlock::Heading { level, pango } => {
                let lbl = gtk4::Label::new(None);
                lbl.set_markup(&pango);
                lbl.set_wrap(true);
                lbl.set_selectable(true);
                lbl.set_xalign(0.0);
                lbl.add_css_class(match level {
                    1 => "md-h1",
                    2 => "md-h2",
                    _ => "md-h3",
                });
                container.append(&lbl);
            }
            MdBlock::Paragraph { pango } => {
                let lbl = gtk4::Label::new(None);
                lbl.set_markup(&pango);
                lbl.set_wrap(true);
                lbl.set_selectable(true);
                lbl.set_xalign(0.0);
                lbl.add_css_class("md-paragraph");
                container.append(&lbl);
            }
            MdBlock::ListItem {
                bullet,
                indent,
                pango,
            } => {
                let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
                row.set_margin_start((indent as i32) * 18);
                let b_lbl = gtk4::Label::new(Some(&bullet));
                b_lbl.set_valign(gtk4::Align::Start);
                b_lbl.add_css_class("md-bullet");
                let text_lbl = gtk4::Label::new(None);
                text_lbl.set_markup(&pango);
                text_lbl.set_wrap(true);
                text_lbl.set_selectable(true);
                text_lbl.set_xalign(0.0);
                text_lbl.set_hexpand(true);
                text_lbl.add_css_class("md-paragraph");
                row.append(&b_lbl);
                row.append(&text_lbl);
                container.append(&row);
            }
            MdBlock::Blockquote { pango } => {
                let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
                row.add_css_class("md-blockquote");
                let lbl = gtk4::Label::new(None);
                lbl.set_markup(&pango);
                lbl.set_wrap(true);
                lbl.set_selectable(true);
                lbl.set_xalign(0.0);
                lbl.set_hexpand(true);
                row.append(&lbl);
                container.append(&row);
            }
            MdBlock::Code { lang, code } => {
                container.append(&build_code_card(&lang, &code));
            }
            MdBlock::Table { headers, rows } => {
                container.append(&build_table_card(&headers, &rows));
            }
            MdBlock::Rule => {
                let sep = gtk4::Separator::new(gtk4::Orientation::Horizontal);
                sep.set_margin_top(6);
                sep.set_margin_bottom(6);
                container.append(&sep);
            }
        }
    }
}

fn build_code_card(lang: &str, code: &str) -> gtk4::Box {
    let card = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    card.add_css_class("code-card");

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.add_css_class("code-card-header");
    let lang_lbl = gtk4::Label::new(Some(if lang.is_empty() { "code" } else { lang }));
    lang_lbl.add_css_class("code-card-lang");
    lang_lbl.set_hexpand(true);
    lang_lbl.set_halign(gtk4::Align::Start);

    let copy_btn = gtk4::Button::with_label("Copy code");
    copy_btn.add_css_class("flat");
    copy_btn.add_css_class("code-copy-btn");
    let code_owned = code.to_string();
    copy_btn.connect_clicked(move |btn| {
        copy_to_clipboard(&code_owned);
        btn.set_label("Copied ✓");
    });

    header.append(&lang_lbl);
    header.append(&copy_btn);

    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
    let code_lbl = gtk4::Label::new(Some(code));
    code_lbl.add_css_class("code-card-text");
    code_lbl.set_selectable(true);
    code_lbl.set_xalign(0.0);
    scroller.set_child(Some(&code_lbl));

    card.append(&header);
    card.append(&scroller);
    card
}

fn build_table_card(headers: &[String], rows: &[Vec<String>]) -> gtk4::Frame {
    let frame = gtk4::Frame::new(None);
    frame.add_css_class("md-table-card");

    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);

    let grid = gtk4::Grid::new();
    grid.set_column_spacing(16);
    grid.set_row_spacing(6);
    grid.set_margin_top(8);
    grid.set_margin_bottom(8);
    grid.set_margin_start(12);
    grid.set_margin_end(12);

    for (c, h) in headers.iter().enumerate() {
        let lbl = gtk4::Label::new(Some(h));
        lbl.add_css_class("md-table-header");
        lbl.set_xalign(0.0);
        grid.attach(&lbl, c as i32, 0, 1, 1);
    }

    for (r, row) in rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let lbl = gtk4::Label::new(Some(cell));
            lbl.set_xalign(0.0);
            lbl.set_selectable(true);
            grid.attach(&lbl, c as i32, (r + 1) as i32, 1, 1);
        }
    }

    scroller.set_child(Some(&grid));
    frame.set_child(Some(&scroller));
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_lists_code_quotes_tables_and_escapes_xml() {
        let md = "# Title <1>\n\nHello **world** & `a < b`!\n\n- [x] Task one\n- Item two\n\n> Quoted line\n\n| Col1 | Col2 |\n| --- | --- |\n| A | B |\n\n```bash\necho \"hi\"\n```\n";
        let blocks = parse_markdown_blocks(md);
        assert!(matches!(blocks[0], MdBlock::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], MdBlock::Paragraph { ref pango } if pango.contains("<b>world</b>") && pango.contains("&amp;") && pango.contains("a &lt; b")));
        assert!(matches!(blocks[2], MdBlock::ListItem { ref bullet, .. } if bullet == "☑"));
        assert!(matches!(blocks[3], MdBlock::ListItem { ref bullet, .. } if bullet == "•"));
        assert!(matches!(blocks[4], MdBlock::Blockquote { .. }));
        assert!(matches!(blocks[5], MdBlock::Table { .. }));
        assert!(matches!(blocks[6], MdBlock::Code { ref lang, .. } if lang == "bash"));
    }
}
