//! A process-local, one-use transfer of a regular file or directory (COPY only).
use crate::{Shared, copy_ui, trash_ui};
use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{FileEntry, FileKind};
use std::{
    cell::{Cell, RefCell},
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
    selection: Option<glib::WeakRef<gtk::SingleSelection>>,
    rows: Arc<Vec<FileEntry>>,
    index: u32,
    source: copy_ui::StashedCopy,
    kind: FileKind,
}

impl Pending {
    fn capture_selection(state: &Shared, selection: &gtk::SingleSelection) -> Option<Self> {
        Self::capture(state, selection.selected(), Some(selection), None)
    }

    fn capture(
        state: &Shared,
        index: u32,
        selection: Option<&gtk::SingleSelection>,
        bound: Option<(&crate::row_model::FileRowModel, &glib::Object)>,
    ) -> Option<Self> {
        let browser = state.try_borrow().ok()?;
        if browser.pending.is_some() || browser.dialog_open || browser.mutation.is_some() {
            return None;
        }
        if let Some((model, bound)) = bound
            && (!model.represents(&browser.entries) || model.item(index).as_ref() != Some(bound))
        {
            return None;
        }
        let entry = browser.entries.get(index as usize)?;
        if !matches!(entry.kind, FileKind::Regular | FileKind::Directory) {
            return None;
        }
        Some(Self {
            view: Rc::downgrade(state),
            selection: selection.map(|selection| selection.downgrade()),
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

    fn still_current_in_other_window(&self, target: &Shared) -> bool {
        let Some(view) = self.view.upgrade() else {
            return false;
        };
        if Rc::ptr_eq(&view, target)
            || self.selection.as_ref().is_some_and(|selected| {
                selected
                    .upgrade()
                    .is_none_or(|selected| selected.selected() != self.index)
            })
        {
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

fn install_source(widget: &impl IsA<gtk::Widget>, prepare: impl Fn() -> Option<Pending> + 'static) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::COPY);
    let prepared = Rc::new(RefCell::new(None::<Pending>));
    source.connect_prepare({
        let prepared = prepared.clone();
        move |_, _, _| {
            let pending = prepare();
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
    widget.add_controller(source);
}

pub(super) type RowDragView = Rc<RefCell<Weak<RefCell<crate::Browser>>>>;

pub(super) fn install_row(
    label: &gtk::Label,
    item: &gtk::ListItem,
    model: &crate::row_model::FileRowModel,
    view: RowDragView,
) {
    let item = item.downgrade();
    let model = model.downgrade();
    install_source(label, move || {
        let (item, model, view) = (item.upgrade()?, model.upgrade()?, view.borrow().upgrade()?);
        let bound = item.item()?;
        Pending::capture(&view, item.position(), None, Some((&model, &bound)))
    });
}

#[derive(Default)]
pub(super) struct FolderRowTargets {
    view: glib::WeakRef<gtk::Widget>,
    rows: RefCell<std::collections::HashMap<usize, glib::WeakRef<gtk::ListItem>>>,
    hover: RefCell<Option<FolderHover>>,
    watching_geometry: Cell<bool>,
}

struct FolderHover {
    drag: glib::WeakRef<gdk::Drag>,
    rows: std::sync::Weak<Vec<FileEntry>>,
    active: Option<glib::WeakRef<gtk::ListItem>>,
    bounds: (f32, f32, f32, f32),
    invalidated: bool,
}

impl FolderRowTargets {
    pub(super) fn setup(&self, item: &gtk::ListItem, child: &impl IsA<gtk::Widget>) {
        let key = child.upcast_ref::<gtk::Widget>().as_ptr() as usize;
        self.rows.borrow_mut().insert(key, item.downgrade());
    }

    pub(super) fn teardown(&self, item: &gtk::ListItem) {
        if let Some(child) = item.child() {
            self.rows.borrow_mut().remove(&(child.as_ptr() as usize));
        }
        self.unbind(item);
    }

    fn at(&self, list: &gtk::Widget, x: f64, y: f64) -> Option<gtk::ListItem> {
        let rows = self.rows.borrow();
        let mut hit = list.pick(x, y, gtk::PickFlags::DEFAULT)?;
        while &hit != list {
            if let Some(item) = rows
                .get(&(hit.as_ptr() as usize))
                .and_then(glib::WeakRef::upgrade)
                && item.child().as_ref() == Some(&hit)
            {
                return Some(item);
            }
            hit = hit.parent()?;
        }
        None
    }

    fn set_highlight(item: &gtk::ListItem, active: bool) {
        if let Some(label) = item.child() {
            if active {
                label.set_state_flags(gtk::StateFlags::DROP_ACTIVE, false);
            } else {
                label.unset_state_flags(gtk::StateFlags::DROP_ACTIVE);
            }
        }
    }

    pub(super) fn unbind(&self, item: &gtk::ListItem) {
        let active = {
            let mut hover = self.hover.borrow_mut();
            hover.as_mut().is_some_and(|hover| {
                if hover
                    .active
                    .as_ref()
                    .and_then(glib::WeakRef::upgrade)
                    .as_ref()
                    != Some(item)
                {
                    return false;
                }
                hover.invalidated = true;
                hover.active = None;
                true
            })
        };
        if active {
            Self::set_highlight(item, false);
            if let Some(view) = self.view.upgrade() {
                view.unset_state_flags(gtk::StateFlags::DROP_ACTIVE);
            }
        }
    }

    fn clear_highlight(&self) {
        let old = self
            .hover
            .borrow_mut()
            .as_mut()
            .and_then(|hover| hover.active.take())
            .and_then(|item| item.upgrade());
        if let Some(old) = old {
            Self::set_highlight(&old, false);
        }
    }

    fn bounds(item: &gtk::ListItem, view: &gtk::Widget) -> Option<(f32, f32, f32, f32)> {
        let bounds = item.child()?.compute_bounds(view)?;
        Some((bounds.x(), bounds.y(), bounds.width(), bounds.height()))
    }

    fn invalidate_moved(&self, view: &gtk::Widget) -> bool {
        let active = self
            .hover
            .borrow()
            .as_ref()
            .and_then(|hover| Some((hover.active.as_ref()?.upgrade()?, hover.bounds)));
        if let Some((item, bounds)) = active {
            if Self::bounds(&item, view) == Some(bounds) {
                return true;
            }
            // Scrolling or GridView reflow can move an already bound target.
            self.unbind(&item);
        }
        false
    }

    fn watch_geometry(self: &Rc<Self>, view: &gtk::Widget) {
        if self.watching_geometry.replace(true) {
            return;
        }
        let targets = Rc::downgrade(self);
        view.add_tick_callback(move |view, _| {
            if let Some(targets) = targets.upgrade() {
                if targets.invalidate_moved(view) {
                    return glib::ControlFlow::Continue;
                }
                targets.watching_geometry.set(false);
            }
            glib::ControlFlow::Break
        });
    }

    fn update(
        self: &Rc<Self>,
        drag: &gdk::Drag,
        item: &gtk::ListItem,
        rows: &Arc<Vec<FileEntry>>,
    ) -> bool {
        let Some(view) = self.view.upgrade() else {
            return false;
        };
        self.invalidate_moved(&view);
        let Some(bounds) = Self::bounds(item, &view) else {
            self.clear_highlight();
            return false;
        };
        if self.accepts(drag, item, rows) {
            return true;
        }
        let weak_rows = Arc::downgrade(rows);
        let allowed = {
            let mut hover = self.hover.borrow_mut();
            if let Some(old) = hover.as_mut()
                && old.drag.upgrade().as_ref() == Some(drag)
            {
                if !old.rows.ptr_eq(&weak_rows) {
                    old.invalidated = true;
                }
                !old.invalidated
            } else {
                true
            }
        };
        self.clear_highlight();
        if !allowed {
            return false;
        }
        self.hover.replace(Some(FolderHover {
            drag: drag.downgrade(),
            rows: weak_rows,
            active: Some(item.downgrade()),
            bounds,
            invalidated: false,
        }));
        Self::set_highlight(item, true);
        self.watch_geometry(&view);
        true
    }

    fn accepts(&self, drag: &gdk::Drag, item: &gtk::ListItem, rows: &Arc<Vec<FileEntry>>) -> bool {
        self.hover.borrow().as_ref().is_some_and(|hover| {
            !hover.invalidated
                && hover.drag.upgrade().as_ref() == Some(drag)
                && hover.rows.ptr_eq(&Arc::downgrade(rows))
                && hover
                    .active
                    .as_ref()
                    .and_then(glib::WeakRef::upgrade)
                    .as_ref()
                    == Some(item)
        })
    }
}

fn live_offer_for(drag: &gdk::Drag, state: &Shared) -> bool {
    ACTIVE.with_borrow(|active| {
        active.as_ref().is_some_and(|offer| {
            is_ours(offer, drag) && offer.pending.still_current_in_other_window(state)
        })
    })
}

fn target_drag(target: &gtk::DropTarget) -> Option<gdk::Drag> {
    target.property::<Option<gdk::Drop>>("drop")?.drag()
}

pub(super) fn install_folder_target(
    view: &impl IsA<gtk::Widget>,
    model: &crate::row_model::FileRowModel,
    state: &Shared,
    status: &gtk::Label,
    targets: Rc<FolderRowTargets>,
) {
    // The view, unlike a recycled row or tile, can still answer the native drop.
    targets.view.set(Some(view.upcast_ref()));
    let target = gtk::DropTarget::new(u64::static_type(), gdk::DragAction::COPY);
    target.set_preload(true);
    let state = Rc::downgrade(state);
    target.connect_accept({
        let state = state.clone();
        move |_, drop| {
            drop.actions() == gdk::DragAction::COPY
                && drop.drag().is_some_and(|drag| {
                    state
                        .upgrade()
                        .is_some_and(|state| live_offer_for(&drag, &state))
                })
        }
    });
    let capture = Rc::new({
        let model = model.downgrade();
        let targets = targets.clone();
        move |target: &gtk::DropTarget, drag: &gdk::Drag, x: f64, y: f64| {
            let (model, state) = (model.upgrade()?, state.upgrade()?);
            if !live_offer_for(drag, &state) {
                return None;
            }
            let item = targets.at(&target.widget()?, x, y)?;
            let bound = item.item()?;
            let folder = Pending::capture(&state, item.position(), None, Some((&model, &bound)))?;
            (folder.kind == FileKind::Directory).then_some((item, state, folder))
        }
    });
    let indicate = Rc::new({
        let (capture, targets) = (capture.clone(), targets.clone());
        move |target: &gtk::DropTarget, x: f64, y: f64| {
            let selected = target_drag(target).is_some_and(|drag| {
                capture(target, &drag, x, y)
                    .is_some_and(|(item, _, folder)| targets.update(&drag, &item, &folder.rows))
            });
            if selected {
                gdk::DragAction::COPY
            } else {
                targets.clear_highlight();
                gdk::DragAction::empty()
            }
        }
    });
    target.connect_enter({
        let indicate = indicate.clone();
        move |target, x, y| indicate(target, x, y)
    });
    target.connect_motion(move |target, x, y| indicate(target, x, y));
    target.connect_leave({
        let targets = targets.clone();
        move |_| targets.clear_highlight()
    });
    let status = status.downgrade();
    target.connect_drop(move |target, marker, x, y| {
        let dispatch = || {
            if !marker.get::<u64>().is_ok_and(|value| value == OFFER) {
                return None;
            }
            let drag = target_drag(target)?;
            let (item, state, folder) = capture(target, &drag, x, y)?;
            if !targets.invalidate_moved(&target.widget()?)
                || !targets.accepts(&drag, &item, &folder.rows)
            {
                return None;
            }
            let status = status.upgrade()?;
            let offer = ACTIVE.with_borrow_mut(|active| {
                if active.as_ref().is_some_and(|offer| is_ours(offer, &drag)) {
                    active.take()
                } else {
                    None
                }
            })?;
            Some(copy_ui::dispatch_drop_into_folder(
                &state,
                &status,
                folder.source,
                offer.pending.source,
                offer.pending.kind,
            ))
        };
        let accepted = dispatch().unwrap_or(false);
        targets.clear_highlight();
        accepted
    });
    view.add_controller(target);
}

pub(super) fn install(
    source_handle: &gtk::Label,
    destination: &gtk::Label,
    state: &Shared,
    selection: &gtk::SingleSelection,
    status: &gtk::Label,
) {
    install_source(source_handle, {
        let state = Rc::downgrade(state);
        let selection = selection.downgrade();
        move || Pending::capture_selection(&state.upgrade()?, &selection.upgrade()?)
    });

    let target = gtk::DropTarget::new(u64::static_type(), gdk::DragAction::COPY);
    target.connect_accept({
        let state = state.clone();
        move |_, drop| {
            drop.actions() == gdk::DragAction::COPY
                && drop.drag().is_some_and(|drag| {
                    ACTIVE.with_borrow(|active| {
                        active.as_ref().is_some_and(|offer| {
                            is_ours(offer, &drag)
                                && offer.pending.still_current_in_other_window(&state)
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
            if !offer.pending.still_current_in_other_window(&state) {
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
                        copy_ui::notice(&status, "请选择不同的目标目录。同名项目不会被覆盖。");
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
                let text = Pending::capture_selection(&state, &selection).map_or_else(
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
