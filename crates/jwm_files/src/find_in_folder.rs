//! Search the current directory snapshot without reordering its rows.
use crate::{Browser, Shared};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{Cancellation, FileEntry, NameQuery};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};

struct Matches {
    entries: Arc<Vec<FileEntry>>,
    rows: Vec<usize>,
    cursor: Option<usize>,
}

#[derive(Default)]
struct Requests {
    generation: u64,
    cancel: Option<Cancellation>,
    matches: Option<Matches>,
}

pub(super) struct Finder {
    browser: Weak<RefCell<Browser>>,
    requests: RefCell<Requests>,
    entry: gtk::SearchEntry,
    bar: gtk::Revealer,
    info: gtk::Label,
    next: gtk::Button,
    prev: gtk::Button,
    selection: gtk::SingleSelection,
    list: glib::WeakRef<gtk::Widget>,
    grid: glib::WeakRef<gtk::Widget>,
}

impl Finder {
    fn reset(&self) -> u64 {
        let mut r = self.requests.borrow_mut();
        if let Some(cancel) = r.cancel.take() {
            cancel.cancel();
        }
        r.matches = None;
        r.generation = r.generation.wrapping_add(1);
        self.next.set_sensitive(false);
        self.prev.set_sensitive(false);
        r.generation
    }

    fn close(&self) {
        self.reset();
        self.bar.set_reveal_child(false);
        let views = [self.list.upgrade(), self.grid.upgrade()];
        for view in views.into_iter().flatten() {
            if view.is_mapped() {
                view.grab_focus();
                break;
            }
        }
    }

    fn committed_entries(&self) -> Option<Arc<Vec<FileEntry>>> {
        let browser = self.browser.upgrade()?;
        let browser = browser.borrow();
        (browser.pending.is_none() && browser.parent.is_some() && !browser.dialog_open)
            .then(|| browser.entries.clone())
    }

    pub(super) fn snapshot_changed(self: &Rc<Self>) {
        if self.bar.reveals_child() {
            self.query();
        }
    }

    fn query(self: &Rc<Self>) {
        let generation = self.reset();
        if !self.bar.reveals_child() {
            return;
        }
        let Some(query) = NameQuery::parse(self.entry.text().as_str()) else {
            self.info.set_text("输入文件名中的文字。");
            return;
        };
        let Some(entries) = self.committed_entries() else {
            self.info.set_text("请等待目录加载完成，再按回车查找。");
            return;
        };
        self.info.set_text("正在查找…");
        let cancel = Cancellation::default();
        self.requests.borrow_mut().cancel = Some(cancel.clone());
        let (send, receive) = futures_channel::oneshot::channel();
        let source = entries.clone();
        let spawned = std::thread::Builder::new()
            .name("jwm-files-find".into())
            .spawn(move || {
                let _ = send.send(query.matching_rows(&source, &cancel));
            });
        if let Err(error) = spawned {
            self.requests.borrow_mut().cancel = None;
            self.info.set_text(&format!("查找未启动：{error}"));
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let received = receive.await;
            let Some(finder) = weak.upgrade() else {
                return;
            };
            let mut requests = finder.requests.borrow_mut();
            if requests.generation != generation {
                return;
            }
            requests.cancel = None;
            let current = finder.committed_entries();
            let same = current
                .as_ref()
                .is_some_and(|now| Arc::ptr_eq(now, &entries));
            if !same {
                finder.info.set_text("目录已更新。按回车重新查找。");
                return;
            }
            let Ok(Ok(rows)) = received else {
                finder.info.set_text("未收到查找结果。按回车重试。");
                return;
            };
            finder.info.set_text(&format!("{} 个匹配项", rows.len()));
            finder.next.set_sensitive(!rows.is_empty());
            finder.prev.set_sensitive(!rows.is_empty());
            requests.matches = Some(Matches {
                entries,
                rows,
                cursor: None,
            });
        });
    }

    fn step(self: &Rc<Self>, backward: bool) {
        let current = self.committed_entries();
        let mut request = self.requests.borrow_mut();
        let matched = current
            .as_ref()
            .zip(request.matches.as_mut())
            .filter(|(now, ready)| Arc::ptr_eq(now, &ready.entries));
        let Some((_, matches)) = matched else {
            if request.cancel.is_some() {
                return;
            }
            drop(request);
            self.query();
            return;
        };
        let count = matches.rows.len();
        if count == 0 {
            return;
        }
        let next = match (matches.cursor, backward) {
            (None, false) => 0,
            (None, true) => count - 1,
            (Some(i), false) => (i + 1) % count,
            (Some(i), true) => i.checked_sub(1).unwrap_or(count - 1),
        };
        let Ok(row) = u32::try_from(matches.rows[next]) else {
            return;
        };
        matches.cursor = Some(next);
        drop(request);
        self.selection.set_selected(row);
        self.info.set_text(&format!("{} / {count}", next + 1));
        let views = [self.list.upgrade(), self.grid.upgrade()];
        for view in views.into_iter().flatten() {
            if view.is_mapped() {
                let _ = view.activate_action("list.scroll-to-item", Some(&row.to_variant()));
                break;
            }
        }
    }
}

impl Drop for Finder {
    fn drop(&mut self) {
        if let Some(cancel) = self.requests.get_mut().cancel.take() {
            cancel.cancel();
        }
    }
}

pub(super) fn install(
    app: &gtk::Application,
    window: &gtk::ApplicationWindow,
    layout: &gtk::Box,
    state: &Shared,
    selection: &gtk::SingleSelection,
    list: &gtk::Widget,
    grid: &gtk::Widget,
) -> Rc<Finder> {
    let bar = gtk::Revealer::new();
    bar.set_transition_type(gtk::RevealerTransitionType::None);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::SearchEntry::new();
    entry.set_hexpand(true);
    entry.set_widget_name("jwm-files-folder-find-input");
    let next = gtk::Button::with_label("下一个");
    let prev = gtk::Button::with_label("上一个");
    let close = gtk::Button::with_label("关闭查找");
    let info = gtk::Label::new(Some("输入文件名中的文字。"));
    for child in [
        entry.upcast_ref::<gtk::Widget>(),
        prev.upcast_ref(),
        next.upcast_ref(),
        info.upcast_ref(),
        close.upcast_ref(),
    ] {
        controls.append(child);
    }
    bar.set_child(Some(&controls));
    layout.append(&bar);
    let finder = Rc::new(Finder {
        browser: Rc::downgrade(state),
        requests: RefCell::default(),
        entry,
        bar,
        info,
        next,
        prev,
        selection: selection.clone(),
        list: list.downgrade(),
        grid: grid.downgrade(),
    });
    finder.reset();
    finder.entry.connect_changed({
        let weak = Rc::downgrade(&finder);
        move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.reset();
                finder.info.set_text("输入文件名中的文字。");
            }
        }
    });
    finder.entry.connect_search_changed({
        let weak = Rc::downgrade(&finder);
        move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.query();
            }
        }
    });
    finder.entry.connect_activate({
        let weak = Rc::downgrade(&finder);
        move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.step(false);
            }
        }
    });
    finder.entry.connect_next_match({
        let weak = Rc::downgrade(&finder);
        move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.step(false);
            }
        }
    });
    finder.entry.connect_previous_match({
        let weak = Rc::downgrade(&finder);
        move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.step(true);
            }
        }
    });
    finder.entry.connect_stop_search({
        let weak = Rc::downgrade(&finder);
        move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.close();
            }
        }
    });
    close.connect_clicked({
        let weak = Rc::downgrade(&finder);
        move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.close();
            }
        }
    });
    for (button, backward) in [(&finder.prev, true), (&finder.next, false)] {
        let weak = Rc::downgrade(&finder);
        button.connect_clicked(move |_| {
            if let Some(finder) = weak.upgrade() {
                finder.step(backward);
            }
        });
    }
    let action = gio::SimpleAction::new("find-in-folder", None);
    action.connect_activate({
        let finder = finder.clone();
        move |_, _| {
            finder.bar.set_reveal_child(true);
            if !finder.entry.grab_focus() {
                let weak = Rc::downgrade(&finder);
                glib::idle_add_local_once(move || {
                    if let Some(finder) = weak.upgrade()
                        && finder.bar.reveals_child()
                    {
                        finder.entry.grab_focus();
                    }
                });
            }
            finder.query();
        }
    });
    window.add_action(&action);
    app.set_accels_for_action("win.find-in-folder", &["<Control>f"]);
    finder
}
