//! Literal, case-sensitive document search. Never records a document edit.
use crate::editor::Editor;

#[derive(Default)]
pub(crate) struct Find {
    pub enabled: bool,
    pub query: Editor,
    pub total: usize,
    pub current: usize,
    revisions: Option<(u64, u64)>,
}
impl Find {
    pub fn sync(&mut self, editor: &mut Editor) {
        let revisions = (editor.revision, self.query.revision);
        if self.revisions == Some(revisions) {
            return;
        }
        self.revisions = Some(revisions);
        self.total = 0;
        self.current = 0;
        if self.query.text.is_empty() {
            return;
        }
        let mut selected = None;
        let mut first = None;
        for (offset, _) in editor.text.match_indices(&self.query.text) {
            first.get_or_insert(offset);
            if selected.is_none() && offset >= editor.selection().start {
                selected = Some(offset);
                self.current = self.total;
            }
            self.total += 1;
        }
        if let Some(offset) = selected.or(first) {
            editor.select(offset..offset + self.query.text.len());
        }
    }
    pub fn document_edited(&mut self, editor: &mut Editor) {
        let (anchor, cursor) = (editor.anchor, editor.cursor);
        self.sync(editor);
        editor.anchor = anchor;
        editor.cursor = cursor;
    }
    pub fn navigate(&mut self, editor: &mut Editor, backwards: bool) {
        self.sync(editor);
        if self.total == 0 {
            return;
        }
        let selection = editor.selection();
        let exact = editor
            .text
            .match_indices(&self.query.text)
            .position(|(offset, _)| selection == (offset..offset + self.query.text.len()));
        self.current = if let Some(index) = exact {
            if backwards {
                (index + self.total - 1) % self.total
            } else {
                (index + 1) % self.total
            }
        } else if backwards {
            editor
                .text
                .match_indices(&self.query.text)
                .enumerate()
                .take_while(|(_, (offset, _))| *offset < selection.start)
                .last()
                .map(|(index, _)| index)
                .unwrap_or(self.total - 1)
        } else {
            editor
                .text
                .match_indices(&self.query.text)
                .position(|(offset, _)| offset >= selection.end)
                .unwrap_or(0)
        };
        if let Some((offset, _)) = editor
            .text
            .match_indices(&self.query.text)
            .nth(self.current)
        {
            editor.select(offset..offset + self.query.text.len());
        }
    }
    pub fn label(&self) -> String {
        if self.query.text.is_empty() {
            "区分大小写 · 文本查找".into()
        } else if self.total == 0 {
            "无匹配".into()
        } else {
            format!("{} / {}", self.current + 1, self.total)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(find.current, 1);
        find.navigate(&mut editor, false);
        assert_eq!(find.current, 0);
        assert_eq!(editor.revision, 0);
        editor.insert("替换");
        let cursor = editor.cursor;
        find.document_edited(&mut editor);
        assert_eq!(editor.cursor, cursor);
        assert!(editor.selection().is_empty());
        assert_eq!(find.total, 1);
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
