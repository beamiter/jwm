//! 屏幕录制功能

use crate::core::types::Rect;

const MIN_RECORDING_REGION_SIZE: i32 = 16;
const RESIZE_HANDLE_RADIUS: i32 = 10;
const EDGE_LEFT: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_TOP: u8 = 4;
const EDGE_BOTTOM: u8 = 8;

/// What the pointer is doing over an armed recording region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingPointerIntent {
    /// Outside the region — a press starts a new drag.
    New,
    /// Interior — move the region.
    Move,
    /// Near an edge / corner — resize. Bits use the EDGE_* flags.
    Resize(u8),
}

impl RecordingPointerIntent {
    /// Cursor that advertises this intent.
    #[must_use]
    pub fn cursor(self) -> crate::backend::common_define::StdCursorKind {
        use crate::backend::common_define::StdCursorKind;
        match self {
            Self::New => StdCursorKind::Crosshair,
            Self::Move => StdCursorKind::Fleur,
            Self::Resize(edges) => {
                let left = edges & EDGE_LEFT != 0;
                let right = edges & EDGE_RIGHT != 0;
                let top = edges & EDGE_TOP != 0;
                let bottom = edges & EDGE_BOTTOM != 0;
                match (left, right, top, bottom) {
                    (true, false, true, false) => StdCursorKind::TopLeftCorner,
                    (false, true, true, false) => StdCursorKind::TopRightCorner,
                    (true, false, false, true) => StdCursorKind::BottomLeftCorner,
                    (false, true, false, true) => StdCursorKind::BottomRightCorner,
                    (true, false, false, false) | (false, true, false, false) => {
                        StdCursorKind::HDoubleArrow
                    }
                    (false, false, true, false) | (false, false, false, true) => {
                        StdCursorKind::VDoubleArrow
                    }
                    _ => StdCursorKind::Sizing,
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum RecordingRegionDrag {
    #[default]
    None,
    New {
        anchor_x: i32,
        anchor_y: i32,
        previous_region: Option<Rect>,
    },
    Move {
        pointer_x: i32,
        pointer_y: i32,
        initial: Rect,
    },
    Resize {
        edges: u8,
        pointer_x: i32,
        pointer_y: i32,
        initial: Rect,
    },
}

/// The on-disk identity a rejected recording probe is remembered by. A write,
/// a truncate or a replace-by-rename moves at least one of these, so an
/// unchanged identity means ffprobe would read the same bytes and fail again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordingFileIdentity {
    path: String,
    len: u64,
    modified: Option<std::time::SystemTime>,
    inode: u64,
}

impl RecordingFileIdentity {
    /// The identity of a non-empty file at `path`; `None` for a missing or
    /// empty one, which is never worth a probe.
    pub(crate) fn of(path: &str) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(path).ok()?;
        (metadata.len() > 0).then(|| Self {
            path: path.to_owned(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            inode: metadata.ino(),
        })
    }

    /// Without an mtime an in-place rewrite of the same length is invisible,
    /// so such an identity cannot vouch that the bytes are unchanged.
    pub(crate) fn has_modified_time(&self) -> bool {
        self.modified.is_some()
    }
}

/// 录制状态
#[derive(Debug, Default, Clone)]
pub struct RecordingState {
    /// 录制是否激活
    pub active: bool,
    /// 最终输出文件路径
    pub output_path: Option<String>,
    /// 已完成的分段文件路径
    pub segments: Vec<String>,
    /// 当前正在录制的分段
    pub current_segment: Option<String>,
    /// Whether the final output has passed ffprobe validation.
    pub finalized: bool,
    /// Prevent duplicate `recording/finalized` events while polling status.
    pub finalization_reported: bool,
    /// The finished file the ffprobe check last rejected. The probe runs on
    /// the event thread for `get_recording_status`, so a file left without
    /// its moov atom must not fork it again on every poll; it is probed again
    /// only once its identity changes. Per recorder state rather than per
    /// thread, and cleared when a recording starts.
    pub(crate) rejected_probe: Option<RecordingFileIdentity>,
    /// Current source rectangle in root-compositor coordinates.
    pub region: Option<Rect>,
    /// Fixed encoded video dimensions chosen when recording starts.
    pub output_size: Option<(u32, u32)>,
    /// Interactive region selection/adjustment currently owns input.
    pub selecting_region: bool,
    /// The selection is adjusting an active recording rather than creating one.
    pub adjusting_region: bool,
    /// Output path held while the initial interactive selection is in progress.
    pub pending_output_path: Option<String>,
    /// Last start/stop failure message for `get_recording_status`; cleared on
    /// a successful start. Mirrors audio recording's `last_error`.
    pub last_error: Option<String>,
    /// Region restored when an active adjustment is cancelled.
    original_region: Option<Rect>,
    /// Last valid crop actually sent during the current adjustment.
    last_applied_region: Option<Rect>,
    drag: RecordingRegionDrag,
}

impl RecordingState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开始录制
    pub fn start(&mut self, output_path: String) {
        self.active = true;
        self.output_path = Some(output_path);
        self.segments.clear();
        self.current_segment = None;
        self.finalized = false;
        self.finalization_reported = false;
        self.rejected_probe = None;
        self.region = None;
        self.output_size = None;
        self.selecting_region = false;
        self.adjusting_region = false;
        self.pending_output_path = None;
        self.last_error = None;
        self.original_region = None;
        self.last_applied_region = None;
        self.drag = RecordingRegionDrag::None;
    }

    /// Remember a start/stop failure for status queries.
    pub fn note_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    /// 停止录制
    pub fn stop(&mut self) {
        if let Some(segment) = self.current_segment.take() {
            self.segments.push(segment);
        }
        self.active = false;
        self.selecting_region = false;
        self.adjusting_region = false;
        self.pending_output_path = None;
        self.original_region = None;
        self.last_applied_region = None;
        self.drag = RecordingRegionDrag::None;
    }

    /// 开始新的分段
    pub fn start_segment(&mut self, segment_path: String) {
        // 保存当前分段（如果有）
        if let Some(current) = self.current_segment.replace(segment_path) {
            self.segments.push(current);
        }
    }

    /// 完成当前分段
    pub fn finish_current_segment(&mut self) {
        if let Some(segment) = self.current_segment.take() {
            self.segments.push(segment);
        }
    }

    /// 获取所有分段（包括当前）
    pub fn get_all_segments(&self) -> Vec<String> {
        let mut all = self.segments.clone();
        if let Some(ref current) = self.current_segment {
            all.push(current.clone());
        }
        all
    }

    /// 获取总分段数
    pub fn segment_count(&self) -> usize {
        self.segments.len() + if self.current_segment.is_some() { 1 } else { 0 }
    }

    /// 清除所有数据
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// 取消录制（清除但不保存）
    pub fn cancel(&mut self) {
        self.clear();
    }

    /// 是否有分段数据
    pub fn has_segments(&self) -> bool {
        !self.segments.is_empty() || self.current_segment.is_some()
    }

    /// 获取输出路径
    pub fn get_output_path(&self) -> Option<&str> {
        self.output_path.as_deref()
    }

    pub fn begin_initial_region_selection(&mut self, output_path: String) {
        self.selecting_region = true;
        self.adjusting_region = false;
        self.pending_output_path = Some(output_path);
        self.original_region = None;
        self.last_applied_region = None;
        self.region = None;
        self.output_size = None;
        self.drag = RecordingRegionDrag::None;
    }

    pub fn begin_region_adjustment(&mut self) -> bool {
        if !self.active || self.region.is_none() || self.selecting_region {
            return false;
        }
        self.selecting_region = true;
        self.adjusting_region = true;
        self.original_region = self.region;
        self.last_applied_region = self.region;
        self.drag = RecordingRegionDrag::None;
        true
    }

    pub fn cancel_region_selection(&mut self) -> Option<Rect> {
        if self.adjusting_region {
            self.region = self.original_region;
        } else {
            self.region = None;
            self.output_size = None;
            self.pending_output_path = None;
        }
        self.selecting_region = false;
        self.adjusting_region = false;
        self.original_region = None;
        self.last_applied_region = None;
        self.drag = RecordingRegionDrag::None;
        self.region
    }

    pub fn finish_region_selection(&mut self) {
        self.selecting_region = false;
        self.adjusting_region = false;
        self.original_region = None;
        self.last_applied_region = None;
        self.drag = RecordingRegionDrag::None;
    }

    /// True while a region create / move / resize drag owns the pointer.
    #[must_use]
    pub fn is_region_dragging(&self) -> bool {
        !matches!(self.drag, RecordingRegionDrag::None)
    }

    /// Probe what a press at `(pointer_x, pointer_y)` would do to the armed
    /// region. Soft-probe (no region) always returns [`RecordingPointerIntent::New`].
    #[must_use]
    pub fn pointer_intent(&self, pointer_x: i32, pointer_y: i32) -> RecordingPointerIntent {
        let Some(region) = self.region else {
            return RecordingPointerIntent::New;
        };

        if region.w <= 0 || region.h <= 0 {
            return RecordingPointerIntent::New;
        }
        // Leave an interior for moving/confirming even on the minimum 16px
        // crop. Fixed 10px bands overlapped there and selected opposite edges
        // at once. Fit each axis independently for long, narrow selections.
        let radius_x = i64::from((region.w / 4).min(RESIZE_HANDLE_RADIUS));
        let radius_y = i64::from((region.h / 4).min(RESIZE_HANDLE_RADIUS));
        let left = i64::from(region.x);
        let top = i64::from(region.y);
        let right = left + i64::from(region.w);
        let bottom = top + i64::from(region.h);
        let pointer_x = i64::from(pointer_x);
        let pointer_y = i64::from(pointer_y);
        let within_horizontal = pointer_x >= left - radius_x && pointer_x <= right + radius_x;
        let within_vertical = pointer_y >= top - radius_y && pointer_y <= bottom + radius_y;
        let mut edges = 0;
        if within_vertical && (pointer_x - left).abs() <= radius_x {
            edges |= EDGE_LEFT;
        }
        if within_vertical && (pointer_x - right).abs() <= radius_x {
            edges |= EDGE_RIGHT;
        }
        if within_horizontal && (pointer_y - top).abs() <= radius_y {
            edges |= EDGE_TOP;
        }
        if within_horizontal && (pointer_y - bottom).abs() <= radius_y {
            edges |= EDGE_BOTTOM;
        }

        if edges != 0 {
            RecordingPointerIntent::Resize(edges)
        } else if pointer_x >= left && pointer_x <= right && pointer_y >= top && pointer_y <= bottom
        {
            RecordingPointerIntent::Move
        } else {
            RecordingPointerIntent::New
        }
    }

    pub fn begin_region_drag(&mut self, pointer_x: i32, pointer_y: i32) {
        if !self.selecting_region {
            return;
        }
        let Some(region) = self.region else {
            self.drag = RecordingRegionDrag::New {
                anchor_x: pointer_x,
                anchor_y: pointer_y,
                previous_region: self.last_applied_region,
            };
            return;
        };

        self.drag = match self.pointer_intent(pointer_x, pointer_y) {
            RecordingPointerIntent::Resize(edges) => RecordingRegionDrag::Resize {
                edges,
                pointer_x,
                pointer_y,
                initial: region,
            },
            RecordingPointerIntent::Move => RecordingRegionDrag::Move {
                pointer_x,
                pointer_y,
                initial: region,
            },
            RecordingPointerIntent::New => RecordingRegionDrag::New {
                anchor_x: pointer_x,
                anchor_y: pointer_y,
                previous_region: self.last_applied_region,
            },
        };
    }

    pub fn update_region_drag(
        &mut self,
        pointer_x: i32,
        pointer_y: i32,
        screen_width: i32,
        screen_height: i32,
    ) -> Option<Rect> {
        let screen_width = screen_width.max(MIN_RECORDING_REGION_SIZE);
        let screen_height = screen_height.max(MIN_RECORDING_REGION_SIZE);
        // A monitor can shrink or disappear while this interaction is armed.
        // Refit its saved rectangle before calculating edge clamp bounds.
        let fit_initial = |initial: Rect| {
            let width = initial.w.clamp(MIN_RECORDING_REGION_SIZE, screen_width);
            let height = initial.h.clamp(MIN_RECORDING_REGION_SIZE, screen_height);
            Rect::new(
                initial.x.clamp(0, screen_width - width),
                initial.y.clamp(0, screen_height - height),
                width,
                height,
            )
        };
        let updated = match self.drag {
            RecordingRegionDrag::None => return self.region,
            RecordingRegionDrag::New {
                anchor_x, anchor_y, ..
            } => {
                let x1 = anchor_x.clamp(0, screen_width);
                let y1 = anchor_y.clamp(0, screen_height);
                let x2 = pointer_x.clamp(0, screen_width);
                let y2 = pointer_y.clamp(0, screen_height);
                Rect::new(x1.min(x2), y1.min(y2), (x1 - x2).abs(), (y1 - y2).abs())
            }
            RecordingRegionDrag::Move {
                pointer_x: start_x,
                pointer_y: start_y,
                initial,
            } => {
                let initial = fit_initial(initial);
                let max_x = (screen_width - initial.w).max(0);
                let max_y = (screen_height - initial.h).max(0);
                Rect::new(
                    initial
                        .x
                        .saturating_add(pointer_x.saturating_sub(start_x))
                        .clamp(0, max_x),
                    initial
                        .y
                        .saturating_add(pointer_y.saturating_sub(start_y))
                        .clamp(0, max_y),
                    initial.w.min(screen_width),
                    initial.h.min(screen_height),
                )
            }
            RecordingRegionDrag::Resize {
                edges,
                pointer_x: start_x,
                pointer_y: start_y,
                initial,
            } => {
                let initial = fit_initial(initial);
                // The press may be anywhere in a handle's hit band. Resize
                // by its displacement so a plain click never snaps the edge
                // to the pointer, and keep the same anchor after clamping.
                let dx = pointer_x.saturating_sub(start_x);
                let dy = pointer_y.saturating_sub(start_y);
                let mut left = initial.x;
                let mut top = initial.y;
                let mut right = initial.x + initial.w;
                let mut bottom = initial.y + initial.h;
                if edges & EDGE_LEFT != 0 {
                    left = left
                        .saturating_add(dx)
                        .clamp(0, right - MIN_RECORDING_REGION_SIZE);
                }
                if edges & EDGE_RIGHT != 0 {
                    right = right
                        .saturating_add(dx)
                        .clamp(left + MIN_RECORDING_REGION_SIZE, screen_width);
                }
                if edges & EDGE_TOP != 0 {
                    top = top
                        .saturating_add(dy)
                        .clamp(0, bottom - MIN_RECORDING_REGION_SIZE);
                }
                if edges & EDGE_BOTTOM != 0 {
                    bottom = bottom
                        .saturating_add(dy)
                        .clamp(top + MIN_RECORDING_REGION_SIZE, screen_height);
                }
                Rect::new(left, top, right - left, bottom - top)
            }
        };
        self.region = Some(updated);
        self.region
    }

    pub fn end_region_drag(&mut self) {
        if self
            .region
            .is_some_and(|region| !Self::valid_source_region(region))
        {
            self.region = match self.drag {
                RecordingRegionDrag::New {
                    previous_region, ..
                } if self.adjusting_region => previous_region,
                _ => None,
            };
        }
        self.drag = RecordingRegionDrag::None;
    }

    pub(crate) fn valid_source_region(region: Rect) -> bool {
        region.w >= MIN_RECORDING_REGION_SIZE && region.h >= MIN_RECORDING_REGION_SIZE
    }

    pub(crate) fn note_applied_region(&mut self, region: Rect) {
        if self.adjusting_region && Self::valid_source_region(region) {
            self.last_applied_region = Some(region);
        }
    }

    pub(crate) fn restore_applied_region(&mut self) {
        if self.adjusting_region && self.region.is_none() {
            self.region = self.last_applied_region;
        }
    }

    pub fn set_region(&mut self, region: Rect) {
        self.region = Some(region);
    }

    pub fn set_output_size_from_region(&mut self) {
        self.output_size = self.region.and_then(|region| {
            Some((u32::try_from(region.w).ok()?, u32::try_from(region.h).ok()?))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_resize_remains_valid_after_screen_shrinks() {
        for (press_x, press_y, move_x, move_y) in [
            (1900, 250, 1000, 250),
            (1500, 250, 1000, 250),
            (1700, 100, 1000, 200),
            (1700, 400, 1000, 700),
            (1900, 400, 1000, 700),
        ] {
            let mut state = RecordingState::new();
            state.start("/tmp/synthetic.mp4".into());
            state.set_region(Rect::new(1500, 100, 400, 300));
            assert!(state.begin_region_adjustment());
            state.begin_region_drag(press_x, press_y);
            let region = state.update_region_drag(move_x, move_y, 1280, 720).unwrap();
            assert!(region.x >= 0 && region.y >= 0);
            assert!(region.w >= MIN_RECORDING_REGION_SIZE && region.h >= MIN_RECORDING_REGION_SIZE);
            assert!(region.x + region.w <= 1280 && region.y + region.h <= 720);
        }
    }

    #[test]
    fn test_recording_lifecycle() {
        let mut state = RecordingState::new();
        assert!(!state.active);

        // 开始录制
        state.start("/tmp/output.mp4".to_string());
        assert!(state.active);
        assert_eq!(state.get_output_path(), Some("/tmp/output.mp4"));

        // 添加分段
        state.start_segment("/tmp/segment1.mp4".to_string());
        assert_eq!(state.segment_count(), 1);

        state.start_segment("/tmp/segment2.mp4".to_string());
        assert_eq!(state.segment_count(), 2);
        assert_eq!(state.segments.len(), 1);

        // 停止录制
        state.stop();
        assert!(!state.active);
        assert_eq!(state.segment_count(), 2);
        assert_eq!(state.segments.len(), 2);
    }

    #[test]
    fn test_get_all_segments() {
        let mut state = RecordingState::new();
        state.start("/tmp/output.mp4".to_string());

        state.start_segment("/tmp/seg1.mp4".to_string());
        state.start_segment("/tmp/seg2.mp4".to_string());
        state.start_segment("/tmp/seg3.mp4".to_string());

        let all = state.get_all_segments();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], "/tmp/seg1.mp4");
        assert_eq!(all[1], "/tmp/seg2.mp4");
        assert_eq!(all[2], "/tmp/seg3.mp4");
    }

    #[test]
    fn test_cancel() {
        let mut state = RecordingState::new();
        state.start("/tmp/output.mp4".to_string());
        state.start_segment("/tmp/seg1.mp4".to_string());

        state.cancel();
        assert!(!state.active);
        assert!(!state.has_segments());
        assert!(state.get_output_path().is_none());
    }

    #[test]
    fn test_finish_current_segment() {
        let mut state = RecordingState::new();
        state.start("/tmp/output.mp4".to_string());
        state.start_segment("/tmp/seg1.mp4".to_string());

        assert_eq!(state.segments.len(), 0);
        assert!(state.current_segment.is_some());

        state.finish_current_segment();
        assert_eq!(state.segments.len(), 1);
        assert!(state.current_segment.is_none());
    }

    #[test]
    fn new_recording_resets_finalization_flags() {
        let mut state = RecordingState::new();
        state.finalized = true;
        state.finalization_reported = true;
        state.rejected_probe = Some(RecordingFileIdentity {
            path: "/tmp/old.mp4".to_string(),
            len: 16,
            modified: None,
            inode: 1,
        });
        state.start("/tmp/new.mp4".to_string());
        assert!(!state.finalized);
        assert!(!state.finalization_reported);
        assert!(
            state.rejected_probe.is_none(),
            "a new recording forgets the last rejected file"
        );
    }

    #[test]
    fn direct_output_can_be_the_active_segment() {
        let mut state = RecordingState::new();
        let output = "/home/test/Videos/recording.mp4";
        state.start(output.to_string());
        state.start_segment(output.to_string());

        assert_eq!(state.current_segment.as_deref(), Some(output));
        state.stop();
        assert_eq!(state.segments, vec![output.to_string()]);
    }

    #[test]
    fn recording_region_can_move_and_resize_during_adjustment() {
        let mut state = RecordingState::new();
        state.start("/tmp/output.mp4".to_string());
        state.set_region(Rect::new(100, 100, 640, 360));
        assert!(state.begin_region_adjustment());

        state.begin_region_drag(200, 200);
        assert_eq!(
            state.update_region_drag(250, 230, 1920, 1080),
            Some(Rect::new(150, 130, 640, 360))
        );
        state.end_region_drag();

        state.begin_region_drag(790, 490);
        assert_eq!(
            state.update_region_drag(900, 600, 1920, 1080),
            Some(Rect::new(150, 130, 750, 470))
        );
    }

    #[test]
    fn cancelling_adjustment_restores_original_region() {
        let mut state = RecordingState::new();
        state.start("/tmp/output.mp4".to_string());
        let original = Rect::new(20, 30, 800, 450);
        state.set_region(original);
        assert!(state.begin_region_adjustment());
        state.begin_region_drag(100, 100);
        state.update_region_drag(200, 200, 1920, 1080);

        assert_eq!(state.cancel_region_selection(), Some(original));
        assert!(!state.selecting_region);
    }

    #[test]
    fn starting_a_new_recording_drops_stale_capture_geometry() {
        let mut state = RecordingState::new();
        state.region = Some(Rect::new(10, 20, 640, 360));
        state.output_size = Some((640, 360));

        state.start("/tmp/new-output.mp4".to_string());

        assert_eq!(state.region, None);
        assert_eq!(state.output_size, None);
    }

    #[test]
    fn pointer_intent_classifies_edges_interior_and_outside() {
        let mut state = RecordingState::new();
        state.selecting_region = true;
        state.region = Some(Rect::new(100, 100, 200, 150));

        assert_eq!(
            state.pointer_intent(100, 100),
            RecordingPointerIntent::Resize(EDGE_LEFT | EDGE_TOP)
        );
        assert_eq!(state.pointer_intent(200, 175), RecordingPointerIntent::Move);
        assert_eq!(state.pointer_intent(10, 10), RecordingPointerIntent::New);
        assert_eq!(
            RecordingPointerIntent::Move.cursor(),
            crate::backend::common_define::StdCursorKind::Fleur
        );
    }

    #[test]
    fn minimum_and_narrow_crops_keep_a_movable_confirmable_interior() {
        for (width, height) in [(16, 16), (16, 200), (200, 16), (20, 20), (40, 40)] {
            let mut state = RecordingState::new();
            state.selecting_region = true;
            let original = Rect::new(100, 100, width, height);
            state.set_region(original);
            let (cx, cy) = (100 + width / 2, 100 + height / 2);
            // The double-click confirmation gate uses this same Move intent.
            assert_eq!(state.pointer_intent(cx, cy), RecordingPointerIntent::Move);
            state.begin_region_drag(cx, cy);
            assert_eq!(
                state.update_region_drag(cx + 15, cy + 12, 1000, 1000),
                Some(Rect::new(115, 112, width, height))
            );
        }
    }

    #[test]
    fn resize_hit_bands_never_choose_opposite_edges() {
        for (width, height) in [(16, 16), (16, 200), (200, 16), (200, 150)] {
            let mut state = RecordingState::new();
            state.set_region(Rect::new(100, 100, width, height));
            for x in 89..=111 + width {
                for y in 89..=111 + height {
                    if let RecordingPointerIntent::Resize(edges) = state.pointer_intent(x, y) {
                        assert_ne!(edges & (EDGE_LEFT | EDGE_RIGHT), EDGE_LEFT | EDGE_RIGHT);
                        assert_ne!(edges & (EDGE_TOP | EDGE_BOTTOM), EDGE_TOP | EDGE_BOTTOM);
                    }
                }
            }
            for (x, y, edges) in [
                (100, 100, EDGE_LEFT | EDGE_TOP),
                (100 + width, 100, EDGE_RIGHT | EDGE_TOP),
                (100, 100 + height, EDGE_LEFT | EDGE_BOTTOM),
                (100 + width, 100 + height, EDGE_RIGHT | EDGE_BOTTOM),
            ] {
                assert_eq!(
                    state.pointer_intent(x, y),
                    RecordingPointerIntent::Resize(edges)
                );
            }
        }
    }

    #[test]
    fn resize_handle_click_without_motion_preserves_the_crop() {
        let original = Rect::new(100, 100, 200, 150);
        for (x, y) in [
            (95, 175),
            (105, 175),
            (295, 175),
            (305, 175),
            (200, 95),
            (200, 105),
            (200, 245),
            (200, 255),
            (95, 105),
            (305, 245),
        ] {
            let mut state = RecordingState::new();
            state.selecting_region = true;
            state.set_region(original);
            assert!(matches!(
                state.pointer_intent(x, y),
                RecordingPointerIntent::Resize(_)
            ));
            state.begin_region_drag(x, y);
            assert_eq!(state.update_region_drag(x, y, 1920, 1080), Some(original));
            state.end_region_drag();
            assert_eq!(state.region, Some(original));
        }
    }

    #[test]
    fn resize_preserves_grab_offset_after_minimum_and_output_clamps() {
        let original = Rect::new(100, 100, 200, 150);
        let mut state = RecordingState::new();
        state.selecting_region = true;
        state.set_region(original);
        state.begin_region_drag(295, 245);
        assert_eq!(
            state.update_region_drag(-100, -100, 1920, 1080),
            Some(Rect::new(100, 100, 16, 16))
        );
        assert_eq!(
            state.update_region_drag(295, 245, 1920, 1080),
            Some(original)
        );
        assert_eq!(
            state.update_region_drag(310, 265, 1920, 1080),
            Some(Rect::new(100, 100, 215, 170))
        );
        state.end_region_drag();

        state.set_region(original);
        state.begin_region_drag(95, 105);
        assert_eq!(
            state.update_region_drag(-200, -200, 1920, 1080),
            Some(Rect::new(0, 0, 300, 250))
        );
        assert_eq!(
            state.update_region_drag(95, 105, 1920, 1080),
            Some(original)
        );
        assert_eq!(
            state.update_region_drag(105, 115, 1920, 1080),
            Some(Rect::new(110, 110, 190, 140))
        );
    }

    #[test]
    fn resize_hit_testing_handles_negative_and_extreme_coordinates() {
        let mut state = RecordingState::new();
        state.set_region(Rect::new(-200, -100, 100, 80));
        assert_eq!(
            state.pointer_intent(-150, -60),
            RecordingPointerIntent::Move
        );
        assert_eq!(
            state.pointer_intent(-200, -100),
            RecordingPointerIntent::Resize(EDGE_LEFT | EDGE_TOP)
        );
        state.set_region(Rect::new(i32::MAX - 8, i32::MAX - 8, 16, 16));
        assert_eq!(
            state.pointer_intent(i32::MAX, i32::MAX),
            RecordingPointerIntent::Move
        );
        assert_eq!(
            state.pointer_intent(i32::MIN, i32::MIN),
            RecordingPointerIntent::New
        );
    }
}
