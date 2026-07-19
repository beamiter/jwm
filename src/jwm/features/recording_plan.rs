//! 录制启动 / 收尾决策（Phase 2 服务抽取）。
//!
//! 屏幕录制"能不能录、录到哪、结束后怎么合成"的策略在这里以纯函数
//! 表达：区域归一化（编码器要求的偶数尺寸对齐）、输出路径校验、输出
//! 目录解析链，以及分段收尾计划。`toggles.rs` 与 IPC 处理器只负责把
//! 这些决策执行到平台能力和文件系统上。

use crate::core::types::Rect;
use std::path::{Path, PathBuf};

/// 录制区域在两个方向上的最小尺寸（像素）。
pub const MIN_RECORDING_DIMENSION: i32 = 16;

/// 把初始录制区域归一化到屏幕内。
///
/// 位置夹取到屏幕范围，宽高夹取后向下对齐到偶数（视频编码器要求），
/// 结果仍需满足最小尺寸。纯函数：屏幕尺寸显式传入。
///
/// # Errors
///
/// 屏幕过小或归一化后区域仍小于最小尺寸时返回错误。
pub fn normalize_initial_region(
    region: Rect,
    screen_width: i32,
    screen_height: i32,
) -> Result<Rect, String> {
    if screen_width < MIN_RECORDING_DIMENSION || screen_height < MIN_RECORDING_DIMENSION {
        return Err("screen is too small for region recording".to_string());
    }
    let x = region.x.clamp(0, screen_width - MIN_RECORDING_DIMENSION);
    let y = region.y.clamp(0, screen_height - MIN_RECORDING_DIMENSION);
    let max_width = screen_width - x;
    let max_height = screen_height - y;
    let width = region.w.clamp(MIN_RECORDING_DIMENSION, max_width) & !1;
    let height = region.h.clamp(MIN_RECORDING_DIMENSION, max_height) & !1;
    if width < MIN_RECORDING_DIMENSION || height < MIN_RECORDING_DIMENSION {
        return Err("recording region must be at least 16x16".to_string());
    }
    Ok(Rect::new(x, y, width, height))
}

/// 校验录制输出路径的纯性质：绝对路径且以 .mp4 结尾。
///
/// 文件系统层面的检查（父目录创建、目标已存在）留给调用方。
///
/// # Errors
///
/// 路径不是绝对路径或扩展名不是 `mp4` 时返回错误。
pub fn validate_output_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("recording output path must be absolute".to_string());
    }
    if path.extension().and_then(|value| value.to_str()) != Some("mp4") {
        return Err("recording output path must end in .mp4".to_string());
    }
    Ok(())
}

/// 解析录制输出目录：配置目录优先，然后依次是 `XDG_VIDEOS_DIR`、
/// 系统视频目录、`$HOME/Videos`。纯函数：所有候选都由调用方提供。
///
/// # Errors
///
/// 没有任何候选可用时返回错误，提示设置 `behavior.recording_output_dir`。
pub fn resolve_output_directory(
    configured: &str,
    xdg_videos: Option<PathBuf>,
    system_video_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if !configured.is_empty() {
        return Ok(PathBuf::from(configured));
    }
    xdg_videos
        .filter(|path| !path.as_os_str().is_empty())
        .or(system_video_dir)
        .or_else(|| home.map(|home| home.join("Videos")))
        .ok_or_else(|| {
            "cannot resolve the Videos directory; set behavior.recording_output_dir".to_string()
        })
}

/// 输出文件名（时间戳由调用方生成，便于测试）。
#[must_use]
pub fn output_file_name(timestamp: &str) -> String {
    format!("recording-{timestamp}.mp4")
}

/// 录制结束后的收尾计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizationPlan {
    /// 没有分段：无事可做。
    Nothing,
    /// 单个分段：等待编码器写完（ffprobe 校验）后按需搬移。
    ValidateSingle {
        segment: String,
        /// 分段路径与输出路径不同（旧调用方）时的搬移目标。
        move_to: Option<String>,
    },
    /// 多个分段：用 ffmpeg concat 合并。
    ConcatSegments {
        list_path: PathBuf,
        list_content: String,
        output_path: String,
    },
}

/// 根据分段与输出路径决定收尾动作。纯函数，执行（ffprobe 轮询、
/// ffmpeg 调用、文件搬移）由调用方在线程里完成。
#[must_use]
pub fn plan_finalization(segments: &[String], output_path: &str) -> FinalizationPlan {
    match segments {
        [] => FinalizationPlan::Nothing,
        [segment] => FinalizationPlan::ValidateSingle {
            segment: segment.clone(),
            move_to: (segment != output_path).then(|| output_path.to_string()),
        },
        _ => FinalizationPlan::ConcatSegments {
            list_path: Path::new(output_path).with_extension("concat.txt"),
            list_content: segments
                .iter()
                .map(|segment| format!("file '{segment}'"))
                .collect::<Vec<_>>()
                .join("\n"),
            output_path: output_path.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_clamped_and_aligned_to_even_encoder_dimensions() {
        // 越界位置被夹回屏幕内，奇数尺寸向下对齐到偶数。
        assert_eq!(
            normalize_initial_region(Rect::new(-50, -50, 641, 361), 1920, 1080),
            Ok(Rect::new(0, 0, 640, 360))
        );
        // 溢出屏幕的尺寸收缩到可用空间（再做偶数对齐）。
        assert_eq!(
            normalize_initial_region(Rect::new(1900, 1070, 640, 360), 1920, 1080),
            Ok(Rect::new(1900, 1064, 20, 16))
        );
    }

    #[test]
    fn undersized_screens_and_regions_are_rejected() {
        let error = normalize_initial_region(Rect::new(0, 0, 100, 100), 8, 1080).unwrap_err();
        assert!(error.contains("screen is too small"));

        // 17 宽夹取后对齐到 16：合法；15 高保持最小值 16：也合法。
        assert!(normalize_initial_region(Rect::new(0, 0, 17, 15), 1920, 1080).is_ok());
    }

    #[test]
    fn output_paths_must_be_absolute_mp4_files() {
        assert!(validate_output_path(Path::new("/tmp/out.mp4")).is_ok());
        assert!(
            validate_output_path(Path::new("relative/out.mp4"))
                .unwrap_err()
                .contains("absolute")
        );
        assert!(
            validate_output_path(Path::new("/tmp/out.mkv"))
                .unwrap_err()
                .contains(".mp4")
        );
    }

    #[test]
    fn output_directory_prefers_configuration_then_falls_back() {
        assert_eq!(
            resolve_output_directory("/data/rec", None, None, None),
            Ok(PathBuf::from("/data/rec"))
        );
        assert_eq!(
            resolve_output_directory(
                "",
                Some(PathBuf::from("/xdg/videos")),
                Some(PathBuf::from("/sys/videos")),
                None
            ),
            Ok(PathBuf::from("/xdg/videos"))
        );
        assert_eq!(
            resolve_output_directory("", None, None, Some(PathBuf::from("/home/user"))),
            Ok(PathBuf::from("/home/user/Videos"))
        );
        assert!(
            resolve_output_directory("", None, None, None)
                .unwrap_err()
                .contains("recording_output_dir")
        );
    }

    #[test]
    fn finalization_plans_cover_direct_moved_and_concatenated_outputs() {
        assert_eq!(
            plan_finalization(&[], "/v/out.mp4"),
            FinalizationPlan::Nothing
        );

        // 新录制直接写在输出路径上：只校验，不搬移。
        assert_eq!(
            plan_finalization(&["/v/out.mp4".to_string()], "/v/out.mp4"),
            FinalizationPlan::ValidateSingle {
                segment: "/v/out.mp4".to_string(),
                move_to: None,
            }
        );

        // 旧调用方使用独立分段路径：校验后搬移。
        assert_eq!(
            plan_finalization(&["/v/seg.mp4".to_string()], "/v/out.mp4"),
            FinalizationPlan::ValidateSingle {
                segment: "/v/seg.mp4".to_string(),
                move_to: Some("/v/out.mp4".to_string()),
            }
        );

        // 多分段：concat 清单路径与内容与既有 ffmpeg 约定一致。
        let plan = plan_finalization(
            &["/v/a.mp4".to_string(), "/v/b.mp4".to_string()],
            "/v/out.mp4",
        );
        assert_eq!(
            plan,
            FinalizationPlan::ConcatSegments {
                list_path: PathBuf::from("/v/out.concat.txt"),
                list_content: "file '/v/a.mp4'\nfile '/v/b.mp4'".to_string(),
                output_path: "/v/out.mp4".to_string(),
            }
        );
    }
}
