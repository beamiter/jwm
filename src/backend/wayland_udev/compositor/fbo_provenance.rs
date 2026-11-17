//! Per-attempt FBO evidence. A texture allocation identity is not a content serial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FboPresentationCandidate {
    pub(crate) token: u64,
    pub(crate) window: u64,
    pub(crate) texture: u32,
    pub(crate) rect: (i32, i32, u32, u32),
}
/// Deliberately non-Clone: the backend consumes each successful draw receipt once.
#[derive(Debug)]
pub(crate) struct FboPresentationReceipt {
    pub(crate) candidate_token: u64,
    pub(crate) window: u64,
    pub(crate) texture: u32,
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
        drawn: Option<FboPresentationCandidate>,
        output_texture: u32,
        allocation_generation: u64,
    ) {
        self.counter = self.counter.and_then(|n| n.checked_add(1));
        self.published = self.counter;
        if let (Some(candidate), Some(content_serial)) = (drawn, self.counter) {
            self.receipt = Some(FboPresentationReceipt {
                candidate_token: candidate.token,
                window: candidate.window,
                texture: candidate.texture,
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
            rect: (0, 0, 10, 10),
        }
    }
    #[test]
    fn successful_unproven_frame_changes_serial_and_never_reuses_receipt() {
        let mut p = FboProvenance::default();
        p.stage(Some(candidate()));
        let c = p.begin();
        p.finish(c, 7, 9);
        let first = p.take().unwrap();
        assert_eq!(first.content_serial, 1);
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
        p.finish(Some(candidate()), 7, 9);
        assert!(p.take().is_none());
        assert!(p.serial().is_none());
        p.finish(Some(candidate()), 7, 9);
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
}
