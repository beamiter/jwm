//! Coalesce native invalidations without adding a frame timer or input delay.

#[derive(Debug)]
pub struct RedrawGate {
    queued: bool,
    drawable: bool,
}

impl Default for RedrawGate {
    fn default() -> Self {
        Self {
            queued: false,
            drawable: true,
        }
    }
}

impl RedrawGate {
    /// Return true only when the host needs to enqueue a native redraw.
    pub fn request(&mut self) -> bool {
        if self.queued || !self.drawable {
            return false;
        }
        self.queued = true;
        true
    }

    pub fn set_drawable(&mut self, drawable: bool) {
        self.drawable = drawable;
        if !drawable {
            self.queued = false;
        }
    }

    /// Native expose events also paint, even without a preceding request.
    pub fn begin_frame(&mut self) -> bool {
        self.queued = false;
        self.drawable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_coalesce_and_new_work_after_a_frame_is_not_lost() {
        let mut gate = RedrawGate::default();
        assert_eq!((0..1000).filter(|_| gate.request()).count(), 1);
        assert!(gate.begin_frame());
        assert!(gate.request());
        assert!(!gate.request());
        assert!(gate.begin_frame());
        // An expose still needs to repaint the native back buffer.
        assert!(gate.begin_frame());
    }

    #[test]
    fn zero_extent_suppresses_work_and_resize_can_resume_it() {
        let mut gate = RedrawGate::default();
        assert!(gate.request());
        gate.set_drawable(false);
        assert!(!gate.begin_frame());
        assert_eq!((0..1000).filter(|_| gate.request()).count(), 0);
        gate.set_drawable(true);
        assert!(gate.request());
        assert!(gate.begin_frame());
    }
}
