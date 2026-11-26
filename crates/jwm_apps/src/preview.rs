//! Inert, bounded Markdown rendering: no HTML, script, file or network execution.
use crate::ui::{self, Rect};

const MAX_PREVIEW_BYTES: usize = 256 * 1024;

pub(crate) fn markup(source: &str) -> String {
    jwm_markdown::pango_markup(source, MAX_PREVIEW_BYTES)
        .unwrap_or_else(|| "预览限于 256 KiB；源文件仍可编辑和保存。".into())
}

struct ParsedMarkup {
    text: gio::glib::GString,
    attributes: pango::AttrList,
}

#[derive(Default)]
pub(crate) struct Preview {
    pub enabled: bool,
    pub scroll: f64,
    scroll_limit: f64,
    page_height: f64,
    revision: Option<u64>,
    parsed: Option<ParsedMarkup>,
    layout: Option<(ui::TextLayoutKey, pango::Layout)>,
}
impl Preview {
    pub(crate) fn scroll_by(&mut self, delta: f64) {
        if delta.is_finite() {
            self.scroll = (self.scroll + delta).clamp(0., self.scroll_limit);
        }
    }

    pub(crate) fn scroll_key(&mut self, key: winit::keyboard::NamedKey) {
        use winit::keyboard::NamedKey;
        let page = (self.page_height - 36.).max(36.);
        let next = match key {
            NamedKey::ArrowUp => self.scroll - 36.,
            NamedKey::ArrowDown => self.scroll + 36.,
            NamedKey::PageUp => self.scroll - page,
            NamedKey::PageDown => self.scroll + page,
            NamedKey::Home => 0.,
            NamedKey::End => self.scroll_limit,
            _ => return,
        };
        self.scroll = next.clamp(0., self.scroll_limit);
    }

    fn layout_for_draw(
        &mut self,
        cr: &cairo::Context,
        width: f64,
        source: &str,
        revision: u64,
    ) -> pango::Layout {
        if self.revision != Some(revision) {
            self.parsed = pango::parse_markup(&markup(source), '\0')
                .ok()
                .map(|(attributes, text, _)| ParsedMarkup { text, attributes });
            self.revision = Some(revision);
            self.layout = None;
        }
        let key = ui::TextLayoutKey::for_draw(cr, width);
        if let (Some(key), Some((cached_key, layout))) = (&key, &self.layout)
            && key == cached_key
        {
            // Even an unchanged PangoCairo update can invalidate shaped lines.
            return layout.clone();
        }

        let layout = ui::layout(cr, "", 15., width, true);
        if let Some(parsed) = &self.parsed {
            layout.set_text(parsed.text.as_str());
            layout.set_attributes(Some(&parsed.attributes));
        }
        self.layout = key.map(|key| (key, layout.clone()));
        layout
    }

    pub fn draw(&mut self, cr: &cairo::Context, area: Rect, source: &str, revision: u64) {
        let layout = self.layout_for_draw(cr, area.w, source, revision);
        self.page_height = area.h;
        self.scroll_limit = (f64::from(layout.pixel_size().1) - area.h).max(0.);
        self.scroll = self.scroll.clamp(0., self.scroll_limit);
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
        let text = &notes.preview.parsed.as_ref().unwrap().text;
        assert!(text.contains("first"));
        notes.preview.enabled = true;
        notes.preview.scroll = 20.;
        std::fs::write(&path, "# second").unwrap();
        notes.load(path.clone()).unwrap();
        assert!(!notes.preview.enabled);
        assert_eq!(notes.preview.scroll, 0.);
        notes
            .preview
            .draw(&cr, area, &notes.editor.text, notes.editor.revision);
        let text = &notes.preview.parsed.as_ref().unwrap().text;
        assert!(text.contains("second"));
        assert!(!text.contains("first"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cached_markup_keeps_pixels_and_reflows_at_current_width_and_scale() {
        let source = "# Heading\n\n**Bold** and *italic* text that wraps.";
        let next_source = "second revision";
        let mut preview = Preview::default();
        for (width, scale) in [(180., 1.), (180., 1.), (92., 1.), (92., 2.), (92., 2.)] {
            let height = 140.;
            let area = Rect::new(0., 0., width, height);
            let render = |draw: &mut dyn FnMut(&cairo::Context)| {
                let mut surface = cairo::ImageSurface::create(
                    cairo::Format::ARgb32,
                    (width * scale) as i32,
                    (height * scale) as i32,
                )
                .unwrap();
                surface.set_device_scale(scale, scale);
                let cr = cairo::Context::new(&surface).unwrap();
                draw(&cr);
                drop(cr);
                surface.data().unwrap().to_vec()
            };
            let expected = render(&mut |cr| {
                let layout = ui::layout(cr, "", 15., width, true);
                layout.set_markup(&markup(source));
                cr.move_to(0., 0.);
                ui::color(cr, ui::text_color());
                pangocairo::functions::show_layout(cr, &layout);
            });
            let actual = render(&mut |cr| {
                // The revision, not a redraw, drives replacement of the parse.
                let text = if preview.revision.is_some() {
                    next_source
                } else {
                    source
                };
                preview.draw(cr, area, text, 1);
            });
            assert!(expected.iter().any(|&byte| byte != 0));
            assert_eq!(actual, expected, "width {width}, scale {scale}");
        }
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 180, 140).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        preview.draw(&cr, Rect::new(0., 0., 180., 140.), next_source, 2);
        assert_eq!(
            preview.parsed.as_ref().unwrap().text.as_str(),
            "second revision\n\n"
        );
    }

    #[test]
    fn scrolling_preserves_shaped_lines_until_render_inputs_change() {
        let source = "# Heading\n\n".repeat(24);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 400, 280).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        let mut preview = Preview::default();
        preview.draw(&cr, Rect::new(0., 0., 180., 90.), &source, 1);
        let initial = preview.layout.as_ref().unwrap().1.clone();
        let serial = initial.serial();
        preview.scroll_by(36.);
        assert_eq!(preview.scroll, 36.);

        let next_cr = cairo::Context::new(&surface).unwrap();
        preview.draw(&next_cr, Rect::new(12., 12., 180., 110.), &source, 1);
        assert_eq!(&initial, &preview.layout.as_ref().unwrap().1);
        assert_eq!(initial.serial(), serial);
        assert_eq!(preview.scroll, 36.);

        next_cr.translate(4., 6.);
        let translated = preview.layout_for_draw(&next_cr, 180., &source, 1);
        assert_eq!(initial, translated);
        assert_eq!(translated.serial(), serial);

        next_cr.scale(1.25, 1.);
        let transformed = preview.layout_for_draw(&next_cr, 180., &source, 1);
        assert_ne!(initial, transformed);
        assert_eq!(initial.serial(), serial);

        let mut options = next_cr.font_options().unwrap();
        options.set_hint_metrics(cairo::HintMetrics::Off);
        next_cr.set_font_options(&options);
        let options_changed = preview.layout_for_draw(&next_cr, 180., &source, 1);
        assert_ne!(transformed, options_changed);

        let narrow = preview.layout_for_draw(&next_cr, 92., &source, 1);
        assert_ne!(options_changed, narrow);
        let hidpi = cairo::ImageSurface::create(cairo::Format::ARgb32, 400, 280).unwrap();
        hidpi.set_device_scale(2., 2.);
        let hidpi_cr = cairo::Context::new(&hidpi).unwrap();
        let resized = preview.layout_for_draw(&hidpi_cr, 92., &source, 1);
        assert_ne!(narrow, resized);

        let updated = preview.layout_for_draw(&hidpi_cr, 92., "New document revision", 2);
        assert_ne!(resized, updated);
        assert!(updated.text().contains("New document revision"));
    }

    #[test]
    fn large_preview_is_bounded_without_truncating_source() {
        let source = "文".repeat(MAX_PREVIEW_BYTES);
        assert!(markup(&source).contains("256 KiB"));
        assert_eq!(source.len(), MAX_PREVIEW_BYTES * 3);
    }
}
