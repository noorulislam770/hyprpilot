use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::copy_to_clipboard;
use super::markdown_gtk::render_markdown_widgets;
use crate::preview::{
    PreviewKind, detect_preview_kind, expand_user_path, extract_pdf_text, inspect_pdf,
    render_pdf_page_png, wrap_svg_as_html,
};
use crate::storage::config::state_dir;

#[derive(Clone)]
pub struct PreviewPane {
    pub root: gtk4::Box,
    state: Rc<RefCell<PreviewState>>,
}

struct PreviewState {
    path_entry: gtk4::Entry,
    stack: gtk4::Stack,
    pdf_bar: gtk4::Box,
    pdf_prev_btn: gtk4::Button,
    pdf_page_lbl: gtk4::Label,
    pdf_next_btn: gtk4::Button,
    pdf_text_btn: gtk4::ToggleButton,
    picture: gtk4::Picture,
    md_box: gtk4::Box,
    text_view: gtk4::TextView,
    status_lbl: gtk4::Label,
    #[cfg(feature = "web-portal")]
    web_container: gtk4::Box,
    #[cfg(feature = "web-portal")]
    webview: Option<webkit6::WebView>,
    current_pdf: Option<PathBuf>,
    pdf_page: u32,
    pdf_total: u32,
    pdf_dpi: u32,
    on_close: Option<Box<dyn Fn()>>,
}

impl PreviewPane {
    pub fn new(parent_window: Option<gtk4::Window>) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.set_vexpand(true);
        root.set_hexpand(true);
        root.add_css_class("preview-split-panel");

        // Top bar
        let top_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        top_bar.add_css_class("portal-tab-bar");

        let open_btn = gtk4::Button::with_label("Open");
        open_btn.add_css_class("suggested-action");
        open_btn.set_tooltip_text(Some("Open HTML, PDF, SVG, Markdown, Image, or Code file"));

        let path_entry = gtk4::Entry::new();
        path_entry.set_placeholder_text(Some("Path to .html, .pdf, .svg, .md, image..."));
        path_entry.set_hexpand(true);

        let go_btn = gtk4::Button::from_icon_name("go-next-symbolic");
        go_btn.set_tooltip_text(Some("Load path"));

        let close_btn = gtk4::Button::from_icon_name("window-close-symbolic");
        close_btn.add_css_class("flat");
        close_btn.set_tooltip_text(Some("Close Preview Panel"));

        top_bar.append(&open_btn);
        top_bar.append(&path_entry);
        top_bar.append(&go_btn);
        top_bar.append(&close_btn);

        // PDF Controls Bar (hidden unless viewing PDF)
        let pdf_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        pdf_bar.add_css_class("portal-tab-bar");
        pdf_bar.set_visible(false);

        let pdf_prev_btn = gtk4::Button::from_icon_name("go-previous-symbolic");
        let pdf_page_lbl = gtk4::Label::new(Some("Page 1 / 1"));
        let pdf_next_btn = gtk4::Button::from_icon_name("go-next-symbolic");
        let pdf_zoom_out = gtk4::Button::with_label("−");
        let pdf_zoom_in = gtk4::Button::with_label("+");
        let pdf_text_btn = gtk4::ToggleButton::with_label("Extract Text");
        pdf_text_btn.add_css_class("claude-pill-toggle");

        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);

        pdf_bar.append(&pdf_prev_btn);
        pdf_bar.append(&pdf_page_lbl);
        pdf_bar.append(&pdf_next_btn);
        pdf_bar.append(&spacer);
        pdf_bar.append(&pdf_zoom_out);
        pdf_bar.append(&pdf_zoom_in);
        pdf_bar.append(&pdf_text_btn);

        // Content Stack
        let stack = gtk4::Stack::new();
        stack.set_vexpand(true);
        stack.set_hexpand(true);

        // 1. Empty view
        let empty_box = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        empty_box.set_valign(gtk4::Align::Center);
        empty_box.set_halign(gtk4::Align::Center);
        let e_spark = gtk4::Label::new(Some("✦"));
        e_spark.add_css_class("claude-welcome-spark");
        let e_title = gtk4::Label::new(Some("Preview Engine"));
        e_title.add_css_class("claude-welcome-title");
        let e_sub = gtk4::Label::new(Some(
            "Render HTML, SVG, PDFs, Markdown, Images, and source files.\nOr click Preview ↗ on any HTML/SVG block inside Native Chat.",
        ));
        e_sub.add_css_class("claude-welcome-sub");
        e_sub.set_justify(gtk4::Justification::Center);
        empty_box.append(&e_spark);
        empty_box.append(&e_title);
        empty_box.append(&e_sub);
        stack.add_named(&empty_box, Some("empty"));

        // 2. Picture view (Images & PDF pages)
        let picture = gtk4::Picture::new();
        picture.set_content_fit(gtk4::ContentFit::Contain);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);

        let pic_scroll = gtk4::ScrolledWindow::new();
        pic_scroll.set_vexpand(true);
        pic_scroll.set_hexpand(true);
        pic_scroll.set_child(Some(&picture));
        stack.add_named(&pic_scroll, Some("picture"));

        // 3. Markdown view
        let md_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        md_box.set_margin_top(16);
        md_box.set_margin_bottom(24);
        md_box.set_margin_start(20);
        md_box.set_margin_end(20);
        let md_clamp = adw::Clamp::new();
        md_clamp.set_maximum_size(780);
        md_clamp.set_child(Some(&md_box));
        let md_scroll = gtk4::ScrolledWindow::new();
        md_scroll.set_vexpand(true);
        md_scroll.set_hexpand(true);
        md_scroll.set_child(Some(&md_clamp));
        stack.add_named(&md_scroll, Some("markdown"));

        // 4. Text / Code view
        let text_outer = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        let text_toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        text_toolbar.set_margin_top(6);
        text_toolbar.set_margin_end(10);
        let text_spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        text_spacer.set_hexpand(true);
        let copy_text_btn = gtk4::Button::with_label("Copy All");
        copy_text_btn.add_css_class("flat");
        text_toolbar.append(&text_spacer);
        text_toolbar.append(&copy_text_btn);

        let text_view = gtk4::TextView::new();
        text_view.set_editable(false);
        text_view.set_monospace(true);
        text_view.set_wrap_mode(gtk4::WrapMode::WordChar);
        text_view.set_margin_top(8);
        text_view.set_margin_bottom(12);
        text_view.set_margin_start(14);
        text_view.set_margin_end(14);
        let text_scroll = gtk4::ScrolledWindow::new();
        text_scroll.set_vexpand(true);
        text_scroll.set_hexpand(true);
        text_scroll.set_child(Some(&text_view));
        text_outer.append(&text_toolbar);
        text_outer.append(&text_scroll);
        stack.add_named(&text_outer, Some("text"));

        // 5. Web view (HTML / SVG)
        #[cfg(feature = "web-portal")]
        let web_container = {
            let b = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            b.set_vexpand(true);
            b.set_hexpand(true);
            stack.add_named(&b, Some("web"));
            b
        };
        #[cfg(not(feature = "web-portal"))]
        {
            let lbl = gtk4::Label::new(Some("WebKitGTK is required for live HTML rendering."));
            stack.add_named(&lbl, Some("web"));
        }

        stack.set_visible_child_name("empty");

        let status_lbl = gtk4::Label::new(None);
        status_lbl.add_css_class("dim-label");
        status_lbl.set_halign(gtk4::Align::Start);
        status_lbl.set_margin_start(10);
        status_lbl.set_margin_bottom(4);
        status_lbl.set_visible(false);

        root.append(&top_bar);
        root.append(&pdf_bar);
        root.append(&stack);
        root.append(&status_lbl);

        let state = Rc::new(RefCell::new(PreviewState {
            path_entry: path_entry.clone(),
            stack,
            pdf_bar,
            pdf_prev_btn: pdf_prev_btn.clone(),
            pdf_page_lbl,
            pdf_next_btn: pdf_next_btn.clone(),
            pdf_text_btn: pdf_text_btn.clone(),
            picture,
            md_box,
            text_view: text_view.clone(),
            status_lbl,
            #[cfg(feature = "web-portal")]
            web_container,
            #[cfg(feature = "web-portal")]
            webview: None,
            current_pdf: None,
            pdf_page: 1,
            pdf_total: 1,
            pdf_dpi: 150,
            on_close: None,
        }));

        let pane = Self { root, state };
        pane.wire_signals(
            parent_window,
            open_btn,
            path_entry,
            go_btn,
            close_btn,
            pdf_prev_btn,
            pdf_next_btn,
            pdf_zoom_out,
            pdf_zoom_in,
            pdf_text_btn,
            copy_text_btn,
        );
        pane
    }

    pub fn set_on_close(&self, cb: impl Fn() + 'static) {
        self.state.borrow_mut().on_close = Some(Box::new(cb));
    }

    #[allow(clippy::too_many_arguments)]
    fn wire_signals(
        &self,
        parent_window: Option<gtk4::Window>,
        open_btn: gtk4::Button,
        path_entry: gtk4::Entry,
        go_btn: gtk4::Button,
        close_btn: gtk4::Button,
        pdf_prev_btn: gtk4::Button,
        pdf_next_btn: gtk4::Button,
        pdf_zoom_out: gtk4::Button,
        pdf_zoom_in: gtk4::Button,
        pdf_text_btn: gtk4::ToggleButton,
        copy_text_btn: gtk4::Button,
    ) {
        {
            let st = self.state.clone();
            close_btn.connect_clicked(move |_| {
                if let Some(cb) = st.borrow().on_close.as_ref() {
                    cb();
                }
            });
        }
        {
            let pane = self.clone();
            open_btn.connect_clicked(move |_| {
                let dialog = gtk4::FileDialog::new();
                dialog.set_title("Open File to Preview");
                let pane2 = pane.clone();
                dialog.open(parent_window.as_ref(), gtk4::gio::Cancellable::NONE, move |res| {
                    if let Ok(file) = res {
                        if let Some(p) = file.path() {
                            pane2.open_file(&p);
                        }
                    }
                });
            });
        }

        {
            let pane = self.clone();
            let entry = path_entry.clone();
            go_btn.connect_clicked(move |_| {
                let raw = entry.text().to_string();
                if !raw.trim().is_empty() {
                    pane.open_file(&expand_user_path(&raw));
                }
            });
        }
        {
            let pane = self.clone();
            path_entry.connect_activate(move |e| {
                let raw = e.text().to_string();
                if !raw.trim().is_empty() {
                    pane.open_file(&expand_user_path(&raw));
                }
            });
        }

        {
            let pane = self.clone();
            pdf_prev_btn.connect_clicked(move |_| pane.step_pdf_page(-1));
        }
        {
            let pane = self.clone();
            pdf_next_btn.connect_clicked(move |_| pane.step_pdf_page(1));
        }
        {
            let pane = self.clone();
            pdf_zoom_out.connect_clicked(move |_| pane.adjust_pdf_dpi(-30));
        }
        {
            let pane = self.clone();
            pdf_zoom_in.connect_clicked(move |_| pane.adjust_pdf_dpi(30));
        }
        {
            let pane = self.clone();
            pdf_text_btn.connect_toggled(move |_| pane.refresh_current_pdf());
        }

        {
            let st = self.state.clone();
            copy_text_btn.connect_clicked(move |_| {
                let tv = st.borrow().text_view.clone();
                let buf = tv.buffer();
                let txt = buf.text(&buf.start_iter(), &buf.end_iter(), false);
                copy_to_clipboard(&txt);
            });
        }
    }

    pub fn open_file(&self, path: &Path) {
        {
            let st = self.state.borrow();
            st.path_entry.set_text(&path.display().to_string());
            st.status_lbl.set_visible(false);
        }

        if !path.exists() {
            self.show_error(&format!("File not found: {}", path.display()));
            return;
        }

        let kind = detect_preview_kind(path);
        {
            let mut st = self.state.borrow_mut();
            st.pdf_bar.set_visible(kind == PreviewKind::Pdf);
            if kind != PreviewKind::Pdf {
                st.current_pdf = None;
            }
        }

        match kind {
            PreviewKind::Html => {
                let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                self.load_web_uri(&format!("file://{}", abs.display()));
            }
            PreviewKind::Svg => match fs::read_to_string(path) {
                Ok(svg) => self.load_web_html(&wrap_svg_as_html(&svg)),
                Err(e) => self.show_error(&format!("Failed to read SVG: {e}")),
            },
            PreviewKind::Markdown => match fs::read_to_string(path) {
                Ok(md) => self.render_markdown(&md),
                Err(e) => self.show_error(&format!("Failed to read Markdown: {e}")),
            },
            PreviewKind::Image => {
                let st = self.state.borrow();
                st.picture.set_filename(Some(path));
                st.stack.set_visible_child_name("picture");
            }
            PreviewKind::Pdf => match inspect_pdf(path) {
                Ok(info) => {
                    {
                        let mut st = self.state.borrow_mut();
                        st.current_pdf = Some(path.to_path_buf());
                        st.pdf_page = 1;
                        st.pdf_total = info.pages;
                        st.pdf_text_btn.set_active(false);
                    }
                    self.refresh_current_pdf();
                }
                Err(e) => self.show_error(&format!("PDF error: {e:#}")),
            },
            PreviewKind::CodeOrText => match fs::read_to_string(path) {
                Ok(txt) => self.render_plain_text(&txt),
                Err(e) => self.show_error(&format!("Cannot preview binary/unreadable file: {e}")),
            },
        }
    }

    pub fn preview_inline(&self, kind: PreviewKind, title: &str, content: &str) {
        {
            let mut st = self.state.borrow_mut();
            st.path_entry.set_text(title);
            st.pdf_bar.set_visible(false);
            st.current_pdf = None;
            st.status_lbl.set_visible(false);
        }
        match kind {
            PreviewKind::Html => self.load_web_html(content),
            PreviewKind::Svg => self.load_web_html(&wrap_svg_as_html(content)),
            PreviewKind::Markdown => self.render_markdown(content),
            _ => self.render_plain_text(content),
        }
    }

    fn step_pdf_page(&self, delta: i32) {
        {
            let mut st = self.state.borrow_mut();
            let next = (st.pdf_page as i32 + delta).clamp(1, st.pdf_total as i32) as u32;
            if next == st.pdf_page {
                return;
            }
            st.pdf_page = next;
        }
        self.refresh_current_pdf();
    }

    fn adjust_pdf_dpi(&self, delta: i32) {
        {
            let mut st = self.state.borrow_mut();
            st.pdf_dpi = (st.pdf_dpi as i32 + delta).clamp(90, 300) as u32;
        }
        self.refresh_current_pdf();
    }

    fn refresh_current_pdf(&self) {
        let (pdf_opt, page, total, dpi, text_mode) = {
            let st = self.state.borrow();
            st.pdf_page_lbl
                .set_text(&format!("Page {} / {}", st.pdf_page, st.pdf_total));
            st.pdf_prev_btn.set_sensitive(st.pdf_page > 1);
            st.pdf_next_btn.set_sensitive(st.pdf_page < st.pdf_total);
            (
                st.current_pdf.clone(),
                st.pdf_page,
                st.pdf_total,
                st.pdf_dpi,
                st.pdf_text_btn.is_active(),
            )
        };
        let Some(pdf_path) = pdf_opt else { return };

        if text_mode {
            match extract_pdf_text(&pdf_path, Some(page)) {
                Ok(txt) => self.render_plain_text(&txt),
                Err(e) => self.show_error(&format!("PDF text error: {e:#}")),
            }
            return;
        }

        let cache_dir = state_dir().join("pdf-preview");
        match render_pdf_page_png(&pdf_path, page, dpi, &cache_dir) {
            Ok(png_path) => {
                let st = self.state.borrow();
                st.picture.set_filename(Some(&png_path));
                st.stack.set_visible_child_name("picture");
                st.status_lbl
                    .set_text(&format!("Rendered page {page}/{total} at {dpi} DPI"));
                st.status_lbl.set_visible(true);
            }
            Err(e) => self.show_error(&format!("PDF render error: {e:#}")),
        }
    }

    fn render_markdown(&self, md: &str) {
        let (md_box, stack) = {
            let st = self.state.borrow();
            (st.md_box.clone(), st.stack.clone())
        };
        while let Some(child) = md_box.first_child() {
            md_box.remove(&child);
        }
        render_markdown_widgets(&md_box, md);
        stack.set_visible_child_name("markdown");
    }

    fn render_plain_text(&self, txt: &str) {
        let st = self.state.borrow();
        st.text_view.buffer().set_text(txt);
        st.stack.set_visible_child_name("text");
    }

    fn show_error(&self, msg: &str) {
        let st = self.state.borrow();
        st.status_lbl.set_text(msg);
        st.status_lbl.set_visible(true);
    }

    #[cfg(feature = "web-portal")]
    fn ensure_webview(&self) -> webkit6::WebView {
        use webkit6::prelude::*;
        let existing = self.state.borrow().webview.clone();
        if let Some(wv) = existing {
            return wv;
        }
        let settings = webkit6::Settings::new();
        settings.set_enable_javascript(true);
        settings.set_enable_webgl(true);
        let wv = webkit6::WebView::builder().settings(&settings).build();
        wv.set_vexpand(true);
        wv.set_hexpand(true);
        let mut st = self.state.borrow_mut();
        st.web_container.append(&wv);
        st.webview = Some(wv.clone());
        wv
    }

    fn load_web_uri(&self, uri: &str) {
        #[cfg(feature = "web-portal")]
        {
            use webkit6::prelude::*;
            let wv = self.ensure_webview();
            wv.load_uri(uri);
            self.state.borrow().stack.set_visible_child_name("web");
        }
        #[cfg(not(feature = "web-portal"))]
        {
            self.show_error(&format!("WebKitGTK disabled; cannot render {uri}"));
        }
    }

    fn load_web_html(&self, html: &str) {
        #[cfg(feature = "web-portal")]
        {
            use webkit6::prelude::*;
            let wv = self.ensure_webview();
            wv.load_html(html, None);
            self.state.borrow().stack.set_visible_child_name("web");
        }
        #[cfg(not(feature = "web-portal"))]
        {
            self.render_plain_text(html);
        }
    }

    pub fn self_test_preview(&self) {
        self.preview_inline(
            PreviewKind::Markdown,
            "inline://readme.md",
            "# Test\n\nPreview **OK**",
        );
        assert_eq!(
            self.state.borrow().stack.visible_child_name().as_deref(),
            Some("markdown")
        );

        self.preview_inline(
            PreviewKind::Html,
            "inline://card.html",
            "<h1>Hello HTML</h1>",
        );
        #[cfg(feature = "web-portal")]
        assert_eq!(
            self.state.borrow().stack.visible_child_name().as_deref(),
            Some("web")
        );

        self.preview_inline(
            PreviewKind::Svg,
            "inline://icon.svg",
            "<svg width=\"40\" height=\"40\"><circle cx=\"20\" cy=\"20\" r=\"16\"/></svg>",
        );
        self.preview_inline(PreviewKind::CodeOrText, "inline://note.txt", "plain text");
        assert_eq!(
            self.state.borrow().stack.visible_child_name().as_deref(),
            Some("text")
        );
    }
}
