use crate::tabs::{TabId, Tabs};
use glib::translate::ToGlibPtr;
use gtk::{glib, prelude::*};
use gtk4 as gtk;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

type Presentation = (TabId, Vec<(TabId, PathBuf)>);

fn display_path(path: &Path) -> String {
    path.to_string_lossy().escape_debug().to_string()
}

fn suffix(path: &Path, depth: usize) -> String {
    let mut tail = path.components().rev().take(depth).collect::<Vec<_>>();
    if tail.is_empty() {
        return display_path(path);
    }
    tail.reverse();
    let tail: PathBuf = tail
        .into_iter()
        .map(|segment| segment.as_os_str())
        .collect();
    display_path(&tail)
}

fn tab_labels(paths: &[(TabId, PathBuf)]) -> Vec<String> {
    let mut labels: Vec<_> = paths.iter().map(|(_, path)| suffix(path, 1)).collect();
    let counts: Vec<_> = paths
        .iter()
        .map(|(_, path)| path.components().count())
        .collect();
    for depth in 2..=counts.iter().copied().max().unwrap_or_default() {
        let previous = labels.clone();
        let mut changed = false;
        for (index, label) in labels.iter_mut().enumerate() {
            if counts[index] >= depth
                && previous
                    .iter()
                    .enumerate()
                    .any(|(peer, name)| peer != index && name == &previous[index])
            {
                *label = suffix(&paths[index].1, depth);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Repeated paths can represent distinct navigation histories. Lossy
    // display names can also collide for different raw bytes. Give them a
    // stable numeric marker without deriving any actions from the label.
    let previous = labels.clone();
    for (index, label) in labels.iter_mut().enumerate() {
        if previous
            .iter()
            .enumerate()
            .any(|(peer, name)| peer != index && name == &previous[index])
        {
            *label = format!(
                "〔{}〕{}",
                paths[index].0.action_parameter(),
                previous[index]
            );
        }
    }
    labels
}

// GTK's stable C API exposes this from 4.0. gtk4-rs's convenience
// wrapper is gated at 4.10. Keep the existing library floor.
#[link(name = "gtk-4")]
unsafe extern "C" {
    fn gtk_accessible_update_state_value(
        accessible: *mut gtk::ffi::GtkAccessible,
        number: std::ffi::c_int,
        states: *mut gtk::ffi::GtkAccessibleState,
        values: *const glib::gobject_ffi::GValue,
    );

    #[cfg(test)]
    fn gtk_test_accessible_check_state(
        accessible: *mut gtk::ffi::GtkAccessible,
        state: gtk::ffi::GtkAccessibleState,
        ...
    ) -> *mut std::ffi::c_char;
}

fn set_selected(button: &gtk::Button, selected: bool) {
    let mut state = gtk::ffi::GTK_ACCESSIBLE_STATE_SELECTED;
    // GTK encodes optional booleans as gint: -1 undefined, 0 false, 1 true.
    let value = i32::from(selected).to_value();
    // SAFETY: button implements GtkAccessible and stays live. Both arrays
    // have exactly one valid value; GTK borrows them for the call only.
    unsafe {
        gtk_accessible_update_state_value(
            button.as_ptr().cast(),
            1,
            &mut state,
            value.to_glib_none().0,
        );
    }
}

#[cfg(test)]
pub(super) fn assert_selected(button: &gtk::Button, selected: bool) {
    // SAFETY: GTK expects a single promoted gint for its SELECTED state.
    // A non-null diagnostic is a newly allocated string owned by the caller.
    let failure: Option<glib::GString> = unsafe {
        glib::translate::from_glib_full(gtk_test_accessible_check_state(
            button.as_ptr().cast(),
            gtk::ffi::GTK_ACCESSIBLE_STATE_SELECTED,
            i32::from(selected),
        ))
    };
    assert!(
        failure.is_none(),
        "tab a11y selected={selected}: {failure:?}"
    );
}

pub(super) struct TabStrip {
    pub(super) container: gtk::Box,
    buttons: gtk::Box,
    shown: RefCell<Option<Presentation>>,
}

impl TabStrip {
    pub(super) fn new() -> Self {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_visible(false);
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .accessible_role(gtk::AccessibleRole::TabList)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&buttons)
            .build();
        container.append(&scroll);
        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.set_action_name(Some("win.new-tab"));
        add.set_tooltip_text(Some("新建标签页（Ctrl+T）"));
        container.append(&add);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.set_action_name(Some("win.close-tab"));
        close.set_tooltip_text(Some("关闭当前标签页（Ctrl+W）"));
        container.append(&close);
        Self {
            container,
            buttons,
            shown: RefCell::new(None),
        }
    }

    pub(super) fn sync(&self, tabs: &Tabs) {
        let active = tabs.active_id();
        let paths: Vec<_> = tabs
            .iter()
            .map(|(id, path)| (id, path.to_path_buf()))
            .collect();
        let next = (active, paths);
        if self.shown.borrow().as_ref() == Some(&next) {
            return;
        }
        while let Some(child) = self.buttons.first_child() {
            self.buttons.remove(&child);
        }
        for ((id, path), name) in next.1.iter().zip(tab_labels(&next.1)) {
            let label = gtk::Label::new(Some(&name));
            label.set_max_width_chars(22);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let current = *id == active;
            let path_text = display_path(path);
            let tooltip = if current {
                format!("当前标签页：{path_text}")
            } else {
                path_text
            };
            let button = gtk::Button::builder()
                .child(&label)
                .accessible_role(gtk::AccessibleRole::Tab)
                .tooltip_text(tooltip)
                .build();
            button.set_action_target_value(Some(&id.action_parameter().to_variant()));
            button.set_action_name(Some("win.activate-tab"));
            set_selected(&button, current);
            if current {
                button.add_css_class("suggested-action");
            }
            self.buttons.append(&button);
        }
        self.container.set_visible(next.1.len() > 1);
        self.shown.replace(Some(next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_basenames_gain_only_the_context_they_need() {
        let paths = ["/work/jwm/src", "/work/companion/src", "/var/tmp/dist"];
        let paths = paths
            .into_iter()
            .enumerate()
            .map(|(n, p)| (TabId::from_action_parameter(n as u64 + 1), PathBuf::from(p)))
            .collect::<Vec<_>>();
        assert_eq!(tab_labels(&paths), ["jwm/src", "companion/src", "dist"]);
    }

    #[test]
    fn identical_paths_keep_distinct_stable_visual_ids() {
        let paths = ["/work/src", "/work/src", "/other/src"];
        let paths = paths
            .into_iter()
            .enumerate()
            .map(|(n, p)| (TabId::from_action_parameter(n as u64 + 1), PathBuf::from(p)))
            .collect::<Vec<_>>();
        let names = tab_labels(&paths);
        assert!(names[0].starts_with("〔1〕"));
        assert!(names[1].starts_with("〔2〕"));
        assert_eq!(names[2], "other/src");
    }
}
