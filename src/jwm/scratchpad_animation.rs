//! Dropdown motion shares one endpoint and resumes from the current visual rect.

use super::Jwm;
use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::animation::{AnimationKind, Easing};
use crate::core::models::ClientKey;
use crate::core::types::Rect;

fn above_output(rect: Rect, output_top: i32, border: i32) -> Rect {
    Rect::new(
        rect.x,
        output_top
            .saturating_sub(rect.h)
            .saturating_sub(border.max(0).saturating_mul(2))
            .saturating_sub(1),
        rect.w,
        rect.h,
    )
}

// Entry can retain a configured playful curve. Exit should settle smoothly
// without a first-frame lurch, bounce back, or overshoot outside its path.
// Explicit linear / accelerating exits remain available through those presets.
fn dropdown_easing(configured: Easing, showing: bool, terminal: bool) -> Easing {
    if showing || !terminal {
        configured
    } else {
        match configured {
            Easing::EaseOut | Easing::Bounce | Easing::Elastic => Easing::EaseInOut,
            other => other,
        }
    }
}

impl Jwm {
    pub(super) fn start_scratchpad_slide(
        &mut self,
        backend: &mut dyn Backend,
        key: ClientKey,
        showing: bool,
        current_visual: Option<Rect>,
    ) {
        let cfg = CONFIG.load();
        if !cfg.motion_enabled() {
            self.animations.remove(key);
            return;
        }
        let Some(client) = self.state.clients.get(key) else {
            return;
        };
        let Some(monitor) = client.mon.and_then(|key| self.state.monitors.get(key)) else {
            return;
        };
        let visible = Rect::new(
            client.geometry.x,
            client.geometry.y,
            client.geometry.w,
            client.geometry.h,
        );
        let hidden = above_output(visible, monitor.geometry.m_y, client.geometry.border_w);
        let from = current_visual.unwrap_or(if showing { hidden } else { visible });
        let to = if showing { visible } else { hidden };
        // Reversing a partly completed slide should take only the remaining
        // fraction of a full slide, rather than starting another full timer.
        let full_distance = (f64::from(visible.y) - f64::from(hidden.y)).abs().max(1.0);
        let remaining =
            ((f64::from(to.y) - f64::from(from.y)).abs() / full_distance).clamp(0.0, 1.0);
        self.animations.start(
            key,
            from,
            to,
            cfg.animation_duration().mul_f64(remaining),
            dropdown_easing(
                cfg.animation_easing(),
                showing,
                self.scratchpads.get("term") == Some(&key),
            ),
            if showing {
                AnimationKind::Appear
            } else {
                AnimationKind::Hide
            },
        );
        // A native reveal must also start above the output on its first
        // frame. Composited windows keep their logical input geometry.
        if showing && !backend.has_compositor() {
            let _ = backend.window_ops().configure(
                client.win,
                from.x,
                from.y,
                from.w.max(1) as u32,
                from.h.max(1) as u32,
                client.geometry.border_w.max(0) as u32,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropdown_exit_curve_is_scoped_and_preserves_explicit_non_rebounding_choices() {
        for easing in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseInOut,
            Easing::EaseOut,
            Easing::Bounce,
            Easing::Elastic,
        ] {
            assert_eq!(dropdown_easing(easing, true, true), easing);
            assert_eq!(
                dropdown_easing(easing, false, false),
                easing,
                "other named scratchpads are unchanged"
            );
            let exit = dropdown_easing(easing, false, true);
            assert_eq!(
                exit,
                match easing {
                    Easing::EaseOut | Easing::Bounce | Easing::Elastic => Easing::EaseInOut,
                    other => other,
                }
            );
            let mut previous = 0.;
            for frame in 0..=1000 {
                let progress = exit.apply(frame as f32 / 1000.);
                assert!(
                    (previous..=1.).contains(&progress),
                    "exit curve must be monotonic and bounded"
                );
                previous = progress;
            }
        }
    }

    #[test]
    fn default_exit_frame_timing_is_monotonic_with_soft_start_and_finish() {
        use crate::core::animation::ClientAnimation;
        use std::time::{Duration, Instant};
        // Sample the actual animation implementation, not a duplicate curve.
        // Cover frame rates and negative/stacked output origins without a wall clock sleep.
        for top in [-1080, 0, 1080] {
            let visible = Rect::new(400, top + 100, 800, 600);
            let hidden = above_output(visible, top, 3);
            let distance = visible.y - hidden.y;
            for hz in [60, 120, 144] {
                let animation = ClientAnimation {
                    from: visible,
                    to: hidden,
                    started_at: Instant::now(),
                    duration: Duration::from_millis(250),
                    easing: dropdown_easing(Easing::EaseOut, false, true),
                    kind: AnimationKind::Hide,
                };
                let frame = Duration::from_secs_f64(1.0 / f64::from(hz));
                let first = animation.sample(animation.started_at + frame).0;
                let before_end = animation
                    .sample(animation.started_at + animation.duration - frame)
                    .0;
                assert!(
                    visible.y - first.y < distance / 30,
                    "gentle first frame at {hz}Hz"
                );
                assert!(
                    before_end.y - hidden.y < distance / 30,
                    "gentle final frame at {hz}Hz"
                );
                assert!(
                    ((visible.y - first.y) - (before_end.y - hidden.y)).abs() <= 1,
                    "default exit should have symmetric ease-in/ease-out frame displacements"
                );
                let midpoint = animation
                    .sample(animation.started_at + animation.duration / 2)
                    .0;
                assert!((visible.y - midpoint.y - distance / 2).abs() <= 1);
                let legacy = ClientAnimation {
                    easing: Easing::EaseOut,
                    ..animation.clone()
                };
                assert!(
                    visible.y - legacy.sample(legacy.started_at + frame).0.y > visible.y - first.y
                );
                let mut previous = visible.y;
                for step in 0..=1000 {
                    let elapsed = animation.duration.mul_f64(f64::from(step) / 1000.);
                    let (rect, done) = animation.sample(animation.started_at + elapsed);
                    assert!(
                        (hidden.y..=previous).contains(&rect.y),
                        "exit must not rebound"
                    );
                    assert_eq!((rect.x, rect.w, rect.h), (visible.x, visible.w, visible.h));
                    assert_eq!(done, step == 1000);
                    previous = rect.y;
                }
                assert_eq!(previous, hidden.y);
            }
        }
    }

    #[test]
    fn scratchpad_slide_endpoint_clears_the_output_including_borders() {
        for top in [-1080, 0, 1080] {
            let visible = Rect::new(400, top + 100, 800, 600);
            let hidden = above_output(visible, top, 3);
            assert_eq!(hidden.x, visible.x);
            assert_eq!((hidden.w, hidden.h), (visible.w, visible.h));
            assert!(hidden.y + hidden.h + 6 < top);
        }
        assert_eq!(
            above_output(Rect::new(0, 0, 800, 600), i32::MIN, 3).y,
            i32::MIN
        );
    }
}
