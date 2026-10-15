//! Native-only routing into existing unscaled, unrotated output readbacks.
//! Unsupported layouts retain their legacy path; this is not a desktop atlas.

use crate::backend::compositor_common::capture::{
    CaptureRegion, allocate_rgba_capture, rgba_capture_len,
};
use std::time::{Duration, Instant};

pub(super) const ROUTE_TIMEOUT: Duration = Duration::from_secs(3);
pub(super) const MAX_ROUTE_OUTPUTS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputSnapshot<K> {
    pub identity: K,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub unscaled_normal: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Route {
    pub output_index: usize,
    pub local: CaptureRegion,
}

/// Only extend the known-correct normal/1 pixel contract to a wholly contained
/// nonprimary crop. Do not change primary, overlapping, negative-origin,
/// cross-output, rotated or scaled behavior through this narrow route.
pub(super) fn select_nonprimary<K>(
    outputs: &[OutputSnapshot<K>],
    request: (i32, i32, u32, u32),
) -> Option<Route> {
    if outputs.len() > MAX_ROUTE_OUTPUTS
        || outputs.iter().any(|o| {
            !o.unscaled_normal
                || o.x < 0
                || o.y < 0
                || o.width <= 0
                || o.height <= 0
                || i64::from(o.x) + i64::from(o.width) > i64::from(i32::MAX)
                || i64::from(o.y) + i64::from(o.height) > i64::from(i32::MAX)
        })
    {
        return None;
    }
    for (index, a) in outputs.iter().enumerate() {
        for b in &outputs[index + 1..] {
            if i64::from(a.x) < i64::from(b.x) + i64::from(b.width)
                && i64::from(b.x) < i64::from(a.x) + i64::from(a.width)
                && i64::from(a.y) < i64::from(b.y) + i64::from(b.height)
                && i64::from(b.y) < i64::from(a.y) + i64::from(a.height)
            {
                return None;
            }
        }
    }
    let (x, y, width, height) = request;
    rgba_capture_len(width, height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let left = i64::from(x);
    let top = i64::from(y);
    let right = left + i64::from(width);
    let bottom = top + i64::from(height);
    let mut target = None;
    for (index, output) in outputs.iter().enumerate() {
        let ox = i64::from(output.x);
        let oy = i64::from(output.y);
        let or = ox + i64::from(output.width);
        let ob = oy + i64::from(output.height);
        if left < or && top < ob && right > ox && bottom > oy {
            if target.is_some() || left < ox || top < oy || right > or || bottom > ob {
                return None;
            }
            rgba_capture_len(output.width as u32, output.height as u32).ok()?;
            target = Some(Route {
                output_index: index,
                local: CaptureRegion {
                    x: (left - ox) as u32,
                    y: (top - oy) as u32,
                    width,
                    height,
                },
            });
        }
    }
    target.filter(|route| route.output_index != 0)
}

pub(super) struct Pending<T, K> {
    pub payload: T,
    pub topology: Vec<OutputSnapshot<K>>,
    pub route: Route,
    pub deadline: Instant,
}

/// The existing screenshot permit travels inside T through queue and worker.
pub(super) struct RoutedQueue<T, K> {
    requests: Vec<Pending<T, K>>,
}

impl<T, K> Default for RoutedQueue<T, K> {
    fn default() -> Self {
        Self {
            requests: Vec::new(),
        }
    }
}

impl<T, K: Eq> RoutedQueue<T, K> {
    pub fn push(
        &mut self,
        payload: T,
        topology: Vec<OutputSnapshot<K>>,
        route: Route,
        now: Instant,
    ) {
        self.requests.push(Pending {
            payload,
            topology,
            route,
            deadline: now + ROUTE_TIMEOUT,
        });
    }
    pub fn clear(&mut self) {
        self.requests.clear();
    }
    pub fn has_pending(&self) -> bool {
        !self.requests.is_empty()
    }
    pub fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.requests
            .iter()
            .map(|r| r.deadline.saturating_duration_since(now))
            .min()
    }
    /// Runs before presentation gates, including on inactive sessions. None
    /// topology means its bounded snapshot cannot be represented anymore.
    pub fn retire_unservable(
        &mut self,
        topology: Option<&[OutputSnapshot<K>]>,
        available: impl Fn(usize) -> bool,
        cancelled: impl Fn(&T) -> bool,
        now: Instant,
    ) -> Vec<(T, &'static str)> {
        let mut retired = Vec::new();
        let mut index = 0;
        while index < self.requests.len() {
            let request = &self.requests[index];
            let reason = if cancelled(&request.payload) {
                Some("cancelled")
            } else if topology != Some(request.topology.as_slice()) {
                Some("output topology changed")
            } else if !available(request.route.output_index) {
                Some("output is not presenting")
            } else if now >= request.deadline {
                Some("output capture timed out")
            } else {
                None
            };
            if let Some(reason) = reason {
                retired.push((self.requests.remove(index).payload, reason));
            } else {
                index += 1;
            }
        }
        retired
    }
    pub fn take_for_output(&mut self, output_index: usize) -> Vec<(T, CaptureRegion)> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.requests.len() {
            if self.requests[index].route.output_index == output_index {
                let request = self.requests.remove(index);
                ready.push((request.payload, request.route.local));
            } else {
                index += 1;
            }
        }
        ready
    }
}

/// A failed atlas resize can leave an older, smaller framebuffer alive.
pub(super) fn output_fits_source(origin: (i32, i32), size: (i32, i32), source: (u32, u32)) -> bool {
    origin.0 >= 0
        && origin.1 >= 0
        && size.0 > 0
        && size.1 > 0
        && i64::from(origin.0) + i64::from(size.0) <= i64::from(source.0)
        && i64::from(origin.1) + i64::from(size.1) <= i64::from(source.1)
}

/// Exact top-down pixel copy used by the production native readback path.
/// GL orientation is established by the existing normal/1 renderer, not here.
pub(super) fn crop_rgba(
    source: &[u8],
    width: u32,
    height: u32,
    region: CaptureRegion,
) -> std::io::Result<Vec<u8>> {
    if source.len() != rgba_capture_len(width, height)?
        || region
            .x
            .checked_add(region.width)
            .is_none_or(|right| right > width)
        || region
            .y
            .checked_add(region.height)
            .is_none_or(|bottom| bottom > height)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid output-local capture mapping",
        ));
    }
    let mut pixels = allocate_rgba_capture(region.width, region.height)?;
    let stride = width as usize * 4;
    let row_bytes = region.width as usize * 4;
    for row in 0..region.height as usize {
        let from = (region.y as usize + row) * stride + region.x as usize * 4;
        let to = row * row_bytes;
        pixels[to..to + row_bytes].copy_from_slice(&source[from..from + row_bytes]);
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output(id: u8, x: i32, y: i32) -> OutputSnapshot<u8> {
        OutputSnapshot {
            identity: id,
            x,
            y,
            width: 4,
            height: 3,
            unscaled_normal: true,
        }
    }
    fn outputs() -> Vec<OutputSnapshot<u8>> {
        vec![output(1, 0, 0), output(2, 4, 0)]
    }
    #[test]
    fn production_route_and_copy_select_second_output_pixels() {
        for (x, y) in [(4, 0), (0, 3), (20, 30)] {
            let outputs = vec![output(1, 0, 0), output(2, x, y)];
            let route = select_nonprimary(&outputs, (x + 1, y + 1, 2, 2)).unwrap();
            assert_eq!(route.output_index, 1);
            let source: Vec<_> = (0..12u8).flat_map(|pixel| [2, pixel, 0, 255]).collect();
            assert_eq!(
                crop_rgba(&source, 4, 3, route.local).unwrap(),
                [5, 6, 9, 10]
                    .into_iter()
                    .flat_map(|pixel| [2, pixel, 0, 255])
                    .collect::<Vec<_>>()
            );
        }
    }
    #[test]
    fn boundaries_and_unsupported_paths_preserve_legacy_routing() {
        let mut outputs = outputs();
        assert!(select_nonprimary(&outputs, (0, 0, 4, 3)).is_none());
        assert!(select_nonprimary(&outputs, (3, 0, 2, 1)).is_none());
        assert!(select_nonprimary(&outputs, (8, 0, 1, 1)).is_none());
        assert!(select_nonprimary(&outputs, (4, 0, 0, 1)).is_none());
        assert_eq!(
            select_nonprimary(&outputs, (4, 0, 4, 3)).unwrap().local.x,
            0
        );
        outputs[0].x = -4;
        assert!(select_nonprimary(&outputs, (4, 0, 4, 3)).is_none());
        outputs[0].x = 0;
        outputs[1].unscaled_normal = false;
        assert!(select_nonprimary(&outputs, (4, 0, 4, 3)).is_none());
        outputs[1].unscaled_normal = true;
        outputs[0].width = 5;
        assert!(select_nonprimary(&outputs, (4, 0, 4, 3)).is_none());
    }
    #[test]
    fn queue_pins_identity_and_ends_dark_changed_cancelled_or_expired_requests() {
        for case in 0..5 {
            let original = outputs();
            let mut current = original.clone();
            let now = Instant::now();
            let mut queue = RoutedQueue::default();
            queue.push(
                42,
                original.clone(),
                select_nonprimary(&original, (4, 0, 1, 1)).unwrap(),
                now,
            );
            if case == 0 {
                current[1].identity = 3;
            }
            if case == 1 {
                current[1].x = 5;
            }
            let retired = queue.retire_unservable(
                Some(&current),
                |_| case != 2,
                |_| case == 3,
                if case == 4 { now + ROUTE_TIMEOUT } else { now },
            );
            assert_eq!(retired.len(), 1);
            assert_eq!(retired[0].0, 42);
            assert!(!queue.has_pending());
            assert!(queue.take_for_output(1).is_empty());
        }
    }
    #[test]
    fn primary_frame_does_not_drain_secondary_and_pending_frames_have_deadline() {
        let topology = outputs();
        let now = Instant::now();
        let mut queue = RoutedQueue::default();
        queue.push(
            7,
            topology.clone(),
            select_nonprimary(&topology, (4, 0, 1, 1)).unwrap(),
            now,
        );
        assert!(queue.take_for_output(0).is_empty());
        assert_eq!(queue.next_wakeup(now), Some(ROUTE_TIMEOUT));
        assert!(
            queue
                .retire_unservable(Some(&topology), |_| true, |_| false, now)
                .is_empty()
        );
        assert_eq!(queue.take_for_output(1)[0].0, 7);
        assert!(!queue.has_pending());
        assert_eq!(queue.next_wakeup(now), None);
    }
    #[test]
    fn pixel_copy_rejects_bad_lengths_and_out_of_bounds_without_panicking() {
        let region = CaptureRegion {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        assert!(crop_rgba(&[0; 4], 4, 3, region).is_err());
        assert!(
            crop_rgba(
                &[0; 48],
                4,
                3,
                CaptureRegion {
                    x: u32::MAX,
                    ..region
                }
            )
            .is_err()
        );
        assert!(
            crop_rgba(
                &[0; 48],
                4,
                3,
                CaptureRegion {
                    height: 3,
                    ..region
                }
            )
            .is_err()
        );
    }
    #[test]
    fn failed_atlas_resize_cannot_read_missing_output_pixels() {
        assert!(output_fits_source((4, 3), (4, 3), (8, 6)));
        assert!(!output_fits_source((4, 3), (4, 3), (7, 6)));
        assert!(!output_fits_source((4, 3), (4, 3), (8, 5)));
        assert!(!output_fits_source((-4, 0), (4, 3), (8, 6)));
        assert!(!output_fits_source((i32::MAX, 0), (4, 3), (8, 6)));
    }
    #[test]
    fn teardown_releases_owned_payload_once_and_reorder_invalidates_route() {
        use std::{cell::Cell, rc::Rc};
        struct Permit(Rc<Cell<usize>>);
        impl Drop for Permit {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let topology = outputs();
        let route = select_nonprimary(&topology, (4, 0, 1, 1)).unwrap();
        let drops = Rc::new(Cell::new(0));
        let mut queue = RoutedQueue::default();
        queue.push(
            Permit(drops.clone()),
            topology.clone(),
            route,
            Instant::now(),
        );
        queue.clear();
        assert_eq!(drops.get(), 1);
        assert!(!queue.has_pending());
        queue.clear();
        assert_eq!(drops.get(), 1);
        queue.push(
            Permit(drops.clone()),
            topology.clone(),
            route,
            Instant::now(),
        );
        let mut reordered = topology;
        reordered.swap(0, 1);
        drop(queue.retire_unservable(Some(&reordered), |_| true, |_| false, Instant::now()));
        assert_eq!(drops.get(), 2);
    }
    #[test]
    fn endpoints_and_unrelated_transformed_output_do_not_enter_new_route() {
        let mut topology = outputs();
        topology[1].x = i32::MAX;
        assert!(select_nonprimary(&topology, (i32::MAX, 0, 1, 1)).is_none());
        topology = outputs();
        topology[0].unscaled_normal = false;
        assert!(select_nonprimary(&topology, (4, 0, 1, 1)).is_none());
        assert!(select_nonprimary(&outputs(), (4, 0, u32::MAX, 1)).is_none());
    }
    #[test]
    fn production_wiring_polls_before_presentation_and_wakes_and_retires() {
        let backend = include_str!("wayland_udev/backend.rs");
        let maintenance = backend
            .find(".poll_routed_screenshots(&self.state.soft_disabled_outputs)")
            .unwrap();
        let gate = backend[maintenance..].find("let can_present").unwrap();
        assert!(gate > 0);
        assert!(backend.contains("kms.next_routed_screenshot_wakeup()"));
        let teardown = backend
            .split("fn drop_kms(&mut self)")
            .nth(1)
            .unwrap()
            .split("fn release_retired_kms_devices")
            .next()
            .unwrap();
        assert!(
            teardown.find("cancel_routed_screenshots()").unwrap()
                < teardown.find("registration_token.take()").unwrap()
        );
        let kms = include_str!("udev_kms.rs");
        assert!(kms.contains("|| self.routed_screenshots.has_pending()"));
        assert!(kms.contains("self.routed_screenshots.take_for_output(out_idx)"));
        assert!(kms.contains("screenshot_route::output_fits_source("));
        assert!(kms.contains("screenshot_route::crop_rgba(full_pixels"));
    }
}
