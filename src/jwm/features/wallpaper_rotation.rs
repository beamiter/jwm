//! Daily wallpaper refresh, sharing the existing one-second maintenance tick.

use std::time::{Duration, Instant};

use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::jwm::Jwm;

const CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct WallpaperRotationState {
    initialized: bool,
    paths: Vec<String>,
    day: Option<i32>,
    next_check: Option<Instant>,
}

impl WallpaperRotationState {
    pub(crate) fn configure(
        &mut self,
        mode: &str,
        paths: &[String],
        day: i32,
        now: Instant,
    ) -> bool {
        let enabled = mode == "daily" && !paths.is_empty();
        let changed = self.day.is_some() != enabled
            || (enabled && (self.paths != paths || self.day != Some(day)));
        self.initialized = true;
        self.paths = if enabled { paths.to_vec() } else { Vec::new() };
        self.day = enabled.then_some(day);
        self.next_check = enabled.then_some(now + CHECK_INTERVAL);
        changed
    }

    pub(crate) fn observed_day(&self) -> Option<i32> {
        self.day
    }

    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.next_check
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn due(&self, now: Instant) -> bool {
        !self.initialized || self.next_check.is_some_and(|deadline| now >= deadline)
    }

    fn observe_day(&mut self, day: i32, now: Instant) -> bool {
        if self.day.is_none() || !self.due(now) {
            return false;
        }
        self.next_check = Some(now + CHECK_INTERVAL);
        let changed = self.day != Some(day);
        self.day = Some(day);
        changed
    }
}

pub(crate) fn local_day() -> i32 {
    chrono::Datelike::num_days_from_ce(&chrono::Local::now().date_naive())
}

impl Jwm {
    /// Reloads already publish monitors and refresh native wallpaper/theme.
    /// Only reset this lightweight observer; do not repeat config application.
    pub(crate) fn reset_wallpaper_rotation(&mut self, now: Instant) {
        Self::expire_daily_wallpaper_pick(local_day());
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        let day = if behavior.wallpaper_rotation == "daily"
            && !behavior.wallpaper_rotation_paths.is_empty()
        {
            local_day()
        } else {
            0
        };
        self.features.wallpaper_rotation.configure(
            &behavior.wallpaper_rotation,
            &behavior.wallpaper_rotation_paths,
            day,
            now,
        );
    }

    pub(crate) fn poll_wallpaper_rotation(&mut self, backend: &mut dyn Backend, now: Instant) {
        if !self.features.wallpaper_rotation.due(now) {
            return;
        }
        if !self.features.wallpaper_rotation.initialized {
            let cfg = CONFIG.load();
            let behavior = cfg.behavior();
            let day = if behavior.wallpaper_rotation == "daily"
                && !behavior.wallpaper_rotation_paths.is_empty()
            {
                local_day()
            } else {
                0
            };
            self.configure_wallpaper_rotation_at(
                backend,
                &behavior.wallpaper_rotation,
                &behavior.wallpaper_rotation_paths,
                now,
                day,
            );
            return;
        }
        self.poll_wallpaper_rotation_at(backend, now, local_day());
    }

    pub(crate) fn configure_wallpaper_rotation_at(
        &mut self,
        backend: &mut dyn Backend,
        mode: &str,
        paths: &[String],
        now: Instant,
        day: i32,
    ) {
        if self
            .features
            .wallpaper_rotation
            .configure(mode, paths, day, now)
        {
            self.refresh_rotating_wallpaper(backend);
        }
    }

    pub(crate) fn poll_wallpaper_rotation_at(
        &mut self,
        backend: &mut dyn Backend,
        now: Instant,
        day: i32,
    ) {
        if self.features.wallpaper_rotation.observe_day(day, now) {
            Self::expire_daily_wallpaper_pick(day);
            self.refresh_rotating_wallpaper(backend);
        }
    }

    fn expire_daily_wallpaper_pick(day: i32) {
        let cfg = CONFIG.load();
        if cfg.behavior().wallpaper_daily_pick.is_none() {
            return;
        }
        let mut updated = (**cfg).clone();
        if updated.expire_picker_wallpaper(day) {
            CONFIG.store(std::sync::Arc::new(updated));
        }
    }

    fn refresh_rotating_wallpaper(&mut self, backend: &mut dyn Backend) {
        self.refresh_compositor_monitors(backend);
        self.ensure_native_root_wallpaper(backend);
        self.refresh_wallpaper_theme();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_observes_midnight_backward_dates_and_changed_configuration() {
        let now = Instant::now();
        let paths = vec!["a".to_string(), "b".to_string()];
        let mut state = WallpaperRotationState::default();
        assert!(state.configure("daily", &paths, 100, now));
        assert!(!state.observe_day(101, now));
        assert!(!state.observe_day(100, now + CHECK_INTERVAL));
        assert!(state.observe_day(101, now + CHECK_INTERVAL * 2));
        assert!(state.observe_day(99, now + CHECK_INTERVAL * 3));
        assert!(!state.configure("daily", &paths, 99, now));
        assert!(state.configure("daily", &["c".to_string()], 99, now));
        assert!(state.configure("off", &paths, 99, now));
        assert_eq!(state.next_wakeup(now), None);
        assert!(!state.observe_day(102, now + CHECK_INTERVAL * 4));
        assert!(!state.configure("off", &paths, 99, now));
    }

    #[test]
    fn disabled_and_empty_rotation_add_no_poll_deadline() {
        let now = Instant::now();
        let mut state = WallpaperRotationState::default();
        assert!(!state.configure("off", &[], 100, now));
        assert_eq!(state.next_wakeup(now), None);
        assert!(!state.due(now + CHECK_INTERVAL));
        assert!(!state.configure("daily", &[], 100, now));
        assert_eq!(state.next_wakeup(now), None);
    }
}
