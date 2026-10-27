//! Dropdown motion shares one endpoint and resumes from the current visual rect.

use super::Jwm;
use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
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
            cfg.animation_easing(),
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
