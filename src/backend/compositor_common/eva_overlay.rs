//! JWM-owned Eva status, separate from the transient OSD and notifications.
//! The provider cannot publish here directly: snapshots are filtered by the WM
//! permit and never claim a microphone was released just because a lease died.
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ConfirmationLine {
    critical: String,
    title: String,
}
type ConfirmationCache =
    std::cell::RefCell<Option<(String, [f32; 4], std::sync::Arc<ConfirmationPrepared>)>>;
type TextCache = std::cell::RefCell<Option<(String, std::sync::Arc<EvaTextRaster>)>>;

/// One cached raster shared by the native GL and nested renderers. Typography,
/// wrapping and spacing must not vary with the active window-system backend.
#[derive(Debug)]
pub(crate) struct EvaTextRaster {
    pub(crate) pixels: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Identity of the complete shared typography/layout inputs. Outer texture
    /// caches must not infer pixel identity from label or dimensions alone.
    pub(crate) layout_key: u64,
}

struct TextSection {
    text: String,
    heading: bool,
    secondary: bool,
    rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvaLocalAction {
    Retry,
    Diagnostics,
    Undo,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EvaOverlayState {
    active: bool,
    session_active: bool,
    undo_available: bool,
    caption: String,
    progress: Option<String>,
    progress_fraction: Option<f32>,
    result_status: Option<String>,
    retryable: Option<bool>,
    error_code: Option<String>,
    voice_fallback: bool,
    state: String,
    text: String,
    level: f32,
    microphone: Option<bool>,
    confirmation: bool,
    confirmation_items: Vec<ConfirmationLine>,
    confirmation_total: usize,
    details_page: Option<usize>,
    seen_pages: Vec<usize>,
    deadline: Option<Instant>,
    followup_deadline: Option<Instant>,
    followup_drawn: Option<u64>,
    viewport: Option<[f32; 4]>,
    expiry_drawn: bool,
    appeared: Option<Instant>,
    dismissed: Option<(Instant, f32)>,
    motion_suppressed: bool,
    last_rect: std::cell::Cell<Option<[f32; 4]>>,
    prepared: ConfirmationCache,
    text_cache: TextCache,
}

impl EvaOverlayState {
    pub(crate) fn update(&mut self, snapshot: &Value, now: Instant) -> bool {
        self.update_with_motion(snapshot, now, crate::config::CONFIG.load().motion_enabled())
    }
    fn update_with_motion(&mut self, snapshot: &Value, now: Instant, motion_enabled: bool) -> bool {
        let previous_extra = (
            self.session_active,
            self.undo_available,
            self.caption.clone(),
            self.followup_seconds(now),
            self.progress.clone(),
            self.progress_fraction,
            self.result_status.clone(),
            self.retryable,
            self.error_code.clone(),
            self.voice_fallback,
        );
        let previous = (
            self.active,
            self.state.clone(),
            self.text.clone(),
            self.level,
            self.microphone,
            self.confirmation,
            self.viewport,
            self.confirmation_items.clone(),
            self.confirmation_total,
            self.details_page,
            self.seen_pages.clone(),
        );
        let old_alpha = self.alpha_with_motion(now, motion_enabled);
        let was_visible = self.active;
        self.expiry_drawn = false;
        self.motion_suppressed = snapshot["suppress_animation"].as_bool().unwrap_or(false);
        self.active = snapshot["visible"]
            .as_bool()
            .or_else(|| snapshot["active"].as_bool())
            .unwrap_or(false);
        let motion = motion_enabled && !self.motion_suppressed;
        if self.active && !was_visible {
            self.appeared = Some(
                now.checked_sub(Duration::from_secs_f32(
                    old_alpha * super::toast::TOAST_FADE_IN,
                ))
                .unwrap_or(now),
            );
            self.dismissed = None;
        } else if !self.active && was_visible {
            self.dismissed = motion.then_some((now, old_alpha));
        }
        if !motion {
            self.dismissed = None;
        }
        self.session_active = snapshot["active"].as_bool().unwrap_or(self.active);
        self.undo_available = snapshot["undo_available"].as_bool().unwrap_or(false);
        self.caption = clean_message(
            snapshot["input_caption"]["text"].as_str().unwrap_or(""),
            160,
        );
        self.state = snapshot["state"]
            .as_str()
            .unwrap_or("idle")
            .to_ascii_lowercase();
        self.text = clean_message(snapshot["text"].as_str().unwrap_or(""), 256);
        self.progress = progress_label(&snapshot["task_progress"]);
        self.progress_fraction = verified_progress_fraction(&snapshot["task_progress"]);
        self.result_status = snapshot["result_status"]
            .as_str()
            .filter(|value| {
                matches!(
                    *value,
                    "succeeded"
                        | "partial"
                        | "clarify"
                        | "noop"
                        | "unknown"
                        | "failed"
                        | "cancelled"
                )
            })
            .map(str::to_owned);
        self.retryable = snapshot["retryable"].as_bool();
        self.error_code = snapshot["error_code"]
            .as_str()
            .filter(|value| {
                value.len() <= 64
                    && value
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            })
            .map(str::to_owned);
        self.voice_fallback = snapshot["voice_fallback"].as_bool().unwrap_or(false);
        self.level = snapshot["level"]
            .as_f64()
            .filter(|x| x.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0) as f32;
        self.microphone = snapshot["microphone_active"].as_bool();
        self.confirmation = snapshot["confirmation"]["approved"].as_bool() != Some(true)
            && (snapshot["confirmation"].as_bool().unwrap_or(false)
                || snapshot["confirmation"].is_object()
                || snapshot["confirmation"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()));
        self.confirmation_items = snapshot["confirmation"]["display_items"]
            .as_array()
            .into_iter()
            .flatten()
            .take(jwm_eva_core::actions::MAX_ACTIONS_PER_PLAN)
            .map(|item| ConfirmationLine {
                critical: clean(
                    item.get("critical")
                        .and_then(Value::as_str)
                        .or_else(|| item.as_str())
                        .unwrap_or(""),
                    1024 * 1024,
                ),
                title: clean(item["title"].as_str().unwrap_or(""), 80),
            })
            .collect();
        self.confirmation_total = snapshot["confirmation"]["total_actions"]
            .as_u64()
            .unwrap_or(0)
            .min(1024) as usize;
        self.details_page = snapshot["confirmation"]["details_page"]
            .as_u64()
            .map(|p| p.min(8192) as usize);
        self.seen_pages = snapshot["confirmation"]["details_seen_pages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .take(8192)
            .map(|p| p as usize)
            .collect();
        self.deadline = self.session_active.then(|| {
            now + Duration::from_millis(
                snapshot["lease_remaining_ms"]
                    .as_u64()
                    .unwrap_or(3000)
                    .min(3000),
            )
        });
        // This is the real follow-up deadline supplied by the WM, not the
        // heartbeat/permit lease. Merely repainting never grants more time.
        self.followup_deadline =
            if self.session_active && matches!(self.state.as_str(), "followup" | "follow_up") {
                snapshot["followup_remaining_ms"]
                    .as_u64()
                    .map(|remaining| now + Duration::from_millis(remaining.min(600_000)))
            } else {
                None
            };
        self.viewport = snapshot["monitor_rect"].as_array().and_then(|v| {
            if v.len() != 4 {
                return None;
            }
            let mut result = [0.0; 4];
            for (dst, src) in result.iter_mut().zip(v) {
                *dst = src.as_f64()? as f32;
                if !dst.is_finite() {
                    return None;
                }
            }
            (result[2] > 0.0 && result[3] > 0.0).then_some(result)
        });
        if self.viewport != previous.6
            || self.confirmation_items != previous.7
            || self.confirmation_total != previous.8
        {
            self.prepared.get_mut().take();
        }
        previous_extra
            != (
                self.session_active,
                self.undo_available,
                self.caption.clone(),
                self.followup_seconds(now),
                self.progress.clone(),
                self.progress_fraction,
                self.result_status.clone(),
                self.retryable,
                self.error_code.clone(),
                self.voice_fallback,
            )
            || previous.0 != self.active
            || (self.active
                && previous
                    != (
                        self.active,
                        self.state.clone(),
                        self.text.clone(),
                        self.level,
                        self.microphone,
                        self.confirmation,
                        self.viewport,
                        self.confirmation_items.clone(),
                        self.confirmation_total,
                        self.details_page,
                        self.seen_pages.clone(),
                    ))
    }

    pub(crate) fn visible(&self) -> bool {
        self.active || self.dismissed.is_some()
    }
    pub(crate) fn alpha_with_motion(&self, now: Instant, motion_enabled: bool) -> f32 {
        if !motion_enabled || self.motion_suppressed {
            return if self.active { 1.0 } else { 0.0 };
        }
        if let Some((at, from)) = self.dismissed {
            return from
                * (1.0
                    - now.saturating_duration_since(at).as_secs_f32()
                        / super::toast::TOAST_DISMISS_FADE)
                    .clamp(0.0, 1.0);
        }
        if !self.active {
            return 0.0;
        }
        self.appeared.map_or(1.0, |at| {
            (now.saturating_duration_since(at).as_secs_f32() / super::toast::TOAST_FADE_IN)
                .clamp(0.0, 1.0)
        })
    }
    pub(crate) fn alpha(&self, now: Instant) -> f32 {
        self.alpha_with_motion(now, crate::config::CONFIG.load().motion_enabled())
    }
    #[cfg(test)]
    pub(crate) fn settle_motion_for_test(&mut self, now: Instant) {
        self.appeared = now.checked_sub(Duration::from_secs_f32(super::toast::TOAST_FADE_IN));
        if self.dismissed.is_some() {
            self.dismissed = Some((
                now - Duration::from_secs_f32(super::toast::TOAST_DISMISS_FADE),
                1.0,
            ));
        }
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.active && self.deadline.is_some_and(|at| now >= at)
    }

    pub(crate) fn needs_frame(&self, now: Instant) -> bool {
        (self.expired(now) && !self.expiry_drawn)
            || self.dismissed.is_some()
            || (self.active && self.alpha(now) < 1.0)
            || self.followup_seconds(now) != self.followup_drawn
    }

    pub(crate) fn mark_drawn(&mut self, now: Instant) {
        self.expiry_drawn = self.expired(now);
        self.followup_drawn = self.followup_seconds(now);
        if self.dismissed.is_some() && self.alpha(now) <= 0.0 {
            self.dismissed = None;
        }
    }

    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        let animation = if self.needs_frame(now) {
            Some(now + Duration::from_millis(16))
        } else {
            None
        };
        self.deadline
            .into_iter()
            .chain(animation)
            .chain(
                self.followup_deadline
                    .filter(|at| self.active && !self.expired(now) && *at > now)
                    .map(|at| {
                        let remaining = at.saturating_duration_since(now).as_millis() as u64;
                        now + Duration::from_millis(remaining.saturating_sub(1) % 1000 + 1)
                    }),
            )
            .filter(|at| *at > now)
            .map(|at| at.saturating_duration_since(now))
            .min()
    }

    pub(crate) fn microphone_on(&self, now: Instant) -> bool {
        self.active && !self.expired(now) && self.microphone == Some(true)
    }

    pub(crate) fn level(&self, now: Instant) -> Option<f32> {
        self.microphone_on(now).then_some(self.level)
    }

    pub(crate) fn label(&self, now: Instant) -> String {
        if !self.active && self.dismissed.is_some() {
            return "Eva · 正在退出".into();
        }
        if self.expired(now) {
            return "Eva · 连接已中断\n麦克风状态待确认 · 请检查语音服务".into();
        }
        if self.confirmation {
            // Reserve this line even without ASR: a late caption cannot
            // reflow target pages or alter which pages were already reviewed.
            return format!(
                "{}\n{}",
                self.confirmation_layout().text,
                if self.caption.is_empty() {
                    self.microphone_label(now)
                } else {
                    format!("{} · 识别：{}", self.microphone_label(now), self.caption)
                }
            );
        }
        self.text_sections(now)
            .into_iter()
            .map(|section| section.text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn followup_seconds(&self, now: Instant) -> Option<u64> {
        (self.active && !self.expired(now))
            .then_some(self.followup_deadline)
            .flatten()
            .map(|deadline| {
                deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .div_ceil(1000) as u64
            })
    }

    fn outcome(&self) -> Option<&str> {
        matches!(
            self.state.as_str(),
            "responding" | "follow_up" | "followup" | "error"
        )
        .then_some(self.result_status.as_deref())
        .flatten()
    }
    fn standby_card(&self) -> bool {
        !self.session_active
            && matches!(
                self.state.as_str(),
                "standby" | "paused" | "connecting" | "disabled"
            )
    }
    fn status_title(&self) -> &str {
        if self.voice_fallback
            && !self.microphone_on(Instant::now())
            && matches!(self.outcome(), None | Some("noop"))
        {
            return "回复已保留";
        }
        match self.outcome() {
            Some("succeeded") => "已完成",
            Some("partial") => "部分完成",
            Some("clarify") => "需要补充",
            Some("unknown") => "结果待确认",
            Some("failed") => "未能完成",
            Some("cancelled") => "已停止",
            Some("noop") if self.state == "responding" => "正在回答",
            _ => match self.state.as_str() {
                "connecting" => "正在连接",
                "standby" => "唤醒待机",
                "paused" => "唤醒已暂停",
                "disabled" | "closing" => "正在结束",
                "listening" => "正在听",
                "planning" => "正在理解",
                "confirming" => "请确认操作",
                "executing" => "正在执行",
                "responding" => "正在回答",
                "followup" | "follow_up" => "可以继续说",
                "error"
                    if matches!(
                        self.error_code.as_deref(),
                        Some(
                            "auth_failed"
                                | "configuration_invalid"
                                | "model_access_denied"
                                | "protocol_unsupported"
                        )
                    ) =>
                {
                    "需要设置"
                }
                "error" => "需要处理",
                "unknown" => "结果待确认",
                _ => "语音助手",
            },
        }
    }
    fn microphone_label(&self, now: Instant) -> String {
        if self.microphone.is_none() {
            "麦克风状态待确认".into()
        } else if self.microphone_on(now) {
            match self.followup_seconds(now) {
                Some(0) => "麦克风已开启 · 本轮即将结束".into(),
                Some(seconds) => format!("麦克风已开启 · 还可说话 {seconds} 秒"),
                None => "麦克风已开启".into(),
            }
        } else if self.state == "error" || self.standby_card() {
            "麦克风未开启".into()
        } else {
            "麦克风已暂停".into()
        }
    }
    fn text_sections(&self, now: Instant) -> Vec<TextSection> {
        let height = self.viewport.map_or(720., |rect| rect[3]);
        let compact = height < 360.;
        let tiny = height < 220.;
        let mut sections = vec![TextSection {
            text: format!("Eva · {}", self.status_title()),
            heading: true,
            secondary: false,
            rows: 1,
        }];
        let microphone = self.microphone_label(now);
        if self.standby_card() {
            let body = if !self.text.is_empty() {
                self.text.as_str()
            } else {
                match self.state.as_str() {
                    "standby" => "说 hi eva 开始对话",
                    "disabled" => "正在释放麦克风",
                    _ => "正在准备语音唤醒",
                }
            };
            sections.push(TextSection {
                text: body.into(),
                heading: false,
                secondary: true,
                rows: 1,
            });
            sections.push(TextSection {
                text: format!("{microphone} · 快捷键关闭监听"),
                heading: false,
                secondary: true,
                rows: 1,
            });
            return sections;
        }
        // Executor-owned step text is shown once in the progress row. Ordinary
        // result/question prose remains primary and never gets a fake percent.
        let processing_progress =
            self.progress.is_some() && matches!(self.state.as_str(), "planning" | "executing");
        let body = if !self.text.is_empty() && !processing_progress {
            self.text.clone()
        } else {
            match self.state.as_str() {
                "listening" => "直接说你想做的事".into(),
                "planning" if !processing_progress => "正在结合当前桌面理解你的意思".into(),
                "followup" | "follow_up" => "可以继续补充或修正刚才的指令".into(),
                "connecting" => "正在准备语音连接".into(),
                _ => String::new(),
            }
        };

        if !body.is_empty() {
            sections.push(TextSection {
                text: body,
                heading: false,
                secondary: false,
                rows: if tiny {
                    if self.progress.is_some() || self.voice_fallback {
                        1
                    } else {
                        2
                    }
                } else if compact {
                    2
                } else {
                    5
                },
            });
        }
        if let Some(progress) = &self.progress {
            sections.push(TextSection {
                text: progress.clone(),
                heading: false,
                secondary: true,
                rows: 1,
            });
        }
        let actions = if self.state == "error" {
            if self.retryable == Some(false) {
                "Alt+Ctrl+I 诊断 · Esc 退出"
            } else {
                "Alt+Ctrl+R 重试 · Alt+Ctrl+I 诊断 · Esc 退出"
            }
        } else if self.undo_available
            && matches!(
                self.state.as_str(),
                "listening" | "followup" | "follow_up" | "responding"
            )
        {
            "Alt+Ctrl+Z 撤销 · Esc 退出"
        } else {
            "Esc 退出 · Alt+Ctrl+E 结束会话"
        };
        if tiny {
            sections.push(TextSection {
                text: format!("Esc 退出 · {microphone}"),
                heading: false,
                secondary: true,
                rows: 1,
            });
        } else {
            sections.push(TextSection {
                text: microphone,
                heading: false,
                secondary: true,
                rows: 1,
            });
            sections.push(TextSection {
                text: actions.into(),
                heading: false,
                secondary: true,
                rows: if compact { 1 } else { 2 },
            });
        }
        if !self.caption.is_empty() && !tiny {
            sections.push(TextSection {
                text: format!("你说：{}", self.caption),
                heading: false,
                secondary: true,
                rows: if compact { 1 } else { 2 },
            });
        }
        if self.voice_fallback {
            sections.push(TextSection {
                text: "语音回复暂不可用 · Alt+Ctrl+I 诊断".into(),
                heading: false,
                secondary: true,
                rows: 1,
            });
        }
        sections
    }

    pub(crate) fn progress_fill(&self, now: Instant) -> Option<f32> {
        if self.progress_fraction == Some(1.)
            && matches!(
                self.outcome(),
                Some("partial" | "unknown" | "failed" | "cancelled" | "clarify")
            )
        {
            return None;
        }
        (self.active && !self.expired(now) && !self.confirmation && !self.standby_card())
            .then_some(self.progress_fraction)
            .flatten()
    }
    pub(crate) fn progress_rect(&self, card: [f32; 4]) -> Option<[f32; 4]> {
        (self.progress_fraction.is_some()
            && !self.confirmation
            && card[2] >= 160.
            && card[3] >= 70.)
            .then_some([
                card[0] + 20.,
                card[1] + card[3] - 11.,
                (card[2] - 40.).max(1.),
                3.,
            ])
    }
    /// Five static segments use only measured audio level. Zero input lights
    /// zero bars; no animation pretends that an open microphone is hearing speech.
    pub(crate) fn mic_bars(&self, card: [f32; 4], now: Instant) -> Vec<([f32; 4], bool)> {
        let Some(level) = self.level(now) else {
            return Vec::new();
        };
        if card[2] < 32. || card[3] < 24. {
            return Vec::new();
        }
        let left = card[0] + (card[2] - 44.).max(4.);
        let top = if self.confirmation || card[2] < 160. {
            card[1] + (card[3] - 18.).max(2.)
        } else {
            card[1] + 20.
        };
        let available = (card[3] - 4.).clamp(1., 12.);
        let top = top
            .min(card[1] + card[3] - available - 2.)
            .max(card[1] + 2.);
        let active = (level * 5.).ceil() as usize;
        (0..5)
            .map(|index| {
                let height = (4. + index as f32 * 2.).min(available);
                (
                    [
                        left + index as f32 * 4.,
                        top + available - height,
                        2.5,
                        height,
                    ],
                    index < active,
                )
            })
            .collect()
    }

    /// Text never takes its color from the status accent. The existing OSD
    /// ink/surface contrast remains valid for light, dark and flat themes.
    pub(crate) fn accent(&self, now: Instant) -> [f32; 4] {
        if self.expired(now) || self.state == "error" || self.outcome() == Some("failed") {
            [0.92, 0.35, 0.40, 1.]
        } else if self.confirmation
            || matches!(self.outcome(), Some("partial" | "unknown"))
            || self.state == "unknown"
        {
            [0.93, 0.67, 0.24, 1.]
        } else if self.outcome() == Some("succeeded") {
            [0.16, 0.72, 0.56, 1.]
        } else if self.outcome() == Some("clarify") {
            [0.40, 0.53, 0.91, 1.]
        } else if self.microphone_on(now) {
            [0.16, 0.72, 0.56, 1.]
        } else {
            [0.40, 0.53, 0.91, 1.]
        }
    }
    fn outer_margin(&self) -> f32 {
        if !self.confirmation && self.viewport.is_some_and(|r| r[2] < 180. || r[3] < 140.) {
            8.
        } else {
            32.
        }
    }
    fn padding(&self) -> f32 {
        if !self.confirmation && self.viewport.is_some_and(|r| r[2] < 180. || r[3] < 140.) {
            4.
        } else {
            20.
        }
    }
    fn meter_space(&self) -> f32 {
        if self.progress_fraction.is_some() && !self.confirmation {
            10.
        } else {
            0.
        }
    }

    pub(crate) fn text_width(&self, width: f32) -> u32 {
        (width
            - if self.confirmation {
                40.
            } else {
                self.padding() * 2.
            })
        .max(1.) as u32
    }
    pub(crate) fn text_rect(&self, rect: [f32; 4], size: [u32; 2]) -> [f32; 4] {
        let [x, y, w, h] = rect;
        let [tw, th] = size.map(|n| n.max(1) as f32);
        if self.confirmation {
            let scale = ((w - 40.).max(1.) / tw).min((h - 24.).max(1.) / th).min(1.);
            let (lw, lh) = (tw * scale, th * scale);
            [x + (w - lw) * 0.5, y + (h - lh) * 0.5 - 3., lw, lh]
        } else {
            let padding = self.padding();
            [
                x + padding.min(w - 1.),
                y + padding.min(h - 1.),
                tw.min((w - padding * 2.).max(1.)),
                th.min((h - padding * 2. - self.meter_space()).max(1.)),
            ]
        }
    }

    pub(crate) fn text_raster(&self, now: Instant, width: u32) -> std::sync::Arc<EvaTextRaster> {
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(font);
        let ink = super::ui_theme::palette().osd_ink;
        let label = self.label(now);
        let compact = self.viewport.is_some_and(|rect| rect[3] < 360.0);
        let maximum_height = (self.viewport.map_or(720., |r| r[3])
            - self.outer_margin()
            - self.padding() * 2.
            - self.meter_space())
        .max(1.) as u32;
        let key = format!(
            "{width}|{font}|{size}|{ink:?}|{}|{compact}|{maximum_height}|{}|{}|{label}",
            self.confirmation,
            self.padding(),
            self.meter_space()
        );
        if let Some((old, raster)) = self.text_cache.borrow().as_ref()
            && old == &key
        {
            return raster.clone();
        }
        use std::hash::{Hash, Hasher};
        let mut identity = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut identity);
        let layout_key = identity.finish();
        let raster = if self.confirmation || self.expired(now) || !self.active {
            // Confirmation target rows retain their exact baseline, font and
            // reserved caption row. Presentation never alters review gating.
            let fitted = fit_eva_label(&label, font, size, width);
            let (pixels, width, height) =
                crate::backend::compositor_font::render_ui_text_to_rgba(&fitted, font, size, ink);
            EvaTextRaster {
                pixels,
                width,
                height,
                layout_key,
            }
        } else {
            rasterize_sections(
                self.text_sections(now),
                font,
                size,
                width,
                ink,
                maximum_height,
                layout_key,
            )
        };
        let raster = std::sync::Arc::new(raster);
        *self.text_cache.borrow_mut() = Some((key, raster.clone()));
        raster
    }

    pub(crate) fn local_actions(snapshot: &Value) -> Vec<EvaLocalAction> {
        let visible = snapshot["visible"]
            .as_bool()
            .or_else(|| snapshot["active"].as_bool())
            .unwrap_or(false);
        if !visible {
            return Vec::new();
        }
        let state = snapshot["state"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if state == "error" {
            return if snapshot["retryable"] == false {
                vec![EvaLocalAction::Diagnostics]
            } else {
                vec![EvaLocalAction::Retry, EvaLocalAction::Diagnostics]
            };
        }
        let mut actions = Vec::new();
        if snapshot["active"] == true
            && snapshot["undo_available"] == true
            && matches!(
                state.as_str(),
                "listening" | "followup" | "follow_up" | "responding"
            )
        {
            actions.push(EvaLocalAction::Undo);
        }
        if snapshot["voice_fallback"] == true {
            actions.push(EvaLocalAction::Diagnostics);
        }
        actions
    }

    /// Native confirmation and its detail pages share exactly the same layout
    /// across GL and nested backends. Only titles may be abbreviated. Long
    /// critical values require viewing every readable detail page first.
    #[cfg(test)]
    pub(crate) fn confirmation_reviewable_snapshot(snapshot: &Value) -> bool {
        let mut state = Self::default();
        state.update(snapshot, Instant::now());
        state.confirmation && state.confirmation_layout().reviewable
    }
    #[cfg(test)]
    pub(crate) fn confirmation_page_count(snapshot: &Value) -> usize {
        let mut state = Self::default();
        state.update(snapshot, Instant::now());
        state.confirmation_layout().pages
    }
    pub(crate) fn confirmation_reviewable(&self) -> bool {
        self.confirmation && self.confirmation_layout().reviewable
    }
    pub(crate) fn confirmation_layout_signature(&self) -> Option<u64> {
        if !self.confirmation || self.confirmation_items.is_empty() {
            return None;
        }
        // The immutable Prepared owns this signature; browsing and captions
        // do not rebuild or rehash potentially large frozen values.
        let _ = self.confirmation_layout();
        self.prepared
            .borrow()
            .as_ref()
            .map(|(_, _, prepared)| prepared.signature)
    }
    pub(crate) fn confirmation_page_count_cached(&self) -> usize {
        self.confirmation_layout().pages
    }
    fn confirmation_layout(&self) -> ConfirmationLayout {
        use crate::backend::compositor_font::{fit_ui_text_lines, measure_ui_text_width};
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(font);
        let [_, _, w, h] = self.viewport.unwrap_or([0.0, 0.0, 1280.0, 720.0]);
        let viewport = [0.0, 0.0, w, h];
        let cached = self
            .prepared
            .borrow()
            .as_ref()
            .filter(|(old_font, old_view, _)| old_font == font && *old_view == viewport)
            .map(|(_, _, prepared)| prepared.clone());
        let prepared = cached.unwrap_or_else(|| {
            let width = ((w - 32.0).clamp(1.0, 880.0) - 44.0).max(1.0) as u32;
            let line_h = (size * 1.5).ceil();
            // Four fixed rows: header, navigation, confirmation/escape and ASR.
            // ASR is always reserved and never affects page capacity.
            let rows = (((h - 64.0) / line_h).floor() as isize - 4).clamp(1, 8) as usize;
            let valid = self.confirmation_total > 0
                && self.confirmation_total <= jwm_eva_core::actions::MAX_ACTIONS_PER_PLAN
                && self.confirmation_items.len() == self.confirmation_total
                && self
                    .confirmation_items
                    .iter()
                    .all(|item| !item.critical.is_empty());
            let mut expanded = Vec::new();
            let mut short = Vec::new();
            let mut fits = valid && self.confirmation_items.len() <= rows;
            for (index, item) in self.confirmation_items.iter().enumerate() {
                let critical = format!("{}. {}", index + 1, item.critical);
                let critical_fits = critical.chars().count() <= 256
                    && measure_ui_text_width(&critical, font, size) <= width;
                fits &= critical_fits;
                let mut line = if critical_fits {
                    critical.clone()
                } else {
                    fit_ui_text_lines(
                        &critical.chars().take(256).collect::<String>(),
                        font,
                        size,
                        width,
                    )
                };
                if critical_fits && !item.title.is_empty() {
                    let space = width.saturating_sub(measure_ui_text_width(&line, font, size));
                    if space > 24 {
                        line.push_str(&fit_ui_text_lines(
                            &format!(" · {}", item.title),
                            font,
                            size,
                            space,
                        ));
                    }
                }
                short.push(line);
                expanded.extend(crate::backend::compositor_font::wrap_ui_text_lines(
                    &critical, font, size, width,
                ));
            }
            let pages = expanded.len().div_ceil(rows).max(1);
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            "eva-confirmation-layout/v2".hash(&mut hasher);
            font.hash(&mut hasher);
            size.to_bits().hash(&mut hasher);
            w.to_bits().hash(&mut hasher);
            h.to_bits().hash(&mut hasher);
            self.confirmation_total.hash(&mut hasher);
            for item in &self.confirmation_items {
                item.critical.hash(&mut hasher);
            }
            let prepared = std::sync::Arc::new(ConfirmationPrepared {
                signature: hasher.finish(),
                expanded,
                short,
                pages,
                rows,
                fits,
                valid,
                width,
                line_h,
            });
            *self.prepared.borrow_mut() = Some((font.to_owned(), viewport, prepared.clone()));
            prepared
        });
        let ConfirmationPrepared {
            expanded,
            short,
            pages,
            rows,
            fits,
            valid,
            width,
            line_h,
            ..
        } = &*prepared;
        let (pages, rows, fits, valid, width, line_h) =
            (*pages, *rows, *fits, *valid, *width, *line_h);
        let all_seen = (0..pages).all(|p| self.seen_pages.contains(&p));
        let reviewable = valid && (fits || all_seen) && width >= 220 && h >= line_h * 5.0 + 64.0;
        let mut lines = Vec::new();
        if let Some(page) = self.details_page {
            let page = page.min(pages - 1);
            lines.push(format!(
                "Eva · 确认 {} 项 · 详情 {}/{} 页",
                self.confirmation_total,
                page + 1,
                pages
            ));
            lines.extend(expanded.iter().skip(page * rows).take(rows).cloned());
            lines.push("PgUp/PgDn 翻页 · Alt+Ctrl+D 返回".into());
        } else {
            lines.push(format!("Eva · 请确认 {} 项操作", self.confirmation_total));
            lines.extend(short.iter().take(rows).cloned());
            lines.push(
                if fits {
                    "Alt+Ctrl+D 查看完整详情"
                } else {
                    "清单含长参数 · Alt+Ctrl+D 逐页查看"
                }
                .into(),
            );
        }
        lines.push(
            if reviewable {
                "Alt+Ctrl+Enter 确认 · Esc 取消"
            } else {
                "尚未完整查看 · Esc 取消"
            }
            .into(),
        );
        if !valid {
            lines = vec![
                "Eva · 确认清单不可用".into(),
                "请取消并重新发起操作".into(),
                "Esc 取消".into(),
            ];
        }
        ConfirmationLayout {
            text: lines.join("\n"),
            pages,
            reviewable,
        }
    }

    /// Bottom center inside the activation monitor; fit even very small outputs.
    pub(crate) fn rect(&self, screen: [f32; 2]) -> [f32; 4] {
        if !self.active
            && self.dismissed.is_some()
            && let Some(rect) = self.last_rect.get()
        {
            return rect;
        }
        let [x, y, w, h] = self.viewport.unwrap_or([0.0, 0.0, screen[0], screen[1]]);
        let margin = self.outer_margin();
        let width = (w - margin).clamp(
            1.,
            if self.confirmation {
                880.
            } else if self.standby_card() {
                360.
            } else {
                480.
            },
        );
        let desired_h = if self.confirmation {
            let config = crate::config::CONFIG.load();
            let size = crate::backend::compositor_font::ui_font_pixel_size(config.system_ui_font());
            (self.confirmation_layout().text.lines().count() + 1) as f32 * (size * 1.5).ceil()
                + 32.0
        } else {
            (self
                .text_raster(Instant::now(), self.text_width(width))
                .height as f32
                + self.padding() * 2.
                + self.meter_space())
            .max(if self.standby_card() { 84. } else { 112. })
        };
        let height = (h - margin).clamp(1.0, desired_h);
        let rect = [
            x + (w - width) * 0.5,
            y + (h - height - margin).max(0.),
            width,
            height,
        ];
        self.last_rect.set(Some(rect));
        rect
    }
}

fn progress_label(value: &Value) -> Option<String> {
    let completed = value["completed"].as_u64()?;
    let total = value["total"].as_u64()?;
    let current = value["current_step"].as_u64()?;
    let stage = value["stage"].as_str()?;
    if !(1..=32).contains(&total)
        || completed > total
        || !(1..=total).contains(&current)
        || completed > current
    {
        return None;
    }
    if matches!(stage, "preparing" | "executing" | "verifying") && completed + 1 != current {
        return None;
    }
    let prefix = format!("已确认完成 {completed}/{total} 步");
    Some(match stage {
        "preparing" => format!("{prefix} · 正在准备第 {current} 步"),
        "executing" => format!("{prefix} · 正在执行第 {current} 步"),
        "verifying" => format!("{prefix} · 正在核验第 {current} 步"),
        "succeeded" if completed == total => prefix,
        "partial" => format!("{prefix} · 其余待处理"),
        "unknown" => format!("{prefix} · 第 {current} 步结果待确认"),
        "failed" => format!("{prefix} · 第 {current} 步未完成"),
        "cancelled" => format!("已停止 · {prefix}"),
        _ => return None,
    })
}
fn verified_progress_fraction(value: &Value) -> Option<f32> {
    progress_label(value)?;
    let completed = value["completed"].as_u64()?;
    let total = value["total"].as_u64()?;
    if completed == total && value["stage"] != "succeeded" {
        return None;
    }
    Some(completed as f32 / total as f32)
}

fn rasterize_sections(
    sections: Vec<TextSection>,
    font: &str,
    size: f32,
    width: u32,
    ink: [u8; 4],
    maximum_height: u32,
    layout_key: u64,
) -> EvaTextRaster {
    use crate::backend::compositor_font::{
        fit_ui_text_lines, render_ui_text_to_rgba, wrap_ui_text_lines,
    };
    let mut parts = Vec::new();
    let mut total_width = 1_u32;
    let mut total_height = 0_u32;
    for section in sections {
        let section_size = if section.heading {
            size * 1.12
        } else if section.secondary {
            (size * 0.88).max(14.)
        } else {
            size
        };
        let section_width = if section.heading && width > 160 {
            width.saturating_sub(42)
        } else {
            width
        };
        let mut lines = if section.heading {
            vec![fit_ui_text_lines(
                &section.text,
                font,
                section_size,
                section_width,
            )]
        } else {
            wrap_ui_text_lines(&section.text, font, section_size, section_width)
        };
        if lines.len() > section.rows {
            lines.truncate(section.rows);
            if let Some(last) = lines.last_mut() {
                *last = fit_ui_text_lines(&format!("{last}…"), font, section_size, section_width);
            }
        }
        let gap = if parts.is_empty() { 0 } else { 5 };
        let remaining = maximum_height.saturating_sub(total_height + gap);
        let (pixels, w, h) = loop {
            let fitted = fit_ui_text_lines(&lines.join("\n"), font, section_size, section_width);
            let raster = render_ui_text_to_rgba(&fitted, font, section_size, ink);
            if raster.2 <= remaining || lines.len() <= 1 {
                break raster;
            }
            lines.pop();
            if let Some(last) = lines.last_mut() {
                *last = fit_ui_text_lines(&format!("{last}…"), font, section_size, section_width);
            }
        };
        // Preserve native text size: omit lower-priority rows on tiny outputs
        // instead of scaling the entire card into unreadable miniature text.
        if w == 0 || h == 0 || h > remaining {
            continue;
        }
        total_height += gap;
        parts.push((pixels, w, h, total_height));
        total_width = total_width.max(w);
        total_height += h;
    }
    total_height = total_height.max(1);
    let mut pixels = vec![0_u8; total_width as usize * total_height as usize * 4];
    for (part, w, h, top) in parts {
        for row in 0..h as usize {
            let from = row * w as usize * 4;
            let to = (top as usize + row) * total_width as usize * 4;
            pixels[to..to + w as usize * 4].copy_from_slice(&part[from..from + w as usize * 4]);
        }
    }
    EvaTextRaster {
        pixels,
        width: total_width,
        height: total_height,
        layout_key,
    }
}

#[derive(Debug, Clone)]
struct ConfirmationPrepared {
    signature: u64,
    expanded: Vec<String>,
    short: Vec<String>,
    pages: usize,
    rows: usize,
    fits: bool,
    valid: bool,
    width: u32,
    line_h: f32,
}
struct ConfirmationLayout {
    text: String,
    pages: usize,
    reviewable: bool,
}
pub(crate) fn eva_nested_host_presentable(
    visible: Option<bool>,
    minimized: Option<bool>,
    presented: bool,
    focused: bool,
) -> bool {
    presented
        && visible != Some(false)
        && minimized != Some(true)
        && (visible == Some(true) || focused)
}
pub(crate) fn eva_output_presentable(enabled: bool, powered: bool, output: [f32; 4]) -> bool {
    enabled && powered && output.iter().all(|v| v.is_finite()) && output[2] > 0.0 && output[3] > 0.0
}
pub(crate) fn eva_viewport_on_output(viewport: [f32; 4], output: [f32; 4]) -> bool {
    eva_output_presentable(true, true, viewport)
        && eva_output_presentable(true, true, output)
        && viewport[0] >= output[0] - 1.0
        && viewport[1] >= output[1] - 1.0
        && viewport[0] + viewport[2] <= output[0] + output[2] + 1.0
        && viewport[1] + viewport[3] <= output[1] + output[3] + 1.0
}

/// Preserve the reserved ASR raster row. Generic fitting trims whitespace;
/// losing this final row would move every confirmation target when late ASR
/// arrives, despite an unchanged card size and page count.
pub(crate) fn fit_eva_label(text: &str, font: &str, size: f32, width: u32) -> String {
    let mut fitted = crate::backend::compositor_font::fit_ui_text_lines(text, font, size, width);
    if text.ends_with("\n ") && fitted.ends_with('\n') {
        fitted.push(' ');
    }
    fitted
}
fn clean(text: &str, limit: usize) -> String {
    text.chars()
        .take(limit)
        .flat_map(|c| {
            if c.is_control() || matches!(c,'\u{061c}'|'\u{200b}'..='\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2060}'..='\u{206f}'|'\u{feff}') {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn clean_message(text: &str, limit: usize) -> String {
    // Prose may contain natural line breaks. Fold whitespace without allowing
    // a message or caption to manufacture native microphone/shortcut rows.
    let normalized: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    clean(&normalized, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn standby_is_compact_and_result_headers_describe_the_local_outcome() {
        let now = Instant::now();
        let mut standby = EvaOverlayState::default();
        standby.update(&json!({"active":false,"visible":true,"state":"standby","microphone_active":true,"monitor_rect":[0,0,1280,720]}),now);
        let standby_rect = standby.rect([1280., 720.]);
        assert_eq!(standby_rect[2], 360.);
        assert!(standby_rect[3] < 120.);
        let mut active = EvaOverlayState::default();
        for (outcome, title) in [
            ("succeeded", "已完成"),
            ("partial", "部分完成"),
            ("clarify", "需要补充"),
            ("unknown", "结果待确认"),
            ("failed", "未能完成"),
            ("cancelled", "已停止"),
            ("noop", "正在回答"),
        ] {
            active.update(&json!({"active":true,"state":"responding","result_status":outcome,"text":"具体结果或需要补充的问题。","microphone_active":false,"monitor_rect":[0,0,1280,720]}),now);
            assert!(active.label(now).starts_with(&format!("Eva · {title}\n")));
            assert!(active.rect([1280., 720.])[2] > standby_rect[2]);
        }
    }
    #[test]
    fn actual_microphone_zero_unknown_and_verified_progress_are_independent() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        let mut snapshot = json!({"active":true,"state":"executing","microphone_active":true,"level":0.,"task_progress":{"completed":2,"total":4,"current_step":3,"stage":"verifying"}});
        state.update(&snapshot, now);
        assert_eq!(state.progress_fill(now), Some(0.5));
        assert_eq!(state.level(now), Some(0.));
        let card = [20., 20., 480., 160.];
        assert_eq!(
            state
                .mic_bars(card, now)
                .iter()
                .filter(|(_, on)| *on)
                .count(),
            0
        );
        snapshot["level"] = json!(0.4);
        state.update(&snapshot, now);
        assert_eq!(
            state
                .mic_bars(card, now)
                .iter()
                .filter(|(_, on)| *on)
                .count(),
            2
        );
        snapshot["microphone_active"] = Value::Null;
        state.update(&snapshot, now);
        assert!(state.label(now).contains("麦克风状态待确认"));
        assert!(state.mic_bars(card, now).is_empty());
        assert_eq!(state.progress_fill(now), Some(0.5));
        snapshot["task_progress"] =
            json!({"completed":4,"total":4,"current_step":4,"stage":"unknown"});
        state.update(&snapshot, now);
        assert_eq!(state.progress_fill(now), None);
        snapshot["task_progress"]["stage"] = json!("succeeded");
        state.update(&snapshot, now);
        assert_eq!(state.progress_fill(now), Some(1.));
        snapshot["state"] = json!("responding");
        snapshot["result_status"] = json!("unknown");
        state.update(&snapshot, now);
        assert_eq!(
            state.progress_fill(now),
            None,
            "a final unknown outcome must not display a completed task rail"
        );
        assert_eq!(state.progress_fill(now + Duration::from_secs(4)), None);
    }
    #[test]
    fn microphone_segments_stay_inside_tiny_and_confirmation_cards() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        state.update(
            &json!({"active":true,"state":"listening","microphone_active":true,"level":1.}),
            now,
        );
        for [w, h] in [[480., 160.], [72., 52.], [32., 24.], [20., 10.], [1., 1.]] {
            let rect = [-100., 30., w, h];
            for (bar, _) in state.mic_bars(rect, now) {
                assert!(bar[0] >= rect[0] && bar[1] >= rect[1]);
                assert!(bar[0] + bar[2] <= rect[0] + w && bar[1] + bar[3] <= rect[1] + h);
            }
        }
    }
    #[test]
    fn confirmation_reserved_caption_row_reports_microphone_without_changing_review_proof() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        let mut snapshot = json!({"active":true,"state":"confirming","microphone_active":true,"level":0.,"monitor_rect":[0,0,1280,720],
            "confirmation":{"total_actions":1,"display_items":[{"critical":format!("w1 set_config original-key = {}","原始配置值0123456789".repeat(200))}]}});
        let prepared_pages = EvaOverlayState::confirmation_page_count(&snapshot);
        assert!(prepared_pages > 1);
        snapshot["confirmation"]["details_seen_pages"] =
            json!((0..prepared_pages).collect::<Vec<_>>());
        state.update(&snapshot, now);
        let signature = state.confirmation_layout_signature();
        let pages = state.confirmation_page_count_cached();
        let rows = state.label(now).lines().count();
        let reviewable = state.confirmation_reviewable();
        assert!(reviewable);
        assert!(state.label(now).contains("麦克风已开启"));
        assert!(
            state
                .mic_bars(state.rect([1280., 720.]), now)
                .iter()
                .all(|(_, active)| !*active)
        );
        snapshot["microphone_active"] = json!(false);
        snapshot["input_caption"] = json!({"text":"实际提交的字幕"});
        state.update(&snapshot, now);
        assert!(
            state
                .label(now)
                .contains("麦克风已暂停 · 识别：实际提交的字幕")
        );
        assert_eq!(state.label(now).lines().count(), rows);
        assert_eq!(state.confirmation_layout_signature(), signature);
        assert_eq!(state.confirmation_page_count_cached(), pages);
        assert_eq!(state.confirmation_reviewable(), reviewable);
    }
    #[test]
    fn progress_is_not_duplicated_and_permanent_errors_do_not_offer_retry() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        let progress = json!({"completed":1,"total":3,"current_step":2,"stage":"executing"});
        state.update(&json!({"active":true,"state":"executing","microphone_active":false,"text":progress_label(&progress),"task_progress":progress}),now);
        assert_eq!(state.label(now).matches("已确认完成").count(), 1);
        let error = json!({"active":false,"visible":true,"state":"error","text":"请检查凭据后重新开启监听。","error_code":"auth_failed","retryable":false,"microphone_active":false});
        state.update(&error, now);
        assert_eq!(
            EvaOverlayState::local_actions(&error),
            vec![EvaLocalAction::Diagnostics]
        );
        assert!(!state.label(now).contains("重试"));
        assert!(state.label(now).contains("诊断"));
    }
    #[test]
    fn voice_fallback_keeps_success_and_uses_one_secondary_hint() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        state.update(&json!({"active":true,"state":"follow_up","result_status":"succeeded","text":"已切到第三工作区。","voice_fallback":true,"microphone_active":true,"level":0.}),now);
        let label = state.label(now);
        assert!(label.starts_with("Eva · 已完成\n"));
        assert!(label.contains("已切到第三工作区。"));
        assert_eq!(label.matches("语音回复暂不可用").count(), 1);
        assert!(!label.contains("未能完成"));
        assert!(label.contains("Alt+Ctrl+I 诊断"));
        let snapshot = json!({"active":true,"visible":true,"state":"follow_up","result_status":"succeeded","text":"已完成。","voice_fallback":true,"microphone_active":false,"undo_available":true});
        assert_eq!(
            EvaOverlayState::local_actions(&snapshot),
            vec![EvaLocalAction::Undo, EvaLocalAction::Diagnostics]
        );
        state.update(&snapshot, now);
        assert!(!state.label(now).contains("可以继续说"));
        assert!(!state.label(now).contains("还可说话"));
    }
    #[test]
    fn small_outputs_keep_native_text_size_and_height_changes_invalidate_layout_cache() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        let mut snapshot = json!({"active":true,"state":"responding","result_status":"partial","text":"这是具体执行结果，需要保留正常字号并在较小屏幕换行。".repeat(30),"microphone_active":true,"input_caption":{"text":"实际字幕"},"voice_fallback":true,"monitor_rect":[0,0,320,240]});
        for height in [240., 180.] {
            snapshot["monitor_rect"][3] = json!(height);
            state.update(&snapshot, now);
            let rect = state.rect([320., height]);
            let raster = state.text_raster(now, state.text_width(rect[2]));
            let positioned = state.text_rect(rect, [raster.width, raster.height]);
            assert_eq!(positioned[2], raster.width as f32);
            assert_eq!(
                positioned[3], raster.height as f32,
                "small outputs must wrap/omit secondary rows, never miniaturize all text"
            );
            assert!(state.label(now).contains("Esc 退出"));
        }
        snapshot["monitor_rect"] = json!([0, 0, 560, 720]);
        state.update(&snapshot, now);
        let high = state.text_raster(now, 480);
        snapshot["monitor_rect"] = json!([0, 0, 560, 360]);
        state.update(&snapshot, now);
        let low = state.text_raster(now, 480);
        assert_ne!(high.layout_key, low.layout_key);
        assert!(
            !std::sync::Arc::ptr_eq(&high, &low),
            "same-width height change must invalidate raster budget"
        );
    }
    #[test]
    fn task_progress_displays_verified_counts_and_changes_the_typography_cache() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        let mut snapshot = json!({"active":true,"state":"executing","microphone_active":false,
            "task_progress":{"completed":1,"total":5,"current_step":2,"stage":"verifying","action":"switch_tag"}});
        state.update(&snapshot, now);
        let label = state.label(now);
        assert!(label.contains("已确认完成 1/5 步"));
        assert!(label.contains("正在核验第 2 步"));
        let before = state.text_raster(now, 480);
        snapshot["task_progress"]["stage"] = json!("unknown");
        assert!(state.update(&snapshot, now));
        assert!(state.label(now).contains("第 2 步结果待确认"));
        assert!(!std::sync::Arc::ptr_eq(
            &before,
            &state.text_raster(now, 480)
        ));
        snapshot["task_progress"] = Value::Null;
        state.update(&snapshot, now);
        assert!(!state.label(now).contains("已确认完成"));
    }
    #[test]
    fn wake_standby_uses_adaptive_status_sections_and_its_own_exit_hint() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        state.update(
            &json!({"active":false,"visible":true,"state":"standby",
            "microphone_active":true,"monitor_rect":[0,0,560,720]}),
            now,
        );
        let label = state.label(now);
        assert!(label.starts_with("Eva · 唤醒待机\n"));
        assert!(label.contains("hi eva"));
        assert!(label.contains("麦克风已开启"));
        assert!(label.contains("快捷键关闭监听"));
        assert!(!label.contains("Esc"));
        assert!(!label.contains("还可说话"));
        let raster = state.text_raster(now, 480);
        assert!(raster.width <= 480);
        assert_eq!(
            raster.pixels.len(),
            raster.width as usize * raster.height as usize * 4
        );
    }
    #[test]
    fn eva_result_keeps_its_state_header_and_wraps_unicode_message() {
        let now = Instant::now();
        let question =
            "你是说正在播放视频的浏览器，还是用于查看项目文档的另一个浏览器窗口？".repeat(4);
        let mut state = EvaOverlayState::default();
        state.update(
            &json!({"active":true,"state":"follow_up","text":question,"microphone_active":true,"followup_remaining_ms":5000,"monitor_rect":[0,0,560,720]}),
            now,
        );
        let label = state.label(now);
        assert!(label.starts_with("Eva · 可以继续说\n"));
        assert!(label.contains(&question));
        assert!(label.contains("麦克风已开启"));
        let raster = state.text_raster(now, 480);
        assert!(raster.width <= 480);
        assert!(raster.height > 80);
        assert_eq!(
            raster.pixels.len(),
            raster.width as usize * raster.height as usize * 4
        );
        assert!(std::sync::Arc::ptr_eq(
            &raster,
            &state.text_raster(now, 480)
        ));
    }

    #[test]
    fn eva_audio_level_heartbeat_reuses_the_typography_cache() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        let mut snapshot =
            json!({"active":true,"state":"listening","microphone_active":true,"level":0.1});
        state.update(&snapshot, now);
        let before = state.text_raster(now, 512);
        snapshot["level"] = json!(0.8);
        state.update(&snapshot, now + Duration::from_millis(100));
        let after = state.text_raster(now + Duration::from_millis(100), 512);
        assert!(std::sync::Arc::ptr_eq(&before, &after));
        assert_eq!(state.level(now + Duration::from_millis(100)), Some(0.8));
    }

    #[test]
    fn eva_countdown_uses_followup_deadline_and_only_wakes_on_second_boundaries() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        state.update(
            &json!({"active":true,"state":"follow_up","microphone_active":true,"followup_remaining_ms":5000,"lease_remaining_ms":3000}),
            now,
        );
        state.settle_motion_for_test(now);
        state.mark_drawn(now);
        assert!(state.label(now).contains("还可说话 5 秒"));
        assert!(!state.needs_frame(now));
        assert_eq!(state.next_wakeup(now), Some(Duration::from_secs(1)));
        let later = now + Duration::from_millis(1100);
        assert!(state.label(later).contains("还可说话 4 秒"));
        assert!(state.needs_frame(later));
        state.mark_drawn(later);
        assert!(!state.needs_frame(later));
        assert_eq!(state.next_wakeup(later), Some(Duration::from_millis(900)));
        let expired = now + Duration::from_secs(4);
        assert!(!state.label(expired).contains("还可说话"));
        assert!(state.label(expired).contains("状态待确认"));
        assert!(!state.microphone_on(expired));
    }

    #[test]
    fn eva_absent_followup_deadline_never_invents_a_countdown() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        state.update(
            &json!({"active":true,"state":"follow_up","microphone_active":true}),
            now,
        );
        state.settle_motion_for_test(now);
        state.mark_drawn(now);
        assert!(!state.label(now).contains("还可说话"));
        assert_eq!(state.next_wakeup(now), Some(Duration::from_secs(3)));
    }

    #[test]
    fn eva_adaptive_text_stays_inside_small_and_offset_outputs() {
        let now = Instant::now();
        for [x, y, w, h] in [
            [0.0, 0.0, 320.0, 240.0],
            [-1920.0, 32.0, 1920.0, 1048.0],
            [0.0, 0.0, 80.0, 60.0],
        ] {
            let mut state = EvaOverlayState::default();
            state.update(
                &json!({"active":true,"state":"error","text":"麦克风暂不可用，请检查输入设备后重试。","microphone_active":false,"monitor_rect":[x,y,w,h]}),
                now,
            );
            let rect = state.rect([w, h]);
            let raster = state.text_raster(now, state.text_width(rect[2]));
            let [tx, ty, tw, th] = state.text_rect(rect, [raster.width, raster.height]);
            assert!(rect[0] >= x && rect[1] >= y);
            assert!(rect[0] + rect[2] <= x + w && rect[1] + rect[3] <= y + h);
            assert!(tx >= rect[0] && ty >= rect[1]);
            assert!(tx + tw <= rect[0] + rect[2] + 0.01);
            assert!(ty + th <= rect[1] + rect[3] + 0.01);
        }
    }

    #[test]
    fn eva_message_controls_cannot_replace_the_mic_or_shortcut_rows() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        state.update(
            &json!({"active":true,"state":"planning","text":"目标\u{202e}伪装\nEsc 已关闭","microphone_active":false}),
            now,
        );
        let label = state.label(now);
        assert!(label.contains("\\u{202e}"));
        assert!(!label.contains("\\u{a}"));
        assert!(label.contains("伪装 Esc 已关闭"));
        assert!(label.contains("麦克风已暂停"));
        assert!(label.contains("Esc 退出"));
    }

    #[test]
    fn lease_loss_never_claims_the_microphone_was_released() {
        let now = Instant::now();
        let mut overlay = EvaOverlayState::default();
        overlay.update(
            &json!({"active":true,"state":"listening","microphone_active":true,"lease_remaining_ms":3000,"level":2.0}),
            now,
        );
        assert_eq!(overlay.level(now), Some(1.0));
        assert!(overlay.label(now).contains("麦克风已开启"));
        let mut paused = overlay.clone();
        paused.update(
            &json!({"active":true,"state":"listening","microphone_active":false}),
            now,
        );
        assert!(!paused.microphone_on(now));
        assert!(paused.label(now).contains("已暂停"));

        assert!(
            overlay
                .label(now + Duration::from_secs(4))
                .contains("状态待确认")
        );
        assert!(!overlay.microphone_on(now + Duration::from_secs(4)));
        let mut failed = EvaOverlayState::default();
        failed.update(&json!({"active":false,"visible":true,"state":"error","text":"无法启动 Eva，请检查用户服务安装","microphone_active":null,"lease_remaining_ms":0}),now);
        assert!(!failed.expired(now));
        assert!(failed.label(now).contains("无法启动"));
        assert!(failed.label(now).contains("状态待确认"));
        assert!(!failed.label(now).contains("已暂停"));
        assert!(failed.label(now).contains("Alt+Ctrl+I"));
        overlay.update(&json!({"active":false}), now + Duration::from_secs(4));
        assert!(!overlay.session_active);
        assert!(!overlay.microphone_on(now + Duration::from_secs(4)));
        overlay.mark_drawn(now + Duration::from_secs(5));
        assert!(!overlay.visible());
    }
    #[test]
    fn motion_reuses_toast_times_and_never_extends_permission_or_lock_visibility() {
        let now = Instant::now();
        let mut state = EvaOverlayState::default();
        state.update_with_motion(
            &json!({"active":true,"state":"listening","microphone_active":true}),
            now,
            true,
        );
        assert_eq!(state.alpha_with_motion(now, true), 0.0);
        assert_eq!(state.alpha_with_motion(now, false), 1.0);
        let end = now + Duration::from_secs_f32(super::super::toast::TOAST_FADE_IN);
        assert!(state.alpha_with_motion(now + Duration::from_millis(80), true) > 0.0);
        assert_eq!(state.alpha_with_motion(end, true), 1.0);
        state.update_with_motion(
            &json!({"active":true,"state":"listening","microphone_active":true}),
            end,
            true,
        );
        assert_eq!(
            state.alpha_with_motion(end, true),
            1.0,
            "heartbeats must not restart appearance"
        );
        state.update_with_motion(&json!({"active":false}), end, true);
        assert!(!state.session_active);
        assert!(!state.microphone_on(end));
        assert!(state.label(end).contains("正在退出"));
        assert!(!state.label(end).contains("已开启"));
        let gone = end + Duration::from_secs_f32(super::super::toast::TOAST_DISMISS_FADE);
        assert_eq!(state.alpha_with_motion(gone, true), 0.0);
        state.mark_drawn(gone);
        assert!(!state.visible());
        state.update_with_motion(&json!({"active":true}), gone, true);
        state.update_with_motion(
            &json!({"active":false,"suppress_animation":true}),
            gone + Duration::from_millis(30),
            true,
        );
        assert!(
            !state.visible(),
            "locked/exiting sessions must erase immediately"
        );
    }

    #[test]
    fn zero_disabled_dark_and_clipped_outputs_do_not_authorize_a_visible_card() {
        let output = [0.0, 0.0, 1920.0, 1080.0];
        assert!(eva_output_presentable(true, true, output));
        assert!(!eva_output_presentable(false, true, output));
        assert!(!eva_output_presentable(true, false, output));
        assert!(!eva_output_presentable(true, true, [0.0, 0.0, 0.0, 1080.0]));
        assert!(!eva_output_presentable(
            true,
            true,
            [0.0, 0.0, f32::NAN, 1080.0]
        ));
        assert!(eva_viewport_on_output([0.0, 32.0, 1920.0, 1048.0], output));
        assert!(!eva_viewport_on_output(
            [0.0, 0.0, 1920.0, 1080.0],
            [0.0, 0.0, 1280.0, 720.0]
        ));
        assert!(!eva_viewport_on_output(
            [1920.0, 0.0, 1920.0, 1080.0],
            output
        ));
        assert!(clean("id\u{061c}\u{200e}\u{200f}value", 80).contains("\\u{61c}"));
        assert_eq!(
            clean("key\u{200b}value\u{feff}", 80),
            "key\\u{200b}value\\u{feff}"
        );
        assert!(eva_nested_host_presentable(
            Some(true),
            Some(false),
            true,
            false
        ));
        assert!(eva_nested_host_presentable(None, None, true, true));
        assert!(!eva_nested_host_presentable(None, None, false, true));
        assert!(!eva_nested_host_presentable(None, None, true, false));
        assert!(!eva_nested_host_presentable(
            Some(true),
            Some(true),
            true,
            true
        ));
        assert!(!eva_nested_host_presentable(Some(false), None, true, true));
    }

    #[test]
    fn confirmation_and_viewport_are_bounded() {
        let now = Instant::now();
        let mut overlay = EvaOverlayState::default();
        overlay.update(&json!({"active":true,"state":"confirming","confirmation":{"display_items":[{"critical":"w1 close window","title":"终端"}],"total_actions":1},"monitor_rect":[1920,0,1920,1080]}), now);
        assert!(overlay.label(now).contains("Alt+Ctrl+Enter"));
        assert!(!overlay.microphone_on(now));
        let rect = overlay.rect([3840.0, 1080.0]);
        assert!(rect[0] >= 1920.0 && rect[0] + rect[2] <= 3840.0);
        assert_eq!(rect[2], 880.0);
    }
    #[test]
    fn thirty_two_action_confirmation_requires_review_of_every_page() {
        let now = Instant::now();
        let items: Vec<_> = (1..=32)
            .map(|n| json!({"critical":format!("w{n} close_window → target-{n}")}))
            .collect();
        let mut snapshot = json!({"active":true,"state":"confirming","lease_remaining_ms":3000,"monitor_rect":[0,0,1280,720],"confirmation":{"total_actions":32,"display_items":items}});
        let pages = EvaOverlayState::confirmation_page_count(&snapshot);
        assert!(pages >= 4);
        assert!(!EvaOverlayState::confirmation_reviewable_snapshot(
            &snapshot
        ));
        let mut state = EvaOverlayState::default();
        state.update(&snapshot, now);
        assert_eq!(state.confirmation_items.len(), 32);
        let mut rendered = String::new();
        for page in 0..pages {
            snapshot["confirmation"]["details_page"] = json!(page);
            snapshot["confirmation"]["details_seen_pages"] = json!((0..=page).collect::<Vec<_>>());
            state.update(&snapshot, now);
            rendered.push_str(&state.label(now));
        }
        assert!(EvaOverlayState::confirmation_reviewable_snapshot(&snapshot));
        for n in 1..=32 {
            assert!(rendered.contains(&format!("w{n} close_window → target-{n}")));
        }
    }
    #[test]
    fn eight_confirmation_actions_preserve_identity_and_target_inside_bounds() {
        let now = Instant::now();
        let items: Vec<_> = (1..=8).map(|n|json!({"critical":format!("w{n}@g42 close_window → tag 8 · monitor m2"),"title":"一个很长的本地窗口标题".repeat(12)})).collect();
        let snapshot = json!({"active":true,"state":"confirming","lease_remaining_ms":3000,"monitor_rect":[1920,32,1920,1048],"confirmation":{"total_actions":8,"display_items":items}});
        let mut state = EvaOverlayState::default();
        state.update(&snapshot, now);
        let label = state.label(now);
        assert!(label.contains("8 项操作"));
        for n in 1..=8 {
            assert!(label.contains(&format!("w{n}@g42 close_window → tag 8 · monitor m2")));
        }
        assert!(EvaOverlayState::confirmation_reviewable_snapshot(&snapshot));
        let [x, y, w, h] = state.rect([3840.0, 1080.0]);
        assert!(x >= 1920.0 && x + w <= 3840.0 && y >= 32.0 && y + h <= 1080.0);
        assert!(h > 104.0);
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(font);
        let (_, tw, th) =
            crate::backend::compositor_font::render_ui_text_to_rgba(&label, font, size, [255; 4]);
        assert!(
            tw as f32 <= w - 40.0 && th as f32 <= h - 24.0,
            "confirmation text escapes card: {tw}x{th} in {w}x{h}"
        );
    }
    #[test]
    fn confirmation_keeps_batch_values_beyond_the_old_short_text_limits() {
        let critical = format!(
            "config changes = {} END_OF_ALL_VALUES",
            "完整配置值\\\"0123456789".repeat(3000)
        );
        assert!(critical.len() > 32 * 1024);
        let mut state = EvaOverlayState::default();
        state.update(&json!({"active":true,"state":"confirming","monitor_rect":[0,0,1280,720],"confirmation":{"total_actions":1,"display_items":[{"critical":critical}]}}),Instant::now());
        assert_eq!(state.confirmation_items[0].critical, critical);
        assert!(
            state.confirmation_items[0]
                .critical
                .ends_with("END_OF_ALL_VALUES")
        );
    }
    #[test]
    fn long_config_value_is_readable_in_full_before_local_confirmation() {
        let critical = format!(
            "config ui.wallpaper = {}",
            "路径与完整值0123456789/".repeat(80)
        );
        let mut snapshot = json!({"active":true,"state":"confirming","lease_remaining_ms":3000,"monitor_rect":[0,0,1280,720],"confirmation":{"total_actions":1,"display_items":[{"critical":critical}]}});
        let pages = EvaOverlayState::confirmation_page_count(&snapshot);
        let mut with_caption = snapshot.clone();
        with_caption["input_caption"] =
            json!({"item_id":"voice1","turn_epoch":1,"text":"真实输入字幕"});
        assert_eq!(
            pages,
            EvaOverlayState::confirmation_page_count(&with_caption)
        );
        let mut a = EvaOverlayState::default();
        a.update(&snapshot, Instant::now());
        let mut b = EvaOverlayState::default();
        b.update(&with_caption, Instant::now());
        assert_eq!(
            a.confirmation_layout_signature(),
            b.confirmation_layout_signature(),
            "late captions must never invalidate or reassign reviewed pages"
        );
        let config = crate::config::CONFIG.load();
        let font = config.system_ui_font();
        let size = crate::backend::compositor_font::ui_font_pixel_size(font);
        let no_caption = fit_eva_label(&a.label(Instant::now()), font, size, 836);
        let caption = fit_eva_label(&b.label(Instant::now()), font, size, 836);
        assert_eq!(
            crate::backend::compositor_font::render_ui_text_to_rgba(
                &no_caption,
                font,
                size,
                [255; 4]
            )
            .2,
            crate::backend::compositor_font::render_ui_text_to_rgba(&caption, font, size, [255; 4])
                .2,
            "late ASR must not move protected confirmation rows"
        );
        assert!(pages > 1);
        assert!(!EvaOverlayState::confirmation_reviewable_snapshot(
            &snapshot
        ));
        let mut rendered = String::new();
        for page in 0..pages {
            snapshot["confirmation"]["details_page"] = json!(page);
            snapshot["confirmation"]["details_seen_pages"] = json!((0..=page).collect::<Vec<_>>());
            let mut state = EvaOverlayState::default();
            state.update(&snapshot, Instant::now());
            let label = state.label(Instant::now());
            let lines: Vec<_> = label.lines().collect();
            rendered.push_str(&lines[1..lines.len() - 3].concat());
            assert!(
                !label.contains('…'),
                "critical config value was silently abbreviated"
            );
            assert_eq!(
                EvaOverlayState::confirmation_reviewable_snapshot(&snapshot),
                page + 1 == pages
            );
        }
        assert_eq!(rendered, format!("1. {critical}"));
        snapshot["monitor_rect"] = json!([0, 0, 80, 60]);
        let mut state = EvaOverlayState::default();
        state.update(&snapshot, Instant::now());
        let [x, y, w, h] = state.rect([80.0, 60.0]);
        assert!(x >= 0.0 && y >= 0.0 && x + w <= 80.0 && y + h <= 60.0);
        assert!(!EvaOverlayState::confirmation_reviewable_snapshot(
            &snapshot
        ));
    }
}
