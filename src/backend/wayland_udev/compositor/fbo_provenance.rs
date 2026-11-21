//! Per-attempt FBO evidence. A texture allocation identity is not a content serial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FboPresentationCandidate {
    pub(crate) token: u64,
    pub(crate) window: u64,
    pub(crate) texture: u32,
    pub(crate) texture_size: (u32, u32),
    pub(crate) rect: (i32, i32, u32, u32),
}
/// Deliberately non-Clone: the backend consumes each successful draw receipt once.
#[derive(Debug)]
pub(crate) struct FboPresentationReceipt {
    pub(crate) candidate_token: u64,
    pub(crate) window: u64,
    pub(crate) texture: u32,
    pub(crate) source_rect: (i32, i32, u32, u32),
    pub(crate) visible_rect: (i32, i32, u32, u32),
    pub(crate) output_texture: u32,
    pub(crate) allocation_generation: u64,
    pub(crate) content_serial: u64,
}
pub(super) struct FboProvenance {
    candidate: Option<FboPresentationCandidate>,
    receipt: Option<FboPresentationReceipt>,
    counter: Option<u64>,
    published: Option<u64>,
}
impl Default for FboProvenance {
    fn default() -> Self {
        Self {
            candidate: None,
            receipt: None,
            counter: Some(0),
            published: None,
        }
    }
}
impl FboProvenance {
    pub(super) fn stage(&mut self, candidate: Option<FboPresentationCandidate>) {
        self.candidate = candidate;
        self.receipt = None;
        self.published = None;
    }
    pub(super) fn begin(&mut self) -> Option<FboPresentationCandidate> {
        self.receipt = None;
        self.published = None;
        self.candidate.take()
    }
    pub(super) fn finish(
        &mut self,
        drawn: Option<(FboPresentationCandidate, (i32, i32, u32, u32))>,
        output_texture: u32,
        allocation_generation: u64,
    ) {
        self.counter = self.counter.and_then(|n| n.checked_add(1));
        self.published = self.counter;
        if let (Some((candidate, visible_rect)), Some(content_serial)) = (drawn, self.counter) {
            self.receipt = Some(FboPresentationReceipt {
                candidate_token: candidate.token,
                window: candidate.window,
                texture: candidate.texture,
                source_rect: candidate.rect,
                visible_rect,
                output_texture,
                allocation_generation,
                content_serial,
            });
        }
    }
    pub(super) fn take(&mut self) -> Option<FboPresentationReceipt> {
        self.receipt.take()
    }
    pub(super) fn serial(&self) -> Option<u64> {
        self.published
    }
}
impl FboPresentationCandidate {
    pub(super) fn visible_interior(
        self,
        window_radius: f32,
        ring_extent: Option<f32>,
    ) -> Option<(i32, i32, u32, u32)> {
        let ring = ring_extent.unwrap_or(0.0);
        if !window_radius.is_finite() || window_radius < 0.0 || !ring.is_finite() || ring < 0.0 {
            return None;
        }
        let inset = if window_radius == 0.0 && ring_extent.is_none() {
            0
        } else {
            // Stay inside both rounded-corner SDFs and clear their
            // antialiasing bands. The ring's outer quad expands outwards
            // by its border width, so its inner straight edge is the
            // window edge. The supplied extent also bounds the outer radius.
            (window_radius.max(ring).ceil() as u32).checked_add(2)?
        };
        let (x, y, w, h) = self.rect;
        let trim = inset.checked_mul(2)?;
        let w = w.checked_sub(trim).filter(|width| *width > 0)?;
        let h = h.checked_sub(trim).filter(|height| *height > 0)?;
        let offset = i32::try_from(inset).ok()?;
        Some((x.checked_add(offset)?, y.checked_add(offset)?, w, h))
    }

    pub(super) fn covers_scene(
        self,
        scene: &[(u64, i32, i32, u32, u32)],
        screen: (u32, u32),
    ) -> bool {
        let (x, y, w, h) = self.rect;
        self.token != 0
            && self.texture != 0
            && w > 0
            && h > 0
            && x >= 0
            && y >= 0
            && u64::from(x as u32) + u64::from(w) <= u64::from(screen.0)
            && u64::from(y as u32) + u64::from(h) <= u64::from(screen.1)
            && scene == [(self.window, x, y, w, h)]
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn candidate() -> FboPresentationCandidate {
        FboPresentationCandidate {
            token: 1,
            window: 2,
            texture: 3,
            texture_size: (10, 10),
            rect: (0, 0, 10, 10),
        }
    }
    #[test]
    fn successful_unproven_frame_changes_serial_and_never_reuses_receipt() {
        let mut p = FboProvenance::default();
        p.stage(Some(candidate()));
        let c = p.begin().map(|c| (c, c.rect));
        p.finish(c, 7, 9);
        let first = p.take().unwrap();
        assert_eq!(first.content_serial, 1);
        assert_eq!(first.source_rect, candidate().rect);
        assert_eq!(first.visible_rect, candidate().rect);
        assert_eq!(
            (
                first.candidate_token,
                first.window,
                first.texture,
                first.output_texture,
                first.allocation_generation
            ),
            (1, 2, 3, 7, 9)
        );
        assert!(p.take().is_none());
        p.begin();
        p.finish(None, 7, 9);
        assert_eq!(p.serial(), Some(2));
        assert!(p.take().is_none());
    }
    #[test]
    fn failed_attempt_and_overflow_never_publish() {
        let mut p = FboProvenance::default();
        p.stage(Some(candidate()));
        p.begin();
        assert!(p.serial().is_none());
        assert!(p.take().is_none());
        p.counter = Some(u64::MAX);
        p.finish(Some((candidate(), candidate().rect)), 7, 9);
        assert!(p.take().is_none());
        assert!(p.serial().is_none());
        p.finish(Some((candidate(), candidate().rect)), 7, 9);
        assert!(p.serial().is_none());
    }
    #[test]
    fn only_one_fully_contained_scene_can_prove_a_quad() {
        let c = candidate();
        assert!(c.covers_scene(&[(2, 0, 0, 10, 10)], (10, 10)));
        for scene in [
            vec![],
            vec![(2, 0, 0, 10, 10), (4, 0, 0, 1, 1)],
            vec![(2, -1, 0, 10, 10)],
            vec![(2, 0, 0, 9, 10)],
        ] {
            assert!(!c.covers_scene(&scene, (10, 10)));
        }
        assert!(!c.covers_scene(&[(2, 0, 0, 10, 10)], (9, 10)));
    }

    #[test]
    fn interior_excludes_rounded_corners_and_ring_coverage() {
        let c = candidate();
        assert_eq!(c.visible_interior(0.0, None), Some(c.rect));
        assert_eq!(c.visible_interior(0.0, Some(0.0)), Some((2, 2, 6, 6)));
        assert_eq!(c.visible_interior(1.0, Some(2.0)), Some((4, 4, 2, 2)));
        assert!(c.visible_interior(3.0, Some(3.0)).is_none());
        assert!(c.visible_interior(f32::NAN, None).is_none());
    }
}
