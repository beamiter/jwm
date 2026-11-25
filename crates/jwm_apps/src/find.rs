//! Literal, case-sensitive document search. Never records a document edit.
use crate::editor::Editor;
use std::ops::Range;

// Literal matches are nonoverlapping and ordered. An exact selection follows
// the same boundaries as any other selection: before its start or at/after
// its end. Carry the ordinal and byte offset together so we never walk back
// through the matches just to select the result.
fn navigation_match(
    mut hits: impl Iterator<Item = (usize, usize)>,
    selection: Range<usize>,
    backwards: bool,
) -> Option<(usize, usize)> {
    if backwards {
        let mut previous = None;
        let mut last = None;
        for hit in hits {
            if hit.1 < selection.start {
                previous = Some(hit);
            } else if previous.is_some() {
                return previous;
            }
            last = Some(hit);
        }
        previous.or(last)
    } else {
        let first = hits.next()?;
        if first.1 >= selection.end {
            return Some(first);
        }
        hits.find(|&(_, offset)| offset >= selection.end)
            .or(Some(first))
    }
}

#[derive(Default)]
pub(crate) struct Find {
    pub enabled: bool,
    pub query: Editor,
    pub total: usize,
    current: Option<(usize, usize)>,
    revisions: Option<(u64, u64)>,
}
impl Find {
    /// Seed a literal query without editing or moving the source selection.
    pub fn use_selection(&mut self, editor: &Editor) -> bool {
        let range = editor.selection();
        let Some(query) = editor.text.get(range.clone()) else {
            return false;
        };
        if query.is_empty() || query.len() > 4096 || query.contains(['\r', '\n']) {
            return false;
        }
        let mut total = 0;
        let mut current = None;
        for (index, (offset, _)) in editor.text.match_indices(query).enumerate() {
            if offset == range.start {
                current = Some(index);
            }
            total = index + 1;
        }
        // Do not move to a different result if existing non-overlap semantics
        // would skip the chosen occurrence.
        let Some(current) = current else {
            return false;
        };
        self.query = Editor::new(query.to_owned());
        self.query.select_all();
        self.total = total;
        self.current = Some((current, range.start));
        self.revisions = Some((editor.revision, self.query.revision));
        self.enabled = true;
        true
    }

    pub fn sync(&mut self, editor: &mut Editor) {
        self.refresh(editor, true);
    }

    pub fn document_edited(&mut self, editor: &mut Editor) {
        self.refresh(editor, false);
    }

    fn refresh(&mut self, editor: &mut Editor, select_match: bool) {
        let revisions = (editor.revision, self.query.revision);
        if self.revisions == Some(revisions) {
            return;
        }
        self.revisions = Some(revisions);
        self.total = 0;
        self.current = None;
        if self.query.text.is_empty() {
            return;
        }
        let selection = editor.selection();
        let query_len = self.query.text.len();
        let mut first = None;
        for (index, (offset, _)) in editor.text.match_indices(&self.query.text).enumerate() {
            let hit = (index, offset);
            first.get_or_insert(hit);
            let eligible = if select_match {
                offset >= selection.start
            } else {
                selection == (offset..offset + query_len)
            };
            if self.current.is_none() && eligible {
                self.current = Some(hit);
            }
            self.total += 1;
        }
        if select_match {
            self.current = self.current.or(first);
            if let Some((_, offset)) = self.current {
                editor.select(offset..offset + query_len);
            }
        }
    }

    pub fn navigate(&mut self, editor: &mut Editor, backwards: bool) {
        self.sync(editor);
        if self.total == 0 {
            return;
        }
        let hits = editor
            .text
            .match_indices(&self.query.text)
            .enumerate()
            .map(|(index, (offset, _))| (index, offset));
        if let Some((index, offset)) = navigation_match(hits, editor.selection(), backwards) {
            self.current = Some((index, offset));
            editor.select(offset..offset + self.query.text.len());
        }
    }
    fn current_index(&self, editor: &Editor) -> Option<usize> {
        let (index, offset) = self.current?;
        (self.revisions == Some((editor.revision, self.query.revision))
            && editor.selection() == (offset..offset + self.query.text.len()))
            .then_some(index)
    }

    pub fn has_current_match(&self, editor: &Editor) -> bool {
        self.current_index(editor).is_some()
    }

    pub fn label(&self, editor: &Editor) -> String {
        if self.query.text.is_empty() {
            "区分大小写 · 文本查找".into()
        } else if self.total == 0 {
            "无匹配".into()
        } else if let Some(index) = self.current_index(editor) {
            format!("{} / {}", index + 1, self.total)
        } else {
            format!("{} 处匹配", self.total)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn find_navigation_stops_after_the_first_candidate_and_wraps_in_one_pass() {
        let samples = [4, 10, 20, 54];
        let visited = std::cell::Cell::new(0);
        let run = |selection, backwards| {
            visited.set(0);
            let hits = samples.into_iter().enumerate().inspect(|_| {
                visited.set(visited.get() + 1);
            });
            (navigation_match(hits, selection, backwards), visited.get())
        };
        assert_eq!(run(10..14, false), (Some((2, 20)), 3));
        assert_eq!(run(10..14, true), (Some((0, 4)), 2));
        assert_eq!(run(13..17, false), (Some((2, 20)), 3));
        assert_eq!(run(13..17, true), (Some((1, 10)), 3));
        assert_eq!(run(0..0, true), (Some((3, 54)), 4));
        assert_eq!(run(54..58, false), (Some((0, 4)), 4));
    }

    #[test]
    fn find_selection_keeps_the_selected_unicode_occurrence() {
        let term = "编译🙂Error";
        let text = format!("{term} 编译🙂error {term}");
        let mut editor = Editor::new(text);
        let second = editor.text.rfind(term).unwrap();
        editor.anchor = second + term.len();
        editor.cursor = second;
        let mut find = Find::default();
        find.query.insert("previous");
        find.sync(&mut editor);
        assert!(find.use_selection(&editor));
        assert_eq!((find.current_index(&editor), find.total), (Some(1), 2));
        assert_eq!(find.query.text, term);
        assert_eq!(editor.cursor, second);
        assert_eq!(editor.anchor, second + term.len());
        find.navigate(&mut editor, false);
        assert_eq!(editor.selection(), 0..term.len());
        assert_eq!(editor.revision, 0);
    }

    #[test]
    fn find_selection_ignores_empty_multiline_and_overlapping_ranges() {
        let mut editor = Editor::new("ababa\r\nend".into());
        let mut find = Find::default();
        find.query.insert("end");
        for range in [0..0, 4..8, 2..5] {
            editor.select(range.clone());
            assert!(!find.use_selection(&editor));
            assert_eq!(editor.selection(), range);
            assert_eq!(find.query.text, "end");
            assert!(!find.enabled);
        }
    }

    #[test]
    fn find_only_marks_the_actual_selection_as_current() {
        let mut editor = Editor::new("前猫 后猫".into());
        let mut find = Find::default();
        find.query.insert("猫");
        find.sync(&mut editor);
        assert_eq!(find.label(&editor), "1 / 2");
        assert!(find.has_current_match(&editor));

        editor.select(0..3);
        assert_eq!(find.label(&editor), "2 处匹配");
        assert!(!find.has_current_match(&editor));

        find.navigate(&mut editor, true);
        assert_eq!(find.label(&editor), "2 / 2");
        assert_eq!(&editor.text[editor.selection()], "猫");
        assert_eq!(editor.revision, 0);
    }

    #[test]
    fn unicode_wrap_empty_missing_and_edits() {
        let mut editor = Editor::new("前🙂猫 后🙂猫".into());
        let mut find = Find::default();
        find.query.insert("🙂猫");
        find.sync(&mut editor);
        assert_eq!(find.total, 2);
        assert_eq!(&editor.text[editor.selection()], "🙂猫");
        assert_eq!(editor.selection().start, 3);
        find.navigate(&mut editor, true);
        assert_eq!(find.current_index(&editor), Some(1));
        find.navigate(&mut editor, false);
        assert_eq!(find.current_index(&editor), Some(0));
        assert_eq!(editor.revision, 0);
        editor.insert("替换");
        let cursor = editor.cursor;
        find.document_edited(&mut editor);
        assert_eq!(editor.cursor, cursor);
        assert!(editor.selection().is_empty());
        assert_eq!(find.total, 1);
        assert_eq!(find.label(&editor), "1 处匹配");
        assert!(!find.has_current_match(&editor));
        find.navigate(&mut editor, false);
        assert_eq!(&editor.text[editor.selection()], "🙂猫");
        find.query.select_all();
        find.query.insert("missing");
        let selection = editor.selection();
        find.sync(&mut editor);
        find.navigate(&mut editor, false);
        assert_eq!(find.total, 0);
        assert_eq!(editor.selection(), selection);
        find.query.select_all();
        find.query.insert("");
        find.sync(&mut editor);
        assert_eq!(find.total, 0);
    }
}
