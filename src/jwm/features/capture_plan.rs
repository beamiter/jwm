//! 截图完成决策与执行（Phase 2 服务抽取）。
//!
//! 交互式截图结束时"做什么"的决策在这里以纯函数表达：不触碰 `Jwm`
//! 状态，也不依赖完整 `Backend` 接口。执行捕获只需要窄能力
//! `CompositorMedia`，因此测试可以用几行的小型伪造实现，而不是
//! mock 整个平台接口。

use crate::backend::api::CompositorMedia;
use crate::backend::error::BackendError;
use crate::core::types::Rect;

/// 交互式截图选区在两个方向上的最小尺寸（像素）。
pub const MIN_SCREENSHOT_SIZE: i32 = 3;

/// 截图完成时的决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureCompletion {
    /// 没有输出路径或选区：按取消处理（保留动画式预览清除）。
    Cancel,
    /// 选区太小，退出选择模式但不捕获。
    TooSmall { width: u32, height: u32 },
    /// 执行捕获。
    Capture(CapturePlan),
}

/// 一次待执行捕获的完整描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePlan {
    /// 捕获落盘路径；剪贴板模式下是私有暂存文件。
    pub save_path: String,
    /// 捕获区域 (x, y, w, h)。
    pub region: (i32, i32, u32, u32),
    /// 捕获后复制到剪贴板（并删除暂存文件）。
    pub to_clipboard: bool,
    /// 捕获后把屏幕标注烘焙进 PNG。
    pub bake_annotations: bool,
}

/// 剪贴板模式使用的私有暂存文件路径。
#[must_use]
pub fn clipboard_staging_path(pid: u32, stamp_micros: u128) -> String {
    format!("/tmp/.jwm-screenshot-clipboard-{pid}-{stamp_micros}.png")
}

/// 根据选择状态决定截图完成动作。
///
/// 纯函数：调用方提供输出路径、选区与标注数量，剪贴板暂存路径通过
/// `staging_path` 惰性计算，便于测试注入固定值。
pub fn plan_capture_completion(
    output_path: Option<String>,
    selection: Option<Rect>,
    annotation_count: usize,
    to_clipboard: bool,
    staging_path: impl FnOnce() -> String,
) -> CaptureCompletion {
    let Some(output_path) = output_path else {
        return CaptureCompletion::Cancel;
    };
    let Some(rect) = selection else {
        return CaptureCompletion::Cancel;
    };

    // 负尺寸按 0 处理，落入 TooSmall；合法选区不受影响。
    let width = u32::try_from(rect.w).unwrap_or(0);
    let height = u32::try_from(rect.h).unwrap_or(0);
    let minimum = u32::try_from(MIN_SCREENSHOT_SIZE).unwrap_or(u32::MAX);
    if width < minimum || height < minimum {
        return CaptureCompletion::TooSmall { width, height };
    }

    let save_path = if to_clipboard {
        staging_path()
    } else {
        output_path
    };
    CaptureCompletion::Capture(CapturePlan {
        save_path,
        region: (rect.x, rect.y, width, height),
        to_clipboard,
        bake_annotations: annotation_count > 0,
    })
}

/// 捕获执行结果；日志由调用方按既有格式输出。
#[derive(Debug)]
pub enum CaptureExecution {
    /// 区域捕获成功。
    CapturedRegion,
    /// 后端不支持区域捕获，回退全屏捕获成功。
    CapturedFullFallback,
    /// 区域捕获不受支持，全屏回退也失败或不受支持。
    Unavailable,
    /// 区域捕获返回错误。
    Failed(BackendError),
}

impl CaptureExecution {
    /// 是否产生了捕获文件（后续剪贴板 / 标注烘焙的前提）。
    #[must_use]
    pub fn captured(&self) -> bool {
        matches!(self, Self::CapturedRegion | Self::CapturedFullFallback)
    }
}

/// 依计划执行捕获：区域优先，后端不支持时回退全屏。
///
/// 只依赖 `CompositorMedia` 窄能力；完整 `Backend` 通过 trait 向上转型
/// 传入，测试传入小型伪造实现。
pub fn execute_capture_plan(
    media: &mut dyn CompositorMedia,
    plan: &CapturePlan,
) -> CaptureExecution {
    let path = std::path::PathBuf::from(&plan.save_path);
    let (x, y, width, height) = plan.region;
    match media.take_screenshot_region_to_file(&path, x, y, width, height) {
        Ok(true) => CaptureExecution::CapturedRegion,
        Ok(false) => {
            if media.take_screenshot_to_file(&path).unwrap_or(false) {
                CaptureExecution::CapturedFullFallback
            } else {
                CaptureExecution::Unavailable
            }
        }
        Err(error) => CaptureExecution::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staging() -> String {
        "/tmp/.staging.png".to_string()
    }

    #[test]
    fn missing_path_or_selection_cancels() {
        assert_eq!(
            plan_capture_completion(None, Some(Rect::new(0, 0, 100, 100)), 0, false, staging),
            CaptureCompletion::Cancel
        );
        assert_eq!(
            plan_capture_completion(Some("/tmp/shot.png".into()), None, 0, false, staging),
            CaptureCompletion::Cancel
        );
    }

    #[test]
    fn selections_below_the_minimum_are_ignored_not_captured() {
        let plan = plan_capture_completion(
            Some("/tmp/shot.png".into()),
            Some(Rect::new(10, 10, 2, 50)),
            0,
            false,
            staging,
        );
        assert_eq!(
            plan,
            CaptureCompletion::TooSmall {
                width: 2,
                height: 50
            }
        );
    }

    #[test]
    fn file_capture_uses_the_output_path_and_bakes_only_with_annotations() {
        let CaptureCompletion::Capture(plan) = plan_capture_completion(
            Some("/tmp/shot.png".into()),
            Some(Rect::new(5, 6, 100, 50)),
            0,
            false,
            staging,
        ) else {
            panic!("expected a capture plan");
        };
        assert_eq!(plan.save_path, "/tmp/shot.png");
        assert_eq!(plan.region, (5, 6, 100, 50));
        assert!(!plan.to_clipboard);
        assert!(!plan.bake_annotations);

        let CaptureCompletion::Capture(plan) = plan_capture_completion(
            Some("/tmp/shot.png".into()),
            Some(Rect::new(5, 6, 100, 50)),
            2,
            false,
            staging,
        ) else {
            panic!("expected a capture plan");
        };
        assert!(plan.bake_annotations);
    }

    #[test]
    fn clipboard_capture_stages_into_a_private_temp_file() {
        let CaptureCompletion::Capture(plan) = plan_capture_completion(
            Some("/tmp/shot.png".into()),
            Some(Rect::new(0, 0, 64, 64)),
            0,
            true,
            staging,
        ) else {
            panic!("expected a capture plan");
        };
        assert_eq!(plan.save_path, "/tmp/.staging.png");
        assert!(plan.to_clipboard);

        assert_eq!(
            clipboard_staging_path(42, 7),
            "/tmp/.jwm-screenshot-clipboard-42-7.png"
        );
    }

    /// Phase 2 退出标准演示：策略测试只需要一个几行的伪造
    /// `CompositorMedia`，不必 mock 完整 `Backend` 接口。
    #[derive(Default)]
    struct FakeMedia {
        region_result: Option<Result<bool, ()>>,
        full_result: bool,
        region_calls: Vec<(i32, i32, u32, u32)>,
        full_calls: usize,
    }

    impl CompositorMedia for FakeMedia {
        fn take_screenshot_region_to_file(
            &mut self,
            _path: &std::path::Path,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
        ) -> Result<bool, BackendError> {
            self.region_calls.push((x, y, width, height));
            match self.region_result.take().unwrap_or(Ok(true)) {
                Ok(supported) => Ok(supported),
                Err(()) => Err(BackendError::Message("capture failed".into())),
            }
        }

        fn take_screenshot_to_file(
            &mut self,
            _path: &std::path::Path,
        ) -> Result<bool, BackendError> {
            self.full_calls += 1;
            Ok(self.full_result)
        }
    }

    fn plan() -> CapturePlan {
        CapturePlan {
            save_path: "/tmp/shot.png".into(),
            region: (1, 2, 30, 40),
            to_clipboard: false,
            bake_annotations: false,
        }
    }

    #[test]
    fn execution_prefers_region_capture() {
        let mut media = FakeMedia {
            region_result: Some(Ok(true)),
            ..Default::default()
        };
        let outcome = execute_capture_plan(&mut media, &plan());
        assert!(matches!(outcome, CaptureExecution::CapturedRegion));
        assert!(outcome.captured());
        assert_eq!(media.region_calls, vec![(1, 2, 30, 40)]);
        assert_eq!(media.full_calls, 0);
    }

    #[test]
    fn execution_falls_back_to_full_capture_when_region_is_unsupported() {
        let mut media = FakeMedia {
            region_result: Some(Ok(false)),
            full_result: true,
            ..Default::default()
        };
        assert!(execute_capture_plan(&mut media, &plan()).captured());
        assert_eq!(media.full_calls, 1);

        let mut media = FakeMedia {
            region_result: Some(Ok(false)),
            full_result: false,
            ..Default::default()
        };
        let outcome = execute_capture_plan(&mut media, &plan());
        assert!(matches!(outcome, CaptureExecution::Unavailable));
        assert!(!outcome.captured());
    }

    #[test]
    fn execution_reports_region_errors_without_a_fallback_attempt() {
        let mut media = FakeMedia {
            region_result: Some(Err(())),
            ..Default::default()
        };
        let outcome = execute_capture_plan(&mut media, &plan());
        assert!(matches!(outcome, CaptureExecution::Failed(_)));
        assert_eq!(media.full_calls, 0);
    }
}
