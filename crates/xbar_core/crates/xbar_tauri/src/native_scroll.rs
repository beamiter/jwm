//! Preserve GTK's discrete/precise source distinction lost by WebKit's DOM delta.
use gtk::gdk::ScrollDirection;
use gtk::prelude::WidgetExt;
use webkit2gtk::WebViewExt;

pub const INSTALLER: &str = include_str!("native_scroll.js");

fn packet_script(
    direction: ScrollDirection,
    delta: (f64, f64),
    position: (f64, f64),
    stop: bool,
    native_time: u32,
) -> Option<String> {
    let (kind, dx, dy) = match direction {
        ScrollDirection::Up => ("line", 0.0, -1.0),
        ScrollDirection::Down => ("line", 0.0, 1.0),
        ScrollDirection::Left => ("line", -1.0, 0.0),
        ScrollDirection::Right => ("line", 1.0, 0.0),
        ScrollDirection::Smooth => ("pixel", delta.0, delta.1),
        _ => return None,
    };
    let (x, y) = position;
    if ![x, y, dx, dy].into_iter().all(f64::is_finite) {
        return None;
    }
    // Only a closed source kind, boolean and finite numbers enter the script.
    // Reinstalling is idempotent and covers a page reload between GTK events.
    Some(format!(
        "{INSTALLER}\nwindow.__xbarNativeScroll.record({{kind:'{kind}',nativeTime:{native_time},x:{x},y:{y},dx:{dx},dy:{dy},stop:{stop}}});"
    ))
}

/// Call once for each native WebView. Never consume GTK's original event:
/// normal browser scrolling and all precise deltas keep their existing path.
pub fn install(view: &webkit2gtk::WebView) {
    view.connect_scroll_event(|view, event| {
        if let Some(script) = packet_script(
            event.direction(),
            event.delta(),
            event.position(),
            event.is_stop(),
            event.time(),
        ) {
            // This runs before WebKit's default GTK handler posts its wheel
            // event to the same web process. JS matches and consumes bounded
            // metadata; only workspace notches become one DOM line event.
            view.evaluate_javascript(
                &script,
                None,
                None,
                None::<&gtk::gio::Cancellable>,
                |result| {
                    if let Err(error) = result {
                        log::debug!("native scroll metadata unavailable: {error}");
                    }
                },
            );
        }
        gtk::glib::Propagation::Proceed
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk_source_not_equal_dom_numbers_decides_precision() {
        let notch =
            packet_script(ScrollDirection::Down, (0.0, 0.0), (68.0, 21.0), false, 123).unwrap();
        let smooth = packet_script(
            ScrollDirection::Smooth,
            (0.0, 1.0),
            (68.0, 21.0),
            false,
            123,
        )
        .unwrap();
        assert!(
            notch.ends_with("record({kind:'line',nativeTime:123,x:68,y:21,dx:0,dy:1,stop:false});")
        );
        assert!(
            smooth
                .ends_with("record({kind:'pixel',nativeTime:123,x:68,y:21,dx:0,dy:1,stop:false});")
        );
    }

    #[test]
    fn stop_is_explicit_and_nonfinite_native_coordinates_are_rejected() {
        let stop =
            packet_script(ScrollDirection::Smooth, (0.0, 0.0), (68.0, 21.0), true, 123).unwrap();
        assert!(
            stop.ends_with("record({kind:'pixel',nativeTime:123,x:68,y:21,dx:0,dy:0,stop:true});")
        );
        assert!(
            packet_script(
                ScrollDirection::Down,
                (0.0, 0.0),
                (f64::NAN, 0.0),
                false,
                123
            )
            .is_none()
        );
        assert!(
            packet_script(
                ScrollDirection::Smooth,
                (0.0, f64::INFINITY),
                (0.0, 0.0),
                false,
                123
            )
            .is_none()
        );
    }
}
