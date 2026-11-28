//! A process-local, one-use transfer of a regular file or directory (COPY only).
use crate::{Shared, copy_ui, trash_ui};
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{FileEntry, FileKind};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};

const OFFER: u64 = 1;
pub(super) const SOURCE: &str = "拖拽复制所选项目";
pub(super) const TARGET: &str = "拖到这里复制到当前目录";

pub(super) fn available() -> bool {
    gdk::Display::default().is_some_and(|display| {
        matches!(
            display.type_().name(),
            "GdkX11Display" | "GdkWaylandDisplay"
        )
    })
}

struct Pending {
    view: Weak<RefCell<crate::Browser>>,
    selection: glib::WeakRef<gtk::SingleSelection>,
    rows: Arc<Vec<FileEntry>>,
    index: u32,
    source: copy_ui::StashedCopy,
    kind: FileKind,
}

impl Pending {
    fn capture(state: &Shared, selection: &gtk::SingleSelection) -> Option<Self> {
        let browser = state.try_borrow().ok()?;
        if browser.pending.is_some() || browser.dialog_open || browser.mutation.is_some() {
            return None;
        }
        let index = selection.selected();
        let entry = browser.entries.get(index as usize)?;
        if !matches!(entry.kind, FileKind::Regular | FileKind::Directory) {
            return None;
        }
        Some(Self {
            view: Rc::downgrade(state),
            selection: selection.downgrade(),
            rows: browser.entries.clone(),
            index,
            kind: entry.kind,
            source: copy_ui::StashedCopy {
                parent: browser.parent.as_ref()?.clone(),
                path: browser.path().clone(),
                name: entry.name.clone(),
                identity: entry.identity?,
            },
        })
    }

    fn still_selected_in_other_window(&self, target: &Shared) -> bool {
        let (Some(view), Some(selection)) = (self.view.upgrade(), self.selection.upgrade()) else {
            return false;
        };
        if Rc::ptr_eq(&view, target) || selection.selected() != self.index {
            return false;
        }
        let Ok(browser) = view.try_borrow() else {
            return false;
        };
        browser.pending.is_none()
            && !browser.dialog_open
            && browser.mutation.is_none()
            && Arc::ptr_eq(&browser.entries, &self.rows)
            && browser.path() == &self.source.path
            && browser
                .parent
                .as_ref()
                .is_some_and(|parent| Arc::ptr_eq(parent, &self.source.parent))
    }
}

struct Offer {
    drag: glib::WeakRef<gdk::Drag>,
    pending: Pending,
}

thread_local! {
    static ACTIVE: RefCell<Option<Offer>> = const { RefCell::new(None) };
}

fn is_ours(offer: &Offer, drag: &gdk::Drag) -> bool {
    offer.drag.upgrade().as_ref() == Some(drag)
}

fn clear(drag: &gdk::Drag) {
    ACTIVE.with_borrow_mut(|active| {
        if active.as_ref().is_some_and(|offer| is_ours(offer, drag)) {
            let _ = active.take();
        }
    });
}

pub(super) fn install(
    source_handle: &gtk::Label,
    destination: &gtk::Label,
    state: &Shared,
    selection: &gtk::SingleSelection,
    status: &gtk::Label,
) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::COPY);
    let prepared = Rc::new(RefCell::new(None::<Pending>));
    source.connect_prepare({
        let (state, selection, prepared) = (state.clone(), selection.clone(), prepared.clone());
        move |_, _, _| {
            let pending = Pending::capture(&state, &selection);
            let content = pending
                .as_ref()
                .map(|_| gdk::ContentProvider::for_value(&OFFER.to_value()));
            prepared.replace(pending);
            content
        }
    });
    source.connect_drag_begin(move |_, drag| {
        if let Some(pending) = prepared.take() {
            ACTIVE.with_borrow_mut(|active| {
                active.replace(Offer {
                    drag: drag.downgrade(),
                    pending,
                });
            });
        }
    });
    source.connect_drag_cancel(|_, drag, _| {
        clear(drag);
        false
    });
    source.connect_drag_end(|_, drag, _| clear(drag));
    source_handle.add_controller(source);

    let target = gtk::DropTarget::new(u64::static_type(), gdk::DragAction::COPY);
    target.connect_accept({
        let state = state.clone();
        move |_, drop| {
            drop.actions() == gdk::DragAction::COPY
                && drop.drag().is_some_and(|drag| {
                    ACTIVE.with_borrow(|active| {
                        active.as_ref().is_some_and(|offer| {
                            is_ours(offer, &drag)
                                && offer.pending.still_selected_in_other_window(&state)
                        })
                    })
                })
        }
    });
    target.connect_drop({
        let (state, status) = (state.clone(), status.clone());
        move |target, marker, _, _| {
            // The 4.0 property is available without changing gtk-rs feature
            // requirements; get_current_drop() requires the v4_4 feature.
            let Some(drop) = target.property::<Option<gdk::Drop>>("drop") else {
                return false;
            };
            if !marker.get::<u64>().is_ok_and(|value| value == OFFER)
                || drop.actions() != gdk::DragAction::COPY
            {
                return false;
            }
            let Some(drag) = drop.drag() else {
                return false;
            };
            let offered = ACTIVE.with_borrow_mut(|active| {
                if active.as_ref().is_some_and(|offer| is_ours(offer, &drag)) {
                    active.take()
                } else {
                    None
                }
            });
            let Some(offer) = offered else {
                return false;
            };
            if !offer.pending.still_selected_in_other_window(&state) {
                return false;
            }
            let target = {
                let Ok(browser) = state.try_borrow() else {
                    return false;
                };
                if browser.pending.is_some() || browser.dialog_open || browser.mutation.is_some() {
                    return false;
                }
                browser
                    .parent
                    .as_ref()
                    .map(|parent| (parent.clone(), browser.path().clone()))
            };
            let Some((parent, path)) = target else {
                return false;
            };
            // true only acknowledges enqueue; the terminal receipt is separate.
            match offer.pending.kind {
                FileKind::Regular => copy_ui::dispatch_regular_copy(
                    &state,
                    &status,
                    parent,
                    path,
                    offer.pending.source,
                    false,
                ),
                FileKind::Directory => {
                    if parent.identity() == offer.pending.source.parent.identity() {
                        copy_ui::notice(
                            &status,
                            "请选择不同的目标目录。同名项目不会被覆盖。",
                        );
                        return false;
                    }
                    copy_ui::dispatch_tree_copy(&state, &status, parent, path, offer.pending.source)
                }
                _ => false,
            }
        }
    });
    destination.add_controller(target);

    let update = Rc::new({
        let (handle, state, selection) = (
            source_handle.downgrade(),
            Rc::downgrade(state),
            selection.downgrade(),
        );
        move || {
            if let (Some(handle), Some(state), Some(selection)) =
                (handle.upgrade(), state.upgrade(), selection.upgrade())
            {
                let text = Pending::capture(&state, &selection).map_or_else(
                    || SOURCE.to_owned(),
                    |pending| format!("{SOURCE}：{}", trash_ui::display_path(&pending.source.name)),
                );
                handle.set_tooltip_text(Some(&text));
            }
        }
    });
    update();
    selection.connect_selected_notify(move |_| {
        // GTK notifies inline while the model refresh holds the browser mutably.
        // Observe the committed source selection only after that refresh returns.
        let update = update.clone();
        glib::idle_add_local_once(move || update());
    });
}
