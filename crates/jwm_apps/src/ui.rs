//! Shared GTK chrome using JWM Player's palette, spacing and line icons.
use gtk::{cairo, gdk, prelude::*};

pub(crate) fn install() {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(true);
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("ui.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Notes,
    Image,
    New,
    Folder,
    Save,
    Export,
    Trash,
    Previous,
    Next,
    ZoomIn,
    ZoomOut,
    Actual,
    Fit,
    Rotate,
    Fullscreen,
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn icon(kind: Icon) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(18)
        .content_height(18)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["jwm-icon"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    area.set_draw_func(move |area, cr, width, height| {
        let color = area.color();
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            color.alpha().into(),
        );
        cr.scale(f64::from(width) / 24.0, f64::from(height) / 24.0);
        cr.set_line_width(1.5);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        let path = |points: &[(f64, f64)]| {
            cr.move_to(points[0].0, points[0].1);
            for &(x, y) in &points[1..] {
                cr.line_to(x, y);
            }
        };
        let rect = |x, y, w, h| cr.rectangle(x, y, w, h);
        match kind {
            Icon::Notes | Icon::New => {
                path(&[
                    (6., 3.),
                    (15., 3.),
                    (19., 7.),
                    (19., 21.),
                    (6., 21.),
                    (6., 3.),
                ]);
                path(&[(15., 3.), (15., 7.), (19., 7.)]);
                path(&[(9., 12.), (16., 12.)]);
                if matches!(kind, Icon::New) {
                    path(&[(12.5, 8.5), (12.5, 15.5)]);
                } else {
                    path(&[(9., 16.), (16., 16.)]);
                }
            }
            Icon::Image => {
                rect(3., 4., 18., 16.);
                path(&[(3., 17.), (9., 11.), (13., 15.), (16., 12.), (21., 17.)]);
                cr.new_sub_path();
                cr.arc(16., 8., 1., 0., std::f64::consts::TAU);
            }
            // These paths match the player's folder and window-control icons.
            Icon::Folder => path(&[
                (3., 6.),
                (10., 6.),
                (12., 9.),
                (21., 9.),
                (21., 20.),
                (3., 20.),
                (3., 6.),
            ]),
            Icon::Save => {
                path(&[
                    (4., 3.),
                    (17., 3.),
                    (21., 7.),
                    (21., 21.),
                    (4., 21.),
                    (4., 3.),
                ]);
                rect(8., 3., 8., 6.);
                rect(8., 14., 9., 7.);
            }
            Icon::Export => {
                path(&[(4., 14.), (4., 21.), (20., 21.), (20., 14.)]);
                path(&[(12., 16.), (12., 3.)]);
                path(&[(7., 8.), (12., 3.), (17., 8.)]);
            }
            Icon::Trash => {
                path(&[(3., 6.), (21., 6.)]);
                path(&[(5., 6.), (6., 21.), (18., 21.), (19., 6.)]);
                path(&[(9., 6.), (9., 3.), (15., 3.), (15., 6.)]);
                path(&[(10., 10.), (10., 17.)]);
                path(&[(14., 10.), (14., 17.)]);
            }
            Icon::Previous => path(&[(15., 5.), (8., 12.), (15., 19.)]),
            Icon::Next => path(&[(9., 5.), (16., 12.), (9., 19.)]),
            Icon::ZoomIn | Icon::ZoomOut => {
                cr.arc(10., 10., 6.5, 0., std::f64::consts::TAU);
                path(&[(15., 15.), (21., 21.)]);
                path(&[(7., 10.), (13., 10.)]);
                if matches!(kind, Icon::ZoomIn) {
                    path(&[(10., 7.), (10., 13.)]);
                }
            }
            Icon::Actual => {
                path(&[(3., 9.), (6., 6.), (6., 18.)]);
                path(&[(15., 9.), (18., 6.), (18., 18.)]);
                path(&[(11., 9.), (12., 9.)]);
                path(&[(11., 15.), (12., 15.)]);
            }
            Icon::Fit | Icon::Fullscreen => {
                path(&[(3., 9.), (3., 3.), (9., 3.)]);
                path(&[(15., 3.), (21., 3.), (21., 9.)]);
                path(&[(21., 15.), (21., 21.), (15., 21.)]);
                path(&[(9., 21.), (3., 21.), (3., 15.)]);
                if matches!(kind, Icon::Fit) {
                    rect(8., 8., 8., 8.);
                }
            }
            Icon::Rotate => {
                cr.arc(12., 12., 8., 0., std::f64::consts::PI * 1.5);
                path(&[(8., 1.), (12., 4.), (8., 7.)]);
            }
            Icon::Minimize => path(&[(5., 12.), (19., 12.)]),
            Icon::Maximize => rect(5., 5., 14., 14.),
            Icon::Restore => {
                rect(5., 7., 12., 12.);
                path(&[(9., 3.), (21., 3.), (21., 15.)]);
            }
            Icon::Close => {
                path(&[(6., 6.), (18., 18.)]);
                path(&[(18., 6.), (6., 18.)]);
            }
        }
        let _ = cr.stroke();
    });
    area
}

fn plain_button(kind: Icon, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .child(&icon(kind))
        .tooltip_text(tooltip)
        .css_classes(["jwm-icon-button"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
    button
}

pub(crate) fn button(kind: Icon, tooltip: &str, action: &str) -> gtk::Button {
    let button = plain_button(kind, tooltip);
    button.set_action_name(Some(action));
    button
}

pub(crate) fn group(buttons: &[&gtk::Button]) -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    group.set_valign(gtk::Align::Center);
    group.add_css_class("jwm-tools");
    for button in buttons {
        group.append(*button);
    }
    group
}

pub(crate) fn header(
    window: &gtk::ApplicationWindow,
    kind: Icon,
    name: &str,
    title: &gtk::Label,
) -> gtk::HeaderBar {
    window.add_css_class("jwm-app");
    let header = gtk::HeaderBar::builder()
        .show_title_buttons(false)
        .css_classes(["jwm-header"])
        .build();
    let brand = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    brand.add_css_class("jwm-brand");
    brand.append(&icon(kind));
    brand.append(&gtk::Label::new(Some(name)));
    header.pack_start(&brand);
    title.add_css_class("jwm-document-title");
    title.set_width_chars(1);
    title.set_max_width_chars(32);
    header.set_title_widget(Some(title));

    let minimize = plain_button(Icon::Minimize, "最小化");
    let weak = window.downgrade();
    minimize.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.minimize();
        }
    });
    let maximize = plain_button(Icon::Maximize, "最大化 / 还原");
    let weak = window.downgrade();
    maximize.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            if window.is_maximized() {
                window.unmaximize();
            } else {
                window.maximize();
            }
        }
    });
    let weak = maximize.downgrade();
    window.connect_maximized_notify(move |window| {
        if let Some(button) = weak.upgrade() {
            button.set_child(Some(&icon(if window.is_maximized() {
                Icon::Restore
            } else {
                Icon::Maximize
            })));
        }
    });
    let close = plain_button(Icon::Close, "关闭 (Ctrl+W)");
    close.add_css_class("jwm-close");
    let weak = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            // Go through close-request so notes still save or prompt.
            window.close();
        }
    });
    let controls = group(&[&minimize, &maximize, &close]);
    controls.add_css_class("jwm-window-controls");
    header.pack_end(&controls);
    // HeaderBar's native WindowHandle owns dragging, double-click and the
    // window menu. Buttons retain GTK focus, keyboard and accessibility support.
    window.set_titlebar(Some(&header));
    header
}

pub(crate) fn status() -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["jwm-status"])
        .build()
}
