//! Read-only metadata for one native item. Never follows the last symlink.
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{FileIdentity, Location};
use std::path::{Path, PathBuf};

const TITLE: &str = "JWM Files · Properties";
const MAX_PANELS: usize = 4;
const ATTRS: &str = "standard::type,standard::size,standard::symlink-target,time::modified,unix::device,unix::inode,unix::mode,unix::uid,unix::gid";

fn identity_matches(info: &gio::FileInfo, expected: FileIdentity) -> bool {
    info.has_attribute("unix::device")
        && info.has_attribute("unix::inode")
        && u64::from(info.attribute_uint32("unix::device")) == expected.device
        && info.attribute_uint64("unix::inode") == expected.inode
}

fn render(info: &gio::FileInfo) -> String {
    let kind = match info.file_type() {
        gio::FileType::Directory => "目录",
        gio::FileType::Regular => "普通文件",
        gio::FileType::SymbolicLink => "符号链接",
        _ => "其他",
    };
    let mut text = vec![format!("类型：{kind}")];
    if info.file_type() == gio::FileType::Regular && info.has_attribute("standard::size") {
        text.push(format!("大小：{} 字节", info.size()));
    }
    if let Some(target) = info.symlink_target() {
        text.push(format!(
            "链接指向：{}",
            crate::trash_ui::display_path(target.as_os_str())
        ));
    }
    if let Some(time) = info
        .modification_date_time()
        .and_then(|time| time.format("%Y-%m-%d %H:%M:%S %Z").ok())
    {
        text.push(format!("修改时间：{time}"));
    }
    if info.has_attribute("unix::mode") {
        text.push(format!(
            "权限：{:04o}",
            info.attribute_uint32("unix::mode") & 0o7777
        ));
    }
    if info.has_attribute("unix::uid") && info.has_attribute("unix::gid") {
        text.push(format!(
            "所有者：{}:{}",
            info.attribute_uint32("unix::uid"),
            info.attribute_uint32("unix::gid")
        ));
    }
    text.join("\n")
}

pub(super) fn show(
    app: &gtk::Application,
    parent: Option<&gtk::Window>,
    path: &Path,
    expected: Option<FileIdentity>,
) -> bool {
    if !path.is_absolute()
        || app
            .windows()
            .iter()
            .filter(|window| window.title().as_deref() == Some(TITLE))
            .count()
            >= MAX_PANELS
    {
        return false;
    }
    let panel = gtk::Window::builder()
        .application(app)
        .title(TITLE)
        .default_width(560)
        .default_height(250)
        .build();
    if let Some(parent) = parent {
        panel.set_transient_for(Some(parent));
        panel.set_destroy_with_parent(true);
    }
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 12);
    for label in ["路径", "文件属性"] {
        let heading = gtk::Label::new(Some(label));
        heading.set_xalign(0.0);
        heading.add_css_class("heading");
        layout.append(&heading);
        if label == "路径" {
            let name = gtk::Label::new(Some(&crate::trash_ui::display_path(path.as_os_str())));
            name.set_xalign(0.0);
            name.set_wrap(true);
            name.set_selectable(true);
            layout.append(&name);
        }
    }
    let detail = gtk::Label::new(Some("正在读取属性…"));
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.set_selectable(true);
    detail.set_widget_name("jwm-files-property-detail");
    layout.append(&detail);
    let close = gtk::Button::with_label("关闭");
    close.set_halign(gtk::Align::End);
    close.connect_clicked({
        let panel = panel.downgrade();
        move |_| {
            if let Some(panel) = panel.upgrade() {
                panel.close();
            }
        }
    });
    layout.append(&close);
    layout.set_margin_start(16);
    layout.set_margin_end(16);
    layout.set_margin_top(14);
    layout.set_margin_bottom(14);
    let scroll = gtk::ScrolledWindow::builder().child(&layout).build();
    panel.set_child(Some(&scroll));

    let cancel = gio::Cancellable::new();
    panel.connect_close_request({
        let cancel = cancel.clone();
        move |_| {
            cancel.cancel();
            glib::Propagation::Proceed
        }
    });
    panel.connect_destroy({
        let cancel = cancel.clone();
        move |_| cancel.cancel()
    });
    let detail = detail.downgrade();
    let active = cancel.clone();
    gio::File::for_path(path).query_info_async(
        ATTRS,
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        glib::Priority::DEFAULT,
        Some(&cancel),
        move |result| {
            if active.is_cancelled() {
                return;
            }
            if let Some(detail) = detail.upgrade() {
                let text = match result {
                    Ok(info)
                        if expected.is_none_or(|expected| identity_matches(&info, expected)) =>
                    {
                        render(&info)
                    }
                    Ok(_) => "选中的项目已改变，请重新选择。".to_owned(),
                    Err(error) => format!("无法读取属性：{error}"),
                };
                detail.set_text(&text);
            }
        },
    );
    panel.present();
    true
}

pub(super) fn install(
    window: &gtk::ApplicationWindow,
    state: &crate::Shared,
    selection: &gtk::SingleSelection,
    status: &gtk::Label,
) {
    let action = gio::SimpleAction::new("item-properties", None);
    let (weak, state, selection, status) = (
        window.downgrade(),
        state.clone(),
        selection.clone(),
        status.clone(),
    );
    action.connect_activate(move |_, _| {
        let Some(window) = weak.upgrade() else { return };
        if crate::comparison::has_focus(&window) {
            return;
        }
        let target: Option<(PathBuf, FileIdentity)> = {
            let state = state.borrow();
            if state.pending.is_some() || state.parent.is_none() {
                return;
            }
            state
                .entries
                .get(selection.selected() as usize)
                .and_then(|entry| {
                    let Location::Local(path) = &entry.location;
                    Some((path.clone(), entry.identity?))
                })
        };
        let Some((path, identity)) = target else {
            status.set_text("请选择要查看属性的项目。");
            return;
        };
        if let Some(app) = window.application()
            && !show(&app, Some(window.upcast_ref()), &path, Some(identity))
        {
            status.set_text("最多同时打开四个属性窗口。");
        }
    });
    window.add_action(&action);
    if let Some(app) = window.application() {
        app.set_accels_for_action("win.item-properties", &["<Alt>Return"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_rendering_distinguishes_link_kind_and_mismatched_identity() {
        let info = gio::FileInfo::new();
        info.set_file_type(gio::FileType::SymbolicLink);
        info.set_symlink_target("missing-target");
        info.set_size(14);
        info.set_attribute_uint32("unix::device", 7);
        info.set_attribute_uint64("unix::inode", 52);
        info.set_attribute_uint32("unix::mode", 0o120755);
        assert!(identity_matches(
            &info,
            FileIdentity {
                device: 7,
                inode: 52
            }
        ));
        assert!(!identity_matches(
            &info,
            FileIdentity {
                device: 7,
                inode: 53
            }
        ));
        let text = render(&info);
        assert!(text.contains("类型：符号链接"));
        assert!(text.contains("链接指向：missing-target"));
        assert!(text.contains("权限：0755"));
        assert!(
            !text.contains("大小："),
            "a link is not the target's file size"
        );
    }
}
