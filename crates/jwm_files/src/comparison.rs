//! An independent local comparison view with an explicit copy destination.
//! It does not share the main view's navigation history.
use crate::{
    copy_ui::{CopyBuffer, paste_into},
    navigation::{Destination, Navigation},
    row_model::{FileRowModel, RowPresentation},
    wake::Wake,
};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    DirectoryChange, DirectoryHandle, FileEntry, FileKind, ListOptions, LocalProvider, Location,
    ScanId, ScanWorker,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

pub(super) const FOCUS_CLASS: &str = "jwm-files-readonly-comparison";

struct State {
    scanner: ScanWorker,
    pending: Option<(ScanId, Destination)>,
    path: PathBuf,
    navigation: Navigation,
    entries: Rc<Vec<FileEntry>>,
    parent: Option<Arc<DirectoryHandle>>,
    watched: Option<ScanId>,
}

impl State {
    fn request(&mut self, destination: Destination) {
        let id = self.scanner.submit(
            Location::Local(destination.path().to_path_buf()),
            ListOptions::default(),
        );
        self.pending = Some((id, destination));
    }
}

fn browse_widgets(
    initial: PathBuf,
    source: &crate::Shared,
    buffer: &CopyBuffer,
    copy_status: &gtk::Label,
) -> Result<(gtk::Box, glib::JoinHandle<()>), std::io::Error> {
    let (wake, mut notifications) = Wake::channel();
    let scanner = ScanWorker::start_notifying(LocalProvider, move || wake.signal())?;
    let state = Rc::new(RefCell::new(State {
        scanner,
        pending: None,
        path: initial.clone(),
        navigation: Navigation::default(),
        entries: Rc::default(),
        parent: None,
        watched: None,
    }));
    let pane = gtk::Box::new(gtk::Orientation::Vertical, 6);
    pane.set_hexpand(true);
    pane.set_vexpand(true);
    pane.set_width_request(230);
    pane.add_css_class(FOCUS_CLASS);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let back = gtk::Button::with_label("后退");
    let forward = gtk::Button::with_label("前进");
    let up = gtk::Button::with_label("上一级");
    let reload = gtk::Button::with_label("刷新");
    for button in [&back, &forward, &up, &reload] {
        toolbar.append(button);
    }
    pane.append(&toolbar);
    let path = gtk::Label::new(None);
    path.set_xalign(0.0);
    path.set_selectable(true);
    path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    pane.append(&path);
    let status = gtk::Label::new(Some("对照目录"));
    status.set_xalign(0.0);
    status.set_wrap(true);
    pane.append(&status);
    let paste = gtk::Button::with_label("粘贴副本到此");
    paste.set_tooltip_text(Some(
        "先在主栏复制一个普通文件，再粘贴到此目录；不覆盖同名文件",
    ));
    paste.set_sensitive(false);
    pane.append(&paste);
    paste.connect_clicked({
        let (state, source, buffer, copy_status) = (
            state.clone(),
            source.clone(),
            buffer.clone(),
            copy_status.clone(),
        );
        move |_| {
            let committed = {
                let state = state.borrow();
                state
                    .parent
                    .clone()
                    .filter(|_| state.pending.is_none())
                    .map(|parent| (parent, state.path.clone()))
            };
            if let Some((parent, path)) = committed {
                paste_into(&source, &buffer, &copy_status, parent, path);
            }
        }
    });

    let model = FileRowModel::new();
    let selection = gtk::SingleSelection::new(Some(model.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_margin_start(8);
        label.set_margin_top(3);
        label.set_margin_bottom(3);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let (Some(row), Some(label)) = (
            item.item().and_downcast::<glib::BoxedAnyObject>(),
            item.child().and_downcast::<gtk::Label>(),
        ) {
            label.set_text(&row.borrow::<RowPresentation>().list_label());
        }
    });
    let listing = gtk::ListView::new(Some(selection.clone()), Some(factory));
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&listing)
        .build();
    pane.append(&scroll);
    let navigate = |destination: Destination,
                    state: &Rc<RefCell<State>>,
                    status: &gtk::Label,
                    paste: &gtk::Button| {
        state.borrow_mut().request(destination);
        paste.set_sensitive(false);
        status.set_text("正在读取对照目录…");
    };
    for (button, direction) in [(&back, 0), (&forward, 1), (&up, 2), (&reload, 3)] {
        let (state, status, paste) = (state.clone(), status.clone(), paste.clone());
        button.connect_clicked(move |_| {
            let destination = {
                let state = state.borrow();
                match direction {
                    0 => state.navigation.back(),
                    1 => state.navigation.forward(),
                    2 => state.path.parent().map(|p| Destination::Visit(p.into())),
                    _ => Some(Destination::Visit(state.path.clone())),
                }
            };
            if let Some(destination) = destination {
                navigate(destination, &state, &status, &paste);
            }
        });
    }
    listing.connect_activate({
        let (state, status, paste) = (state.clone(), status.clone(), paste.clone());
        move |_, row| {
            let destination = {
                let state = state.borrow();
                if state.pending.is_some() {
                    return;
                }
                state.entries.get(row as usize).and_then(|entry| {
                    let Location::Local(path) = &entry.location;
                    (entry.kind == FileKind::Directory).then(|| Destination::Visit(path.clone()))
                })
            };
            if let Some(destination) = destination {
                navigate(destination, &state, &status, &paste);
            }
        }
    });

    navigate(Destination::Visit(initial), &state, &status, &paste);
    let worker = glib::MainContext::default().spawn_local(async move {
        while notifications.recv().await.is_ok() {
            let update = {
                let mut state = state.borrow_mut();
                let watch = state.scanner.take_directory_change();
                let Some(reply) = state.scanner.take_reply() else {
                    if let Some(watch) = watch
                        && state.watched == Some(watch.id)
                        && (state.pending.is_none() || watch.change == DirectoryChange::Invalidated)
                    {
                        if watch.change == DirectoryChange::Invalidated {
                            state.parent = None;
                            state.watched = None;
                            paste.set_sensitive(false);
                            status.set_text("对照目录已变化，请刷新后再粘贴。");
                        } else if watch.change == DirectoryChange::Dirty {
                            let target = Destination::Visit(state.path.clone());
                            state.request(target);
                            paste.set_sensitive(false);
                        }
                    }
                    continue;
                };
                let Some((id, destination)) = state.pending.clone() else {
                    continue;
                };
                if id != reply.id {
                    continue;
                }
                state.pending = None;
                match reply.result {
                    Ok(snapshot) => {
                        let committed = destination.path().to_path_buf();
                        if !state.navigation.accept(destination) {
                            continue;
                        }
                        state.path = committed;
                        let change = watch
                            .filter(|watch| watch.id == id)
                            .map(|watch| watch.change);
                        let invalidated = change == Some(DirectoryChange::Invalidated);
                        state.watched = (!invalidated).then_some(id);
                        state.parent = if invalidated {
                            None
                        } else {
                            snapshot.local_parent
                        };
                        if change == Some(DirectoryChange::Dirty) {
                            let target = Destination::Visit(state.path.clone());
                            state.request(target);
                        }
                        paste.set_sensitive(state.parent.is_some() && state.pending.is_none());
                        state.entries = Rc::new(snapshot.entries);
                        let unreadable = snapshot.skipped_entries;
                        Ok((
                            state.path.clone(),
                            state.entries.clone(),
                            unreadable,
                            invalidated,
                        ))
                    }
                    Err(error) => {
                        state.parent = None;
                        paste.set_sensitive(false);
                        Err(error)
                    }
                }
            };
            match update {
                Ok((committed, entries, unreadable, invalidated)) => {
                    let text = committed.to_string_lossy().escape_debug().to_string();
                    path.set_text(&text);
                    path.set_tooltip_text(Some(&text));
                    selection.set_selected(gtk::INVALID_LIST_POSITION);
                    let count = entries.len();
                    model.replace(entries);
                    status.set_text(&if invalidated {
                        "对照目录已变化，请刷新后再粘贴。".to_owned()
                    } else if unreadable == 0 {
                        format!("对照目录 · {count} 项")
                    } else {
                        format!("对照目录 · {count} 项 · {unreadable} 项读取失败")
                    });
                }
                Err(error) => status.set_text(&format!("无法读取对照目录：{error}")),
            }
        }
    });
    Ok((pane, worker))
}

pub(super) fn has_focus(window: &impl IsA<gtk::Window>) -> bool {
    let mut focused = gtk::prelude::GtkWindowExt::focus(window);
    while let Some(widget) = focused {
        if widget.has_css_class(FOCUS_CLASS) {
            return true;
        }
        focused = widget.parent();
    }
    false
}

pub(super) fn install(
    window: &gtk::ApplicationWindow,
    split: &gtk::Paned,
    source: &crate::Shared,
    status: &gtk::Label,
    copy_buffer: &CopyBuffer,
    copy_status: &gtk::Label,
) {
    let action = gio::SimpleAction::new("comparison-pane", None);
    let worker = Rc::new(RefCell::new(None::<glib::JoinHandle<()>>));
    let (split, status) = (split.downgrade(), status.downgrade());
    let source = source.clone();
    let (copy_buffer, copy_status) = (copy_buffer.clone(), copy_status.clone());
    let state = worker.clone();
    action.connect_activate(move |_, _| {
        let Some(split) = split.upgrade() else { return };
        if let Some(worker) = state.take() {
            worker.abort();
            split.set_end_child(None::<&gtk::Widget>);
            return;
        }
        let path = source.borrow().path().clone();
        match browse_widgets(path, &source, &copy_buffer, &copy_status) {
            Ok((pane, worker)) => {
                let midpoint = (split.width() / 2).max(230);
                split.set_end_child(Some(&pane));
                split.set_position(midpoint);
                state.replace(Some(worker));
            }
            Err(error) => {
                if let Some(status) = status.upgrade() {
                    status.set_text(&format!("无法打开对照栏：{error}"));
                }
            }
        }
    });
    window.add_action(&action);
    window.connect_destroy(move |_| {
        if let Some(worker) = worker.take() {
            worker.abort();
        }
    });
}
