//! Match the DOM material to the actual native WebKit surface capability.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gtk::prelude::WidgetExt;
use webkit2gtk::WebViewExt;

fn clear_and_detect(view: &webkit2gtk::WebView) -> bool {
    view.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    let depth = view.window().map(|window| window.visual().depth());
    let composited = view.screen().is_some_and(|screen| screen.is_composited());
    // WebKit's legacy Cairo path retains translucent backing pixels between
    // updates. Respect an explicit request for it, using readable solid paint.
    let legacy_cairo = std::env::var("WEBKIT_DISABLE_COMPOSITING_MODE")
        .is_ok_and(|value| !value.is_empty() && value != "0");
    let transparent = depth == Some(32) && composited && !legacy_cairo;
    log::debug!(
        "WebKit native background: depth={depth:?}, compositor={composited}, legacy_cairo={legacy_cairo}, transparent={transparent}"
    );
    transparent
}

/// Native realization may follow Tauri setup; replay the capability then.
/// The page paints one material layer, while WebKit clears its own backing.
pub fn configure(view: &webkit2gtk::WebView, transparent: Arc<AtomicBool>) {
    transparent.store(clear_and_detect(view), Ordering::Release);
    view.connect_realize(move |view| {
        transparent.store(clear_and_detect(view), Ordering::Release);
    });
}
