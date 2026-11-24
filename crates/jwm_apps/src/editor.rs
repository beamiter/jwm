//! UTF-8 editor shared by the document, search and file-path fields.
use crate::ui::{self, Rect};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use winit::{
    event::KeyEvent,
    keyboard::{Key, ModifiersState, NamedKey},
};

struct Edit {
    start: usize,
    removed: String,
    inserted: String,
}
#[derive(Default)]
struct Metrics {
    position: Option<(u64, usize, (usize, usize))>,
    newlines: Option<(u64, &'static str)>,
}
#[derive(Default)]
pub(crate) struct Editor {
    pub text: String,
    pub cursor: usize,
    pub anchor: usize,
    pub revision: u64,
    pub scroll: f64,
    horizontal: f64,
    pub preedit: String,
    pub highlight_selection: bool,
    pub monospace: bool,
    metrics: Box<Metrics>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    layout: Option<pango::Layout>,
    area: Rect,
    reveal: bool,
}
impl Editor {
    pub fn new(text: String) -> Self {
        Self {
            text,
            reveal: true,
            ..Self::default()
        }
    }
    pub fn toggle_monospace(&mut self) {
        self.monospace = !self.monospace;
        self.layout = None;
        self.horizontal = 0.;
        self.reveal = true;
    }
    /// Logical lines, not visual wraps; one-based extended grapheme columns.
    /// Tabs count as one column. CRLF is a single line break.
    pub fn position(&mut self) -> (usize, usize) {
        if let Some((revision, cursor, position)) = self.metrics.position
            && revision == self.revision
            && cursor == self.cursor
        {
            return position;
        }
        let mut position = (1, 1);
        for grapheme in self.text[..self.before_crlf(self.cursor)].graphemes(true) {
            if matches!(grapheme, "\n" | "\r\n" | "\r") {
                position = (position.0 + 1, 1);
            } else {
                position.1 += 1;
            }
        }
        self.metrics.position = Some((self.revision, self.cursor, position));
        position
    }
    /// Restore a logical caret in changed text, clamping at line/document end.
    pub fn move_to_position(&mut self, line: usize, column: usize) {
        let (mut row, mut col) = (1, 1);
        let mut target = self.text.len();
        for (offset, grapheme) in self.text.grapheme_indices(true) {
            let newline = matches!(grapheme, "\n" | "\r\n" | "\r");
            if row == line && (col >= column || newline) {
                target = offset;
                break;
            }
            if newline {
                row += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        self.select(target..target);
    }
    pub fn line_endings(&mut self) -> &'static str {
        if let Some((revision, label)) = self.metrics.newlines
            && revision == self.revision
        {
            return label;
        }
        let mut kinds = 0;
        let mut bytes = self.text.bytes().peekable();
        while let Some(byte) = bytes.next() {
            match byte {
                b'\r' if bytes.peek() == Some(&b'\n') => {
                    bytes.next();
                    kinds |= 2;
                }
                b'\r' => kinds |= 4,
                b'\n' => kinds |= 1,
                _ => (),
            }
        }
        let label = match kinds {
            0 => "无换行",
            1 => "LF",
            2 => "CRLF",
            4 => "CR",
            _ => "混合换行",
        };
        self.metrics.newlines = Some((self.revision, label));
        label
    }
    pub fn selection(&self) -> Range<usize> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }
    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map(|s| self.cursor + s.len())
            .unwrap_or(self.text.len())
    }
    fn previous_word(&self) -> usize {
        self.text[..self.cursor]
            .split_word_bound_indices()
            .rev()
            .find(|(_, s)| s.chars().any(char::is_alphanumeric))
            .map_or(0, |(i, _)| i)
    }
    fn next_word(&self) -> usize {
        self.text[self.cursor..]
            .split_word_bound_indices()
            .find(|(_, s)| s.chars().any(char::is_alphanumeric))
            .map_or(self.text.len(), |(i, s)| self.cursor + i + s.len())
    }
    fn erase(&mut self, backwards: bool, by_word: bool) {
        if self.selection().is_empty() {
            self.anchor = match (backwards, by_word) {
                (true, true) => self.previous_word(),
                (false, true) => self.next_word(),
                (true, false) => self.previous(),
                (false, false) => self.next(),
            };
        }
        self.insert("");
    }
    fn before_crlf(&self, index: usize) -> usize {
        if index > 0
            && self.text.as_bytes().get(index) == Some(&b'\n')
            && self.text.as_bytes()[index - 1] == b'\r'
        {
            index - 1
        } else {
            index
        }
    }
    fn line_start(&self) -> usize {
        let cursor = self.before_crlf(self.cursor);
        let prefix = &self.text[..cursor];
        prefix.rfind(['\r', '\n']).map_or(0, |i| i + 1)
    }
    fn line_end(&self) -> usize {
        let cursor = self.before_crlf(self.cursor);
        self.text[cursor..]
            .find(['\r', '\n'])
            .map_or(self.text.len(), |i| cursor + i)
    }
    pub fn insert(&mut self, text: &str) {
        let range = self.selection();
        if text.is_empty() && range.is_empty() {
            return;
        }
        // Bound undo memory even when editing the largest accepted file.
        let cost: usize = self
            .undo
            .iter()
            .map(|e| e.removed.len() + e.inserted.len())
            .sum();
        if cost > 16 * 1024 * 1024 || self.undo.len() >= 2048 {
            self.undo.drain(..self.undo.len() / 2);
        }
        self.undo.push(Edit {
            start: range.start,
            removed: self.text[range.clone()].into(),
            inserted: text.into(),
        });
        self.redo.clear();
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        self.anchor = self.cursor;
        self.revision += 1;
        self.layout = None;
        self.reveal = true;
    }
    pub fn select(&mut self, range: Range<usize>) {
        self.anchor = range.start;
        self.cursor = range.end;
        self.reveal = true;
    }
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
        self.reveal = true;
    }
    fn undo(&mut self, redo: bool) {
        let edit = if redo {
            self.redo.pop()
        } else {
            self.undo.pop()
        };
        let Some(edit) = edit else {
            return;
        };
        let (old, new) = if redo {
            (&edit.removed, &edit.inserted)
        } else {
            (&edit.inserted, &edit.removed)
        };
        self.text
            .replace_range(edit.start..edit.start + old.len(), new);
        self.cursor = edit.start + new.len();
        self.anchor = self.cursor;
        self.revision += 1;
        self.layout = None;
        self.reveal = true;
        if redo {
            self.undo.push(edit);
        } else {
            self.redo.push(edit);
        }
    }
    fn indent_lines(&mut self, backwards: bool) {
        let selection = self.selection();
        if selection.is_empty() && !backwards {
            self.insert("\t");
            return;
        }
        let start = self.text[..selection.start]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let end = if !selection.is_empty() && self.text[..selection.end].ends_with('\n') {
            selection.end
        } else {
            self.text[selection.end..]
                .find('\n')
                .map_or(self.text.len(), |i| selection.end + i + 1)
        };
        let original = &self.text[start..end];
        let mut replacement = String::with_capacity(original.len());
        let mut removed_first = 0;
        for (index, line) in original.split_inclusive('\n').enumerate() {
            if backwards {
                let remove = if line.starts_with('\t') {
                    1
                } else {
                    line.bytes()
                        .take(4)
                        .take_while(|byte| *byte == b' ')
                        .count()
                };
                if index == 0 {
                    removed_first = remove;
                }
                replacement.push_str(&line[remove..]);
            } else {
                replacement.push('\t');
                replacement.push_str(line);
            }
        }
        if replacement == original {
            return;
        }
        let reverse = self.cursor < self.anchor;
        let old_cursor = self.cursor;
        let length = replacement.len();
        self.select(start..end);
        self.insert(&replacement); // One transaction, including all selected lines.
        if selection.is_empty() {
            self.cursor = old_cursor.saturating_sub(removed_first).max(start);
            self.anchor = self.cursor;
        } else if reverse {
            self.anchor = start + length;
            self.cursor = start;
        } else {
            self.anchor = start;
            self.cursor = start + length;
        }
    }
    fn code_key(&mut self, key: &Key, mods: ModifiersState, multiline: bool) -> bool {
        if !multiline || !self.monospace || mods.control_key() || mods.alt_key() || mods.super_key()
        {
            return false;
        }
        match key {
            Key::Named(NamedKey::Tab) => self.indent_lines(mods.shift_key()),
            Key::Named(NamedKey::Enter) => {
                let insertion = self.selection().start;
                let start = self.text[..insertion].rfind('\n').map_or(0, |i| i + 1);
                let indentation: String = self.text[start..insertion]
                    .chars()
                    .take_while(|ch| matches!(ch, ' ' | '\t'))
                    .collect();
                self.insert(&format!("\n{indentation}"));
            }
            _ => return false,
        }
        true
    }
    pub fn key(
        &mut self,
        event: &KeyEvent,
        mods: ModifiersState,
        clipboard: &mut Option<arboard::Clipboard>,
        multiline: bool,
    ) -> Result<(), String> {
        let ctrl = mods.control_key();
        let shift = mods.shift_key();
        if ctrl && let Key::Character(key) = &event.logical_key {
            match key.to_lowercase().as_str() {
                "a" => self.select_all(),
                "c" | "x" => {
                    let range = self.selection();
                    if !range.is_empty() {
                        clipboard
                            .as_mut()
                            .ok_or("剪贴板不可用")?
                            .set_text(self.text[range].to_owned())
                            .map_err(|e| e.to_string())?;
                        if key.eq_ignore_ascii_case("x") {
                            self.insert("");
                        }
                    }
                }
                "v" => {
                    let value = clipboard
                        .as_mut()
                        .ok_or("剪贴板不可用")?
                        .get_text()
                        .map_err(|e| e.to_string())?;
                    self.insert(&if multiline {
                        value.replace("\r\n", "\n")
                    } else {
                        value.replace(['\r', '\n'], "")
                    });
                }
                "z" => self.undo(shift),
                "y" => self.undo(true),
                _ => (),
            }
            return Ok(());
        }
        if self.code_key(&event.logical_key, mods, multiline) {
            return Ok(());
        }
        let destination = match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => Some(if !shift && !self.selection().is_empty() {
                self.selection().start
            } else if ctrl {
                self.previous_word()
            } else {
                self.previous()
            }),
            Key::Named(NamedKey::ArrowRight) => Some(if !shift && !self.selection().is_empty() {
                self.selection().end
            } else if ctrl {
                self.next_word()
            } else {
                self.next()
            }),
            Key::Named(NamedKey::Home) => Some(if ctrl { 0 } else { self.line_start() }),
            Key::Named(NamedKey::End) => Some(if ctrl {
                self.text.len()
            } else {
                self.line_end()
            }),
            Key::Named(
                NamedKey::ArrowUp | NamedKey::ArrowDown | NamedKey::PageUp | NamedKey::PageDown,
            ) if multiline => self.layout.as_ref().map(|layout| {
                let pos = layout.index_to_pos(self.cursor as i32);
                let down = matches!(
                    event.logical_key,
                    Key::Named(NamedKey::ArrowDown | NamedKey::PageDown)
                );
                let page = matches!(
                    event.logical_key,
                    Key::Named(NamedKey::PageUp | NamedKey::PageDown)
                );
                let amount = if page {
                    self.area.h as i32 * pango::SCALE
                } else {
                    pos.height()
                };
                self.index_at(
                    pos.x(),
                    pos.y() + pos.height() / 2 + if down { amount } else { -amount },
                )
            }),
            Key::Named(NamedKey::Backspace) => {
                self.erase(true, ctrl);
                None
            }
            Key::Named(NamedKey::Delete) => {
                self.erase(false, ctrl);
                None
            }
            Key::Named(NamedKey::Enter) if multiline => {
                self.insert("\n");
                None
            }
            Key::Named(NamedKey::Tab) if multiline => {
                self.insert("\t");
                None
            }
            _ => {
                if !ctrl
                    && !mods.alt_key()
                    && let Some(text) = &event.text
                {
                    let text: String = text.chars().filter(|c| !c.is_control()).collect();
                    self.insert(&text);
                }
                None
            }
        };
        if let Some(destination) = destination {
            self.cursor = destination;
            if !shift {
                self.anchor = destination;
            }
            self.reveal = true;
        }
        Ok(())
    }
    fn index_at(&self, x: i32, y: i32) -> usize {
        let Some(layout) = &self.layout else {
            return 0;
        };
        if y < 0 {
            return 0;
        }
        if y >= layout.size().1 {
            return self.text.len();
        }
        let (_, index, trailing) = layout.xy_to_index(x, y);
        let index = (index.max(0) as usize).min(self.text.len());
        let count = self.text[index..]
            .chars()
            .take(trailing.max(0) as usize)
            .map(char::len_utf8)
            .sum::<usize>();
        self.before_crlf(index + count)
    }
    pub fn click(&mut self, x: f64, y: f64, extend: bool) {
        self.cursor = self.index_at(
            ((x - self.area.x + self.horizontal) * pango::SCALE as f64) as i32,
            ((y - self.area.y + self.scroll) * pango::SCALE as f64) as i32,
        );
        if !extend {
            self.anchor = self.cursor;
        }
        self.reveal = true;
    }
    pub fn draw(
        &mut self,
        cr: &cairo::Context,
        area: Rect,
        focused: bool,
        caret: bool,
        multiline: bool,
    ) {
        let layout = ui::layout(
            cr,
            &self.text,
            if multiline { 15. } else { 13. },
            area.w,
            multiline,
        );
        if multiline && self.monospace {
            let mut font = layout.font_description().unwrap_or_default();
            font.set_family("monospace");
            layout.set_font_description(Some(&font));
        }
        if !multiline {
            layout.set_width(-1);
            layout.set_ellipsize(pango::EllipsizeMode::None);
        }
        let pos = layout.index_to_pos(self.cursor as i32);
        let cx = pos.x() as f64 / pango::SCALE as f64;
        let cy = pos.y() as f64 / pango::SCALE as f64;
        let ch = pos.height() as f64 / pango::SCALE as f64;
        if self.reveal {
            if cx < self.horizontal {
                self.horizontal = cx;
            }
            if !multiline && cx + 2. > self.horizontal + area.w {
                self.horizontal = cx + 2. - area.w;
            }
            if cy < self.scroll {
                self.scroll = cy;
            }
            if cy + ch > self.scroll + area.h {
                self.scroll = cy + ch - area.h;
            }
            self.reveal = false;
        }
        self.scroll = self
            .scroll
            .clamp(0., (layout.pixel_size().1 as f64 - area.h).max(0.));
        let _ = cr.save();
        cr.rectangle(area.x, area.y, area.w, area.h);
        cr.clip();
        cr.translate(area.x - self.horizontal, area.y - self.scroll);
        let range = self.selection();
        if (focused || self.highlight_selection) && !range.is_empty() {
            ui::color(cr, ui::select());
            for line in layout.lines_readonly() {
                let start = (line.start_index() as usize).max(range.start);
                let end = ((line.start_index() + line.length()) as usize).min(range.end);
                if start >= end {
                    continue;
                }
                let rect = layout.index_to_pos(line.start_index());
                for pair in line.x_ranges(start as i32, end as i32).as_chunks::<2>().0 {
                    cr.rectangle(
                        pair[0] as f64 / pango::SCALE as f64,
                        rect.y() as f64 / pango::SCALE as f64,
                        (pair[1] - pair[0]) as f64 / pango::SCALE as f64,
                        rect.height() as f64 / pango::SCALE as f64,
                    );
                }
            }
            let _ = cr.fill();
        }
        ui::color(cr, ui::text_color());
        cr.move_to(0., 0.);
        pangocairo::functions::show_layout(cr, &layout);
        if focused && caret {
            ui::fill(cr, Rect::new(cx, cy, 1.5, ch.max(17.)), ui::accent());
        }
        if focused && !self.preedit.is_empty() {
            let preedit = ui::layout(cr, &self.preedit, 15., area.w - cx, false);
            if multiline && self.monospace {
                preedit.set_font_description(layout.font_description().as_ref());
            }
            ui::fill(
                cr,
                Rect::new(cx, cy, (preedit.pixel_size().0 as f64).max(8.), ch),
                ui::hover(),
            );
            cr.move_to(cx, cy);
            ui::color(cr, ui::accent());
            pangocairo::functions::show_layout(cr, &preedit);
            ui::fill(
                cr,
                Rect::new(cx, cy + ch - 1., preedit.pixel_size().0 as f64, 1.),
                ui::accent(),
            );
        }
        let _ = cr.restore();
        self.area = area;
        self.layout = Some(layout);
    }
    pub fn caret_position(&self) -> (f64, f64) {
        let pos = self
            .layout
            .as_ref()
            .map(|l| l.index_to_pos(self.cursor as i32));
        pos.map(|p| {
            (
                self.area.x + p.x() as f64 / pango::SCALE as f64 - self.horizontal,
                self.area.y + p.y() as f64 / pango::SCALE as f64 - self.scroll + 20.,
            )
        })
        .unwrap_or((self.area.x, self.area.y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn word_erase_uses_navigation_boundaries_and_is_one_undo_step() {
        let mut editor = Editor::new("cafe\u{301}  déjà!".into());
        let split = "cafe\u{301}  ".len();
        editor.select(split..split);
        editor.erase(true, true);
        assert_eq!(editor.text, "déjà!");
        editor.undo(false);
        assert_eq!(editor.text, "cafe\u{301}  déjà!");

        let start = "cafe\u{301}".len();
        editor.select(start..start);
        editor.erase(false, true);
        assert_eq!(editor.text, "cafe\u{301}!");
        editor.undo(false);
        assert_eq!(editor.text, "cafe\u{301}  déjà!");

        editor.select(start..split);
        editor.erase(true, true);
        assert_eq!(editor.text, "cafe\u{301}déjà!");
        editor.select(start..start);
        editor.erase(true, false);
        assert_eq!(editor.text, "cafdéjà!");
    }

    #[test]
    fn home_and_end_respect_mixed_line_breaks_without_splitting_crlf() {
        let mut editor = Editor::new("甲\rβ\r\n尾\nend".into());
        let cr = "甲".len();
        let crlf = "甲\rβ".len();
        let lf = "甲\rβ\r\n尾".len();
        for (cursor, start, end) in [
            (0, 0, cr),
            (cr, 0, cr),
            (cr + 1, cr + 1, crlf),
            (crlf + 1, cr + 1, crlf),
            (crlf + 2, crlf + 2, lf),
            (lf + 1, lf + 1, editor.text.len()),
            (editor.text.len(), lf + 1, editor.text.len()),
        ] {
            editor.select(cursor..cursor);
            assert_eq!((editor.line_start(), editor.line_end()), (start, end));
        }
    }

    #[test]
    fn logical_caret_clamps_without_splitting_graphemes_or_crlf() {
        let text = "header\r\n👩‍💻e\u{301}\r\n尾";
        let mut editor = Editor::new(text.into());
        editor.move_to_position(2, 2);
        assert_eq!(editor.cursor, "header\r\n👩‍💻".len());
        assert_eq!(editor.position(), (2, 2));
        editor.move_to_position(2, 99);
        assert_eq!(&editor.text[editor.cursor..], "\r\n尾");
        assert_eq!(editor.position(), (2, 3));
        editor.move_to_position(99, 99);
        assert_eq!(editor.selection(), text.len()..text.len());
        assert_eq!(editor.position(), (3, 2));
    }

    #[test]
    fn code_indent_selected_lines_preserves_unicode_crlf_and_single_undo() {
        let source = "中e\u{301}\r\n    二\nthird";
        let mut e = Editor::new(source.into());
        e.monospace = true;
        let end = source.find("third").unwrap();
        // Reverse selection ends precisely at the next line's start.
        e.anchor = end;
        e.cursor = 0;
        assert!(e.code_key(&Key::Named(NamedKey::Tab), ModifiersState::empty(), true));
        assert_eq!(e.text, "\t中e\u{301}\r\n\t    二\nthird");
        assert_eq!(e.selection(), 0..end + 2);
        assert!(e.cursor < e.anchor);
        assert_eq!(e.undo.len(), 1);
        e.undo(false);
        assert_eq!(e.text, source);
        e.undo(true);
        assert_eq!(e.text, "\t中e\u{301}\r\n\t    二\nthird");
        e.select(0..end + 2);
        e.code_key(&Key::Named(NamedKey::Tab), ModifiersState::SHIFT, true);
        assert_eq!(e.text, source);
        e.undo(false);
        assert_eq!(e.text, "\t中e\u{301}\r\n\t    二\nthird");
    }

    #[test]
    fn code_outdent_clamps_caret_and_does_not_record_noops() {
        let mut e = Editor::new("  中\r\n\t二\nthird".into());
        e.monospace = true;
        e.cursor = 1;
        e.anchor = 1;
        e.code_key(&Key::Named(NamedKey::Tab), ModifiersState::SHIFT, true);
        assert_eq!(e.text, "中\r\n\t二\nthird");
        assert_eq!(e.cursor, 0);
        let revision = e.revision;
        e.code_key(&Key::Named(NamedKey::Tab), ModifiersState::SHIFT, true);
        assert_eq!(e.revision, revision);
        e.select_all();
        e.code_key(&Key::Named(NamedKey::Tab), ModifiersState::SHIFT, true);
        assert_eq!(e.text, "中\r\n二\nthird");
        e.undo(false);
        assert_eq!(e.text, "中\r\n\t二\nthird");
        let mut e = Editor::new("      x".into());
        e.monospace = true;
        e.cursor = 7;
        e.anchor = 7;
        e.code_key(&Key::Named(NamedKey::Tab), ModifiersState::SHIFT, true);
        assert_eq!(e.text, "  x");
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn auto_indent_is_code_only_and_copies_only_prefix_before_insertion() {
        let mut e = Editor::new("\t  中\r\nnext".into());
        let enter = Key::Named(NamedKey::Enter);
        assert!(!e.code_key(&enter, ModifiersState::empty(), true));
        e.monospace = true;
        assert!(!e.code_key(&enter, ModifiersState::empty(), false));
        assert!(!e.code_key(&enter, ModifiersState::CONTROL, true));
        e.cursor = "\t  中".len();
        e.anchor = e.cursor;
        e.code_key(&enter, ModifiersState::empty(), true);
        assert_eq!(e.text, "\t  中\n\t  \r\nnext");
        assert_eq!(e.undo.len(), 1);
        e.undo(false);
        assert_eq!(e.text, "\t  中\r\nnext");
        e.cursor = 1;
        e.anchor = 1;
        e.code_key(&enter, ModifiersState::empty(), true);
        assert_eq!(e.text, "\t\n\t  中\r\nnext");
        let mut e = Editor::new("    first\nsecond".into());
        e.monospace = true;
        e.select(4..e.text.len());
        e.code_key(&enter, ModifiersState::empty(), true);
        assert_eq!(e.text, "    \n    ");
        e.undo(false);
        assert_eq!(e.text, "    first\nsecond");
    }

    #[test]
    fn separately_typed_characters_are_separate_undo_edits() {
        let source = "needle first\nneedle second\n";
        let mut editor = Editor::new(source.into());
        editor.select(13..19);
        for character in ["M", "A", "T", "C", "H"] {
            editor.insert(character);
        }
        assert_eq!(editor.text, "needle first\nMATCH second\n");
        editor.undo(false);
        assert_eq!(editor.text, "needle first\nMATC second\n");
        for _ in 0..4 {
            editor.undo(false);
        }
        assert_eq!(editor.text, source);
    }

    #[test]
    fn logical_position_and_newlines_are_unicode_safe_and_revision_cached() {
        let mut e = Editor::new("中e\u{301}👩‍💻\t\r\nnext\nlast".into());
        e.cursor = "中e\u{301}👩‍💻\t".len();
        assert_eq!(e.position(), (1, 5));
        assert_eq!(e.line_endings(), "混合换行");
        e.cursor += 1; // Defensive CRLF interior maps before the pair.
        assert_eq!(e.position(), (1, 5));
        e.cursor += 1;
        assert_eq!(e.position(), (2, 1));
        e.cursor = e.text.len();
        assert_eq!(e.position(), (3, 5));
        e.select_all();
        e.insert("a\r\nb");
        assert_eq!(e.position(), (2, 2));
        assert_eq!(e.line_endings(), "CRLF");
        e.select_all();
        e.insert("a\rb");
        assert_eq!(e.position(), (2, 2));
        assert_eq!(e.line_endings(), "CR");
        e.select_all();
        e.insert("");
        assert_eq!(e.position(), (1, 1));
        assert_eq!(e.line_endings(), "无换行");
    }

    #[test]
    fn monospace_view_preserves_selection_and_logical_lines_across_wraps() {
        let mut e = Editor::new("WiWiWiWiWiWiWiWiWiWiWiWiWiWiWiWi\r\n中e\u{301}".into());
        e.select(0..2);
        let source = e.text.clone();
        let selection = e.selection();
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 120, 400).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        e.toggle_monospace();
        e.draw(&cr, Rect::new(0., 0., 100., 400.), true, true, true);
        let layout = e.layout.as_ref().unwrap();
        assert_eq!(
            layout.font_description().unwrap().family().as_deref(),
            Some("monospace")
        );
        assert_eq!(
            layout.index_to_pos(0).width(),
            layout.index_to_pos(1).width()
        );
        assert!(layout.line_count() > 2, "soft wrapping remains active");
        assert_eq!(e.position(), (1, 3));
        for y in 0..200 {
            for x in [0, 10, 50, 99] {
                let index = e.index_at(x * pango::SCALE, y * pango::SCALE);
                assert!(source.is_char_boundary(index));
                assert_ne!(
                    index,
                    source.find("\r\n").unwrap() + 1,
                    "hit testing must not split CRLF"
                );
            }
        }
        e.toggle_monospace();
        e.draw(&cr, Rect::new(0., 0., 100., 400.), true, true, true);
        assert_eq!(e.text, source);
        assert_eq!(e.selection(), selection);
        assert_eq!(e.revision, 0);
        assert!(e.undo.is_empty());
    }

    #[test]
    fn mixed_newlines_are_single_editing_boundaries_and_render_as_lines() {
        let mut e = Editor::new("a\r\nb\nc".into());
        assert_eq!(e.line_end(), 1);
        e.cursor = e.line_end();
        e.anchor = e.cursor;
        e.insert("X");
        assert_eq!(e.text, "aX\r\nb\nc");
        // Enter at end of line inserts LF without splitting the existing CRLF.
        e.insert("\n");
        assert_eq!(e.text, "aX\n\r\nb\nc");
        e.undo(false);
        e.anchor = e.next();
        e.insert(""); // Delete consumes both CR and LF.
        assert_eq!(e.text, "aXb\nc");
        e.undo(false);
        e.cursor = 4; // beginning of b, after CRLF
        e.anchor = e.previous();
        e.insert(""); // Backspace also consumes the whole separator.
        assert_eq!(e.text, "aXb\nc");
        e.undo(false);
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 300, 200).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        e.draw(&cr, Rect::new(0., 0., 300., 200.), true, true, true);
        let layout = e.layout.as_ref().unwrap();
        assert_eq!(layout.line_count(), 3);
        for y in 0..200 {
            for x in [0, 10, 30, 200, 299] {
                let index = e.index_at(x * pango::SCALE, y * pango::SCALE);
                assert_ne!(index, 3, "cursor must not split CRLF");
            }
        }
    }

    #[test]
    fn unicode_selection_and_undo_are_document_local() {
        let mut e = Editor::new("备忘 e\u{301}👩‍💻".into());
        e.cursor = e.text.len();
        e.anchor = e.previous();
        e.insert("");
        assert_eq!(e.text, "备忘 e\u{301}");
        e.anchor = e.previous();
        e.insert("文");
        assert_eq!(e.text, "备忘 文");
        e.undo(false);
        e.undo(false);
        assert_eq!(e.text, "备忘 e\u{301}👩‍💻");
        e.undo(true);
        assert_eq!(e.text, "备忘 e\u{301}");
        e.select_all();
        e.insert("新内容");
        e.undo(false);
        assert_eq!(e.text, "备忘 e\u{301}");
    }
}
