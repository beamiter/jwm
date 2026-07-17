//! 屏幕录制功能

use crate::core::types::Rect;

const MIN_RECORDING_REGION_SIZE: i32 = 16;
const RESIZE_HANDLE_RADIUS: i32 = 10;
const EDGE_LEFT: u8 = 1;
const EDGE_RIGHT: u8 = 2;
const EDGE_TOP: u8 = 4;
const EDGE_BOTTOM: u8 = 8;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum RecordingRegionDrag {
    #[default]
    None,
    New {
        anchor_x: i32,
        anchor_y: i32,
    },
    Move {
        pointer_x: i32,
        pointer_y: i32,
        initial: Rect,
    },
    Resize {
        edges: u8,
        initial: Rect,
    },
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
    /// Region restored when an active adjustment is cancelled.
    original_region: Option<Rect>,
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
        self.region = None;
        self.output_size = None;
        self.selecting_region = false;
        self.adjusting_region = false;
        self.pending_output_path = None;
        self.original_region = None;
        self.drag = RecordingRegionDrag::None;
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
        self.drag = RecordingRegionDrag::None;
        self.region
    }

    pub fn finish_region_selection(&mut self) {
        self.selecting_region = false;
        self.adjusting_region = false;
        self.original_region = None;
        self.drag = RecordingRegionDrag::None;
    }

    pub fn begin_region_drag(&mut self, pointer_x: i32, pointer_y: i32) {
        if !self.selecting_region {
            return;
        }
        let Some(region) = self.region else {
            self.drag = RecordingRegionDrag::New {
                anchor_x: pointer_x,
                anchor_y: pointer_y,
            };
            return;
        };

        let right = region.x + region.w;
        let bottom = region.y + region.h;
        let within_horizontal = pointer_x >= region.x - RESIZE_HANDLE_RADIUS
            && pointer_x <= right + RESIZE_HANDLE_RADIUS;
        let within_vertical = pointer_y >= region.y - RESIZE_HANDLE_RADIUS
            && pointer_y <= bottom + RESIZE_HANDLE_RADIUS;
        let mut edges = 0;
        if within_vertical && (pointer_x - region.x).abs() <= RESIZE_HANDLE_RADIUS {
            edges |= EDGE_LEFT;
        }
        if within_vertical && (pointer_x - right).abs() <= RESIZE_HANDLE_RADIUS {
            edges |= EDGE_RIGHT;
        }
        if within_horizontal && (pointer_y - region.y).abs() <= RESIZE_HANDLE_RADIUS {
            edges |= EDGE_TOP;
        }
        if within_horizontal && (pointer_y - bottom).abs() <= RESIZE_HANDLE_RADIUS {
            edges |= EDGE_BOTTOM;
        }

        self.drag = if edges != 0 {
            RecordingRegionDrag::Resize {
                edges,
                initial: region,
            }
        } else if pointer_x >= region.x
            && pointer_x <= right
            && pointer_y >= region.y
            && pointer_y <= bottom
        {
            RecordingRegionDrag::Move {
                pointer_x,
                pointer_y,
                initial: region,
            }
        } else {
            RecordingRegionDrag::New {
                anchor_x: pointer_x,
                anchor_y: pointer_y,
            }
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
        let updated = match self.drag {
            RecordingRegionDrag::None => return self.region,
            RecordingRegionDrag::New { anchor_x, anchor_y } => {
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
                let max_x = (screen_width - initial.w).max(0);
                let max_y = (screen_height - initial.h).max(0);
                Rect::new(
                    (initial.x + pointer_x - start_x).clamp(0, max_x),
                    (initial.y + pointer_y - start_y).clamp(0, max_y),
                    initial.w.min(screen_width),
                    initial.h.min(screen_height),
                )
            }
            RecordingRegionDrag::Resize { edges, initial } => {
                let mut left = initial.x;
                let mut top = initial.y;
                let mut right = initial.x + initial.w;
                let mut bottom = initial.y + initial.h;
                if edges & EDGE_LEFT != 0 {
                    left = pointer_x.clamp(0, right - MIN_RECORDING_REGION_SIZE);
                }
                if edges & EDGE_RIGHT != 0 {
                    right = pointer_x.clamp(left + MIN_RECORDING_REGION_SIZE, screen_width);
                }
                if edges & EDGE_TOP != 0 {
                    top = pointer_y.clamp(0, bottom - MIN_RECORDING_REGION_SIZE);
                }
                if edges & EDGE_BOTTOM != 0 {
                    bottom = pointer_y.clamp(top + MIN_RECORDING_REGION_SIZE, screen_height);
                }
                Rect::new(left, top, right - left, bottom - top)
            }
        };
        self.region = Some(updated);
        self.region
    }

    pub fn end_region_drag(&mut self) {
        self.drag = RecordingRegionDrag::None;
        if self.region.is_some_and(|region| {
            region.w < MIN_RECORDING_REGION_SIZE || region.h < MIN_RECORDING_REGION_SIZE
        }) {
            self.region = None;
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
        state.start("/tmp/new.mp4".to_string());
        assert!(!state.finalized);
        assert!(!state.finalization_reported);
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
}
