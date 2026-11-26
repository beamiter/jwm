//! A snapshot-backed GListModel that only constructs row objects on demand.
use gtk4::prelude::*;
use gtk4::{gio, glib};
use jwm_file_core::{FileEntry, FileKind};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

pub(crate) struct RowPresentation {
    pub(crate) name: String,
    pub(crate) kind: FileKind,
}

impl RowPresentation {
    pub(crate) fn list_label(&self) -> String {
        let prefix = match self.kind {
            FileKind::Directory => "▸ ",
            FileKind::Symlink => "↪ ",
            _ => "",
        };
        format!("{prefix}{}", self.name)
    }
}

#[derive(Default)]
struct Rows {
    entries: Rc<Vec<FileEntry>>,
    live: HashMap<u32, glib::WeakRef<glib::BoxedAnyObject>>,
    sweep_at: usize,
}

mod imp {
    use super::*;
    use gio::subclass::prelude::*;

    #[derive(Default)]
    pub struct FileRowModel {
        pub(super) state: RefCell<Rows>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileRowModel {
        const NAME: &'static str = "JwmFileRowModel";
        type Type = super::FileRowModel;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for FileRowModel {}

    impl ListModelImpl for FileRowModel {
        fn item_type(&self) -> glib::Type {
            glib::BoxedAnyObject::static_type()
        }

        fn n_items(&self) -> u32 {
            self.state
                .borrow()
                .entries
                .len()
                .try_into()
                .unwrap_or(u32::MAX)
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            let mut state = self.state.borrow_mut();
            if let Some(item) = state.live.get(&position).and_then(glib::WeakRef::upgrade) {
                return Some(item.upcast());
            }
            let entry = state.entries.get(position as usize)?;
            let item = glib::BoxedAnyObject::new(RowPresentation {
                name: entry.name.to_string_lossy().escape_debug().to_string(),
                kind: entry.kind,
            });
            // GListModel requires stable objects while anyone still owns
            // them. Dead weak slots are pruned; we never own unused rows.
            if state.live.len() >= state.sweep_at {
                state.live.retain(|_, weak| weak.upgrade().is_some());
                state.sweep_at = state.live.len().saturating_mul(2).max(512);
            }
            state.live.insert(position, item.downgrade());
            Some(item.upcast())
        }
    }
}

glib::wrapper! {
    pub struct FileRowModel(ObjectSubclass<imp::FileRowModel>) @implements gio::ListModel;
}

impl FileRowModel {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    pub(crate) fn replace(&self, entries: Rc<Vec<FileEntry>>) {
        use glib::subclass::prelude::ObjectSubclassIsExt;

        let old_count = self.n_items();
        {
            let mut state = self.imp().state.borrow_mut();
            state.entries = entries;
            state.live.clear();
            state.sweep_at = 512;
        }
        let new_count = self.n_items();
        if old_count != 0 || new_count != 0 {
            self.items_changed(0, old_count, new_count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::subclass::prelude::ObjectSubclassIsExt;
    use jwm_file_core::Location;
    use std::{cell::Cell, path::PathBuf};

    fn entry(name: String, kind: FileKind) -> FileEntry {
        FileEntry {
            location: Location::Local(PathBuf::from(&name)),
            name: name.into(),
            kind,
            identity: None,
            bytes: None,
            modified: None,
        }
    }

    #[test]
    fn snapshot_has_full_large_count_without_prebuilding_row_objects() {
        let model = FileRowModel::new();
        let entries = Rc::new(
            (0..100_000)
                .map(|i| entry(format!("entry-{i}"), FileKind::Regular))
                .collect(),
        );
        let changed = Rc::new(Cell::new(None));
        model.connect_items_changed({
            let changed = changed.clone();
            move |_, position, removed, added| changed.set(Some((position, removed, added)))
        });
        model.replace(entries);
        assert_eq!(changed.get(), Some((0, 0, 100_000)));
        assert_eq!(model.n_items(), 100_000);
        assert!(model.imp().state.borrow().live.is_empty());
        assert!(model.item(100_000).is_none());
        let last = model
            .item(99_999)
            .and_downcast::<glib::BoxedAnyObject>()
            .unwrap();
        assert_eq!(last.borrow::<RowPresentation>().name, "entry-99999");
        assert_eq!(
            Some(last.clone().upcast::<glib::Object>()),
            model.item(99_999)
        );
        for i in 0..2048 {
            assert!(model.item(i).is_some());
        }
        assert!(model.imp().state.borrow().live.len() <= 512);
        assert_eq!(Some(last.upcast::<glib::Object>()), model.item(99_999));
    }

    #[test]
    fn presentation_kind_is_not_inferred_from_filename_and_snapshot_drops_old_cache() {
        let model = FileRowModel::new();
        model.replace(Rc::new(vec![entry("▸ 文件".into(), FileKind::Regular)]));
        let old = model
            .item(0)
            .and_downcast::<glib::BoxedAnyObject>()
            .unwrap();
        assert_eq!(old.borrow::<RowPresentation>().kind, FileKind::Regular);
        assert_eq!(old.borrow::<RowPresentation>().list_label(), "▸ 文件");
        model.replace(Rc::new(vec![entry("文件".into(), FileKind::Directory)]));
        let next = model
            .item(0)
            .and_downcast::<glib::BoxedAnyObject>()
            .unwrap();
        assert_ne!(old, next);
        assert_eq!(next.borrow::<RowPresentation>().kind, FileKind::Directory);
    }
}
