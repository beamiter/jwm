//! Exact production bridge source with a deterministic no-device history stub.
//! Does not claim native PCM, IPC sockets, GPU or physical playback validation.
#![allow(dead_code, unused_imports)]
mod audio {
    #[derive(Clone)]
    pub struct PlaybackHistory;
    impl PlaybackHistory {
        pub(crate) fn companion_snapshot(&self) -> Option<(u64, Option<u64>, bool)> {
            None
        }
    }
}
#[path = "../../../eva/src/companion.rs"]
mod companion;

// Native-font lookup is deliberately unavailable: tests exercise the exact
// production built-in ASCII bitmap raster, never spawn fc-match or a display.
mod external_command {
    pub(crate) fn output_with_limits(
        _: &str,
        _: &[&str],
        _: std::time::Duration,
        _: usize,
    ) -> std::io::Result<std::process::Output> {
        Err(std::io::Error::other("fixture_no_external_commands"))
    }
}
#[path = "../../../src/backend/compositor_font.rs"]
mod actual_font;
#[path = "../../../src/backend/compositor_common/companion_scene.rs"]
mod actual_scene;
mod backend {
    pub(crate) use crate::actual_font as compositor_font;
    pub(crate) mod compositor_common {
        pub(crate) use crate::actual_scene as companion_scene;
    }
    pub(crate) mod update_notifier {
        #[derive(Clone)]
        pub(crate) struct AsyncUpdateNotifier;
        impl AsyncUpdateNotifier {
            pub(crate) fn notify(&self) -> bool {
                true
            }
        }
    }
}

#[path = "../../../src/backend/compositor_common/companion_desktop.rs"]
mod actual_desktop;
