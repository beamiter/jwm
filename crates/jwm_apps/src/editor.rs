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
pub(crate) struct Editor {
    pub text: String,
    pub cursor: usize,
    pub anchor: usize,
    pub revision: u64,
    pub scroll: f64,
    horizontal: f64,
    pub preedit: String,
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
        let destination = match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => Some(if !shift && !self.selection().is_empty() {
                self.selection().start
            } else if ctrl {
                self.text[..self.cursor]
                    .split_word_bound_indices()
                    .rev()
                    .find(|(_, s)| s.chars().any(char::is_alphanumeric))
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            } else {
                self.previous()
            }),
            Key::Named(NamedKey::ArrowRight) => Some(if !shift && !self.selection().is_empty() {
                self.selection().end
            } else if ctrl {
                self.text[self.cursor..]
                    .split_word_bound_indices()
                    .find(|(_, s)| s.chars().any(char::is_alphanumeric))
                    .map(|(i, s)| self.cursor + i + s.len())
                    .unwrap_or(self.text.len())
            } else {
                self.next()
            }),
            Key::Named(NamedKey::Home) => Some(if ctrl {
                0
            } else {
                self.text[..self.cursor]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0)
            }),
            Key::Named(NamedKey::End) => Some(if ctrl {
                self.text.len()
            } else {
                self.text[self.cursor..]
                    .find('\n')
                    .map(|i| self.cursor + i)
                    .unwrap_or(self.text.len())
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
                if self.selection().is_empty() {
                    self.anchor = self.previous();
                }
                self.insert("");
                None
            }
            Key::Named(NamedKey::Delete) => {
                if self.selection().is_empty() {
                    self.anchor = self.next();
                }
                self.insert("");
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
        index + count
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
        if focused && !range.is_empty() {
            ui::color(cr, ui::SELECT);
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
        ui::color(cr, ui::TEXT);
        cr.move_to(0., 0.);
        pangocairo::functions::show_layout(cr, &layout);
        if focused && caret {
            ui::fill(cr, Rect::new(cx, cy, 1.5, ch.max(17.)), ui::ACCENT);
        }
        if focused && !self.preedit.is_empty() {
            let preedit = ui::layout(cr, &self.preedit, 15., area.w - cx, false);
            ui::fill(
                cr,
                Rect::new(cx, cy, (preedit.pixel_size().0 as f64).max(8.), ch),
                ui::HOVER,
            );
            cr.move_to(cx, cy);
            ui::color(cr, ui::ACCENT);
            pangocairo::functions::show_layout(cr, &preedit);
            ui::fill(
                cr,
                Rect::new(cx, cy + ch - 1., preedit.pixel_size().0 as f64, 1.),
                ui::ACCENT,
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
