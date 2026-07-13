//! 屏幕录制功能

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
    }

    /// 停止录制
    pub fn stop(&mut self) {
        if let Some(segment) = self.current_segment.take() {
            self.segments.push(segment);
        }
        self.active = false;
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
}
