//! GIF is a recording destination. Capture stays on the existing MP4 path;
//! only after that exact writer finishes do we run a bounded two-pass export.

use super::connectivity::BackgroundJob;
use crate::backend::api::{Backend, ToastNotification};
use crate::backend::compositor_common::screenshot_toolbar::{
    self, ScreenshotToolbar, ToolbarButton,
};
use crate::backend::recording_completion::{RecordingStopReceipt, WriterOutcome};
use crate::jwm::Jwm;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_GIF_EXPORTS: usize = 2;
const WRITER_TIMEOUT: Duration = Duration::from_secs(90);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const GIF_SCALE: &str = "fps=15,scale=w='min(960,iw)':h=-1:flags=lanczos";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RecordingFormat {
    #[default]
    Mp4,
    Gif,
}

impl RecordingFormat {
    pub(crate) fn of(path: &str) -> Self {
        if Path::new(path)
            .extension()
            .is_some_and(|extension| extension == "gif")
        {
            Self::Gif
        } else {
            Self::Mp4
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Mp4 => "MP4",
            Self::Gif => "GIF",
        }
    }

    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Gif => "gif",
        }
    }
}

pub(crate) fn capture_path(output: &str) -> String {
    if RecordingFormat::of(output) == RecordingFormat::Gif {
        Path::new(output)
            .with_extension("source.mp4")
            .to_string_lossy()
            .into_owned()
    } else {
        output.to_owned()
    }
}

#[derive(Debug, Default)]
pub(crate) struct RecordingPreferences {
    pub(crate) format: RecordingFormat,
    path: Option<PathBuf>,
}

impl RecordingPreferences {
    pub(crate) fn load() -> Self {
        #[cfg(not(test))]
        if let Some(path) = dirs::state_dir()
            .filter(|p| p.is_absolute())
            .map(|p| p.join("jwm/capture.json"))
        {
            return Self::load_from_path(path);
        }
        Self::default()
    }

    fn load_from_path(path: PathBuf) -> Self {
        let format = (|| {
            let mut contents = String::new();
            std::fs::File::open(&path)
                .ok()?
                .take(1024)
                .read_to_string(&mut contents)
                .ok()?;
            let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
            serde_json::from_value(value.get("recording_format")?.clone()).ok()
        })()
        .unwrap_or_default();
        Self {
            format,
            path: Some(path),
        }
    }

    fn select(&mut self, format: RecordingFormat) {
        if self.format == format {
            return;
        }
        self.format = format;
        if let Some(path) = &self.path {
            if let Err(error) = self.save_to(path) {
                log::warn!("could not remember recording format: {error}");
            }
        }
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("missing preference directory"))?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".capture-{:016x}.tmp", rand::random::<u64>()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let result = (|| {
            file.write_all(
                serde_json::json!({"recording_format": self.format})
                    .to_string()
                    .as_bytes(),
            )?;
            drop(file);
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

/// Private palette/staging files in the destination directory. Publishing with
/// a hard link is atomic and cannot overwrite a destination created meanwhile.
struct ExportDirectory(PathBuf);

impl ExportDirectory {
    fn new(output: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let parent = output
            .parent()
            .ok_or_else(|| std::io::Error::other("missing GIF directory"))?;
        let directory = parent.join(format!(".jwm-gif-{:016x}", rand::random::<u64>()));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(Self(directory))
    }
}

impl Drop for ExportDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_encoder(args: &[&str], deadline: Instant) -> Result<(), String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("GIF export timed out".into());
    }
    let output = super::external_command::output_with_limits("ffmpeg", args, remaining, 64 * 1024)
        .map_err(|e| format!("GIF encoder failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "GIF encoder: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn export_gif(source: &str, output: &str) -> Result<(), String> {
    let identity = super::recording::RecordingFileIdentity::of(source)
        .ok_or("The recorded video is missing or empty")?;
    if Path::new(output).exists() {
        return Err("GIF output already exists".into());
    }
    let private = ExportDirectory::new(Path::new(output)).map_err(|e| e.to_string())?;
    let palette = private.0.join("palette.png");
    let staging = private.0.join("animation.gif");
    let palette = palette.to_str().ok_or("Invalid palette path")?;
    let staging = staging.to_str().ok_or("Invalid GIF staging path")?;
    let deadline = Instant::now() + EXPORT_TIMEOUT;
    // Two passes avoid buffering a whole recording in a split/palettegen graph.
    let palette_filter = format!("{GIF_SCALE},palettegen=stats_mode=diff");
    run_encoder(
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-n",
            "-threads",
            "2",
            "-filter_threads",
            "1",
            "-i",
            source,
            "-vf",
            &palette_filter,
            "-frames:v",
            "1",
            "-update",
            "1",
            palette,
        ],
        deadline,
    )?;
    let filter = format!(
        "[0:v]{GIF_SCALE}[frames];[frames][1:v]paletteuse=dither=sierra2_4a:diff_mode=rectangle"
    );
    run_encoder(
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-n",
            "-threads",
            "2",
            "-filter_complex_threads",
            "1",
            "-i",
            source,
            "-i",
            palette,
            "-filter_complex",
            &filter,
            "-an",
            "-loop",
            "0",
            "-f",
            "gif",
            staging,
        ],
        deadline,
    )?;
    let probe = super::external_command::output_with_limits(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height,nb_frames",
            "-of",
            "json",
            staging,
        ],
        Duration::from_secs(5),
        16 * 1024,
    )
    .map_err(|e| e.to_string())?;
    let value: serde_json::Value =
        serde_json::from_slice(&probe.stdout).map_err(|e| e.to_string())?;
    let valid = probe.status.success()
        && value["streams"].as_array().is_some_and(|streams| {
            streams.first().is_some_and(|s| {
                s["codec_name"] == "gif"
                    && s["width"].as_u64().is_some_and(|w| w > 0)
                    && s["height"].as_u64().is_some_and(|h| h > 0)
            })
        });
    if !valid {
        return Err("The GIF encoder produced an invalid animation".into());
    }
    if super::recording::RecordingFileIdentity::of(source).as_ref() != Some(&identity) {
        return Err("The source video changed during GIF export".into());
    }
    std::fs::File::open(staging)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::hard_link(staging, output).map_err(|e| format!("Could not publish GIF: {e}"))?;
    // Remove only our completed source, after the GIF is safely published.
    if super::recording::RecordingFileIdentity::of(source).as_ref() == Some(&identity) {
        if let Err(error) = std::fs::remove_file(source) {
            log::warn!("GIF saved; could not remove {source}: {error}");
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct GifExport {
    pub(crate) output: String,
    source: String,
    receipt: Option<RecordingStopReceipt>,
    deadline: Instant,
    job: Option<BackgroundJob<Result<(), String>>>,
}

impl Jwm {
    pub(crate) fn selected_recording_format(&self) -> RecordingFormat {
        self.features
            .recording
            .pending_output_path
            .as_deref()
            .or(self.features.recording.output_path.as_deref())
            .map(RecordingFormat::of)
            .unwrap_or(self.features.recording_preferences.format)
    }

    pub(crate) fn select_recording_format(
        &mut self,
        backend: &mut dyn Backend,
        format: RecordingFormat,
    ) {
        if !self.features.recording.selecting_region || self.features.recording.adjusting_region {
            return;
        }
        if let Some(path) = &mut self.features.recording.pending_output_path {
            *path = Path::new(path)
                .with_extension(format.extension())
                .to_string_lossy()
                .into_owned();
        }
        self.features.recording_preferences.select(format);
        self.features.capture.clear_recording_double_click();
        self.sync_capture_hint(backend);
        self.sync_recording_format_toolbar(backend);
    }

    pub(crate) fn sync_recording_format_toolbar(&mut self, backend: &mut dyn Backend) {
        let region = self.features.recording.region;
        if !self.features.recording.selecting_region
            || self.features.recording.adjusting_region
            || region.is_none()
        {
            if self.features.recording.toolbar.take().is_some() {
                backend.compositor_set_screenshot_toolbar(None);
            }
            return;
        }
        let region = region.expect("recording toolbar has a region");
        let format = self.selected_recording_format();
        let buttons = vec![
            ToolbarButton::label("MP4")
                .available(true)
                .selected(format == RecordingFormat::Mp4),
            ToolbarButton::label("GIF")
                .available(true)
                .selected(format == RecordingFormat::Gif),
            ToolbarButton::label("REC").available(true),
            ToolbarButton::icon(screenshot_toolbar::ToolbarIcon::Close),
        ];
        let screen = [0.0, 0.0, self.s_w as f32, self.s_h as f32];
        let max_width = (screen[2] - 2.0 * screenshot_toolbar::SCREEN_MARGIN).max(1.0);
        let button_size = screenshot_toolbar::fit_button_size(&buttons, max_width);
        let extent = screenshot_toolbar::track_extent_for(&buttons, button_size, max_width);
        let hint = crate::backend::compositor_common::capture_hint::capture_hint_layout(
            screen[2],
            screen[3],
            (screen[2] * 0.55).min(720.0),
            16.0,
            0.0,
        )
        .chip;
        let bar = screenshot_toolbar::place_avoiding(
            [
                region.x as f32,
                region.y as f32,
                region.w as f32,
                region.h as f32,
            ],
            screen,
            extent,
            Some(hint),
        );
        let toolbar = ScreenshotToolbar {
            bar,
            button_size,
            buttons,
            hover_ease: Default::default(),
        };
        if self.features.recording.toolbar.as_ref() != Some(&toolbar) {
            self.features.recording.toolbar = Some(toolbar.clone());
            backend.compositor_set_screenshot_toolbar(Some(toolbar));
        }
    }

    pub(crate) fn recording_toolbar_motion(
        &mut self,
        backend: &mut dyn Backend,
        x: f64,
        y: f64,
    ) -> bool {
        if self.features.recording.is_region_dragging() {
            return false;
        }
        let Some(toolbar) = &mut self.features.recording.toolbar else {
            return false;
        };
        let hit = screenshot_toolbar::button_at(
            toolbar.bar,
            &toolbar.buttons,
            toolbar.button_size,
            x as f32,
            y as f32,
        );
        let mut changed = false;
        for (i, button) in toolbar.buttons.iter_mut().enumerate() {
            let hover = hit == Some(i);
            changed |= button.hovered != hover;
            button.hovered = hover;
        }
        if changed {
            backend.compositor_set_screenshot_toolbar(Some(toolbar.clone()));
        }
        screenshot_toolbar::hits_toolbar(toolbar.bar, x as f32, y as f32)
    }

    pub(crate) fn recording_toolbar_press(
        &mut self,
        backend: &mut dyn Backend,
        x: f64,
        y: f64,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(toolbar) = &self.features.recording.toolbar else {
            return Ok(false);
        };
        if !screenshot_toolbar::hits_toolbar(toolbar.bar, x as f32, y as f32) {
            return Ok(false);
        }
        let hit = screenshot_toolbar::button_at(
            toolbar.bar,
            &toolbar.buttons,
            toolbar.button_size,
            x as f32,
            y as f32,
        );
        self.features.capture.clear_recording_double_click();
        self.features.capture.swallow_next_button_release();
        match hit {
            Some(0) => self.select_recording_format(backend, RecordingFormat::Mp4),
            Some(1) => self.select_recording_format(backend, RecordingFormat::Gif),
            Some(2) => self.finish_recording_region_interaction(backend)?,
            Some(3) => self.cancel_recording_region_interaction(backend),
            _ => {}
        }
        Ok(true)
    }

    pub(crate) fn require_gif_capacity(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.gif_exports.len() >= MAX_GIF_EXPORTS {
            Err("Two GIF exports are already pending; wait for one to finish".into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn queue_gif_export(
        &mut self,
        segments: Vec<String>,
        output: String,
        receipt: Option<RecordingStopReceipt>,
    ) {
        self.features.gif_exports.push(GifExport {
            source: segments
                .first()
                .cloned()
                .unwrap_or_else(|| capture_path(&output)),
            output,
            receipt,
            deadline: Instant::now() + WRITER_TIMEOUT,
            job: None,
        });
    }

    pub(crate) fn poll_gif_exports(&mut self, backend: &mut dyn Backend) {
        let mut pending = Vec::new();
        for mut export in std::mem::take(&mut self.features.gif_exports) {
            let result = if let Some(job) = &export.job {
                match job.take() {
                    Some(result) => Some(result),
                    None if !job.started() => Some(Err("Could not start GIF export worker".into())),
                    None => None,
                }
            } else {
                match export.receipt.as_mut().map(RecordingStopReceipt::poll) {
                    Some(Some(WriterOutcome::Finished)) => {
                        let source = export.source.clone();
                        let output = export.output.clone();
                        export.receipt = None;
                        export.job =
                            Some(self.track_background_job(BackgroundJob::spawn(move || {
                                export_gif(&source, &output)
                            })));
                        None
                    }
                    Some(None) if Instant::now() < export.deadline => None,
                    Some(None) => Some(Err(
                        "Timed out waiting for the recording encoder to finish".into(),
                    )),
                    _ => Some(Err(
                        "The recording encoder did not confirm a successful capture".into(),
                    )),
                }
            };
            let Some(result) = result else {
                pending.push(export);
                continue;
            };
            let current = !self.features.recording.active
                && self.features.recording.output_path.as_deref() == Some(&export.output);
            let (title, body, urgency) = match result {
                Ok(()) => {
                    if current {
                        self.features.recording.finalized = true;
                        self.features.recording.finalization_reported = true;
                    }
                    self.broadcast_ipc_event(
                        "recording/finalized",
                        serde_json::json!({"output_path": export.output, "format": "gif"}),
                    );
                    ("\u{f03d}  GIF saved", export.output, 1)
                }
                Err(error) => {
                    if current {
                        self.features.recording.note_error(error.clone());
                    }
                    self.broadcast_ipc_event("recording/error", serde_json::json!({"operation": "gif_export", "output_path": export.output, "error": error}));
                    (
                        "\u{f03d}  GIF export failed",
                        format!("{error}. Source video: {}", export.source),
                        2,
                    )
                }
            };
            self.push_system_toast(
                backend,
                ToastNotification {
                    title: title.into(),
                    body,
                    urgency,
                    timeout_ms: 8000,
                    ..Default::default()
                },
            );
        }
        self.features.gif_exports = pending;
    }

    pub(crate) fn record_gif_from_screenshot(&mut self, backend: &mut dyn Backend) {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let region = self
                .features
                .screenshot
                .get_selection_rect()
                .ok_or("Select a screenshot region first")?;
            if self.features.recording.active {
                return Err("A recording is already active".into());
            }
            self.require_gif_capacity()?;
            Self::require_recording_runtime()?;
            let path = Path::new(&self.prepare_recording_output_path()?)
                .with_extension("gif")
                .to_string_lossy()
                .into_owned();
            self.cancel_screenshot_select(backend);
            self.features
                .recording_preferences
                .select(RecordingFormat::Gif);
            self.begin_recording_region_selection(backend, path)?;
            self.features.recording.set_region(region);
            self.sync_recording_region_overlay(backend);
            Ok(())
        })();
        if let Err(error) = result {
            self.push_system_toast(
                backend,
                ToastNotification {
                    title: "\u{f03d}  GIF recording unavailable".into(),
                    body: error.to_string(),
                    urgency: 2,
                    timeout_ms: 8000,
                    ..Default::default()
                },
            );
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::backend::recording_completion::{EncoderWaitOutcome, writer_completion_channel};
    use crate::core::types::Rect;
    use crate::jwm::features::monitor_lock::test_support::{LockSpyBackend, jwm_on_two_monitors};
    use image::AnimationDecoder;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("jwm-gif-test-{:016x}", rand::random::<u64>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_owned()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn has_encoder() -> bool {
        let path = std::env::var_os("PATH");
        ["ffmpeg", "ffprobe"]
            .iter()
            .all(|tool| crate::terminal_prober::command_exists_in_path(tool, path.as_deref()))
    }

    pub(crate) fn make_video(path: &str) {
        let result = super::super::external_command::output_with_limits(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-nostdin",
                "-n",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=10",
                "-t",
                "0.6",
                "-threads",
                "1",
                "-c:v",
                "mpeg4",
                path,
            ],
            Duration::from_secs(10),
            16 * 1024,
        )
        .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn recording_format_defaults_to_video_and_persists_the_last_choice() {
        let directory = Scratch::new();
        let path = directory.0.join("state/capture.json");
        let mut preferences = RecordingPreferences::load_from_path(path.clone());
        assert_eq!(preferences.format, RecordingFormat::Mp4);
        preferences.select(RecordingFormat::Gif);
        assert_eq!(
            RecordingPreferences::load_from_path(path.clone()).format,
            RecordingFormat::Gif
        );
        preferences.select(RecordingFormat::Mp4);
        assert_eq!(
            RecordingPreferences::load_from_path(path).format,
            RecordingFormat::Mp4
        );
        assert_eq!(capture_path("/v/a's clip.gif"), "/v/a's clip.source.mp4");
        assert_eq!(capture_path("/v/a.mp4"), "/v/a.mp4");
        assert_eq!(
            std::fs::read_dir(directory.0.join("state"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn recording_toolbar_switches_format_without_moving_the_crop_and_hides_on_cancel() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        let region = Rect::new(40, 50, 320, 240);
        jwm.features
            .recording
            .begin_initial_region_selection("/unused/recording.mp4".into());
        jwm.features.recording.set_region(region);
        jwm.sync_recording_region_overlay(&mut backend);
        let toolbar = jwm.features.recording.toolbar.clone().unwrap();
        assert!(toolbar.buttons[0].active);
        let gif =
            screenshot_toolbar::slot_rect(toolbar.bar, &toolbar.buttons, toolbar.button_size, 1)
                .unwrap();
        assert!(
            jwm.recording_toolbar_press(
                &mut backend,
                (gif[0] + gif[2] / 2.0) as f64,
                (gif[1] + gif[3] / 2.0) as f64
            )
            .unwrap()
        );
        assert_eq!(
            jwm.features.recording.pending_output_path.as_deref(),
            Some("/unused/recording.gif")
        );
        assert_eq!(jwm.features.recording.region, Some(region));
        assert_eq!(
            jwm.features.recording_preferences.format,
            RecordingFormat::Gif
        );
        assert!(jwm.features.recording.toolbar.as_ref().unwrap().buttons[1].active);
        jwm.cancel_recording_region_interaction(&mut backend);
        assert!(jwm.features.recording.toolbar.is_none());
        assert!(!jwm.features.recording.selecting_region);
        assert_eq!(
            jwm.features.recording_preferences.format,
            RecordingFormat::Gif
        );
    }

    #[test]
    fn gif_export_waits_for_exact_writer_and_rejects_failed_and_unknown_outcomes() {
        for failed in [false, true] {
            let mut backend = LockSpyBackend::new();
            let mut jwm = jwm_on_two_monitors(&mut backend);
            jwm.features.recording.start("/unused/movie.gif".into());
            jwm.features.recording.stop();
            let (reporter, receipt) = writer_completion_channel();
            jwm.queue_gif_export(
                vec!["/unused/movie.source.mp4".into()],
                "/unused/movie.gif".into(),
                Some(RecordingStopReceipt::from_writer(Some(receipt))),
            );
            jwm.poll_gif_exports(&mut backend);
            assert_eq!(jwm.features.gif_exports.len(), 1);
            assert!(jwm.features.gif_exports[0].job.is_none());
            if failed {
                reporter.finish(None, Some(EncoderWaitOutcome::Exited(false)));
            } else {
                drop(reporter);
            }
            jwm.poll_gif_exports(&mut backend);
            assert!(jwm.features.gif_exports.is_empty());
            assert!(!jwm.features.recording.finalized);
            assert!(
                jwm.features
                    .recording
                    .last_error
                    .as_deref()
                    .unwrap()
                    .contains("encoder")
            );
        }
    }

    #[test]
    fn old_gif_completion_never_changes_a_new_recording() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features.recording.start("/unused/new.mp4".into());
        for result in [Ok(()), Err("old export failed".to_owned())] {
            jwm.features.gif_exports.push(GifExport {
                output: "/unused/old.gif".into(),
                source: "/unused/old.source.mp4".into(),
                receipt: None,
                deadline: Instant::now(),
                job: Some(BackgroundJob::completed_for_test(result)),
            });
            jwm.poll_gif_exports(&mut backend);
            assert!(jwm.features.recording.active);
            assert_eq!(
                jwm.features.recording.output_path.as_deref(),
                Some("/unused/new.mp4")
            );
            assert!(!jwm.features.recording.finalized);
            assert!(jwm.features.recording.last_error.is_none());
        }
    }

    #[test]
    fn real_gif_export_preserves_animation_delays_and_loops_and_cleans_private_files() {
        if !has_encoder() {
            eprintln!("ffmpeg/ffprobe unavailable; skipping encoder integration");
            return;
        }
        let directory = Scratch::new();
        let output = directory.path("a's short clip.gif");
        let source = capture_path(&output);
        make_video(&source);
        export_gif(&source, &output).unwrap();
        assert!(!Path::new(&source).exists());
        let bytes = std::fs::read(&output).unwrap();
        assert!(bytes.starts_with(b"GIF89a"));
        assert!(bytes.windows(11).any(|w| w == b"NETSCAPE2.0"));
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let frames = decoder.into_frames().collect_frames().unwrap();
        assert!(frames.len() > 1);
        assert!(
            frames
                .iter()
                .all(|f| f.buffer().dimensions() == (64, 48) && f.delay().numer_denom_ms().0 > 0)
        );
        assert!(frames.windows(2).any(|w| w[0].buffer() != w[1].buffer()));
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[test]
    fn gif_export_failure_and_existing_destination_preserve_source_video() {
        if !has_encoder() {
            return;
        }
        let directory = Scratch::new();
        let source = directory.path("broken.source.mp4");
        let output = directory.path("broken.gif");
        std::fs::write(&source, b"not a video").unwrap();
        assert!(export_gif(&source, &output).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"not a video");
        assert!(!Path::new(&output).exists());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
        std::fs::write(&output, b"existing file").unwrap();
        assert!(
            export_gif(&source, &output)
                .unwrap_err()
                .contains("already exists")
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"existing file");
        assert!(Path::new(&source).exists());
    }
}
