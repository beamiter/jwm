//! Inert, bounded Markdown rendering: no HTML, script, file or network execution.
use crate::ui::{self, Rect};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const MAX_PREVIEW_BYTES: usize = 256 * 1024;

pub(crate) fn markup(source: &str) -> String {
    if source.len() > MAX_PREVIEW_BYTES {
        return "预览限于 256 KiB；源文件仍可编辑和保存。".into();
    }
    let mut output = String::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let size = match level {
                        pulldown_cmark::HeadingLevel::H1 => "xx-large",
                        pulldown_cmark::HeadingLevel::H2 => "x-large",
                        _ => "large",
                    };
                    output.push_str(&format!("<span weight=\"bold\" size=\"{size}\">"));
                }
                Tag::Strong => output.push_str("<b>"),
                Tag::Emphasis => output.push_str("<i>"),
                Tag::Strikethrough => output.push_str("<s>"),
                Tag::CodeBlock(_) => output.push_str("<tt>"),
                Tag::Link { .. } => output.push_str("<u>"),
                Tag::Image { .. } => output.push_str("[图片："),
                Tag::List(start) => lists.push(start),
                Tag::Item => {
                    output.push_str(&"  ".repeat(lists.len().saturating_sub(1).min(32)));
                    if let Some(Some(number)) = lists.last_mut() {
                        output.push_str(&format!("{number}. "));
                        *number = number.saturating_add(1);
                    } else {
                        output.push_str("• ");
                    }
                }
                Tag::BlockQuote(_) => output.push_str("│ "),
                _ => (),
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => output.push_str("</span>\n\n"),
                TagEnd::Strong => output.push_str("</b>"),
                TagEnd::Emphasis => output.push_str("</i>"),
                TagEnd::Strikethrough => output.push_str("</s>"),
                TagEnd::CodeBlock => output.push_str("</tt>\n"),
                TagEnd::Link => output.push_str("</u>"),
                TagEnd::Image => output.push(']'),
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::Paragraph | TagEnd::BlockQuote(_) | TagEnd::Table => {
                    output.push_str("\n\n")
                }
                TagEnd::Item | TagEnd::TableRow | TagEnd::TableHead => output.push('\n'),
                TagEnd::TableCell => output.push_str("  │  "),
                _ => (),
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                output.push_str(&gio::glib::markup_escape_text(&text));
            }
            Event::Code(text) => {
                output.push_str("<tt>");
                output.push_str(&gio::glib::markup_escape_text(&text));
                output.push_str("</tt>");
            }
            Event::SoftBreak => output.push(' '),
            Event::HardBreak => output.push('\n'),
            Event::Rule => output.push_str("\n────────────────\n"),
            Event::TaskListMarker(done) => output.push_str(if done { "☑ " } else { "☐ " }),
            _ => (),
        }
    }
    output
}

#[derive(Default)]
pub(crate) struct Preview {
    pub enabled: bool,
    pub scroll: f64,
    revision: Option<u64>,
    markup: String,
}
impl Preview {
    pub fn draw(&mut self, cr: &cairo::Context, area: Rect, source: &str, revision: u64) {
        if self.revision != Some(revision) {
            self.markup = markup(source);
            self.revision = Some(revision);
        }
        // Layout inherits current font/scale on every draw; Markdown parsing is
        // cached across caret redraws and invalidated only by source revisions.
        let layout = ui::layout(cr, "", 15., area.w, true);
        layout.set_markup(&self.markup);
        self.scroll = self
            .scroll
            .clamp(0., (f64::from(layout.pixel_size().1) - area.h).max(0.));
        let _ = cr.save();
        cr.rectangle(area.x, area.y, area.w, area.h);
        cr.clip();
        cr.move_to(area.x, area.y - self.scroll);
        ui::color(cr, ui::text_color());
        pangocairo::functions::show_layout(cr, &layout);
        let _ = cr.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markdown_styles_and_unicode_are_preserved() {
        let rendered = markup("# 标题\n\n**粗体** *斜体* `a < b`\n\n- [x] 完成");
        assert!(rendered.contains("标题</span>"));
        assert!(rendered.contains("<b>粗体</b>"));
        assert!(rendered.contains("<i>斜体</i>"));
        assert!(rendered.contains("<tt>a &lt; b</tt>"));
        assert!(rendered.contains("☑ 完成"));
        assert!(pango::parse_markup(&rendered, '\0').is_ok());
    }
    #[test]
    fn hostile_markup_and_urls_are_inert() {
        let rendered = markup(
            "<script>alert(1)</script>\n\n[x](javascript:alert) ![alt](file:///secret)\n\n<span foreground=\"red\">raw</span>",
        );
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(!rendered.contains("javascript:"));
        assert!(!rendered.contains("file:///"));
        assert!(rendered.contains("[图片：alt]"));
        assert!(pango::parse_markup(&rendered, '\0').is_ok());
    }
    #[test]
    fn ordered_lists_and_complex_markup_remain_valid() {
        for source in [
            "3. one\n4. two\n   - child",
            "# *em **bold*** & <tag>\n",
            "| a | b |\n|---|---|\n| *x* | `y` |",
            "> quote\n>\n> paragraph",
        ] {
            let rendered = markup(source);
            assert!(pango::parse_markup(&rendered, '\0').is_ok(), "{rendered}");
        }
        assert!(markup("3. one\n4. two").contains("4. two"));
    }
    #[test]
    fn same_revision_document_switch_resets_preview_cache() {
        let path =
            std::env::temp_dir().join(format!("jwm-preview-cache-{}.md", std::process::id()));
        std::fs::write(&path, "# first").unwrap();
        let mut notes = crate::notes::Notes::new(Some(path.clone())).unwrap();
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 200, 100).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        let area = Rect::new(0., 0., 200., 100.);
        notes
            .preview
            .draw(&cr, area, &notes.editor.text, notes.editor.revision);
        assert!(notes.preview.markup.contains("first"));
        notes.preview.enabled = true;
        notes.preview.scroll = 20.;
        std::fs::write(&path, "# second").unwrap();
        notes.load(path.clone()).unwrap();
        assert!(!notes.preview.enabled);
        assert_eq!(notes.preview.scroll, 0.);
        notes
            .preview
            .draw(&cr, area, &notes.editor.text, notes.editor.revision);
        assert!(notes.preview.markup.contains("second"));
        assert!(!notes.preview.markup.contains("first"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn large_preview_is_bounded_without_truncating_source() {
        let source = "文".repeat(MAX_PREVIEW_BYTES);
        assert!(markup(&source).contains("256 KiB"));
        assert_eq!(source.len(), MAX_PREVIEW_BYTES * 3);
    }
}
