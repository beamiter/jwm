//! Private Xvfb input fixture for the native GTK desktop-header adapter.
//! The caller supplies a private action log; no WM, IPC or device provider runs.
use gtk4::prelude::*;
use std::io::Write;
use std::rc::Rc;
use xbar_core::{BarEvent, BarModel, ModelConfig, TagState, WmSnapshot};
use xbar_gtk::{BarSurface, BarTheme, Dispatch};

fn main() {
    let log =
        std::env::var_os("JWM_HEADER_FIXTURE_ACTIONS").expect("set a private action log path");
    let app = gtk4::Application::builder()
        .application_id("org.jwm.PrivateHeaderInputFixture")
        .build();
    app.connect_activate(move |app| {
        let mut model = BarModel::new(ModelConfig::default()).unwrap();
        let mut tags = vec![TagState::default(); 9];
        tags[0].selected = true;
        tags[3].occupied = true;
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                tags,
                wm_session_id: 1,
                ..Default::default()
            }))
            .unwrap();
        let theme = Rc::new(BarTheme::from_config_at_scale(
            &xbar_core::config::BarConfig::default(),
            true,
            1.0,
        ));
        theme.install();
        let log = log.clone();
        let dispatch: Dispatch = Rc::new(move |action, completion| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&log)
                .unwrap();
            writeln!(file, "{action:?}").unwrap();
            if let Some(completion) = completion {
                let _ = completion.send(true);
            }
        });
        let surface = Rc::new(BarSurface::new(&theme, dispatch));
        let snapshot = model.snapshot();
        surface.sync(
            &snapshot,
            xbar_core::PresentationProjector::project(snapshot.view(), &theme.presentation),
        );
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("PrivateHeaderInputFixture")
            .decorated(false)
            .default_width(1280)
            .default_height(42)
            .build();
        window.set_child(Some(surface.widget()));
        window.connect_close_request(move |_| {
            let _keep_native_tree_alive = (&surface, &theme);
            gtk4::glib::Propagation::Proceed
        });
        window.present();
    });
    app.run();
}
