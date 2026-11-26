use crate::row_model::RowPresentation;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use jwm_file_core::FileKind;

fn icon(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Directory => "folder-symbolic",
        FileKind::Symlink => "emblem-symbolic-link-symbolic",
        FileKind::Regular => "text-x-generic-symbolic",
        FileKind::Other => "unknown-symbolic",
    }
}

pub(crate) fn new(selection: &gtk::SingleSelection) -> gtk::GridView {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 6);
        tile.set_size_request(84, 70);
        tile.set_margin_top(6);
        tile.set_margin_bottom(6);
        let image = gtk::Image::new();
        image.set_pixel_size(34);
        tile.append(&image);
        let label = gtk::Label::new(None);
        label.set_max_width_chars(16);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        tile.append(&label);
        item.set_child(Some(&tile));
    });
    factory.connect_bind(|_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let Some(tile) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let row = row.borrow::<RowPresentation>();
        if let Some(image) = tile.first_child().and_downcast::<gtk::Image>() {
            image.set_icon_name(Some(icon(row.kind)));
        }
        if let Some(text) = tile.last_child().and_downcast::<gtk::Label>() {
            text.set_text(&row.name);
        }
        tile.set_tooltip_text(Some(&row.name));
    });
    gtk::GridView::builder()
        .model(selection)
        .factory(&factory)
        .min_columns(1)
        .max_columns(12)
        .single_click_activate(false)
        .build()
}
