//! Shared interactive capture target selection for screenshots and recordings.

use crate::backend::api::{Backend, HitTarget};
use crate::core::types::Rect;
use crate::jwm::Jwm;
use log::{info, warn};

const MIN_SCREENSHOT_SIZE: i32 = 3;
const MIN_RECORDING_SIZE: i32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureTarget {
    Region,
    Window,
    Monitor,
    Desktop,
}

impl Default for CaptureTarget {
    fn default() -> Self {
        Self::Region
    }
}

impl CaptureTarget {
    pub const fn next(self) -> Self {
        match self {
            Self::Region => Self::Window,
            Self::Window => Self::Monitor,
            Self::Monitor => Self::Desktop,
            Self::Desktop => Self::Region,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::Region => Self::Desktop,
            Self::Window => Self::Region,
            Self::Monitor => Self::Window,
            Self::Desktop => Self::Monitor,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Window => "window",
            Self::Monitor => "monitor",
            Self::Desktop => "desktop",
        }
    }
}

#[derive(Debug, Default)]
pub struct CaptureInteractionState {
    pub screenshot: CaptureTarget,
    pub recording: CaptureTarget,
}

fn intersect_rect(rect: Rect, bounds: Rect) -> Option<Rect> {
    if rect.w <= 0 || rect.h <= 0 || bounds.w <= 0 || bounds.h <= 0 {
        return None;
    }

    let left = i64::from(rect.x).max(i64::from(bounds.x));
    let top = i64::from(rect.y).max(i64::from(bounds.y));
    let right =
        (i64::from(rect.x) + i64::from(rect.w)).min(i64::from(bounds.x) + i64::from(bounds.w));
    let bottom =
        (i64::from(rect.y) + i64::from(rect.h)).min(i64::from(bounds.y) + i64::from(bounds.h));

    let width = right - left;
    let height = bottom - top;
    if width <= 0 || height <= 0 {
        return None;
    }

    Some(Rect::new(
        i32::try_from(left).ok()?,
        i32::try_from(top).ok()?,
        i32::try_from(width).ok()?,
        i32::try_from(height).ok()?,
    ))
}

impl Jwm {
    fn desktop_capture_rect(&self) -> Option<Rect> {
        (self.s_w > 0 && self.s_h > 0).then(|| Rect::new(0, 0, self.s_w, self.s_h))
    }

    fn clamp_capture_rect(&self, rect: Rect) -> Option<Rect> {
        intersect_rect(rect, self.desktop_capture_rect()?)
    }

    fn monitor_capture_rect(
        &mut self,
        backend: &mut dyn Backend,
        pointer: (f64, f64),
    ) -> Option<Rect> {
        let monitor =
            self.recttomon(backend, pointer.0.round() as i32, pointer.1.round() as i32)?;
        let (x, y, width, height) = self.monitor_rect(monitor);
        self.clamp_capture_rect(Rect::new(
            x,
            y,
            i32::try_from(width).ok()?,
            i32::try_from(height).ok()?,
        ))
    }

    fn window_capture_rect(&self, backend: &mut dyn Backend, hit: HitTarget) -> Option<Rect> {
        let HitTarget::Surface(window) = hit else {
            return None;
        };
        if Some(window) == backend.root_window()
            || Some(window) == backend.compositor_overlay_window()
        {
            return None;
        }

        let rect = if let Some(client_key) = self.wintoclient(window) {
            let client = self.state.clients.get(client_key)?;
            if client.state.is_hidden || client.state.is_swallowed {
                return None;
            }
            Rect::new(
                client.geometry.x,
                client.geometry.y,
                client.total_width(),
                client.total_height(),
            )
        } else {
            let geometry = backend.window_ops().get_geometry(window).ok()?;
            let border = i32::try_from(geometry.border).ok()?.saturating_mul(2);
            Rect::new(
                geometry.x,
                geometry.y,
                i32::try_from(geometry.w).ok()?.saturating_add(border),
                i32::try_from(geometry.h).ok()?.saturating_add(border),
            )
        };

        self.clamp_capture_rect(rect)
    }

    fn capture_target_rect(
        &mut self,
        backend: &mut dyn Backend,
        hit: HitTarget,
        pointer: (f64, f64),
        target: CaptureTarget,
    ) -> Option<Rect> {
        match target {
            CaptureTarget::Region => None,
            CaptureTarget::Window => self.window_capture_rect(backend, hit),
            CaptureTarget::Monitor => self.monitor_capture_rect(backend, pointer),
            CaptureTarget::Desktop => self.desktop_capture_rect(),
        }
    }

    fn commit_screenshot_rect(&mut self, backend: &mut dyn Backend, rect: Rect) -> bool {
        let Some(rect) = self.clamp_capture_rect(rect) else {
            return false;
        };
        if rect.w < MIN_SCREENSHOT_SIZE || rect.h < MIN_SCREENSHOT_SIZE {
            return false;
        }

        self.features.screenshot.select_rect(rect);
        if backend.has_compositor() {
            backend.compositor_set_snap_preview(Some((
                rect.x as f32,
                rect.y as f32,
                rect.w as f32,
                rect.h as f32,
            )));
            self.sync_screenshot_annotation_style(backend);
            self.sync_screenshot_annotation_overlay(backend, false);
        }
        true
    }

    pub(crate) fn set_screenshot_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        target: CaptureTarget,
    ) {
        self.features.capture.screenshot = target;
        self.features.screenshot.reset_selection();
        backend.compositor_set_annotation_mode(false);
        if backend.has_compositor() {
            backend.compositor_set_snap_preview(None);
        }

        if matches!(target, CaptureTarget::Monitor | CaptureTarget::Desktop) {
            let hit = HitTarget::Background { output: None };
            if let Some(rect) = self.capture_target_rect(backend, hit, self.last_mouse_root, target)
            {
                self.commit_screenshot_rect(backend, rect);
            }
        } else {
            backend.compositor_force_full_redraw();
        }

        info!(
            "[capture] screenshot target={} (G region, W window, M monitor, D desktop, Tab cycle)",
            target.label()
        );
    }

    pub(crate) fn cycle_screenshot_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        backwards: bool,
    ) {
        let current = self.features.capture.screenshot;
        let next = if backwards {
            current.previous()
        } else {
            current.next()
        };
        self.set_screenshot_capture_target(backend, next);
    }

    pub(crate) fn commit_screenshot_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        hit: HitTarget,
    ) -> bool {
        let target = self.features.capture.screenshot;
        let Some(rect) = self.capture_target_rect(backend, hit, self.last_mouse_root, target)
        else {
            if target == CaptureTarget::Window {
                warn!("[capture] click a visible window to select it");
            }
            return false;
        };
        self.commit_screenshot_rect(backend, rect)
    }

    pub(crate) fn preview_screenshot_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        hit: HitTarget,
        pointer: (f64, f64),
    ) {
        if !self.features.screenshot.active
            || self.features.screenshot.committed
            || self.features.screenshot.dragging
        {
            return;
        }

        let target = self.features.capture.screenshot;
        if target == CaptureTarget::Region {
            return;
        }

        let preview = self
            .capture_target_rect(backend, hit, pointer, target)
            .map(|rect| (rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32));
        backend.compositor_set_snap_preview(preview);
        backend.compositor_force_full_redraw();
    }

    fn apply_recording_rect(&mut self, backend: &mut dyn Backend, rect: Rect) -> bool {
        let Some(rect) = self.clamp_capture_rect(rect) else {
            return false;
        };
        if rect.w < MIN_RECORDING_SIZE || rect.h < MIN_RECORDING_SIZE {
            warn!(
                "[capture] recording target is too small: {}x{}",
                rect.w, rect.h
            );
            return false;
        }

        self.features.recording.set_region(rect);
        if self.features.recording.adjusting_region {
            if let Some(region) = Self::recording_region_tuple(rect) {
                backend.compositor_set_recording_region(region);
            }
        }
        self.sync_recording_region_overlay(backend);
        true
    }

    pub(crate) fn set_recording_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        target: CaptureTarget,
    ) {
        self.features.capture.recording = target;
        self.features.recording.end_region_drag();

        match target {
            CaptureTarget::Region => {
                if !self.features.recording.adjusting_region {
                    self.features.recording.region = None;
                }
                self.sync_recording_region_overlay(backend);
            }
            CaptureTarget::Window => {
                self.features.recording.region = None;
                self.sync_recording_region_overlay(backend);
            }
            CaptureTarget::Monitor | CaptureTarget::Desktop => {
                let hit = HitTarget::Background { output: None };
                if let Some(rect) =
                    self.capture_target_rect(backend, hit, self.last_mouse_root, target)
                {
                    self.apply_recording_rect(backend, rect);
                }
            }
        }

        info!(
            "[capture] recording target={} (G region, W window, M monitor, D desktop, Tab cycle, Enter confirm)",
            target.label()
        );
    }

    pub(crate) fn cycle_recording_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        backwards: bool,
    ) {
        let current = self.features.capture.recording;
        let next = if backwards {
            current.previous()
        } else {
            current.next()
        };
        self.set_recording_capture_target(backend, next);
    }

    pub(crate) fn commit_recording_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        hit: HitTarget,
    ) -> bool {
        let target = self.features.capture.recording;
        let Some(rect) = self.capture_target_rect(backend, hit, self.last_mouse_root, target)
        else {
            if target == CaptureTarget::Window {
                warn!("[capture] click a visible window to select it for recording");
            }
            return false;
        };
        self.apply_recording_rect(backend, rect)
    }

    pub(crate) fn preview_recording_capture_target(
        &mut self,
        backend: &mut dyn Backend,
        hit: HitTarget,
        pointer: (f64, f64),
    ) {
        if !self.features.recording.selecting_region
            || self.features.capture.recording != CaptureTarget::Window
        {
            return;
        }

        let preview = self
            .capture_target_rect(backend, hit, pointer, CaptureTarget::Window)
            .and_then(Self::recording_region_tuple);
        backend.compositor_set_recording_region_overlay(preview);
        backend.compositor_force_full_redraw();
    }

    pub(crate) fn nudge_recording_capture_region(
        &mut self,
        backend: &mut dyn Backend,
        dx: i32,
        dy: i32,
    ) {
        let Some(bounds) = self.desktop_capture_rect() else {
            return;
        };
        let Some(mut region) = self.features.recording.region else {
            return;
        };
        let max_x = (bounds.x + bounds.w - region.w).max(bounds.x);
        let max_y = (bounds.y + bounds.h - region.h).max(bounds.y);
        region.x = region.x.saturating_add(dx).clamp(bounds.x, max_x);
        region.y = region.y.saturating_add(dy).clamp(bounds.y, max_y);
        self.apply_recording_rect(backend, region);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_target_cycles_in_both_directions() {
        assert_eq!(CaptureTarget::Region.next(), CaptureTarget::Window);
        assert_eq!(CaptureTarget::Window.next(), CaptureTarget::Monitor);
        assert_eq!(CaptureTarget::Monitor.next(), CaptureTarget::Desktop);
        assert_eq!(CaptureTarget::Desktop.next(), CaptureTarget::Region);
        assert_eq!(CaptureTarget::Region.previous(), CaptureTarget::Desktop);
    }

    #[test]
    fn intersection_clips_partially_visible_windows() {
        assert_eq!(
            intersect_rect(Rect::new(-20, 10, 80, 50), Rect::new(0, 0, 100, 100)),
            Some(Rect::new(0, 10, 60, 50))
        );
    }

    #[test]
    fn intersection_rejects_offscreen_or_empty_rectangles() {
        assert_eq!(
            intersect_rect(Rect::new(120, 10, 20, 20), Rect::new(0, 0, 100, 100)),
            None
        );
        assert_eq!(
            intersect_rect(Rect::new(10, 10, 0, 20), Rect::new(0, 0, 100, 100)),
            None
        );
    }
}
