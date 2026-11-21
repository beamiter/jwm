//! Shared GTK presentation for Forge and Anvil.

use crate::gtk;
use crate::jwm_remote_session::{
    ClipboardDirection, ClipboardMode, ClipboardOptions, ConnectionOptions, RemoteDesktop, Scaling,
    SourceChoice, Viewer, CLIPBOARD_FILE_LIMIT_BYTES, TICK,
};
use gtk::{glib, prelude::*};
use std::{cell::RefCell, rc::Rc};

struct Panel {
    address: gtk::Entry,
    key: gtk::Entry,
    backend: gtk::DropDown,
    scaling: gtk::DropDown,
    control: gtk::CheckButton,
    reconnect: gtk::CheckButton,
    gestures: gtk::CheckButton,
    clipboard: gtk::CheckButton,
    clipboard_options: gtk::Box,
    clipboard_mode: gtk::DropDown,
    clipboard_direction: gtk::DropDown,
    clipboard_file_limit: gtk::Label,
    clipboard_consent: gtk::Label,
    connect: gtk::Button,
    disconnect: gtk::Button,
    release: gtk::Button,
    request: gtk::Button,
    sources: gtk::DropDown,
    switch: gtk::Button,
    status: gtk::Label,
    clipboard_summary: gtk::Label,
    metrics: gtk::Label,
    feedback: gtk::Label,
    state: RefCell<RemoteDesktop>,
    sources_shown: RefCell<Vec<SourceChoice>>,
}

impl Panel {
    fn editable(&self, editable: bool) {
        for widget in [&self.address, &self.key] {
            widget.set_sensitive(editable);
        }
        self.backend.set_sensitive(editable);
        self.scaling.set_sensitive(editable);
        self.control.set_sensitive(editable);
        let can_request_gestures = self.control.is_active()
            && Viewer::CHOICES.get(self.backend.selected() as usize) == Some(&Viewer::Wayland);
        self.gestures.set_visible(can_request_gestures);
        self.gestures
            .set_sensitive(editable && can_request_gestures);
        if !can_request_gestures {
            self.gestures.set_active(false);
        }
        self.refresh_clipboard_options(editable);
        self.refresh_reconnect_option(editable);
        self.connect.set_sensitive(editable);
    }

    fn viewer_backend(&self) -> Viewer {
        Viewer::CHOICES
            .get(self.backend.selected() as usize)
            .copied()
            .unwrap_or_default()
    }

    fn clipboard_mode(&self) -> ClipboardMode {
        ClipboardMode::CHOICES
            .get(self.clipboard_mode.selected() as usize)
            .copied()
            .unwrap_or(ClipboardMode::Text)
    }

    fn clipboard_direction(&self) -> ClipboardDirection {
        ClipboardDirection::CHOICES
            .get(self.clipboard_direction.selected() as usize)
            .copied()
            .unwrap_or(ClipboardDirection::FromHost)
    }

    fn refresh_clipboard_options(&self, editable: bool) {
        let available = self.viewer_backend() == Viewer::X11;
        self.clipboard.set_visible(available);
        self.clipboard.set_sensitive(editable && available);
        self.clipboard_options
            .set_visible(editable && available && self.clipboard.is_active());
        self.clipboard_file_limit
            .set_visible(self.clipboard_mode() == ClipboardMode::All);
        self.clipboard_consent.set_text(
            if self.clipboard_direction() == ClipboardDirection::FromHost {
                "The host must allow clipboard sharing."
            } else {
                "The host must allow clipboard sharing. Sending can share the current local clipboard."
            },
        );
    }

    fn refresh_reconnect_option(&self, editable: bool) {
        let available = self
            .viewer_backend()
            .allows_reconnect(self.control.is_active(), self.clipboard.is_active());
        self.reconnect.set_visible(available);
        self.reconnect.set_sensitive(editable && available);
        if !available {
            self.reconnect.set_active(false);
        }
    }

    fn start(&self) {
        let backend = self.viewer_backend();
        let clipboard = (backend == Viewer::X11 && self.clipboard.is_active()).then(|| {
            ClipboardOptions::new(
                self.clipboard_mode(),
                self.clipboard_direction(),
                CLIPBOARD_FILE_LIMIT_BYTES,
            )
        });
        self.state.borrow_mut().connect(ConnectionOptions {
            address: self.address.text().to_string(),
            key_file: self.key.text().to_string(),
            backend,
            scaling: Scaling::CHOICES
                .get(self.scaling.selected() as usize)
                .copied()
                .unwrap_or_default(),
            control: self.control.is_active(),
            reconnect_automatically: self.reconnect.is_active(),
            wayland_gestures: self.gestures.is_active(),
            clipboard,
            x11_embed: None,
        });
        self.refresh();
    }

    fn switch_source(&self) {
        if let Some(source) = self
            .sources_shown
            .borrow()
            .get(self.sources.selected() as usize)
        {
            self.state.borrow_mut().select_source(source);
        }
        self.refresh();
    }

    fn refresh(&self) {
        let ended = {
            let mut state = self.state.borrow_mut();
            let was_active = state.active();
            state.refresh();
            was_active && !state.active()
        };
        if ended {
            self.clipboard.set_active(false);
            self.reconnect.set_active(false);
        }
        let state = self.state.borrow();
        self.editable(!state.active());
        self.disconnect
            .set_sensitive(state.active() && !state.closing());
        self.release.set_sensitive(state.can_release_input());
        self.release.set_visible(!state.can_request_input());
        self.request.set_visible(state.can_request_input());
        self.status.set_text(state.status());
        let clipboard_summary = state.clipboard_summary();
        self.clipboard_summary.set_text(&clipboard_summary);
        self.clipboard_summary
            .set_visible(!clipboard_summary.is_empty());
        self.metrics.set_text(state.metrics());
        self.feedback.set_text(state.feedback());

        let mut shown = self.sources_shown.borrow_mut();
        if *shown != state.sources() {
            *shown = state.sources().to_vec();
            let list = gtk::StringList::new(&[]);
            for source in shown.iter() {
                list.append(&source.label);
            }
            self.sources.set_model(Some(&list));
            if let Some(index) = shown
                .iter()
                .position(|source| Some(source) == state.current_source())
            {
                self.sources.set_selected(index as u32);
            }
        }
        let can_switch = !state.closing() && shown.len() > 1;
        self.sources.set_sensitive(can_switch);
        self.switch.set_sensitive(can_switch);
    }
}

pub(crate) fn present(
    parent: &impl IsA<gtk::Window>,
    active_window: &Rc<glib::WeakRef<gtk::Window>>,
) {
    if let Some(window) = active_window.upgrade() {
        window.present();
        return;
    }
    let window = gtk::Window::builder()
        .title("Remote Desktop")
        .default_width(450)
        .transient_for(parent)
        .destroy_with_parent(true)
        .build();
    active_window.set(Some(&window));
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    let panel = Rc::new(Panel {
        address: gtk::Entry::builder()
            .placeholder_text("Host or IP address")
            .max_length(255)
            .build(),
        key: gtk::Entry::builder()
            .placeholder_text("Default JWM key")
            .build(),
        backend: gtk::DropDown::from_strings(
            &Viewer::CHOICES
                .map(|choice| choice.to_string())
                .each_ref()
                .map(String::as_str),
        ),
        scaling: gtk::DropDown::from_strings(
            &Scaling::CHOICES
                .map(|choice| choice.to_string())
                .each_ref()
                .map(String::as_str),
        ),
        control: gtk::CheckButton::with_label("Allow keyboard and pointer"),
        reconnect: gtk::CheckButton::with_label("Reconnect automatically"),
        gestures: gtk::CheckButton::with_label("Modifier clicks and dragging"),
        clipboard: gtk::CheckButton::with_label("Share clipboard"),
        clipboard_options: gtk::Box::new(gtk::Orientation::Vertical, 8),
        clipboard_mode: gtk::DropDown::from_strings(
            &ClipboardMode::CHOICES
                .map(|choice| choice.to_string())
                .each_ref()
                .map(String::as_str),
        ),
        clipboard_direction: gtk::DropDown::from_strings(
            &ClipboardDirection::CHOICES
                .map(|choice| choice.to_string())
                .each_ref()
                .map(String::as_str),
        ),
        clipboard_file_limit: gtk::Label::new(Some("File limit 2 GiB")),
        clipboard_consent: gtk::Label::new(None),
        connect: gtk::Button::with_label("Connect"),
        disconnect: gtk::Button::with_label("Disconnect"),
        release: gtk::Button::with_label("Release Input"),
        request: gtk::Button::with_label("Request Input"),
        sources: gtk::DropDown::from_strings(&[]),
        switch: gtk::Button::with_label("Switch Source"),
        status: gtk::Label::new(Some("Disconnected")),
        clipboard_summary: gtk::Label::new(None),
        metrics: gtk::Label::new(None),
        feedback: gtk::Label::new(None),
        state: RefCell::new(RemoteDesktop::default()),
        sources_shown: RefCell::new(Vec::new()),
    });
    let fields = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .build();
    for (row, (title, widget)) in [
        ("Host", panel.address.upcast_ref::<gtk::Widget>()),
        ("Key File", panel.key.upcast_ref()),
        ("Viewer", panel.backend.upcast_ref()),
        ("Scaling", panel.scaling.upcast_ref()),
    ]
    .into_iter()
    .enumerate()
    {
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        widget.set_hexpand(true);
        fields.attach(&label, 0, row as i32, 1, 1);
        fields.attach(widget, 1, row as i32, 1, 1);
    }
    body.append(&fields);
    body.append(&panel.control);
    body.append(&panel.gestures);
    body.append(&panel.clipboard);
    body.append(&panel.reconnect);
    let clipboard_fields = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .build();
    for (row, (title, widget)) in [
        ("Clipboard content", &panel.clipboard_mode),
        ("Clipboard direction", &panel.clipboard_direction),
    ]
    .into_iter()
    .enumerate()
    {
        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        label.set_mnemonic_widget(Some(widget));
        widget.set_hexpand(true);
        clipboard_fields.attach(&label, 0, row as i32, 1, 1);
        clipboard_fields.attach(widget, 1, row as i32, 1, 1);
    }
    panel.clipboard_options.append(&clipboard_fields);
    panel.clipboard_options.append(&panel.clipboard_file_limit);
    panel.clipboard_options.append(&panel.clipboard_consent);
    body.append(&panel.clipboard_options);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    for label in [
        &panel.status,
        &panel.metrics,
        &panel.feedback,
        &panel.clipboard_file_limit,
        &panel.clipboard_consent,
        &panel.clipboard_summary,
    ] {
        label.set_xalign(0.0);
        label.set_wrap(true);
    }
    panel.status.add_css_class("heading");
    body.append(&panel.status);
    let sources = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    panel.sources.set_hexpand(true);
    sources.append(&panel.sources);
    sources.append(&panel.switch);
    body.append(&sources);
    body.append(&panel.clipboard_summary);
    body.append(&panel.metrics);
    body.append(&panel.feedback);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    panel.connect.add_css_class("suggested-action");
    actions.append(&panel.release);
    actions.append(&panel.request);
    actions.append(&panel.disconnect);
    actions.append(&panel.connect);
    body.append(&actions);
    window.set_child(Some(&body));
    panel.sources.set_sensitive(false);
    panel.switch.set_sensitive(false);
    panel.release.set_sensitive(false);
    panel.refresh();
    let callbacks: [(&gtk::Button, fn(&Panel)); 5] = [
        (&panel.connect, Panel::start),
        (&panel.switch, Panel::switch_source),
        (&panel.release, |panel| {
            panel.state.borrow_mut().release_input();
            panel.refresh();
        }),
        (&panel.request, |panel| {
            let mut state = panel.state.borrow_mut();
            if let Some(selection) = state
                .current_source()
                .map(|source| source.selection.clone())
            {
                state.request_input(&selection);
            }
            drop(state);
            panel.refresh();
        }),
        (&panel.disconnect, |panel| {
            panel.state.borrow_mut().disconnect();
            panel.refresh();
        }),
    ];
    for (button, callback) in callbacks {
        let weak = Rc::downgrade(&panel);
        button.connect_clicked(move |_| {
            if let Some(panel) = weak.upgrade() {
                callback(&panel);
            }
        });
    }
    let weak = Rc::downgrade(&panel);
    panel.address.connect_activate(move |_| {
        if let Some(panel) = weak.upgrade() {
            panel.start();
        }
    });
    let weak = Rc::downgrade(&panel);
    panel.backend.connect_selected_notify(move |_| {
        if let Some(panel) = weak.upgrade() {
            panel.control.set_active(false);
            panel.clipboard.set_active(false);
            panel.refresh();
        }
    });
    let weak = Rc::downgrade(&panel);
    panel.control.connect_toggled(move |_| {
        if let Some(panel) = weak.upgrade() {
            panel.refresh();
        }
    });
    let weak = Rc::downgrade(&panel);
    panel.clipboard.connect_toggled(move |_| {
        if let Some(panel) = weak.upgrade() {
            let editable = !panel.state.borrow().active();
            panel.refresh_clipboard_options(editable);
            panel.refresh_reconnect_option(editable);
        }
    });
    for choices in [&panel.clipboard_mode, &panel.clipboard_direction] {
        let weak = Rc::downgrade(&panel);
        choices.connect_selected_notify(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.refresh_clipboard_options(!panel.state.borrow().active());
            }
        });
    }
    let weak = Rc::downgrade(&panel);
    glib::timeout_add_local(TICK, move || {
        if let Some(panel) = weak.upgrade() {
            panel.refresh();
            glib::ControlFlow::Continue
        } else {
            glib::ControlFlow::Break
        }
    });
    let active_window = Rc::clone(active_window);
    let address = panel.address.clone();
    window.connect_close_request(move |_| {
        panel.state.borrow_mut().disconnect();
        active_window.set(None);
        glib::Propagation::Proceed
    });
    window.present();
    address.grab_focus();
}
