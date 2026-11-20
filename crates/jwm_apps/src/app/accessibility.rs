//! A small semantic tree for Notes. Document values and destructive actions are
//! intentionally not exposed by this adapter.
use super::{Action, App, Document, Focus};
use accesskit::{Action as Request, ActionRequest, Node, NodeId, Role, Tree, TreeId, TreeUpdate};
use std::{collections::HashMap, path::PathBuf};

const ROOT: NodeId = NodeId(0);
const SEARCH: NodeId = NodeId(1);
const EDITOR: NodeId = NodeId(2);
const RESULTS: NodeId = NodeId(3);
const MODAL: NodeId = NodeId(4);

pub(crate) struct Semantics {
    rows: HashMap<PathBuf, NodeId>,
    next_id: u64,
    previous: Option<Vec<(NodeId, Node)>>,
}

impl Default for Semantics {
    fn default() -> Self {
        Self {
            rows: HashMap::new(),
            next_id: 5,
            previous: None,
        }
    }
}

fn named(role: Role, label: &str) -> Node {
    let mut node = Node::new(role);
    node.set_label(label);
    node
}

impl Semantics {
    pub(crate) fn reset(&mut self) {
        self.previous = None;
        self.rows.clear();
    }

    pub(crate) fn update(&mut self, app: &App, scale: f64) -> TreeUpdate {
        let mut root = named(Role::Window, "JWM Notes");
        let mut children = Vec::new();
        let mut nodes = Vec::new();
        let mut focus = ROOT;
        if app.dialog.is_some() {
            // Do not keep underlying nodes actionable in a modal scope.
            self.rows.clear();
            let mut modal = named(Role::Dialog, "备忘录对话框");
            modal.set_modal();
            focus = MODAL;
            children.push(MODAL);
            nodes.push((MODAL, modal));
        } else if let Document::Notes(notes) = &app.document {
            if notes.managed {
                let mut search = named(Role::TextInput, "搜索备忘录");
                search.add_action(Request::Focus);
                if let Some(hit) = app.hits.iter().find(|hit| hit.action == Action::Search) {
                    search.set_bounds(bounds(hit.rect, scale));
                }
                children.extend([SEARCH, RESULTS]);
                nodes.push((SEARCH, search));
                if app.focus == Focus::Search {
                    focus = SEARCH;
                }

                // Keep identity across reorder/rename of the visible label. A
                // removed/filtered-out path cannot reuse its previous node ID.
                let mut old_rows = std::mem::take(&mut self.rows);
                let mut result_ids = Vec::new();
                // The scene already materializes only rows inside the viewport.
                // Offscreen notes become exposed when the user scrolls them into view.
                for hit in &app.hits {
                    let Action::Note(path) = &hit.action else {
                        continue;
                    };
                    let id = old_rows.remove(path).unwrap_or_else(|| {
                        let id = NodeId(self.next_id);
                        self.next_id = self
                            .next_id
                            .checked_add(1)
                            .expect("accessibility node IDs exhausted");
                        id
                    });
                    self.rows.insert(path.clone(), id);
                    let mut result = named(Role::Button, &hit.label);
                    result.add_action(Request::Click);
                    result.set_bounds(bounds(hit.rect, scale));
                    result_ids.push(id);
                    nodes.push((id, result));
                }
                let mut results = named(Role::Group, "搜索结果");
                results.set_children(result_ids);
                nodes.push((RESULTS, results));
            }
            let mut editor = named(Role::MultilineTextInput, "备忘录正文");
            editor.set_read_only(); // No accessibility text/value/selection interface yet.
            editor.add_action(Request::Focus);
            if let Some(hit) = app.hits.iter().find(|hit| hit.action == Action::Edit) {
                editor.set_bounds(bounds(hit.rect, scale));
            }
            children.push(EDITOR);
            nodes.push((EDITOR, editor));
            if app.focus == Focus::Editor {
                focus = EDITOR;
            }
        }
        root.set_children(children);
        nodes.push((ROOT, root));
        let initial = self.previous.is_none();
        let changed = self.previous.as_ref().is_none_or(|old| old != &nodes);
        let update = TreeUpdate {
            nodes: if changed { nodes.clone() } else { Vec::new() },
            tree: initial.then(|| Tree::new(ROOT)),
            tree_id: TreeId::ROOT,
            focus,
        };
        self.previous = Some(nodes);
        update
    }

    pub(crate) fn action(&self, app: &mut App, request: ActionRequest) -> bool {
        if self.previous.is_none()
            || request.target_tree != TreeId::ROOT
            || app.dialog.is_some()
            || request.data.is_some()
        {
            return false;
        }
        let Document::Notes(notes) = &app.document else {
            return false;
        };
        let action = match (request.target_node, request.action) {
            (SEARCH, Request::Focus) if notes.managed => Action::Search,
            (EDITOR, Request::Focus) => Action::Edit,
            (id, Request::Click) if notes.managed => {
                let Some((path, _)) = notes
                    .rows
                    .iter()
                    .find(|(path, _)| self.rows.get(path) == Some(&id))
                else {
                    return false;
                };
                Action::Note(path.clone())
            }
            _ => return false,
        };
        let opens_note = matches!(action, Action::Note(_));
        app.dragging = false;
        app.action(action);
        if opens_note && app.dialog.is_none() {
            // Action::Note also permits clicking the already open document.
            app.action(Action::Edit);
        }
        true
    }
}

fn bounds(rect: super::Rect, scale: f64) -> accesskit::Rect {
    accesskit::Rect::new(
        rect.x * scale,
        rect.y * scale,
        (rect.x + rect.w) * scale,
        (rect.y + rect.h) * scale,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::prelude::FileExt;
    use std::fs;

    fn request(node: NodeId, action: Request) -> ActionRequest {
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: node,
            data: None,
        }
    }

    #[test]
    fn semantics_focus_actions_and_modal_staleness() {
        let dir = std::env::temp_dir().join(format!("jwm-a11y-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        fs::write(&first, "First title\nPRIVATE_FIRST_BODY").unwrap();
        fs::write(&second, "Second title\nPRIVATE_SECOND_BODY").unwrap();
        let mut app = App::new(true, vec![first.clone()]).unwrap();
        if let Document::Notes(notes) = &mut app.document {
            notes.managed = true;
            notes.directory = dir.clone();
            notes.refresh();
        }

        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 960, 680).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        app.draw(&cr, 960., 680., false);
        let mut semantics = Semantics::default();
        let tree = semantics.update(&app, 1.);
        assert_eq!(tree.focus, EDITOR);
        assert!(tree.tree.is_some());
        let second_id = semantics.rows[&second];
        let node = |id| {
            tree.nodes
                .iter()
                .find(|(candidate, _)| *candidate == id)
                .map(|(_, node)| node)
                .unwrap()
        };
        assert_eq!(node(SEARCH).label(), Some("搜索备忘录"));
        assert_eq!(node(second_id).label(), Some("Second title"));
        assert!(node(second_id).supports_action(Request::Click));
        assert_eq!(node(EDITOR).label(), Some("备忘录正文"));
        assert!(node(EDITOR).value().is_none());
        assert!(!node(EDITOR).supports_action(Request::SetValue));
        assert!(tree.nodes.iter().all(|(_, n)| n.value().is_none()
            && n.label().is_none_or(|label| !label.contains("PRIVATE_"))));
        assert!(semantics.update(&app, 1.).nodes.is_empty());

        assert!(semantics.action(&mut app, request(SEARCH, Request::Focus)));
        assert_eq!(semantics.update(&app, 1.).focus, SEARCH);
        if let Document::Notes(notes) = &mut app.document {
            notes.rows.reverse();
            notes.editor.insert("Pending ");
            notes.edited();
        }
        app.draw(&cr, 960., 680., false);
        semantics.update(&app, 1.);
        assert_eq!(semantics.rows[&second], second_id);
        assert!(semantics.action(&mut app, request(second_id, Request::Click)));
        let Document::Notes(notes) = &app.document else {
            panic!("expected note");
        };
        assert_eq!(
            notes.document.as_ref().unwrap().file.path(),
            Some(second.clone())
        );
        assert_eq!(
            fs::read_to_string(&first).unwrap(),
            "Pending First title\nPRIVATE_FIRST_BODY"
        );
        assert_eq!(semantics.update(&app, 1.).focus, EDITOR);

        // A save conflict must preserve the current unsaved document and not
        // perform any fall-through focus action.
        let first_id = semantics.rows[&first];
        assert!(semantics.action(&mut app, request(SEARCH, Request::Focus)));
        if let Document::Notes(notes) = &mut app.document {
            notes.editor.insert("Unsaved ");
            notes.edited();
        }
        fs::write(&second, "external bytes changed").unwrap();
        assert!(semantics.action(&mut app, request(first_id, Request::Click)));
        assert!(app.focus == Focus::Search);
        let Document::Notes(notes) = &app.document else {
            panic!("expected note");
        };
        assert!(notes.dirty);
        assert_eq!(
            notes.document.as_ref().unwrap().file.path(),
            Some(second.clone())
        );
        assert_eq!(
            fs::read_to_string(&second).unwrap(),
            "external bytes changed"
        );
        assert!(app.dialog.is_some());
        app.action(Action::Cancel);

        app.action(Action::Open);
        assert!(!semantics.action(&mut app, request(second_id, Request::Click)));
        let modal = semantics.update(&app, 1.);
        assert_eq!(modal.nodes.len(), 2);
        assert!(
            modal
                .nodes
                .iter()
                .any(|(id, node)| *id == MODAL && node.role() == Role::Dialog)
        );
        assert!(!semantics.action(&mut app, request(EDITOR, Request::Focus)));
        app.action(Action::Cancel);
        app.draw(&cr, 960., 680., false);
        semantics.update(&app, 1.);
        assert_ne!(semantics.rows[&second], second_id);
        assert!(!semantics.action(&mut app, request(second_id, Request::Click)));

        assert!(semantics.action(&mut app, request(SEARCH, Request::Focus)));
        app.ime("No results".into(), true);
        let old_first = semantics.rows[&first];
        assert!(!semantics.action(&mut app, request(old_first, Request::Click)));
        app.draw(&cr, 960., 680., false);
        semantics.update(&app, 1.);
        assert!(semantics.rows.is_empty());
        assert!(!semantics.action(&mut app, request(EDITOR, Request::SetValue)));
        fs::remove_dir_all(dir).unwrap();
    }
}
