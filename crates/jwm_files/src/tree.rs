//! Lazy folders-only sidebar, independent of the main navigation worker.
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{Cancellation, FileEntry, FileIdentity, ListOptions, LocalProvider, Location};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    time::Duration,
};

const MAX_PENDING: usize = 8;
const ROWS_PER_POLL: usize = 512;
const ROWS_PER_BRANCH: usize = 256;
const MAX_DEPTH: u16 = 64;

#[derive(Clone)]
struct Folder {
    path: PathBuf,
    // Ancestors also protect against mount aliases exposing an ancestor again.
    lineage: Vec<FileIdentity>,
    depth: u16,
}

enum Row {
    Folder(Folder),
    Message(String),
}

type Snapshot = (FileIdentity, Vec<FileEntry>, usize);

struct Work {
    folder: Folder,
    options: ListOptions,
    cancel: Cancellation,
    answer: SyncSender<Result<Snapshot, String>>,
}

struct Pending {
    target: glib::WeakRef<gio::ListStore>,
    folder: Folder,
    cancel: Cancellation,
    answer: Receiver<Result<Snapshot, String>>,
    rows: Option<std::vec::IntoIter<FileEntry>>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn make_row(row: Row) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(row)
}

fn replace_message(target: &gio::ListStore, text: impl Into<String>) {
    target.splice(0, target.n_items(), &[make_row(Row::Message(text.into()))]);
}

fn poll(pending: &mut VecDeque<Pending>) {
    let mut budget = ROWS_PER_POLL;
    for _ in 0..pending.len() {
        let Some(mut work) = pending.pop_front() else {
            break;
        };
        let Some(target) = work.target.upgrade() else {
            continue;
        };
        if work.rows.is_none() {
            match work.answer.try_recv() {
                Ok(Ok((identity, rows, skipped))) => {
                    if work.folder.lineage.is_empty() {
                        work.folder.lineage.push(identity);
                    }
                    if skipped == 0 {
                        target.remove_all();
                    } else {
                        replace_message(&target, "部分目录无法读取");
                    }
                    work.rows = Some(rows.into_iter());
                }
                Ok(Err(message)) => {
                    replace_message(&target, message);
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    replace_message(&target, "目录树读取已停止");
                    continue;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rows) = &mut work.rows {
            // Share the budget with other expanded folders.
            let take = budget.min(ROWS_PER_BRANCH).min(rows.len());
            let chunk: Vec<_> = rows
                .by_ref()
                .take(take)
                .filter_map(|entry| {
                    let identity = entry.identity?;
                    if work.folder.lineage.contains(&identity) {
                        return None;
                    }
                    let mut lineage = work.folder.lineage.clone();
                    lineage.push(identity);
                    let Location::Local(path) = entry.location;
                    Some(make_row(Row::Folder(Folder {
                        path,
                        lineage,
                        depth: work.folder.depth + 1,
                    })))
                })
                .collect();
            budget -= take;
            if !chunk.is_empty() {
                target.splice(target.n_items(), 0, &chunk);
            }
            if rows.len() == 0 {
                continue;
            }
        }
        pending.push_back(work);
    }
}

fn schedule_poll(pending: &Rc<RefCell<VecDeque<Pending>>>, scheduled: &Rc<Cell<bool>>) {
    if scheduled.replace(true) {
        return;
    }
    glib::timeout_add_local(Duration::from_millis(16), {
        let pending = Rc::downgrade(pending);
        let scheduled = Rc::downgrade(scheduled);
        move || {
            let (Some(pending), Some(scheduled)) = (pending.upgrade(), scheduled.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            // Model changes can synchronously construct child models. Do not
            // hold a RefCell borrow across GTK calls made by poll().
            let mut batch = std::mem::take(&mut *pending.borrow_mut());
            poll(&mut batch);
            batch.append(&mut pending.borrow_mut());
            let more = !batch.is_empty();
            *pending.borrow_mut() = batch;
            if more {
                glib::ControlFlow::Continue
            } else {
                scheduled.set(false);
                glib::ControlFlow::Break
            }
        }
    });
}

pub(crate) struct TreeSidebar {
    pub(crate) view: gtk::ScrolledWindow,
    root: gio::ListStore,
}

impl TreeSidebar {
    pub(crate) fn new(
        show_hidden: Rc<Cell<bool>>,
        navigate: impl Fn(PathBuf) + 'static,
    ) -> std::io::Result<Self> {
        let (send, receive) = mpsc::sync_channel::<Work>(MAX_PENDING);
        std::thread::Builder::new()
            .name("jwm-directory-tree".into())
            .spawn(move || {
                while let Ok(work) = receive.recv() {
                    let location = Location::Local(work.folder.path);
                    let expected = work.folder.lineage.last().copied();
                    let result = LocalProvider
                        .list_tree_children(&location, expected, work.options, &work.cancel)
                        .map_err(|err| format!("无法展开：{err}"))
                        .and_then(|snapshot| {
                            let parent = snapshot
                                .local_parent
                                .ok_or_else(|| "无法确认目录身份".to_owned())?;
                            Ok((
                                parent.identity(),
                                snapshot.entries,
                                snapshot.skipped_entries,
                            ))
                        });
                    let _ = work.answer.try_send(result);
                }
            })?;
        let root = gio::ListStore::new::<glib::BoxedAnyObject>();
        let pending = Rc::new(RefCell::new(VecDeque::<Pending>::new()));
        let scheduled = Rc::new(Cell::new(false));
        let model = gtk::TreeListModel::new(root.clone(), false, false, move |item| {
            let item = item.downcast_ref::<glib::BoxedAnyObject>()?;
            let row = item.borrow::<Row>();
            let Row::Folder(folder) = &*row else {
                return None;
            };
            if folder.depth >= MAX_DEPTH {
                return None;
            }
            let target = gio::ListStore::new::<glib::BoxedAnyObject>();
            let mut requests = pending.borrow_mut();
            requests.retain(|work| work.target.upgrade().is_some());
            if requests.len() >= MAX_PENDING {
                replace_message(&target, "目录树仍在读取，请稍后收起并重新展开");
                return Some(target.upcast());
            }
            let (answer, rx) = mpsc::sync_channel(1);
            let cancel = Cancellation::default();
            let request = Work {
                folder: folder.clone(),
                options: ListOptions {
                    show_hidden: show_hidden.get(),
                },
                cancel: cancel.clone(),
                answer,
            };
            if send.try_send(request).is_ok() {
                replace_message(&target, "正在读取…");
                requests.push_back(Pending {
                    target: target.downgrade(),
                    folder: folder.clone(),
                    cancel,
                    answer: rx,
                    rows: None,
                });
                drop(requests);
                schedule_poll(&pending, &scheduled);
            } else {
                replace_message(&target, "无法读取目录树，请收起后重试");
            }
            Some(target.upcast())
        });
        let selection = gtk::SingleSelection::new(Some(model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, object| {
            if let Some(item) = object.downcast_ref::<gtk::ListItem>() {
                let expander = gtk::TreeExpander::new();
                let label = gtk::Label::new(None);
                label.set_xalign(0.0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                expander.set_child(Some(&label));
                item.set_child(Some(&expander));
            }
        });
        factory.connect_bind(|_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else {
                return;
            };
            let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
                return;
            };
            let Some(label) = expander.child().and_downcast::<gtk::Label>() else {
                return;
            };
            let Some(value) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            expander.set_list_row(Some(&row));
            match &*value.borrow::<Row>() {
                Row::Folder(folder) => {
                    let name = folder.path.file_name().unwrap_or(folder.path.as_os_str());
                    label.set_text(&name.to_string_lossy());
                    label.set_tooltip_text(Some(&folder.path.to_string_lossy()));
                }
                Row::Message(message) => {
                    label.set_text(message);
                    label.set_tooltip_text(Some(message));
                }
            }
        });
        factory.connect_unbind(|_, object| {
            if let Some(expander) = object
                .downcast_ref::<gtk::ListItem>()
                .and_then(|item| item.child().and_downcast::<gtk::TreeExpander>())
            {
                expander.set_list_row(None::<&gtk::TreeListRow>);
            }
        });
        let list = gtk::ListView::new(Some(selection), Some(factory));
        list.set_single_click_activate(false);
        list.connect_activate(move |_, index| {
            if let Some(value) = model
                .row(index)
                .and_then(|row| row.item())
                .and_downcast::<glib::BoxedAnyObject>()
                && let Row::Folder(folder) = &*value.borrow::<Row>()
            {
                navigate(folder.path.clone());
            }
        });
        let view = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        Ok(Self { view, root })
    }

    pub(crate) fn set_root(&self, path: PathBuf) {
        self.root.splice(
            0,
            self.root.n_items(),
            &[make_row(Row::Folder(Folder {
                path,
                lineage: Vec::new(),
                depth: 0,
            }))],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsStr,
        fs,
        os::unix::{ffi::OsStrExt, fs::symlink},
        time::Instant,
    };

    fn spin_until(mut condition: impl FnMut() -> bool) {
        let loop_ = glib::MainContext::default();
        let limit = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < limit,
                "timed out driving GTK directory tree"
            );
            while loop_.iteration(false) {}
            std::thread::sleep(Duration::from_millis(8));
        }
    }

    fn row_index(model: &gtk::TreeListModel, path: &std::path::Path) -> Option<u32> {
        (0..model.n_items()).find(|index| {
            model.row(*index).and_then(|row| row.item())
                .and_downcast::<glib::BoxedAnyObject>()
                .is_some_and(|item| matches!(&*item.borrow::<Row>(), Row::Folder(folder) if folder.path == path))
        })
    }

    #[test]
    #[ignore = "requires a private graphical display"]
    fn native_tree_reads_independent_branches_and_activates_exact_paths() {
        gtk::init().unwrap();
        let fixture = std::env::temp_dir().join(format!("jwm-file-tree-{}", std::process::id()));
        fs::create_dir(&fixture).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(fixture.clone());
        let a = fixture.join(OsStr::from_bytes(b"a\x80"));
        let b = fixture.join("branch b");
        let c = fixture.join(".hidden");
        fs::create_dir_all(a.join("child a")).unwrap();
        fs::create_dir_all(b.join("child b")).unwrap();
        fs::create_dir(&c).unwrap();
        symlink(&fixture, fixture.join("cycle")).unwrap();
        let navigated = Rc::new(RefCell::new(None));
        let hidden = Rc::new(Cell::new(false));
        let tree = TreeSidebar::new(hidden.clone(), {
            let navigated = navigated.clone();
            move |path| *navigated.borrow_mut() = Some(path)
        })
        .unwrap();
        tree.set_root(fixture.clone());
        let list = tree.view.child().and_downcast::<gtk::ListView>().unwrap();
        let model = list
            .model()
            .and_downcast::<gtk::SingleSelection>()
            .unwrap()
            .model()
            .and_downcast::<gtk::TreeListModel>()
            .unwrap();

        assert_eq!(model.n_items(), 1);
        model.row(0).unwrap().set_expanded(true);
        spin_until(|| row_index(&model, &a).is_some() && row_index(&model, &b).is_some());
        assert_eq!(model.n_items(), 3);
        assert!(row_index(&model, &a.join("child a")).is_none());
        for path in [&a, &b] {
            model
                .row(row_index(&model, path).unwrap())
                .unwrap()
                .set_expanded(true);
        }
        spin_until(|| {
            row_index(&model, &a.join("child a")).is_some()
                && row_index(&model, &b.join("child b")).is_some()
        });
        list.emit_by_name::<()>("activate", &[&row_index(&model, &a).unwrap()]);
        assert_eq!(navigated.borrow().as_ref(), Some(&a));

        let branch = model.row(row_index(&model, &b).unwrap()).unwrap();
        branch.set_expanded(false);
        fs::create_dir(b.join("new child")).unwrap();
        assert!(row_index(&model, &b.join("child b")).is_none());
        branch.set_expanded(true);
        spin_until(|| row_index(&model, &b.join("new child")).is_some());

        hidden.set(true);
        tree.set_root(fixture.clone());
        assert_eq!(model.n_items(), 1);
        model.row(0).unwrap().set_expanded(true);
        spin_until(|| row_index(&model, &c).is_some());
        assert_eq!(model.n_items(), 4);
        assert!(row_index(&model, &fixture.join("cycle")).is_none());
        println!(
            "GTK4 X11 tree: requested branches expanded independently and exact paths activated"
        );
    }
}
