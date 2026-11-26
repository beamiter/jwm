use gtk::prelude::*;
use gtk4 as gtk;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

const TAIL_COUNT: usize = 2;

fn crumbs(path: &Path) -> Vec<PathBuf> {
    let mut result: Vec<_> = path
        .ancestors()
        .filter(|part| !part.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    result.reverse();
    result
}

fn label(path: &Path) -> String {
    path.file_name()
        .or_else(|| path.components().next_back().map(|part| part.as_os_str()))
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .escape_debug()
        .to_string()
}

type Navigate = Rc<dyn Fn(PathBuf)>;

pub(super) struct Breadcrumb {
    pub(super) container: gtk::Box,
    children: gtk::Box,
    shown: RefCell<Option<PathBuf>>,
    navigate: Navigate,
}

impl Breadcrumb {
    pub(super) fn new(navigate: impl Fn(PathBuf) + 'static) -> Self {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        container.set_hexpand(true);
        let children = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        children.set_hexpand(true);
        container.append(&children);
        let edit = gtk::Button::from_icon_name("document-edit-symbolic");
        edit.set_action_name(Some("win.location"));
        edit.set_tooltip_text(Some("输入位置（Ctrl+L）"));
        container.append(&edit);
        Self {
            container,
            children,
            shown: RefCell::new(None),
            navigate: Rc::new(navigate),
        }
    }

    pub(super) fn show(&self, path: &Path) {
        if self.shown.borrow().as_deref() == Some(path) {
            return;
        }
        self.shown.replace(Some(path.to_path_buf()));
        while let Some(child) = self.children.first_child() {
            self.children.remove(&child);
        }
        let path = crumbs(path);
        let Some((root, rest)) = path.split_first() else {
            return;
        };
        self.children.append(&self.button(root, 8));
        let split = rest.len().saturating_sub(TAIL_COUNT);
        let (overflow, visible) = rest.split_at(split);
        if !overflow.is_empty() {
            let menu = gtk::MenuButton::builder()
                .label("…")
                .tooltip_text("更上层的目录")
                .build();
            let popover = gtk::Popover::new();
            let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let viewport = gtk::Viewport::builder().child(&list).build();
            let scroll = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_width(true)
                .propagate_natural_height(true)
                .max_content_height(300)
                .child(&viewport)
                .build();
            for ancestor in overflow.iter().rev() {
                let row = self.button(ancestor, 48);
                let weak = popover.downgrade();
                row.connect_clicked(move |_| {
                    if let Some(popover) = weak.upgrade() {
                        popover.popdown();
                    }
                });
                list.append(&row);
            }
            popover.set_child(Some(&scroll));
            menu.set_popover(Some(&popover));
            self.children.append(&menu);
        }
        for (index, ancestor) in visible.iter().enumerate() {
            self.children.append(&gtk::Label::new(Some("›")));
            let button = self.button(ancestor, 18);
            if index + 1 == visible.len() {
                button.add_css_class("suggested-action");
            }
            self.children.append(&button);
        }
    }

    fn button(&self, path: &Path, width: i32) -> gtk::Button {
        let name = gtk::Label::new(Some(&label(path)));
        name.set_max_width_chars(width);
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let button = gtk::Button::builder()
            .child(&name)
            .tooltip_text(path.to_string_lossy().escape_debug().to_string())
            .build();
        button.add_css_class("flat");
        let (navigate, path) = (self.navigate.clone(), path.to_path_buf());
        button.connect_clicked(move |_| navigate(path.clone()));
        button
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breadcrumb_uses_lexical_ancestors_and_preserves_path_bytes() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = Path::new(std::ffi::OsStr::from_bytes(b"/root/link/../raw\x80/last"));
            let actual: Vec<_> = crumbs(path)
                .iter()
                .map(|part| part.as_os_str().as_bytes().to_vec())
                .collect();
            assert_eq!(
                actual,
                [
                    b"/".as_slice(),
                    b"/root",
                    b"/root/link",
                    b"/root/link/..",
                    b"/root/link/../raw\x80",
                    b"/root/link/../raw\x80/last",
                ],
            );
            assert_eq!(label(Path::new("/root/link/..")), "..");
        }
        assert_eq!(crumbs(Path::new("/")), vec![PathBuf::from("/")]);
    }
    #[test]
    #[cfg(unix)]
    #[ignore = "requires a private graphical display"]
    fn native_breadcrumb_clicks_use_exact_visible_and_overflow_paths() {
        use std::cell::RefCell;
        use std::os::unix::ffi::OsStrExt;
        gtk::init().unwrap();
        let open = Rc::new(RefCell::new(Vec::new()));
        let breadcrumb = Breadcrumb::new({
            let open = open.clone();
            move |path| open.borrow_mut().push(path)
        });
        let path = |raw: &[u8]| PathBuf::from(std::ffi::OsStr::from_bytes(raw));
        let full = path(b"/home/raw\x80/ordinary/raw\x81/target");
        breadcrumb.show(&full);
        let children = breadcrumb.children.observe_children();
        assert_eq!(children.n_items(), 6);
        let visible = children.item(3).and_downcast::<gtk::Button>().unwrap();
        visible.emit_clicked();
        let overflow = children.item(1).and_downcast::<gtk::MenuButton>().unwrap();
        let popover = overflow.popover().unwrap();
        let scroll = popover
            .child()
            .and_downcast::<gtk::ScrolledWindow>()
            .unwrap();
        let contents = scroll
            .child()
            .and_downcast::<gtk::Viewport>()
            .unwrap()
            .child()
            .and_downcast::<gtk::Box>()
            .unwrap();
        let raw = contents
            .first_child()
            .unwrap()
            .next_sibling()
            .and_downcast::<gtk::Button>()
            .unwrap();
        raw.emit_clicked();
        assert_eq!(
            *open.borrow(),
            [
                path(b"/home/raw\x80/ordinary/raw\x81"),
                path(b"/home/raw\x80")
            ]
        );
        breadcrumb.show(Path::new("/"));
        assert_eq!(children.n_items(), 1);
        println!("GTK4 X11 breadcrumb: visible and overflow buttons preserved exact path bytes");
    }
}
