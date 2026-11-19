//! Inert, bounded Markdown rendering: no HTML, script, file or network execution.
use crate::ui::{self, Rect};
use pango::prelude::*;
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

#[derive(PartialEq)]
struct LayoutKey {
    matrix: cairo::Matrix,
    format: Option<cairo::Format>,
    fallback_resolution: (f64, f64),
    device_scale: (f64, f64),
    device_offset: (f64, f64),
    options: cairo::FontOptions,
    font: Option<pango::FontDescription>,
    width: i32,
    font_map: Option<pango::FontMap>,
    font_map_serial: Option<u32>,
    resolution: f64,
    language: Option<pango::Language>,
    direction: pango::Direction,
    gravity: pango::Gravity,
    gravity_hint: pango::GravityHint,
}

#[derive(Default)]
pub(crate) struct Preview {
    pub enabled: bool,
    pub scroll: f64,
    revision: Option<u64>,
    markup: String,
    source: Option<String>,
    layout: Option<pango::Layout>,
    key: Option<LayoutKey>,
}
impl Preview {
    pub fn draw(&mut self, cr: &cairo::Context, area: Rect, source: &str, revision: u64) {
        // Compare bounded source bytes as well as revision: callers can replace
        // a document with the same revision. Oversized sources share one warning
        // and must never be retained by this cache.
        let bounded = (source.len() <= MAX_PREVIEW_BYTES).then_some(source);
        let changed = self.revision != Some(revision) || self.source.as_deref() != bounded;
        if changed {
            self.markup = markup(source);
            self.source = bounded.map(str::to_owned);
            self.revision = Some(revision);
        }
        // Reuse only the native host's image canvas. Other surface backends
        // remain uncached rather than guessing their font-option dependencies.
        let descriptor = ui::layout(cr, "", 15., area.w, true);
        let context = descriptor.context();
        let target = cr.target();
        let key = cr
            .font_options()
            .ok()
            .filter(|_| target.type_() == cairo::SurfaceType::Image)
            .map(|options| LayoutKey {
                matrix: cr.matrix(),
                format: cairo::ImageSurface::try_from(target.clone())
                    .ok()
                    .map(|s| s.format()),
                fallback_resolution: target.fallback_resolution(),
                device_scale: target.device_scale(),
                device_offset: target.device_offset(),
                options,
                font: descriptor.font_description(),
                width: descriptor.width(),
                font_map: context.font_map(),
                font_map_serial: context.font_map().map(|m| m.serial()),
                resolution: pangocairo::functions::context_get_resolution(&context),
                language: context.language(),
                direction: context.base_dir(),
                gravity: context.gravity(),
                gravity_hint: context.gravity_hint(),
            });
        if changed || key.is_none() || self.key != key || self.layout.is_none() {
            descriptor.set_markup(&self.markup);
            self.layout = Some(descriptor);
            self.key = key;
        }
        let layout = self.layout.as_ref().unwrap();
        self.scroll = self
            .scroll
            .clamp(0., (f64::from(layout.pixel_size().1) - area.h).max(0.));
        let _ = cr.save();
        cr.rectangle(area.x, area.y, area.w, area.h);
        cr.clip();
        cr.move_to(area.x, area.y - self.scroll);
        ui::color(cr, ui::text_color());
        pangocairo::functions::show_layout(cr, layout);
        let _ = cr.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_layout_matches_fresh_context_content_and_geometry() {
        fn render(
            p: &mut Preview,
            source: &str,
            revision: u64,
            width: f64,
            scale: f64,
            scroll: f64,
            translate: f64,
        ) -> Vec<u8> {
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 800, 600).unwrap();
            surface.set_device_scale(scale, scale);
            let cr = cairo::Context::new(&surface).unwrap();
            cr.translate(translate, 0.);
            let mut options = cairo::FontOptions::new().unwrap();
            options.set_antialias(if translate == 0. {
                cairo::Antialias::Gray
            } else {
                cairo::Antialias::None
            });
            cr.set_font_options(&options);
            p.scroll = scroll;
            p.draw(&cr, Rect::new(10., 10., width, 240.), source, revision);
            drop(cr);
            surface.data().unwrap().to_vec()
        }
        let mut cached = Preview::default();
        let source = "# 标题 École\n\n**内容** 中文 ABC\n\n".repeat(30);
        let changed = source.replace("内容", "替换");
        let too_large = "x".repeat(MAX_PREVIEW_BYTES + 1);
        for (text, rev, width, scale, scroll, translate) in [
            (&source, 1, 360., 1., 0., 0.),
            (&source, 1, 360., 1., 0., 0.),
            (&changed, 1, 360., 1., 0., 0.),
            (&source, 2, 180., 1., 70., 0.),
            (&source, 2, 180., 2., 70., 0.),
            (&source, 2, 290., 1., 30., 0.5),
            (&too_large, 2, 290., 1., 0., 0.),
            (&source, 2, 290., 1., 0., 0.),
        ] {
            let expected = render(
                &mut Preview::default(),
                text,
                rev,
                width,
                scale,
                scroll,
                translate,
            );
            assert_eq!(
                render(&mut cached, text, rev, width, scale, scroll, translate),
                expected
            );
            assert!(
                cached
                    .source
                    .as_ref()
                    .is_none_or(|s| s.len() <= MAX_PREVIEW_BYTES)
            );
        }
        let theme = crate::theme::NativeTheme {
            font: "Serif 18".into(),
            ..Default::default()
        };
        ui::apply_theme(theme);
        assert_eq!(
            render(&mut cached, &source, 2, 290., 1., 0., 0.),
            render(&mut Preview::default(), &source, 2, 290., 1., 0., 0.)
        );
        ui::apply_theme(crate::theme::NativeTheme::default());
    }

    #[test]
    fn cache_release_and_oversized_source_are_bounded() {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 80, 80).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        let mut preview = Preview::default();
        let area = Rect::new(0., 0., 70., 70.);
        preview.draw(&cr, area, "owned source", 1);
        let old_layout = preview.layout.as_ref().unwrap().downgrade();
        preview.draw(&cr, area, &"x".repeat(MAX_PREVIEW_BYTES + 1), 1);
        assert!(old_layout.upgrade().is_none());
        assert!(preview.source.is_none());
        let last_layout = preview.layout.as_ref().unwrap().downgrade();
        drop(preview);
        assert!(last_layout.upgrade().is_none());
        drop(cr);
        // Native host must still have exclusive access for its pixel upload.
        let mut surface = surface;
        assert!(surface.data().is_ok());
    }

    #[test]
    #[ignore = "local bounded layout benchmark"]
    fn preview_layout_cost() {
        for bytes in [16 * 1024, 128 * 1024] {
            let line = "## 标题 École Αθήνα\n\n正文 **Unicode** TARGET 目标 example text.\n\n- item `code` 中文\n\n";
            let source = line.repeat(bytes / line.len());
            let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
            let area = Rect::new(0., 0., 432., 500.);
            let mut cached = Preview::default();
            let cold_start = std::time::Instant::now();
            cached.draw(&cairo::Context::new(&surface).unwrap(), area, &source, 1);
            let cold = cold_start.elapsed().as_micros();
            let mut samples = vec![];
            for _ in 0..8 {
                let mut pair = vec![];
                for reuse in [false, true] {
                    if !reuse {
                        cached.layout = None;
                    }
                    // The native host makes a new Cairo context on the same
                    // image canvas each frame. Do exactly that here.
                    let cr = cairo::Context::new(&surface).unwrap();
                    let previous = cached.layout.clone();
                    let start = std::time::Instant::now();
                    cached.draw(&cr, area, &source, 1);
                    surface.flush();
                    pair.push(start.elapsed().as_micros());
                    if reuse {
                        assert_eq!(previous.as_ref(), cached.layout.as_ref());
                    }
                }
                samples.push(pair);
            }
            eprintln!(
                "PREVIEW_PAIRED bytes={} cold_us={cold} samples_rebuild_reuse_us={samples:?}",
                source.len()
            );
        }
    }

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
