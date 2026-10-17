//! Native pointer motion batching without losing click-versus-drag evidence.
//!
//! Hover motion keeps only the final event for one target. Motion while the
//! left button is held must retain every delivered sample: an out-and-back
//! excursion can otherwise disappear and become a click on a solar body.

use crate::backend::api::BackendEvent;

/// Queue a hover sample or return the samples that must be delivered now.
/// The native source supplies the button state before event conversion drops
/// it. Callers flush the remaining hover sample at non-motion/batch boundaries.
pub(crate) fn coalesce_pointer_motion(
    pending: &mut Option<BackendEvent>,
    event: BackendEvent,
    left_button_held: bool,
) -> [Option<BackendEvent>; 2] {
    let BackendEvent::MotionNotify { target, .. } = &event else {
        unreachable!("pointer motion coalescer received a non-motion event");
    };
    if left_button_held {
        return [pending.take(), Some(event)];
    }
    let previous = match pending {
        Some(BackendEvent::MotionNotify {
            target: previous, ..
        }) if previous != target => pending.take(),
        _ => None,
    };
    *pending = Some(event);
    [previous, None]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::HitTarget;
    use crate::backend::common_define::WindowId;

    fn motion(x: f64, surface: Option<u64>) -> BackendEvent {
        BackendEvent::MotionNotify {
            target: surface.map_or(HitTarget::Background { output: None }, |id| {
                HitTarget::Surface(WindowId::from_raw(id))
            }),
            root_x: x,
            root_y: 50.0,
            time: 100,
        }
    }

    fn positions(events: Vec<BackendEvent>) -> Vec<f64> {
        events
            .into_iter()
            .map(|event| match event {
                BackendEvent::MotionNotify { root_x, .. } => root_x,
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn held_out_and_back_motion_survives_one_batch_and_hover_remains_coalesced() {
        let mut pending = None;
        let mut delivered = Vec::new();
        for (x, held) in [
            (1.0, false),
            (2.0, false),
            (50.0, true),
            (2.0, true),
            (3.0, false),
            (4.0, false),
        ] {
            delivered.extend(
                coalesce_pointer_motion(&mut pending, motion(x, None), held)
                    .into_iter()
                    .flatten(),
            );
        }
        delivered.extend(pending);
        assert_eq!(positions(delivered), [2.0, 50.0, 2.0, 4.0]);
    }

    #[test]
    fn target_changes_preserve_hover_order_and_held_motion_never_stays_pending() {
        let mut pending = None;
        let mut delivered = Vec::new();
        for (x, target, held) in [
            (1.0, None, false),
            (2.0, Some(4), false),
            (3.0, None, true),
            (4.0, Some(4), true),
        ] {
            delivered.extend(
                coalesce_pointer_motion(&mut pending, motion(x, target), held)
                    .into_iter()
                    .flatten(),
            );
        }
        assert!(pending.is_none());
        assert_eq!(positions(delivered), [1.0, 2.0, 3.0, 4.0]);
    }
}
