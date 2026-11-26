//! Lazy read-only list of already-mounted native locations reported by GIO.
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
};

const MAX_VISIBLE: usize = 64;

#[derive(Debug, Eq, PartialEq)]
struct MountedPlace {
    name: String,
    path: PathBuf,
}

fn native_places(
    mounts: impl IntoIterator<Item = (String, Option<PathBuf>, bool)>,
) -> Vec<MountedPlace> {
    let mut named = BTreeMap::new();
    for (label, path, shadowed) in mounts {
        if let Some(path) = path.filter(|p| p.is_absolute() && p != Path::new("/"))
            && !shadowed
        {
            let label = if label.trim().is_empty() {
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .escape_debug()
                    .to_string()
            } else {
                label.escape_debug().to_string()
            };
            let name = named.entry(path).or_insert_with(|| label.clone());
            if label < *name {
                *name = label;
            }
        }
    }
    let mut results: Vec<_> = named
        .into_iter()
        .map(|(path, name)| MountedPlace { name, path })
        .collect();
    results.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
    results
}

fn update(list: &gtk::Box, monitor: &gio::VolumeMonitor, open: &Rc<dyn Fn(PathBuf)>) {
    while let Some(widget) = list.first_child() {
        list.remove(&widget);
    }
    let places = native_places(monitor.mounts().into_iter().map(|mount| {
        (
            mount.name().to_string(),
            mount.root().path(),
            mount.is_shadowed(),
        )
    }));
    if places.is_empty() {
        let empty = gtk::Label::new(Some("没有已挂载的本地设备"));
        empty.set_wrap(true);
        empty.set_xalign(0.0);
        list.append(&empty);
    }
    let omitted = places.len().saturating_sub(MAX_VISIBLE);
    for MountedPlace { name, path } in places.into_iter().take(MAX_VISIBLE) {
        let label = gtk::Label::new(Some(&name));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let button = gtk::Button::new();
        button.set_child(Some(&label));
        button.set_tooltip_text(Some(&path.to_string_lossy().escape_debug().to_string()));
        let open = open.clone();
        button.connect_clicked(move |_| open(path.clone()));
        list.append(&button);
    }
    if omitted > 0 {
        let more = gtk::Label::new(Some(&format!("另有 {omitted} 个已挂载位置未列出")));
        more.set_wrap(true);
        list.append(&more);
    }
}

struct Subscription {
    monitor: gio::VolumeMonitor,
    handlers: Vec<glib::SignalHandlerId>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            self.monitor.disconnect(handler);
        }
    }
}

pub(super) fn install(open: impl Fn(PathBuf) + 'static) -> gtk::Expander {
    let expander = gtk::Expander::new(Some("已挂载设备"));
    expander.set_tooltip_text(Some("仅浏览已挂载且有本地路径的位置；不会挂载或弹出设备"));
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.set_widget_name("jwm-files-mounted-list");
    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(72)
        .max_content_height(180)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    expander.set_child(Some(&scroll));
    let open: Rc<dyn Fn(PathBuf)> = Rc::new(open);
    let subscription = RefCell::new(None::<Subscription>);
    let list = list.downgrade();
    expander.connect_expanded_notify(move |expander| {
        if !expander.is_expanded() {
            subscription.take();
            return;
        }
        if subscription.borrow().is_some() {
            return;
        }
        let monitor = gio::VolumeMonitor::get();
        let Some(current) = list.upgrade() else {
            return;
        };
        update(&current, &monitor, &open);
        let on_change = {
            let (list, open) = (list.clone(), open.clone());
            move |monitor: &gio::VolumeMonitor, _: &gio::Mount| {
                if let Some(list) = list.upgrade() {
                    update(&list, monitor, &open);
                }
            }
        };
        let handlers = vec![
            monitor.connect_mount_added(on_change.clone()),
            monitor.connect_mount_changed(on_change.clone()),
            monitor.connect_mount_removed(on_change),
        ];
        subscription.replace(Some(Subscription { monitor, handlers }));
    });
    expander
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    #[test]
    fn uses_native_mount_paths_and_excludes_remote_shadowed_root_and_duplicates() {
        let raw = PathBuf::from(OsString::from_vec(b"/media/photos %\x80".to_vec()));
        let places = native_places([
            ("old label".into(), Some(raw.clone()), false),
            ("Camera".into(), Some(raw.clone()), false),
            (
                "Same text".into(),
                Some(PathBuf::from("/media/other")),
                false,
            ),
            ("remote URI".into(), None, false),
            ("relative".into(), Some(PathBuf::from("mount")), false),
            (
                "shadowed".into(),
                Some(PathBuf::from("/media/shadow")),
                true,
            ),
            ("root".into(), Some(PathBuf::from("/")), false),
        ]);
        assert_eq!(
            places,
            [
                MountedPlace {
                    name: "Camera".into(),
                    path: raw
                },
                MountedPlace {
                    name: "Same text".into(),
                    path: "/media/other".into()
                },
            ]
        );
    }
}
