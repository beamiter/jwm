use crate::tabs::{TabId, Tabs};
use gtk::prelude::*;
use gtk4 as gtk;
use std::{cell::RefCell, path::PathBuf};

type Presentation = (TabId, Vec<(TabId, PathBuf)>);

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
        for (id, path) in &next.1 {
            let name = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .escape_debug()
                .to_string();
            let label = gtk::Label::new(Some(&name));
            label.set_max_width_chars(22);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let current = *id == active;
            let path_text = path.to_string_lossy().escape_debug().to_string();
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
            if current {
                button.add_css_class("suggested-action");
            }
            self.buttons.append(&button);
        }
        self.container.set_visible(next.1.len() > 1);
        self.shown.replace(Some(next));
    }
}
