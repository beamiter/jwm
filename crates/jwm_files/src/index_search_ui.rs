//! Explicit, read-only filename lookup below the currently loaded directory.
use crate::index_worker::IndexWorker;
use crate::{Shared, trash_ui};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{DirectoryHandle, WalkSummary};
use jwm_file_index::IndexError;
use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::Arc;

const TITLE: &str = "JWM Files · 查找子目录文件名";

pub(super) fn install(window: &gtk::ApplicationWindow, state: &Shared, status: &gtk::Label) {
    let action = gio::SimpleAction::new("search-subdirectories", None);
    let window_ref = window.downgrade();
    let state_ref = Rc::downgrade(state);
    let status_ref = status.downgrade();
    let panel_ref = glib::WeakRef::<gtk::Window>::new();
    action.connect_activate(move |_, _| {
        let (Some(window), Some(state), Some(status)) =
            (window_ref.upgrade(), state_ref.upgrade(), status_ref.upgrade())
        else {
            return;
        };
        if let Some(panel) = panel_ref.upgrade()
            && panel.is_visible()
        {
            panel.present();
            return;
        }
        let selected = {
            let browser = state.borrow();
            if browser.pending.is_some() {
                None
            } else {
                browser.parent.as_ref().map(|parent| (browser.path().clone(), parent.clone()))
            }
        };
        let Some((root, held)) = selected else {
            status.set_text("请先打开一个已载入的本地目录。");
            return;
        };
        if let Some(panel) = show(&window, &state, &root, &held, &status) {
            panel_ref.set(Some(&panel));
        }
    });
    window.add_action(&action);
}

type BrowserScope = (Weak<RefCell<crate::Browser>>, std::path::PathBuf, Arc<DirectoryHandle>);

fn current(scope: &BrowserScope) -> bool {
    let Some(state) = scope.0.upgrade() else {
        return false;
    };
    let browser = state.borrow();
    browser.pending.is_none()
        && browser.path() == &scope.1
        && browser.parent.as_ref().is_some_and(|parent| parent.identity() == scope.2.identity())
}

fn ready_label(summary: WalkSummary) -> String {
    format!(
        "目录已读取，可输入文件名。已跳过 {} 个挂载点。",
        summary.excluded_mounts
    )
}

fn show(
    parent: &gtk::ApplicationWindow,
    state: &Shared,
    root: &Path,
    held: &Arc<DirectoryHandle>,
    status: &gtk::Label,
) -> Option<gtk::Window> {
    let (worker, ready) = match IndexWorker::start(root.to_path_buf(), held.identity()) {
        Ok(ready) => ready,
        Err(_) => {
            status.set_text("无法启动目录搜索。");
            return None;
        }
    };
    let panel = gtk::Window::builder()
        .application(&parent.application()?)
        .transient_for(parent)
        .destroy_with_parent(true)
        .modal(true)
        .title(TITLE)
        .default_width(620)
        .default_height(360)
        .build();
    let scope = Rc::new((Rc::downgrade(state), root.to_path_buf(), held.clone()));
    let worker = Rc::new(RefCell::new(Some(worker)));
    let loaded = Rc::new(Cell::new(false));
    let pending = Rc::new(Cell::new(false));
    let generation = Rc::new(Cell::new(0u64));
    let closed = Rc::new(Cell::new(false));

    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    column.set_margin_start(16);
    column.set_margin_end(16);
    column.set_margin_top(12);
    column.set_margin_bottom(12);
    let root_label = gtk::Label::new(Some(&trash_ui::display_path(root.as_os_str())));
    root_label.set_xalign(0.0);
    root_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    root_label.set_selectable(true);
    column.append(&root_label);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let entry = gtk::Entry::builder()
        .hexpand(true)
        .max_length(128)
        .placeholder_text("输入文件名的一部分")
        .build();
    entry.set_widget_name("jwm-files-index-query");
    let search = gtk::Button::with_label("查找");
    search.set_sensitive(false);
    row.append(&entry);
    row.append(&search);
    column.append(&row);
    let info = gtk::Label::new(Some("正在读取此目录和子目录，索引不会写入磁盘…"));
    info.set_widget_name("jwm-files-index-status");
    info.set_xalign(0.0);
    info.set_wrap(true);
    column.append(&info);
    let results = gtk::TextView::builder().editable(false).monospace(true).build();
    results.set_widget_name("jwm-files-index-results");
    let scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&results).build();
    column.append(&scroll);
    let close = gtk::Button::with_label("关闭");
    close.set_halign(gtk::Align::End);
    let panel_ref = panel.downgrade();
    close.connect_clicked(move |_| {
        if let Some(panel) = panel_ref.upgrade() {
            panel.close();
        }
    });
    column.append(&close);
    panel.set_child(Some(&column));

    {
        let (worker, closed) = (worker.clone(), closed.clone());
        panel.connect_close_request(move |_| {
            closed.set(true);
            worker.borrow_mut().take();
            glib::Propagation::Proceed
        });
    }
    {
        let (loaded, pending, generation) = (loaded.clone(), pending.clone(), generation.clone());
        let (search, info, results) = (search.downgrade(), info.downgrade(), results.downgrade());
        entry.connect_changed(move |entry| {
            generation.set(generation.get().wrapping_add(1));
            if let Some(results) = results.upgrade() {
                results.buffer().set_text("");
            }
            if loaded.get() {
                if let Some(info) = info.upgrade() {
                    info.set_text("按 Enter 或点击「查找」。");
                }
                if let Some(search) = search.upgrade() {
                    search.set_sensitive(!pending.get() && !entry.text().is_empty());
                }
            }
        });
    }
    {
        let search = search.downgrade();
        entry.connect_activate(move |_| {
            if let Some(search) = search.upgrade()
                && search.is_sensitive()
            {
                search.emit_clicked();
            }
        });
    }
    {
        let (entry, info, results) = (entry.downgrade(), info.downgrade(), results.downgrade());
        let (worker, loaded, pending, generation, scope, closed) = (
            worker.clone(), loaded.clone(), pending.clone(), generation.clone(), scope.clone(), closed.clone(),
        );
        search.connect_clicked(move |search| {
            if !loaded.get() || pending.get() {
                return;
            }
            let Some(entry_live) = entry.upgrade() else {
                return;
            };
            let Some(info_live) = info.upgrade() else {
                return;
            };
            if !current(&scope) {
                info_live.set_text("当前目录已变化。请关闭后重新打开搜索。");
                search.set_sensitive(false);
                return;
            }
            let literal = entry_live.text().to_string();
            if literal.is_empty() {
                return;
            }
            let answer = worker.borrow().as_ref().map(|worker| worker.query(literal));
            let Some(Ok(answer)) = answer else {
                info_live.set_text("搜索暂不可用。");
                return;
            };
            pending.set(true);
            search.set_sensitive(false);
            info_live.set_text("正在查找…");
            let epoch = generation.get();
            let (entry, info, results, search) =
                (entry.clone(), info.clone(), results.clone(), search.downgrade());
            let (pending, generation, scope, closed) =
                (pending.clone(), generation.clone(), scope.clone(), closed.clone());
            glib::spawn_future_local(async move {
                let response = answer.await;
                pending.set(false);
                if closed.get() {
                    return;
                }
                let (Some(entry), Some(info), Some(results), Some(search)) =
                    (entry.upgrade(), info.upgrade(), results.upgrade(), search.upgrade())
                else {
                    return;
                };
                if !current(&scope) {
                    results.buffer().set_text("");
                    info.set_text("当前目录已变化。请关闭后重新打开搜索。");
                    return;
                }
                search.set_sensitive(!entry.text().is_empty());
                if generation.get() != epoch {
                    return;
                }
                match response {
                    Ok(Ok(rows)) => {
                        let text = rows.iter().map(|raw| {
                            trash_ui::display_path(OsStr::from_bytes(raw))
                        }).collect::<Vec<_>>().join("\n");
                        results.buffer().set_text(&text);
                        info.set_text(match rows.len() {
                            0 => "没有匹配的文件名。",
                            50 => "显示前 50 个匹配项。可缩小搜索范围。",
                            _ => "匹配的相对路径（只读）：",
                        });
                    }
                    _ => info.set_text("查找未完成，请重试。"),
                }
            });
        });
    }
    {
        let (entry, info, search) = (entry.downgrade(), info.downgrade(), search.downgrade());
        glib::spawn_future_local(async move {
            let response = ready.await;
            if closed.get() {
                return;
            }
            let (Some(entry), Some(info), Some(search)) =
                (entry.upgrade(), info.upgrade(), search.upgrade())
            else {
                return;
            };
            if !current(&scope) {
                info.set_text("当前目录已变化。请关闭后重新打开搜索。");
                return;
            }
            match response {
                Ok(Ok(summary)) => {
                    loaded.set(true);
                    info.set_text(&ready_label(summary));
                    search.set_sensitive(!entry.text().is_empty());
                }
                Ok(Err(IndexError::Incomplete | IndexError::ByteBudget)) => {
                    info.set_text("无法完整读取搜索范围。请选择更小的目录，未显示部分结果。");
                }
                _ => info.set_text("无法读取此目录用于搜索。"),
            }
        });
    }
    panel.present();
    entry.grab_focus();
    Some(panel)
}
