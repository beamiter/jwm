// Input event handling: keyboard, mouse, and configure request processing

use crate::Jwm;
use crate::backend::api::{
    AllowMode, Backend, ExposeNavDirection, HitTarget, LayoutFilmCell, LayoutFilmstrip,
    SystemUiOverlay, SystemUiViewport, TagsGrid, WindowChanges, WindowType,
};
use crate::backend::common_define::{ConfigWindowBits, Mods, MouseButton, WindowId, keys};
use crate::backend::compositor_common::annotation_overlay::{AnnotationLabel, AnnotationQuad};
use crate::backend::compositor_common::screenshot_toolbar::{
    self, ScreenshotToolbar, ToolbarButton,
};
use crate::config::CONFIG;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use crate::jwm::features::expose_plan;
use crate::jwm::features::screenshot::{
    ScreenshotAnnotation, ScreenshotTool, ToolbarCommand, marker_ink,
};
use crate::jwm::features::tags_overview::live_cell;
use crate::jwm::features::{CaptureTarget, MonitorDirection};
use crate::jwm::rules::RuleMatcher;
use crate::jwm::types::{WMArgEnum, WMClickType, WMFuncType};
use log::{error, info};

const MAX_X11_CONFIGURE_VALUE: u32 = u16::MAX as u32;

fn bounded_configure_dimension(value: u32) -> i32 {
    value.clamp(1, MAX_X11_CONFIGURE_VALUE) as i32
}

fn bounded_configure_border(value: u32) -> i32 {
    value.min(MAX_X11_CONFIGURE_VALUE) as i32
}

fn clamp_configure_axis(position: i32, total: i32, origin: i32, span: i32) -> i32 {
    let minimum = i64::from(origin);
    let maximum = minimum + i64::from(span.max(0)) - i64::from(total.max(0));
    let clamped = if minimum <= maximum {
        i64::from(position).clamp(minimum, maximum)
    } else {
        minimum
    };
    clamped.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// An in-flight pointer drag on a control-center slider (Volume/Brightness):
/// armed by an in-bar press (click-to-position), driven by motion, disarmed
/// by the button release. Lives on [`Jwm`] rather than in the panel state
/// because it is a gesture, not panel content.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ControlSliderDrag {
    /// The slider being dragged.
    pub(crate) kind: crate::jwm::features::ControlKind,
    /// The last value queued on the controls worker. Motion pays the
    /// submission only when the rounded percent changes, and the worker folds
    /// a drag's worth of queued levels into the newest one.
    pub(crate) last_percent: u8,
    /// Root-x of the list texture's left edge: `root_x − text_x` at the last
    /// hit that carried an x. Cached because a drag can run off the card,
    /// where no hit arrives; refreshed from every motion over a row, so a
    /// card that widens mid-drag (a muted row's "mute" suffix becoming a
    /// percentage) cannot shift the bar out from under the pointer.
    pub(crate) text_origin_x: f64,
}

/// What a button press over a toast card does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToastPress {
    /// The card does not answer this button; the press goes on to whatever
    /// is below it.
    Ignored,
    /// Dismiss the card the press lands on, invoking nothing.
    Dismiss,
    /// Dismiss, and on an action chip invoke the action.
    Activate,
}

/// A card is opaque to every click, not only the left one: it docks right
/// under the status bar, exactly where a monitor's tab strip lies, so a
/// middle or right press that fell past it would close or focus the
/// strip's cell hidden under the card. Only the left button invokes an
/// action chip; every other button merely dismisses. The wheel (X11
/// buttons 4-7) is not a click and never dismisses a notification.
pub(crate) fn toast_press(button: MouseButton) -> ToastPress {
    match button {
        MouseButton::Left => ToastPress::Activate,
        MouseButton::Other(4..=7) => ToastPress::Ignored,
        MouseButton::Middle | MouseButton::Right | MouseButton::Other(_) => ToastPress::Dismiss,
    }
}

/// The argument of the `window_switcher` binding a chord stands for, if it
/// is one. While the switcher is up every key is routed to the panel, so
/// the "re-trigger steps the list" branch in `Jwm::window_switcher` is only
/// reachable from the keyboard through here: the panel's own key handler
/// asks whether the swallowed chord is the user's switcher binding and, if
/// so, re-triggers it. Lock modifiers are already stripped from `mods`;
/// the binding's mask is narrowed the same way the ordinary dispatch does.
pub(crate) fn switcher_binding_step(
    bindings: &[crate::jwm::types::WMKey],
    keysym: u32,
    mods: Mods,
) -> Option<WMArgEnum> {
    let key_mods = Mods::SHIFT
        | Mods::CONTROL
        | Mods::ALT
        | Mods::SUPER
        | Mods::MOD2
        | Mods::MOD3
        | Mods::MOD5;
    bindings
        .iter()
        .find(|binding| {
            keysym == binding.key_sym
                && (binding.mask & key_mods) == (mods & key_mods)
                && binding.func_opt.is_some_and(|func| {
                    std::ptr::fn_addr_eq(func, Jwm::window_switcher as WMFuncType)
                })
        })
        .map(|binding| binding.arg.clone())
}

/// A rectangle from two corners in any order, as `[x, y, w, h]`.
fn normalized_rect(from: (f32, f32), to: (f32, f32)) -> [f32; 4] {
    let x = from.0.min(to.0);
    let y = from.1.min(to.1);
    [x, y, (from.0 - to.0).abs(), (from.1 - to.1).abs()]
}

/// 0-255 ink as the 0-1 floats the compositor draws with.
fn linear_rgba(color: [u8; 4]) -> [f32; 4] {
    [
        f32::from(color[0]) / 255.0,
        f32::from(color[1]) / 255.0,
        f32::from(color[2]) / 255.0,
        f32::from(color[3]) / 255.0,
    ]
}

/// Black or white, whichever reads against a counter bubble of this color.
/// Rec. 601 luma, matching what the baked PNG uses so the preview and the file
/// never disagree about a numeral's color.
fn counter_ink(color: [u8; 4]) -> [f32; 4] {
    let luma =
        0.299 * f32::from(color[0]) + 0.587 * f32::from(color[1]) + 0.114 * f32::from(color[2]);
    if luma > 140.0 {
        [0.08, 0.08, 0.08, 1.0]
    } else {
        [1.0, 1.0, 1.0, 1.0]
    }
}

/// Parse direct launcher input when it is unambiguously a command.
///
/// Normal input requires both a known executable and at least one argument,
/// preserving multi-word application searches. Prefixing the input with `>`
/// explicitly selects command mode and also permits a single executable.
fn parse_direct_launcher_command(
    query: &str,
    executable_available: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let trimmed = query.trim();
    let (explicit, command_line) = match trimmed.strip_prefix('>') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };

    // `/` is the launcher's window-search prefix. An absolute path remains
    // available through explicit command mode: `> /path/to/program arg`.
    if command_line.is_empty() || (!explicit && command_line.starts_with('/')) {
        return None;
    }

    let command = crate::command_line::split_command_line(command_line).ok()?;
    if command.is_empty() || (!explicit && command.len() < 2) {
        return None;
    }
    executable_available(&command[0]).then_some(command)
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn launcher_program_available(
    program: &str,
    entries: &[crate::jwm::features::system_ui::LaunchEntry],
) -> bool {
    if program.contains('/') {
        return is_executable_file(std::path::Path::new(program));
    }

    entries.iter().any(|entry| {
        entry.command.first().is_some_and(|candidate| {
            candidate == program
                || std::path::Path::new(candidate)
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some(program)
        })
    })
}

fn direct_command_from_launcher(
    state: &crate::jwm::features::SystemUiState,
) -> Option<Vec<String>> {
    let crate::jwm::features::SystemUiState::Launcher { query, entries, .. } = state else {
        return None;
    };

    parse_direct_launcher_command(query, |program| {
        launcher_program_available(program, entries)
    })
}

fn choose_system_ui_viewport(
    locked: bool,
    selected_monitor: Option<(i32, i32, i32, i32)>,
    screen: (i32, i32),
) -> SystemUiViewport {
    let fullscreen = SystemUiViewport::fullscreen(screen.0, screen.1);
    if locked {
        return fullscreen;
    }
    selected_monitor
        .and_then(|(x, y, width, height)| SystemUiViewport::new(x, y, width, height))
        .unwrap_or(fullscreen)
}

impl Jwm {
    /// Note that a panel changed in memory. The frame tick pushes it.
    ///
    /// Callers that already have a backend may sync directly instead; this
    /// exists for the ones that do not, and for the ones several layers below
    /// the event loop where threading a backend through would be worse than
    /// the one-tick delay.
    pub(crate) fn mark_system_ui_dirty(&mut self) {
        self.system_ui_dirty = true;
    }

    /// Push a panel that was rebuilt since the last frame. Costs a boolean
    /// test when nothing changed.
    pub(crate) fn flush_system_ui(&mut self, backend: &mut dyn Backend) {
        // A control read-back that contradicts the OSD card on screen — or
        // the first card for a press that had nothing to estimate from — is
        // queued by the frame tick's control-feedback poll, which has no
        // backend; this flush is its backend-carrying counterpart. Runs ahead
        // of the dirty test: the OSD is not the panel.
        if let Some(correction) = self.features.control_feedback.take_pending_osd() {
            use crate::jwm::features::system_controls::ControlDomain;
            let kind = match correction.domain {
                ControlDomain::Volume if correction.muted => {
                    Some(crate::backend::api::OsdKind::VolumeMuted)
                }
                ControlDomain::Volume => Some(crate::backend::api::OsdKind::Volume),
                ControlDomain::Brightness => Some(crate::backend::api::OsdKind::Brightness),
                ControlDomain::MicMute => {
                    Some(crate::backend::api::OsdKind::MicMute(correction.muted))
                }
                // A device switch never queues an OSD correction: its
                // feedback is the picker's re-read rows, not a value card.
                ControlDomain::AudioDevice => None,
            };
            if let Some(kind) = kind {
                backend.compositor_show_osd(kind, correction.percent);
            }
        }
        // The tags grid describes one monitor. When the selection moved to
        // another one underneath it — IPC `focus_monitor`, an activation on
        // the other screen; neither arranges — the cells, the pushed
        // viewport and the hit-test have to move together, so the rebuild
        // is queued here, ahead of the dirty test that then pushes it.
        if self.tags_overview_follows_another_monitor() {
            self.refresh_tags_overview();
        }
        if !self.system_ui_dirty {
            return;
        }
        if self.features.system_ui.is_active() {
            self.sync_system_ui(backend);
            return;
        }
        // Dirty with nothing to draw is one thing: a hand-over whose incoming
        // panel never arrived (see `Jwm::hand_over_system_ui`). The outgoing
        // panel is gone but its grabs are not, and the compositor is still
        // drawing it — so finish the close rather than leaving the session
        // unable to type. Every other `mark_system_ui_dirty` caller rebuilds a
        // panel that is still on screen.
        self.system_ui_dirty = false;
        self.close_system_ui(backend);
    }

    pub(crate) fn sync_system_ui(&mut self, backend: &mut dyn Backend) {
        self.system_ui_dirty = false;
        let active = self.features.system_ui.is_active();
        let viewport = self.system_ui_viewport();
        backend.compositor_set_system_ui(active.then(|| {
            let mut parts = self.features.system_ui.overlay_parts();
            if direct_command_from_launcher(&self.features.system_ui).is_some() {
                let typed = parts
                    .query
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                parts.title = "\u{f120}  COMMAND".into();
                parts.items = vec![format!("\u{f120}  Run command  {typed}")];
                parts.selected = Some(0);
                parts.hint = "Click/Enter  run command    Esc  close".into();
                // One synthesised row replaces the match list, so whatever the
                // launcher was scrolled to no longer describes what is drawn.
                parts.scroll = None;
            }
            SystemUiOverlay {
                title: parts.title,
                query: parts.query,
                items: parts.items,
                selected: parts.selected,
                hint: parts.hint,
                scroll: parts.scroll,
                locked: self.features.system_ui.is_locked(),
                viewport,
                filmstrip: self.features.system_ui.layout_picker().map(|picker| {
                    let now = std::time::Instant::now();
                    LayoutFilmstrip {
                        cells: picker
                            .layouts
                            .iter()
                            .zip(&picker.previews)
                            .map(|(layout, windows)| LayoutFilmCell {
                                windows: windows.clone(),
                                // The fullscreen layout is the one that takes
                                // the bar down with it.
                                shows_bar: !layout.is_fullscreen_layout(),
                            })
                            .collect(),
                        selected: picker.selected,
                        countdown: picker.countdown(now),
                    }
                }),
                tags_grid: self
                    .features
                    .system_ui
                    .tags_overview()
                    .map(|overview| TagsGrid {
                        cells: overview.cells.clone(),
                        cols: overview.cols,
                        selected: overview.selected,
                        // The on-screen tag's cell draws live window textures;
                        // the compositor falls back to the wireframe for any
                        // window whose texture it cannot find.
                        live: live_cell(overview),
                    }),
                // The wallpaper picker's side preview tracks the highlight:
                // the row key is the path the picker would apply, so the
                // compositor decodes exactly what an Enter would load. Any
                // other panel — or none — carries no path and retires it.
                side_preview: self
                    .features
                    .system_ui
                    .selected_wallpaper()
                    .map(str::to_string),
            }
        }));
        backend.compositor_force_full_redraw();
    }

    /// Global output rectangle for ordinary system UI. The lock screen is a
    /// separate security surface and always returns the full virtual desktop;
    /// missing or invalid selected-monitor state safely falls back there too.
    pub(crate) fn system_ui_viewport(&self) -> SystemUiViewport {
        let selected_monitor = self.state.sel_mon.and_then(|key| {
            self.state.monitors.get(key).map(|monitor| {
                let geometry = &monitor.geometry;
                (geometry.m_x, geometry.m_y, geometry.m_w, geometry.m_h)
            })
        });
        choose_system_ui_viewport(
            self.features.system_ui.is_locked(),
            selected_monitor,
            (self.s_w, self.s_h),
        )
    }

    /// The live caps-lock state, read back from the backend's current
    /// modifier mask; `None` when the backend cannot report it right now.
    fn current_caps_lock(backend: &dyn Backend) -> Option<bool> {
        let (_, _, mask, _) = backend.input_ops().query_pointer_root().ok()?;
        Some(backend.key_ops().clean_mods(mask).contains(Mods::CAPS))
    }

    fn system_ui_char(keysym: u32, mods: Mods) -> Option<char> {
        let mut ch = char::from_u32(xkbcommon::xkb::keysym_to_utf32(
            xkbcommon::xkb::Keysym::new(keysym),
        ))?;
        let shifted = mods.contains(Mods::SHIFT);
        let caps = mods.contains(Mods::CAPS);
        if ch.is_ascii_alphabetic() {
            ch = if shifted ^ caps {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            };
            return Some(ch);
        }
        if shifted {
            ch = match ch {
                '1' => '!',
                '2' => '@',
                '3' => '#',
                '4' => '$',
                '5' => '%',
                '6' => '^',
                '7' => '&',
                '8' => '*',
                '9' => '(',
                '0' => ')',
                '-' => '_',
                '=' => '+',
                '[' => '{',
                ']' => '}',
                '\\' => '|',
                ';' => ':',
                '\'' => '"',
                ',' => '<',
                '.' => '>',
                '/' => '?',
                '`' => '~',
                other => other,
            };
        }
        // XKB Unicode keysyms already identify the character selected by an
        // international layout. Restricting this to ASCII made launcher
        // searches and Wi-Fi passphrases reject accented and CJK input even
        // though the font/raster path is UTF-8 throughout.
        (!ch.is_control()).then_some(ch)
    }

    /// Enter on the lock screen: hand the password field to the PAM worker.
    /// A wrong password costs ~2s inside `pam_authenticate` on stock
    /// pam_unix — and pam_sss, fingerprint or faillock modules wait
    /// arbitrarily long — so running it here would freeze the whole
    /// compositor; the frame tick's [`Self::poll_lock_auth_job`] adopts the
    /// outcome instead. One attempt at a time: while the worker holds a
    /// password, Enter is dead and the field keeps collecting the next try.
    /// Esc is untouched — it still only clears the field, never the worker.
    fn submit_lock_password(&mut self) {
        let Some(password) = self.features.system_ui.begin_authentication() else {
            return;
        };
        let job = crate::jwm::features::system_ui::start_authentication(password);
        let job = self.track_background_job(job);
        self.features.system_ui.track_authentication(job);
    }

    /// Adopt the lock screen's finished PAM authentication. Called from the
    /// frame tick beside the other worker polls; does nothing while the
    /// worker is still running, and nothing at all once the lock is gone —
    /// the attempt lives in the lock state, so a dismissed lock took its
    /// worker handle with it. Success runs exactly the path the inline
    /// authentication took (`close_system_ui`), failure shows the same row
    /// it did; the worker has already wiped the password on either outcome.
    pub(crate) fn poll_lock_auth_job(&mut self, backend: &mut dyn Backend) {
        use crate::jwm::features::system_ui::AuthPoll;
        match self.features.system_ui.poll_authentication() {
            AuthPoll::Pending => {}
            AuthPoll::Completed(true) => self.close_system_ui(backend),
            AuthPoll::Completed(false) => {
                self.features.system_ui.authentication_failed();
                self.sync_system_ui(backend);
            }
            AuthPoll::Refused => {
                // The OS refused the worker a thread: no authentication
                // ran, so the progress row comes down and Enter may retry.
                self.features.system_ui.authentication_aborted();
                self.sync_system_ui(backend);
            }
        }
    }

    pub(crate) fn sync_screenshot_annotation_style(&self, backend: &mut dyn Backend) {
        let color = self.features.screenshot.color;
        backend.compositor_set_annotation_color([
            color[0] as f32 / 255.0,
            color[1] as f32 / 255.0,
            color[2] as f32 / 255.0,
            color[3] as f32 / 255.0,
        ]);
        backend.compositor_set_annotation_line_width(self.features.screenshot.line_width as f32);
    }

    fn emit_screenshot_polyline(
        backend: &mut dyn Backend,
        color: [u8; 4],
        width: u32,
        points: &[(f32, f32)],
    ) {
        if points.len() < 2 {
            return;
        }
        backend.compositor_set_annotation_color([
            color[0] as f32 / 255.0,
            color[1] as f32 / 255.0,
            color[2] as f32 / 255.0,
            color[3] as f32 / 255.0,
        ]);
        backend.compositor_set_annotation_line_width(width as f32);
        backend.compositor_annotation_begin_stroke();
        for &(x, y) in points {
            backend.compositor_annotation_add_point(x, y);
        }
    }

    fn emit_screenshot_annotation(backend: &mut dyn Backend, annotation: &ScreenshotAnnotation) {
        match annotation {
            ScreenshotAnnotation::Freehand {
                points,
                color,
                width,
            } => Self::emit_screenshot_polyline(backend, *color, *width, points),
            ScreenshotAnnotation::Line {
                from,
                to,
                color,
                width,
            } => Self::emit_screenshot_polyline(backend, *color, *width, &[*from, *to]),
            ScreenshotAnnotation::Arrow {
                from,
                to,
                color,
                width,
            } => {
                Self::emit_screenshot_polyline(backend, *color, *width, &[*from, *to]);
                let angle = (from.1 - to.1).atan2(from.0 - to.0);
                let head = (*width as f32 * 4.0).max(14.0);
                for offset in [0.55_f32, -0.55_f32] {
                    let p = (
                        to.0 + (angle + offset).cos() * head,
                        to.1 + (angle + offset).sin() * head,
                    );
                    Self::emit_screenshot_polyline(backend, *color, *width, &[*to, p]);
                }
            }
            ScreenshotAnnotation::Rectangle {
                from,
                to,
                color,
                width,
            } => {
                let x0 = from.0.min(to.0);
                let y0 = from.1.min(to.1);
                let x1 = from.0.max(to.0);
                let y1 = from.1.max(to.1);
                let points = [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)];
                Self::emit_screenshot_polyline(backend, *color, *width, &points);
            }
            ScreenshotAnnotation::Ellipse {
                from,
                to,
                color,
                width,
            } => {
                let cx = (from.0 + to.0) * 0.5;
                let cy = (from.1 + to.1) * 0.5;
                let rx = (from.0 - to.0).abs() * 0.5;
                let ry = (from.1 - to.1).abs() * 0.5;
                if rx < 1.0 || ry < 1.0 {
                    return;
                }
                let mut points = Vec::with_capacity(65);
                for i in 0..=64 {
                    let t = i as f32 / 64.0 * std::f32::consts::TAU;
                    points.push((cx + rx * t.cos(), cy + ry * t.sin()));
                }
                Self::emit_screenshot_polyline(backend, *color, *width, &points);
            }
            ScreenshotAnnotation::Marker {
                points,
                color,
                width,
            } => Self::emit_screenshot_polyline(backend, *color, *width, points),
            ScreenshotAnnotation::FilledRectangle { from, to, color } => {
                let [x, y, w, h] = normalized_rect(*from, *to);
                backend.compositor_annotation_add_quad(AnnotationQuad {
                    x,
                    y,
                    w,
                    h,
                    radius: 0.0,
                    color: linear_rgba(*color),
                });
            }
            ScreenshotAnnotation::Pixelate { from, to, block } => {
                // Blocks are baked into the PNG, not previewed one by one — a
                // large region would be thousands of quads redrawn on every
                // pointer motion. A scrim plus a sampled grid says "this will
                // become blocks this big" for two draw calls.
                let rect = normalized_rect(*from, *to);
                backend.compositor_annotation_add_quad(AnnotationQuad {
                    x: rect[0],
                    y: rect[1],
                    w: rect[2],
                    h: rect[3],
                    radius: 0.0,
                    color: [0.05, 0.05, 0.07, 0.55],
                });
                Self::emit_region_grid(backend, rect, *block as f32);
            }
            ScreenshotAnnotation::Invert { from, to } => {
                let [x, y, w, h] = normalized_rect(*from, *to);
                backend.compositor_annotation_add_quad(AnnotationQuad {
                    x,
                    y,
                    w,
                    h,
                    radius: 0.0,
                    color: [0.85, 0.85, 0.9, 0.45],
                });
                Self::emit_screenshot_polyline(
                    backend,
                    [255, 255, 255, 255],
                    1,
                    &[(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)],
                );
            }
            ScreenshotAnnotation::Counter {
                at,
                number,
                color,
                radius,
            } => {
                backend.compositor_annotation_add_quad(AnnotationQuad::disc(
                    at.0,
                    at.1,
                    *radius,
                    linear_rgba(*color),
                ));
                backend.compositor_annotation_add_text(AnnotationLabel {
                    x: at.0,
                    y: at.1,
                    size: (*radius * 1.15).max(8.0),
                    color: counter_ink(*color),
                    text: number.to_string(),
                    anchor_center: true,
                });
            }
            ScreenshotAnnotation::Text {
                at,
                text,
                color,
                size,
            } => backend.compositor_annotation_add_text(AnnotationLabel {
                x: at.0,
                y: at.1,
                size: *size,
                color: linear_rgba(*color),
                text: text.clone(),
                anchor_center: false,
            }),
        }
    }

    /// Draw a grid over `rect` with roughly `stride`-pixel cells, sampled down
    /// so a large region never turns into hundreds of strokes.
    fn emit_region_grid(backend: &mut dyn Backend, rect: [f32; 4], stride: f32) {
        const MAX_LINES: f32 = 24.0;
        let [x, y, w, h] = rect;
        let stride_x = stride.max(w / MAX_LINES).max(2.0);
        let stride_y = stride.max(h / MAX_LINES).max(2.0);
        let ink = [255, 255, 255, 150];
        let mut gx = x + stride_x;
        while gx < x + w {
            Self::emit_screenshot_polyline(backend, ink, 1, &[(gx, y), (gx, y + h)]);
            gx += stride_x;
        }
        let mut gy = y + stride_y;
        while gy < y + h {
            Self::emit_screenshot_polyline(backend, ink, 1, &[(x, gy), (x + w, gy)]);
            gy += stride_y;
        }
    }

    pub(crate) fn sync_screenshot_annotation_overlay(
        &self,
        backend: &mut dyn Backend,
        include_current: bool,
    ) {
        if !backend.has_compositor()
            || !self.features.screenshot.active
            || !self.features.screenshot.committed
        {
            return;
        }
        backend.compositor_set_annotation_mode(false);
        backend.compositor_set_annotation_mode(true);
        for annotation in &self.features.screenshot.annotations {
            Self::emit_screenshot_annotation(backend, annotation);
        }
        if include_current {
            if let Some(annotation) = self.features.screenshot.current_annotation_preview() {
                Self::emit_screenshot_annotation(backend, &annotation);
            }
        }
        // The label being typed is always shown, drag or no drag: you cannot
        // type blind.
        if let Some(annotation) = self.features.screenshot.text_draft_preview() {
            Self::emit_screenshot_annotation(backend, &annotation);
        }
        self.sync_screenshot_annotation_style(backend);
        backend.compositor_force_full_redraw();
    }

    /// Rebuild the toolbar from the current editor state and publish it.
    ///
    /// The model is stored back into `ScreenshotState` because the hit test
    /// has to run against exactly the rectangles that were painted — deriving
    /// them a second time at click time is how a button ends up doing its
    /// neighbour's job.
    pub(crate) fn sync_screenshot_toolbar(&mut self, backend: &mut dyn Backend) {
        if !self.features.screenshot.active || !self.features.screenshot.committed {
            if self.features.screenshot.toolbar.take().is_some() {
                backend.compositor_set_screenshot_toolbar(None);
            }
            return;
        }
        let Some(selection) = self.features.screenshot.get_selection_rect() else {
            if self.features.screenshot.toolbar.take().is_some() {
                backend.compositor_set_screenshot_toolbar(None);
            }
            return;
        };

        let entries = self.features.screenshot.toolbar_entries();
        let hovered = self.features.screenshot.hovered_button;
        let mut buttons: Vec<ToolbarButton> = entries.into_iter().map(|e| e.button).collect();
        if let Some(index) = hovered {
            if let Some(button) = buttons.get_mut(index) {
                button.hovered = true;
            }
        }

        let screen = [0.0, 0.0, self.s_w as f32, self.s_h as f32];
        let button_size = screenshot_toolbar::fit_button_size(
            &buttons,
            screen[2] - 2.0 * screenshot_toolbar::SCREEN_MARGIN,
        );
        let extent = screenshot_toolbar::track_extent(&buttons, button_size);
        let bar = screenshot_toolbar::place(
            [
                selection.x as f32,
                selection.y as f32,
                selection.w as f32,
                selection.h as f32,
            ],
            screen,
            extent,
        );

        let toolbar = ScreenshotToolbar {
            bar,
            button_size,
            buttons,
            // The compositor owns the hover envelope: every published strip
            // starts it fresh.
            hover_ease: Default::default(),
        };
        if self.features.screenshot.toolbar.as_ref() == Some(&toolbar) {
            return;
        }
        self.features.screenshot.toolbar = Some(toolbar.clone());
        backend.compositor_set_screenshot_toolbar(Some(toolbar));
        backend.compositor_force_full_redraw();
    }

    /// Run one toolbar command and republish everything it changed.
    ///
    /// The three commands that end the capture return early: `finish` and
    /// `cancel` already tear the editor down, and re-syncing an overlay that
    /// no longer exists would put the strip back on screen for a frame — the
    /// frame the compositor captures.
    pub(crate) fn apply_screenshot_toolbar_command(
        &mut self,
        backend: &mut dyn Backend,
        command: ToolbarCommand,
    ) {
        match command {
            ToolbarCommand::SelectTool(tool) => {
                // Leaving the text tool finishes whatever was being typed
                // rather than dropping it on the floor.
                if self.features.screenshot.tool == ScreenshotTool::Text
                    && tool != ScreenshotTool::Text
                {
                    self.features.screenshot.commit_text_draft();
                }
                self.features.screenshot.set_tool(tool);
            }
            ToolbarCommand::Thinner => self.features.screenshot.decrease_line_width(),
            ToolbarCommand::Thicker => self.features.screenshot.increase_line_width(),
            ToolbarCommand::NextColor => self.features.screenshot.next_palette_color(),
            ToolbarCommand::Undo => self.features.screenshot.undo_annotation(),
            ToolbarCommand::Redo => self.features.screenshot.redo_annotation(),
            ToolbarCommand::Copy => return self.finish_screenshot_select(backend, true),
            ToolbarCommand::Save => return self.finish_screenshot_select(backend, false),
            ToolbarCommand::Cancel => return self.cancel_screenshot_select(backend),
        }
        self.sync_screenshot_annotation_style(backend);
        self.sync_screenshot_annotation_overlay(backend, true);
        self.sync_screenshot_toolbar(backend);
    }

    /// Which toolbar button, if any, is under `(x, y)`.
    pub(crate) fn screenshot_toolbar_hit(&self, x: f64, y: f64) -> Option<usize> {
        let toolbar = self.features.screenshot.toolbar.as_ref()?;
        screenshot_toolbar::button_at(
            toolbar.bar,
            &toolbar.buttons,
            toolbar.button_size,
            x as f32,
            y as f32,
        )
    }

    /// Whether `(x, y)` is anywhere on the toolbar, button or padding. A press
    /// in the gap between two buttons still belongs to the strip and must not
    /// start drawing on the canvas underneath it.
    pub(crate) fn screenshot_toolbar_contains(&self, x: f64, y: f64) -> bool {
        self.features
            .screenshot
            .toolbar
            .as_ref()
            .is_some_and(|toolbar| {
                screenshot_toolbar::hits_toolbar(toolbar.bar, x as f32, y as f32)
            })
    }

    /// Key handling while the control center is open: Up/Down move between
    /// rows, Left/Right drive sliders, Return/space activates toggles.
    fn handle_control_center_key(
        &mut self,
        backend: &mut dyn Backend,
        control: crate::jwm::features::ControlKind,
        keysym: u32,
        mods: Mods,
    ) {
        use crate::jwm::features::{ControlKind, SLIDER_STEP, ShellHubRoute, system_controls};

        let command_mods = Mods::CONTROL | Mods::ALT | Mods::SUPER;
        let route = (!mods.intersects(command_mods))
            .then(|| Self::system_ui_char(keysym, mods))
            .flatten()
            .and_then(ShellHubRoute::from_shortcut);
        if let Some(route) = route {
            if let Err(error) = self.open_shell_hub_route(backend, route) {
                log::debug!("shell hub route {}: {error}", route.label());
            }
            return;
        }

        let slider_delta = match keysym {
            keys::KEY_Left => Some(-SLIDER_STEP),
            keys::KEY_Right => Some(SLIDER_STEP),
            _ => None,
        };
        let activate = keysym == keys::KEY_Return || keysym == keys::KEY_space;

        if keysym == keys::KEY_Up {
            self.features.system_ui.move_selection(-1);
        } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
            self.features.system_ui.move_selection(1);
        } else {
            match control {
                ControlKind::Shell(route) => {
                    if activate {
                        if let Err(error) = self.open_shell_hub_route(backend, route) {
                            log::debug!("shell hub route {}: {error}", route.label());
                        }
                        return;
                    }
                }
                ControlKind::Media => {
                    // Left/Right skip tracks rather than adjusting a value;
                    // the bridge pushes the new state back, which refreshes
                    // this row.
                    let command = if keysym == keys::KEY_Left {
                        Some(crate::jwm::features::MediaCommand::Previous)
                    } else if keysym == keys::KEY_Right {
                        Some(crate::jwm::features::MediaCommand::Next)
                    } else if activate {
                        Some(crate::jwm::features::MediaCommand::PlayPause)
                    } else {
                        None
                    };
                    if let Some(command) = command
                        && let Err(error) = self.send_media_command(command)
                    {
                        log::debug!("control center media: {error}");
                    }
                }
                ControlKind::Volume => {
                    if let Some(delta) = slider_delta {
                        self.adjust_control_slider(ControlKind::Volume, delta);
                    } else if (activate || keysym == keys::KEY_m)
                        && let Some((_, Some(state))) = self
                            .queue_volume_request(system_controls::ControlRequest::VolumeToggleMute)
                    {
                        self.features.system_ui.update_control(
                            ControlKind::Volume,
                            state.percent,
                            state.muted,
                        );
                    }
                }
                ControlKind::Brightness => {
                    if let Some(delta) = slider_delta {
                        self.adjust_control_slider(ControlKind::Brightness, delta);
                    }
                }
                ControlKind::Network => {
                    use crate::jwm::features::connectivity::{self, NetworkRowAction};

                    let radio_on = self
                        .features
                        .connectivity
                        .network
                        .as_ref()
                        .is_some_and(|state| state.wifi_enabled);
                    match connectivity::plan_network_row(radio_on, activate, slider_delta.is_some())
                    {
                        NetworkRowAction::OpenPicker => {
                            if let Some(scan) = connectivity::start_scan() {
                                self.features.wifi_scan = Some(self.track_background_job(scan));
                                self.features.system_ui_return_to_hub = true;
                                self.features.system_ui =
                                    crate::jwm::features::SystemUiState::wifi_picker(
                                        "Scanning\u{2026}",
                                    );
                                self.sync_system_ui(backend);
                                return;
                            }
                        }
                        NetworkRowAction::EnableRadio => {
                            // Off the event thread: the worker flips the
                            // radio and re-reads, and the row lands on the
                            // truth — the radio may be hard-blocked and
                            // refuse to come back on.
                            self.request_radio_set(connectivity::RadioKind::Wifi, true);
                        }
                        NetworkRowAction::SetRadio(enabled) => {
                            self.request_radio_set(connectivity::RadioKind::Wifi, enabled);
                        }
                        NetworkRowAction::Nothing => {}
                    }
                }
                ControlKind::Bluetooth => {
                    use crate::jwm::features::connectivity::{self, BluetoothRowAction};

                    let powered = self.features.connectivity.bluetooth.powered;
                    match connectivity::plan_bluetooth_row(
                        powered,
                        activate,
                        slider_delta.is_some(),
                    ) {
                        BluetoothRowAction::OpenPicker => {
                            if let Some(scan) = connectivity::start_device_scan() {
                                self.features.bluetooth_scan =
                                    Some(self.track_background_job(scan));
                                self.features.system_ui_return_to_hub = true;
                                self.features.system_ui =
                                    crate::jwm::features::SystemUiState::bluetooth_picker(
                                        "Reading devices\u{2026}",
                                    );
                                self.sync_system_ui(backend);
                                return;
                            }
                        }
                        BluetoothRowAction::PowerOn => {
                            self.request_radio_set(connectivity::RadioKind::Bluetooth, true);
                        }
                        // Powering down can take a Bluetooth keyboard with it,
                        // so `activate_control` withholds it until a second
                        // press confirms.
                        BluetoothRowAction::SetPower(false) => {
                            if self.features.system_ui.activate_control().is_some() {
                                self.request_radio_set(connectivity::RadioKind::Bluetooth, false);
                            }
                        }
                        BluetoothRowAction::SetPower(true) => {
                            self.request_radio_set(connectivity::RadioKind::Bluetooth, true);
                        }
                        BluetoothRowAction::Nothing => {}
                    }
                }
                ControlKind::AudioOutput | ControlKind::AudioInput => {
                    if activate {
                        let direction = if control == ControlKind::AudioOutput {
                            system_controls::AudioDirection::Output
                        } else {
                            system_controls::AudioDirection::Input
                        };
                        let devices = system_controls::audio_devices(direction);
                        if !devices.is_empty() {
                            // Swap the panel for the picker; the grabs stay.
                            self.features.system_ui_return_to_hub = true;
                            self.features.system_ui =
                                crate::jwm::features::SystemUiState::audio_picker(
                                    direction, &devices,
                                );
                            self.sync_system_ui(backend);
                            return;
                        }
                    }
                }
                ControlKind::Battery
                | ControlKind::Cpu
                | ControlKind::Memory
                | ControlKind::NetworkThroughput => {
                    // Read-only: the row is information, not a control.
                }
                ControlKind::PowerProfile => {
                    let profiles = self
                        .features
                        .control_snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.power_profiles.clone());
                    if let Some(delta) = slider_delta
                        && let Some((available, active)) = profiles
                        && let Some(next) = crate::jwm::features::power::cycle_profile(
                            &available,
                            &active,
                            delta.signum() as isize,
                        )
                        && crate::jwm::features::power::set_profile(&next)
                    {
                        // The mutation is authoritative until the next worker
                        // verifies it; its epoch prevents an older read from
                        // rolling the row back.
                        self.cache_control_power_profiles(available, next);
                        self.refresh_open_control_center();
                    }
                }
                ControlKind::NightLight => {
                    if activate {
                        let enabled = !self.night_light_active();
                        self.set_night_light_override(backend, enabled);
                        self.features
                            .system_ui
                            .update_control(ControlKind::NightLight, 0, enabled);
                    }
                }
                ControlKind::DoNotDisturb => {
                    if activate {
                        // Through the toggle so the `dnd/toggle` broadcast
                        // happens here exactly as it does from a keybinding;
                        // a bar subscribed to it would otherwise keep drawing
                        // the state this row just left.
                        let _ = self.toggle_dnd(backend, &WMArgEnum::Int(0));
                        let enabled = self.do_not_disturb;
                        self.features.system_ui.update_control(
                            ControlKind::DoNotDisturb,
                            0,
                            enabled,
                        );
                    }
                }
                ControlKind::Caffeine => {
                    if activate {
                        // Through the toggle so the wake-up and the broadcast
                        // happen here exactly as they do from a keybinding.
                        let _ = self.toggle_idle_inhibit(backend, &WMArgEnum::Int(0));
                        let enabled = self.idle_inhibited;
                        self.features
                            .system_ui
                            .update_control(ControlKind::Caffeine, 0, enabled);
                    }
                }
                ControlKind::Session => {
                    if activate {
                        // Swap the panel for the session menu; the grabs stay.
                        self.features.system_ui_return_to_hub = true;
                        self.features.system_ui =
                            crate::jwm::features::SystemUiState::session_menu();
                        self.sync_system_ui(backend);
                        return;
                    }
                }
                ControlKind::LockScreen => {
                    if activate {
                        // A lock is terminal rather than a child page.
                        self.features.system_ui_return_to_hub = false;
                        // Swap the panel for the lock overlay; the keyboard and
                        // pointer grabs stay in place for the lock screen.
                        self.features.system_ui = crate::jwm::features::SystemUiState::lock();
                        self.features
                            .system_ui
                            .set_lock_now_playing(self.features.media.get());
                        self.sync_system_ui(backend);
                        return;
                    }
                }
            }
        }
        self.sync_system_ui(backend);
    }

    /// Key handling while the notification center is open: Up/Down select,
    /// Return invokes the sender's default action, `d`/Delete dismisses one
    /// row, `c` clears the history.
    /// Key handling while the Alt+Tab switcher is up: Tab and the arrows
    /// walk the list (wrapping), Return commits, Delete or BackSpace closes
    /// the highlighted window without leaving the gesture, Escape cancels,
    /// and every other key is swallowed — the modifier is still down, so
    /// nothing else may fire.
    fn handle_window_switcher_key(
        &mut self,
        backend: &mut dyn Backend,
        keysym: u32,
        mods: Mods,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // The keyboard owns the selection cue now: a stationary pointer's
        // hover must not keep a stale row pinned.
        backend.compositor_set_system_ui_hover(None);
        match keysym {
            keys::KEY_Escape => self.cancel_window_switcher(backend),
            keys::KEY_Return | keys::KEY_KP_Enter => self.commit_window_switcher(backend)?,
            keys::KEY_Delete | keys::KEY_BackSpace => self.close_window_switcher_row(backend)?,
            keys::KEY_Tab | keys::KEY_ISO_Left_Tab => {
                let backwards = mods.contains(Mods::SHIFT) || keysym == keys::KEY_ISO_Left_Tab;
                self.features
                    .system_ui
                    .move_selection(if backwards { -1 } else { 1 });
                self.sync_system_ui(backend);
            }
            keys::KEY_Up => {
                self.features.system_ui.move_selection(-1);
                self.sync_system_ui(backend);
            }
            keys::KEY_Down => {
                self.features.system_ui.move_selection(1);
                self.sync_system_ui(backend);
            }
            _ => {
                // The binding that opened the panel, pressed again, steps the
                // list — for a custom key (Mod4+j, Mod1+grave, ...) just as
                // for the Tab default above. Anything else stays swallowed.
                if let Some(arg) = switcher_binding_step(&self.key_bindings, keysym, mods) {
                    self.window_switcher(backend, &arg)?;
                }
            }
        }
        Ok(())
    }

    /// Delete or BackSpace with the switcher up: close the highlighted window
    /// without leaving the gesture. The close is the one `killclient` sends —
    /// the graceful WM_DELETE request, falling back to killing the client —
    /// then the row leaves the snapshot and the next-oldest window slides
    /// under the highlight, so a commit never lands on the window just
    /// closed. Closing the last row ends the gesture outright: the opener
    /// refuses an empty list, so the panel never sits open over one either.
    /// The grabs stay until then — the close is a request to the client, not
    /// a panel hand-over, and the gesture's modifier is still down.
    fn close_window_switcher_row(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(window) = self.features.system_ui.selected_switcher_window() else {
            return Ok(());
        };
        // The snapshot can name a window that already died mid-gesture; only
        // a live client can take the close, but the stale row leaves either
        // way.
        if let Some(client_key) = self.wintoclient(WindowId::from_raw(window))
            && let Some(client) = self.state.clients.get(client_key)
        {
            backend.window_ops().close_window(client.win)?;
        }
        if let Some((_, true)) = self.features.system_ui.remove_selected_switcher_row() {
            self.cancel_window_switcher(backend);
        } else {
            self.sync_system_ui(backend);
        }
        Ok(())
    }

    /// The expose grid's windows in entry order — the same collection
    /// `toggle_expose` entered with, recomputed live. A close only ever
    /// removes entries, so the rebuilt grid keeps every survivor exactly
    /// where the list had it.
    fn expose_candidates(&self) -> Vec<expose_plan::ExposeCandidate> {
        let mut candidates: Vec<expose_plan::ExposeCandidate> = Vec::new();
        for &mon_key in &self.state.monitor_order {
            if let Some(clients) = self.state.monitor_clients.get(mon_key) {
                for &ck in clients {
                    if !self.is_client_visible_on_monitor(ck, mon_key) {
                        continue;
                    }
                    if let Some(client) = self.state.clients.get(ck) {
                        let g = &client.geometry;
                        candidates.push((client.win, g.x, g.y, g.w, g.h, client.name.clone()));
                    }
                }
            }
        }
        candidates
    }

    /// Delete or BackSpace with expose up: close the highlighted thumbnail's
    /// window without leaving the gesture — the highlighted cell, not the
    /// focused window (the two usually differ mid-gesture; the switcher
    /// resolves its row the same way). The close itself, the in-place grid
    /// rebuild and the close-to-empty exit live in [`Self::apply_expose_close`],
    /// shared with the middle-click path.
    fn close_expose_highlighted(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let highlighted = backend.compositor_expose_selected();
        let action = expose_plan::plan_close(self.expose_candidates(), highlighted);
        self.apply_expose_close(backend, action)
    }

    /// Middle-click with expose up: close the clicked cell's window —
    /// browser-tab semantics: the clicked cell, not the highlighted one
    /// (the two may differ). A middle click never commits the gesture, so a
    /// click on empty space is a plain no-op and expose stays up; so is a
    /// click whose cell names a window that already died mid-expose — the
    /// compositor owns the grid, so the WM's only knowledge of it is the
    /// live candidate list, the same asymmetry `ExposeCloseAction::Keep`
    /// records for a Delete naming a dead window.
    fn close_expose_clicked(
        &mut self,
        backend: &mut dyn Backend,
        hit: Option<WindowId>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(hit) = hit else {
            return Ok(());
        };
        let candidates = self.expose_candidates();
        // The compositor's grid is the eligible candidates in entry order,
        // so the clicked window resolves to its cell's index there.
        let Some(index) = expose_plan::grid_index(&candidates, hit) else {
            return Ok(());
        };
        let action = expose_plan::plan_close_at(candidates, index);
        self.apply_expose_close(backend, action)
    }

    /// Execute a planned expose close, keeping the gesture alive. The close
    /// is the one `killclient` sends — `window_ops().close_window`, the
    /// graceful WM_DELETE request with its forced fallback — then the grid
    /// rebuilds in place from the survivors: the entry after the closed one
    /// slides under the highlight, the tail clamps, and no survivor changes
    /// its order. Closing the last entry ends the gesture through the same
    /// exit sequence Escape uses, focusing nothing: the opener refuses an
    /// empty grid, so the overlay never sits open over one either — and with
    /// the gesture over, no later key, click or release can commit the window
    /// just closed (expose has no modifier-release commit to guard; the
    /// switcher needs one because its commit *is* the release). The grabs
    /// stay until then — the close is a request to the client, not an expose
    /// hand-over.
    fn apply_expose_close(
        &mut self,
        backend: &mut dyn Backend,
        action: expose_plan::ExposeCloseAction,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match action {
            expose_plan::ExposeCloseAction::Keep => {}
            expose_plan::ExposeCloseAction::Close {
                window,
                survivors,
                select,
            } => {
                // The grid can name a window that already died mid-expose;
                // only a live client can take the close, but its cell
                // leaves either way.
                if let Some(client_key) = self.wintoclient(window)
                    && let Some(client) = self.state.clients.get(client_key)
                {
                    backend.window_ops().close_window(client.win)?;
                }
                // Rebuild in place through the same call that entered
                // expose: the compositors rebuild each thumbnail's label
                // texture with the entry set it names, so grid and labels
                // can never disagree. The highlight is re-pointed at the
                // survivor the plan kept under it — for a click close that
                // is the cell that slid into the clicked slot; the rebuild
                // itself cleared the compositor's hover (a fresh entry set
                // starts unhovered), and on Wayland the next pointer motion
                // re-derives the hover from the pointer's true position
                // anyway, while X11's hover only ever moves through this
                // select call.
                let select_id = survivors.get(select).map(|&(win, ..)| win);
                backend.compositor_set_expose_mode(true, survivors);
                backend.compositor_expose_select(select_id);
            }
            expose_plan::ExposeCloseAction::CloseLast { window } => {
                if let Some(client_key) = self.wintoclient(window)
                    && let Some(client) = self.state.clients.get(client_key)
                {
                    backend.window_ops().close_window(client.win)?;
                }
                return self.apply_expose_action(backend, expose_plan::plan_escape());
            }
        }
        Ok(())
    }

    fn handle_notification_center_key(&mut self, backend: &mut dyn Backend, keysym: u32) {
        use crate::jwm::features::notifications::CloseReason;

        use crate::jwm::features::notifications::MAX_ACTIONS;

        // Up/Down move between rows, Left/Right within one — the same rule the
        // control center and the calendar already follow.
        if keysym == keys::KEY_Up {
            self.features.system_ui.move_selection(-1);
        } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
            self.features.system_ui.move_selection(1);
        } else if keysym == keys::KEY_Left {
            self.features.system_ui.move_notification_action(-1);
        } else if keysym == keys::KEY_Right {
            self.features.system_ui.move_notification_action(1);
        } else if keysym == keys::KEY_c {
            self.clear_notifications();
        } else if (keys::KEY_1..keys::KEY_1 + MAX_ACTIONS as u32).contains(&keysym) {
            // The chips carry their numbers, so the mapping is on screen. A
            // digit past what the row offers names nothing and does nothing.
            let index = (keysym - keys::KEY_1) as usize;
            if let Some((id, action)) = self.features.system_ui.notification_action_at(index) {
                self.invoke_notification_action(id, &action);
            }
        } else if let Some((id, action)) = self.features.system_ui.selected_notification() {
            if keysym == keys::KEY_Return || keysym == keys::KEY_space {
                match action {
                    // Without an action there is nothing to hand back to the
                    // sender, so Return just dismisses like `d`.
                    Some(action) => {
                        self.invoke_notification_action(id, &action);
                    }
                    None => {
                        self.close_notification(id, CloseReason::Dismissed);
                    }
                }
            } else if keysym == keys::KEY_d
                || keysym == keys::KEY_Delete
                || keysym == keys::KEY_BackSpace
            {
                self.close_notification(id, CloseReason::Dismissed);
            }
        }
        self.sync_system_ui(backend);
    }

    /// Key handling while the session menu is open: Up/Down move, Return
    /// arms a destructive row and then runs it.
    fn handle_session_menu_key(&mut self, backend: &mut dyn Backend, keysym: u32) {
        if keysym == keys::KEY_Up {
            self.features.system_ui.move_selection(-1);
        } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
            self.features.system_ui.move_selection(1);
        } else if keysym == keys::KEY_Return || keysym == keys::KEY_space {
            if let Some(action) = self.features.system_ui.activate_session_entry() {
                if let Err(error) = self.run_session_action(backend, action) {
                    error!("Session action {} failed: {error}", action.as_str());
                    // Leave the menu open so the failure is visible rather
                    // than dropping the user back to a bare desktop.
                    self.sync_system_ui(backend);
                }
                return;
            }
        }
        self.sync_system_ui(backend);
    }

    /// Key handling while the Wi-Fi picker is open: Up/Down select, Return
    /// joins (prompting for a passphrase first when one is needed), `r`
    /// rescans, `d` forgets the highlighted network's saved profile (arming
    /// on the first press, deleting on the second), and typing feeds the
    /// prompt.
    fn handle_wifi_picker_key(&mut self, backend: &mut dyn Backend, keysym: u32, mods: Mods) {
        let prompting = self.features.system_ui.is_prompting_wifi_passphrase();

        if keysym == keys::KEY_Return {
            self.join_selected_wifi(backend);
            return;
        }
        if keysym == keys::KEY_BackSpace || keysym == keys::KEY_Delete {
            self.features.system_ui.backspace();
        } else if !prompting && (keysym == keys::KEY_Up) {
            self.features.system_ui.move_selection(-1);
        } else if !prompting && (keysym == keys::KEY_Down || keysym == keys::KEY_Tab) {
            self.features.system_ui.move_selection(1);
        } else if !prompting && keysym == keys::KEY_r {
            match crate::jwm::features::connectivity::start_scan() {
                Some(scan) => {
                    self.features.wifi_scan = Some(self.track_background_job(scan));
                    self.features.system_ui.set_wifi_message("Scanning\u{2026}");
                }
                None => self
                    .features
                    .system_ui
                    .set_wifi_message("nmcli is not available"),
            }
        } else if !prompting && keysym == keys::KEY_d {
            self.forget_selected_wifi();
        } else if prompting && let Some(ch) = Self::system_ui_char(keysym, mods) {
            self.features.system_ui.push_char(ch);
        }
        self.sync_system_ui(backend);
    }

    /// `d` in the Wi-Fi picker: arm the highlighted row on the first press,
    /// delete its saved profile on the second. The delete — and the lookup
    /// that decides whether a profile even backs the row — runs on a worker;
    /// the frame tick's connectivity poll adopts the outcome, so the status
    /// line and the control-center row land on the truth.
    fn forget_selected_wifi(&mut self) {
        use crate::jwm::features::connectivity;
        use crate::jwm::features::system_ui::ForgetPlan;

        // Coalesce the way leaning on `r` does: while a delete — or a join —
        // is still being applied, another press is a no-op rather than a
        // second racing nmcli write.
        if connectivity::wifi_forget_in_flight()
            || connectivity::job_in_flight(self.features.wifi_connect.as_ref())
        {
            return;
        }
        match self.features.system_ui.plan_wifi_forget() {
            // Wi-Fi rows arm unconditionally; `Unavailable` is the Bluetooth
            // gate's answer and cannot come back from `plan_wifi_forget`.
            ForgetPlan::Armed | ForgetPlan::Unavailable => {}
            ForgetPlan::Execute(ssid) => {
                // The SSID is the access point's chosen bytes; the status
                // line gets the display form, the worker gets the join key.
                self.features.system_ui.set_wifi_message(format!(
                    "Forgetting {}\u{2026}",
                    connectivity::display_ssid(&ssid)
                ));
                let job = connectivity::start_forget_profile(&ssid);
                connectivity::track_wifi_forget(self.track_background_job(job));
            }
        }
    }

    /// Key handling while the Bluetooth picker is open: Up/Down select,
    /// Return connects/disconnects (or starts pairing on a device never
    /// bonded), `s` runs a bounded discovery scan, `r` re-reads the list,
    /// `a` arms a bounded window in which an incoming request may be
    /// accepted, and `d` removes a bonded device (arming on the first press,
    /// removing on the second). While a pairing prompt is up, the prompt
    /// owns the keys.
    fn handle_bluetooth_picker_key(&mut self, backend: &mut dyn Backend, keysym: u32, mods: Mods) {
        use crate::jwm::features::system_ui::PromptKind;

        let prompt = self.features.system_ui.pairing_prompt();
        if matches!(prompt, Some(PromptKind::Pin { .. })) {
            if keysym == keys::KEY_Return {
                self.submit_bluetooth_pin(backend);
                return;
            }
            if keysym == keys::KEY_BackSpace || keysym == keys::KEY_Delete {
                self.features.system_ui.backspace();
            } else if let Some(ch) = Self::system_ui_char(keysym, mods) {
                self.features.system_ui.push_char(ch);
            }
            self.sync_system_ui(backend);
            return;
        }
        // The numeric comparison and an inbound authorization are the same
        // gesture — y/Enter accepts, n rejects, Esc cancels through the
        // global prompt path — so they share one branch.
        if prompt.is_some_and(PromptKind::is_yes_no) {
            if keysym == keys::KEY_y || keysym == keys::KEY_Return {
                self.answer_bluetooth_confirm(backend, true);
            } else if keysym == keys::KEY_n {
                self.answer_bluetooth_confirm(backend, false);
            }
            return;
        }
        if matches!(prompt, Some(PromptKind::Display { .. })) {
            // Nothing to type here: Esc (handled with the global prompt
            // cancels above) is the way out.
            return;
        }

        if keysym == keys::KEY_Return || keysym == keys::KEY_space {
            self.activate_selected_bluetooth(backend);
            return;
        }
        if keysym == keys::KEY_Up {
            self.features.system_ui.move_selection(-1);
        } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
            self.features.system_ui.move_selection(1);
        } else if keysym == keys::KEY_s {
            // Coalesce, the way `ensure_connectivity_refresh` does: leaning
            // on `s` used to detach a trail of still-running scans, and a
            // real `Adapter1.StartDiscovery` session is refcounted per
            // connection, so overlapping ones are worse than wasteful.
            if self.features.bluetooth_scan.is_some() {
                self.features
                    .system_ui
                    .set_bluetooth_message("Scanning\u{2026}");
                self.sync_system_ui(backend);
                return;
            }
            match crate::jwm::features::connectivity::start_discovery_scan() {
                Some(scan) => {
                    self.features.bluetooth_scan = Some(self.track_background_job(scan));
                    self.features
                        .system_ui
                        .set_bluetooth_message("Scanning\u{2026}");
                }
                None => self
                    .features
                    .system_ui
                    .set_bluetooth_message("bluetoothctl is not available"),
            }
        } else if keysym == keys::KEY_a {
            self.arm_bluetooth_inbound_authorization(backend);
            return;
        } else if keysym == keys::KEY_r {
            if self.features.bluetooth_scan.is_some() {
                self.sync_system_ui(backend);
                return;
            }
            match crate::jwm::features::connectivity::start_device_scan() {
                Some(scan) => {
                    self.features.bluetooth_scan = Some(self.track_background_job(scan));
                    self.features
                        .system_ui
                        .set_bluetooth_message("Reading devices\u{2026}");
                }
                None => self
                    .features
                    .system_ui
                    .set_bluetooth_message("bluetoothctl is not available"),
            }
        } else if keysym == keys::KEY_d {
            self.forget_selected_bluetooth();
        }
        self.sync_system_ui(backend);
    }

    /// `d` in the Bluetooth picker: arm the highlighted device on the first
    /// press, remove its bond on the second. The removal rides the same
    /// worker slot and completion as connect/disconnect — the device-list
    /// re-read that completion kicks off is what makes the row disappear —
    /// so a press while one of them runs coalesces to a no-op. Forgetting
    /// the connected device drops the connection with the bond; the re-read
    /// shows both gone.
    fn forget_selected_bluetooth(&mut self) {
        use crate::jwm::features::connectivity;
        use crate::jwm::features::system_ui::ForgetPlan;

        if connectivity::job_in_flight(self.features.bluetooth_action.as_ref()) {
            return;
        }
        match self.features.system_ui.plan_bluetooth_forget() {
            ForgetPlan::Armed => {}
            ForgetPlan::Unavailable => {
                self.features
                    .system_ui
                    .set_bluetooth_message("Not paired \u{2014} nothing to forget");
            }
            ForgetPlan::Execute(address) => {
                let name = self
                    .features
                    .system_ui
                    .selected_bluetooth()
                    .map(|(_, name, _)| name)
                    .unwrap_or_else(|| address.clone());
                self.features
                    .system_ui
                    .set_bluetooth_message(format!("Forgetting {name}\u{2026}"));
                let job = connectivity::start_device_action(&address, "remove");
                self.features.bluetooth_action = Some(self.track_background_job(job));
            }
        }
    }

    /// Activate the launcher's current result. Returns `true` when the panel
    /// was consumed (copied, focused, or launched).
    fn activate_launcher_selection(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if let Some(result) = self
            .features
            .system_ui
            .computed_result()
            .map(str::to_string)
        {
            if backend.set_clipboard_text(&result) {
                self.record_clipboard(&result);
                log::info!("Launcher: copied {result}");
            } else {
                log::warn!("Launcher: this backend cannot set the clipboard");
            }
            self.close_system_ui(backend);
            return Ok(true);
        }
        if let Some(window) = self.features.system_ui.selected_window() {
            self.close_system_ui(backend);
            if let Err(error) = self.reveal_and_focus(
                backend,
                crate::backend::common_define::WindowId::from_raw(window),
            ) {
                log::warn!("Launcher: could not focus window: {error}");
            }
            return Ok(true);
        }
        if let Some(command) = direct_command_from_launcher(&self.features.system_ui) {
            let id = command[0].clone();
            self.features.system_ui.note_launch(&id);
            log::info!("Launcher: running {id}");
            self.close_system_ui(backend);
            self.spawn(backend, &WMArgEnum::StringVec(command))?;
            return Ok(true);
        }
        if let Some(choice) = self.features.system_ui.selected_launch() {
            self.features.system_ui.note_launch(&choice.id);
            let command = crate::jwm::features::launcher::launch_command(
                &crate::config::Config::get_terminal_exec_prefix(),
                &choice.command,
                choice.terminal,
            );
            self.close_system_ui(backend);
            self.spawn(backend, &WMArgEnum::StringVec(command))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Pointer counterpart of Return: select the visible row under the mouse,
    /// then run the same action path used by the keyboard. The one exception
    /// is the action strip under the selected notification: a press there
    /// fires the chip under the pointer directly — never the row's Return
    /// replay, which would invoke whatever the cursor happens to sit on —
    /// and a press on the gutter or between chips fires nothing.
    pub(crate) fn activate_system_ui_pointer_row(
        &mut self,
        backend: &mut dyn Backend,
        row: usize,
        text_x: f32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.system_ui.notification_strip_visible_row() == Some(row) {
            if let Some((id, action, _)) = self.notification_strip_chip_at(row, text_x) {
                self.invoke_notification_action(id, &action);
                self.sync_system_ui(backend);
            }
            return Ok(());
        }
        let direct_command_row =
            row == 0 && direct_command_from_launcher(&self.features.system_ui).is_some();
        let Some(changed) = self.features.system_ui.select_visible_row(row) else {
            if !direct_command_row {
                return Ok(());
            }
            return self.activate_launcher_selection(backend).map(|_| ());
        };
        if changed {
            self.sync_system_ui(backend);
        }

        if let Some(control) = self.features.system_ui.selected_control() {
            self.handle_control_center_key(backend, control, keys::KEY_Return, Mods::empty());
        } else if self.features.system_ui.is_notification_center() {
            self.handle_notification_center_key(backend, keys::KEY_Return);
        } else if self.features.system_ui.is_session_menu() {
            self.handle_session_menu_key(backend, keys::KEY_Return);
        } else if self.features.system_ui.is_wifi_picker() {
            self.handle_wifi_picker_key(backend, keys::KEY_Return, Mods::empty());
        } else if self.features.system_ui.is_bluetooth_picker() {
            self.handle_bluetooth_picker_key(backend, keys::KEY_Return, Mods::empty());
        } else if self.features.system_ui.audio_picker_direction().is_some() {
            self.use_selected_audio_device(backend);
        } else if self.features.system_ui.is_clipboard_picker() {
            self.copy_selected_clipboard(backend);
        } else if self.features.system_ui.is_wallpaper_picker() {
            self.apply_selected_wallpaper(backend);
        } else {
            let _ = self.activate_launcher_selection(backend)?;
        }
        Ok(())
    }

    /// The chip under a pointer at `text_x` on the selected notification's
    /// action strip: the notification's identifier, the action's key and the
    /// chip's index, measured with the panel's configured font — the numbers
    /// the rasterizer drew. `None` when `row` is not the strip's row, or the
    /// pointer rests on the gutter or a gap between chips.
    fn notification_strip_chip_at(&self, row: usize, text_x: f32) -> Option<(u32, String, usize)> {
        let config = CONFIG.load();
        let description = config.system_ui_font();
        let pixel_size = crate::backend::compositor_font::ui_font_pixel_size(description);
        self.features
            .system_ui
            .notification_strip_chip_at_visible_row(row, text_x, description, pixel_size)
    }

    pub(crate) fn hover_system_ui_pointer_row(
        &mut self,
        backend: &mut dyn Backend,
        hit: Option<(usize, f32)>,
    ) {
        // Keep motion allocation- and I/O-free. Direct command validation can
        // stat an explicit path, so that exceptional synthetic row is checked
        // only on click; ordinary and calculator rows resolve from memory.
        //
        // Over the selected notification's action strip the row's action
        // cursor follows the chip under the pointer, so a Return or a digit
        // right after acts on what the pointer is over — the within-row
        // counterpart of the switcher's pill following the pointer row. Only
        // a cursor that really moved repaints, and only the strip's row ever
        // measures text: ordinary rows keep hover free of both.
        if let Some((row, text_x)) = hit
            && self.features.system_ui.notification_strip_visible_row() == Some(row)
            && let Some((_, _, chip)) = self.notification_strip_chip_at(row, text_x)
            && self.features.system_ui.hover_notification_action(chip)
        {
            self.sync_system_ui(backend);
        }
        let row = hit
            .map(|(row, _)| row)
            .filter(|row| self.features.system_ui.visible_row_target(*row).is_some());
        backend.compositor_set_system_ui_hover(row);
    }

    pub(crate) fn scroll_system_ui_from_pointer(
        &mut self,
        backend: &mut dyn Backend,
        direction: isize,
        hit_row: Option<usize>,
    ) {
        backend.compositor_set_system_ui_hover(None);
        // Scroll-on-slider: a wheel click over a Volume/Brightness row
        // adjusts the value under the pointer instead of browsing the list.
        // The selection pill follows, so a subsequent Left/Right acts on
        // the row the pointer is on.
        if let Some(row) = hit_row
            && let Some((kind, delta)) = self.features.system_ui.wheel_slider_step(row, direction)
        {
            let _ = self.features.system_ui.select_visible_row(row);
            self.adjust_control_slider(kind, delta);
            self.sync_system_ui(backend);
            return;
        }
        if self.features.system_ui.is_calendar() {
            self.features
                .system_ui
                .shift_calendar(direction.signum() as i32, 0, false);
        } else {
            self.features.system_ui.move_selection(direction.signum());
        }
        self.sync_system_ui(backend);
    }

    /// The slider side effect shared by Left/Right and scroll-on-slider:
    /// draw the estimate into the row at once and queue the real change on
    /// the controls worker; its read-back corrects the row if the estimate
    /// drifted.
    fn adjust_control_slider(&mut self, kind: crate::jwm::features::ControlKind, delta: i32) {
        use crate::jwm::features::{ControlKind, system_controls};
        match kind {
            ControlKind::Volume => {
                if let Some((_, Some(state))) =
                    self.queue_volume_request(system_controls::ControlRequest::VolumeAdjust(delta))
                {
                    self.features.system_ui.update_control(
                        ControlKind::Volume,
                        state.percent,
                        state.muted,
                    );
                }
            }
            ControlKind::Brightness => {
                if let Some((_, Some(percent))) = self.queue_brightness_request(
                    system_controls::ControlRequest::BrightnessAdjust(delta),
                ) {
                    self.features
                        .system_ui
                        .update_control(ControlKind::Brightness, percent, false);
                }
            }
            _ => {}
        }
    }

    /// The absolute-set counterpart of [`Self::adjust_control_slider`], for
    /// click-to-position and slider drags. A level set on a muted sink
    /// unmutes it — pointing at a level is an explicit ask for that much
    /// sound, and both wpctl and pactl keep the mute flag on a plain
    /// set-volume — so the estimate is unmuted and the worker runs the
    /// unmute chain before its read-back.
    fn set_control_slider_from_pointer(
        &mut self,
        kind: crate::jwm::features::ControlKind,
        percent: u8,
    ) {
        use crate::jwm::features::{ControlKind, system_controls};
        match kind {
            ControlKind::Volume => {
                if let Some((_, Some(state))) =
                    self.queue_volume_request(system_controls::ControlRequest::VolumeSet(percent))
                {
                    self.features.system_ui.update_control(
                        ControlKind::Volume,
                        state.percent,
                        state.muted,
                    );
                }
            }
            ControlKind::Brightness => {
                if let Some((_, Some(percent))) = self.queue_brightness_request(
                    system_controls::ControlRequest::BrightnessSet(percent),
                ) {
                    self.features
                        .system_ui
                        .update_control(ControlKind::Brightness, percent, false);
                }
            }
            _ => {}
        }
    }

    /// A left press on a control-center row. When the press lands on the
    /// row's slider bar it positions the slider (click-to-position) and arms
    /// a drag; it returns `false` for everything else — non-slider rows and
    /// the icon/label/value parts of a slider row — so the caller runs the
    /// row's ordinary click, Volume's mute toggle included.
    pub(crate) fn press_control_center_slider(
        &mut self,
        backend: &mut dyn Backend,
        row: usize,
        text_x: f32,
        root_x: f64,
    ) -> bool {
        let config = CONFIG.load();
        let description = config.system_ui_font();
        let pixel_size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let Some((kind, percent)) = self.features.system_ui.slider_press_at_visible_row(
            row,
            text_x,
            description,
            pixel_size,
        ) else {
            return false;
        };
        // The selection pill follows the pointer, so a Left/Right right after
        // acts on the row being dragged — scroll-on-slider selects the same
        // way.
        let _ = self.features.system_ui.select_visible_row(row);
        self.control_slider_drag = Some(ControlSliderDrag {
            kind,
            last_percent: percent,
            text_origin_x: root_x - f64::from(text_x),
        });
        self.set_control_slider_from_pointer(kind, percent);
        self.sync_system_ui(backend);
        true
    }

    /// Motion with a slider drag armed: recompute the value from the
    /// pointer's x and queue it on the controls worker, but only when the
    /// rounded percent actually changed — the worker folds the queued storm
    /// into the newest level, and its read-back corrects the row if the
    /// estimate drifted. `hit_text_x` is the x the compositor's hit-test
    /// carried, `None` when the pointer left the list: a drag that runs off
    /// the card keeps tracking from the cached origin. Returns `false` when
    /// no drag is armed — or the panel stopped being a control center — so
    /// the caller falls back to ordinary hover.
    pub(crate) fn drag_control_center_slider(
        &mut self,
        backend: &mut dyn Backend,
        root_x: f64,
        hit_text_x: Option<f32>,
    ) -> bool {
        let Some(mut drag) = self.control_slider_drag else {
            return false;
        };
        if !self.features.system_ui.is_control_center() {
            self.control_slider_drag = None;
            return false;
        }
        if let Some(text_x) = hit_text_x {
            drag.text_origin_x = root_x - f64::from(text_x);
        }
        let text_x = (root_x - drag.text_origin_x) as f32;
        let config = CONFIG.load();
        let description = config.system_ui_font();
        let pixel_size = crate::backend::compositor_font::ui_font_pixel_size(description);
        let percent =
            self.features
                .system_ui
                .slider_drag_value(drag.kind, text_x, description, pixel_size);
        if let Some(percent) = percent
            && percent != drag.last_percent
        {
            drag.last_percent = percent;
            self.set_control_slider_from_pointer(drag.kind, percent);
            self.sync_system_ui(backend);
        }
        self.control_slider_drag = Some(drag);
        true
    }

    pub(crate) fn dismiss_system_ui_from_pointer(&mut self, backend: &mut dyn Backend) {
        if self.features.system_ui.is_locked() {
            return;
        }
        backend.compositor_set_system_ui_hover(None);
        if self.features.system_ui.cancel_wifi_passphrase() {
            self.sync_system_ui(backend);
        } else if self.features.system_ui.pairing_prompt().is_some() {
            // Clicking the scrim out of a pairing prompt cancels the pairing.
            self.cancel_bluetooth_pairing();
            self.sync_system_ui(backend);
        } else if self.features.system_ui_return_to_hub {
            self.return_to_shell_hub(backend);
        } else {
            self.close_system_ui(backend);
        }
    }

    pub(crate) fn on_key_press_internal(
        &mut self,
        backend: &mut dyn Backend,
        keycode: u8,
        state_bits: u16,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let debug_keys = std::env::var("JWM_DEBUG_KEYS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let keysym = backend.key_ops_mut().keysym_from_keycode(keycode)?;
        let clean_state = self.clean_mask(backend, state_bits);
        let key_mods = Mods::SHIFT
            | Mods::CONTROL
            | Mods::ALT
            | Mods::SUPER
            | Mods::MOD2
            | Mods::MOD3
            | Mods::MOD5;

        // The Alt+Tab switcher is the most modal surface of all: its modifier
        // is still held, and any binding that shares it (Alt+C would kill the
        // focused window) must not fire underneath the panel. Every key is
        // consumed until the gesture commits or cancels — even the screenshot
        // exception below has to wait.
        if self.features.system_ui.is_window_switcher() {
            self.handle_window_switcher_key(backend, keysym, clean_state)?;
            return Ok(());
        }

        // Screenshot bindings are compositor-global actions, not panel input.
        // Keep them reachable while a shell surface owns the keyboard grab;
        // otherwise Alt+S is consumed by the modal UI and native capture is
        // impossible until the panel is dismissed. Never bypass the lock
        // screen, which must not expose its contents to a screenshot.
        if self.features.system_ui.is_active() && !self.features.system_ui.is_locked() {
            if let Some((func, arg)) = self
                .key_bindings
                .iter()
                .find(|kc| {
                    keysym == kc.key_sym
                        && (kc.mask & key_mods) == clean_state
                        && kc.func_opt.is_some_and(Self::is_native_screenshot)
                })
                .and_then(|kc| kc.func_opt.map(|func| (func, kc.arg.clone())))
            {
                // The panel's overlay and grabs would otherwise intercept the
                // selection pointer events (and make the panel appear in a
                // fullscreen capture). Close it before entering capture mode.
                self.close_system_ui(backend);
                if let Err(error) = func(self, backend, &arg) {
                    error!("Error executing screenshot shortcut from system UI: {error}");
                }
                return Ok(());
            }
        }

        // Built-in system UI is modal and consumes every key. This branch is
        // shared by X11rb, XCB and Wayland-udev, keeping behavior identical.
        if self.features.system_ui.is_active() {
            // The most recent input modality owns the selection cue. Once the
            // keyboard moves again, a stationary pointer must not keep the
            // pill pinned to its old row.
            backend.compositor_set_system_ui_hover(None);
            let locked = self.features.system_ui.is_locked();
            // `clean_mask` strips Mods::CAPS from `clean_state`, which keeps
            // caps lock out of every binding and type-to-filter — but the
            // lock screen's password field is the one place the modifier
            // must change the character a key produces. `char_mods` restores
            // the live reading for that single consumer (the `system_ui_char`
            // call that feeds the field at the bottom of this branch); every
            // other caller keeps `clean_state`, and `clean_mask` is
            // untouched.
            let mut char_mods = clean_state;
            if locked {
                // The lock holds the keyboard grab, so every key event doubles
                // as a modifier-state reading for the caps-lock row. Read the
                // mask back from the backend instead of trusting this event's
                // state field: on X11 that field is the mask *before* the
                // event, so the very press that turns caps on would show the
                // row one keystroke late (the window switcher reads the live
                // mask back on modifier release for the same reason). The
                // Wayland backends answer with the state this key just left
                // behind. A failed query keeps the previous reading. The flag
                // rides the per-keystroke re-sync the password row already
                // triggers below; no extra sync is added for it.
                if let Some(caps_lock) = Self::current_caps_lock(backend) {
                    self.features.system_ui.set_lock_caps_lock(caps_lock);
                    if caps_lock {
                        char_mods |= Mods::CAPS;
                    }
                }
                // Volume, brightness and media transport keys stay live
                // behind the lock screen, as they do on GNOME, KDE, macOS
                // and Windows. The match runs through the ordinary binding
                // table — same modifier rule, same function pointer, same
                // argument — so the action is byte-for-byte the one the
                // unlocked session would run, controls worker and optimistic
                // OSD state included; the opaque backdrop simply hides the
                // OSD, and the lock card grows no rows for it. The key must
                // be one of the dedicated XF86 keysyms *and* bound to one of
                // the media actions, or it falls through to the swallow like
                // every other key.
                if Self::lock_media_keysym(keysym) {
                    let passthrough = self
                        .key_bindings
                        .iter()
                        .find(|kc| {
                            keysym == kc.key_sym
                                && (kc.mask & key_mods) == clean_state
                                && kc.func_opt.is_some_and(Self::lock_media_func)
                        })
                        .and_then(|kc| kc.func_opt.map(|func| (func, kc.arg.clone())));
                    if let Some((func, arg)) = passthrough {
                        if let Err(error) = func(self, backend, &arg) {
                            error!("Error executing lock-screen media key: {error}");
                        }
                        return Ok(());
                    }
                }
            }
            // Escape backs out of the passphrase prompt before it closes the
            // picker, so a typo does not cost the whole scan.
            if keysym == keys::KEY_Escape && self.features.system_ui.cancel_wifi_passphrase() {
                self.sync_system_ui(backend);
                return Ok(());
            }
            // Escape also abandons a Bluetooth pairing prompt — and with it
            // the pairing session, which must never outlive its panel.
            if keysym == keys::KEY_Escape && self.features.system_ui.pairing_prompt().is_some() {
                self.cancel_bluetooth_pairing();
                self.sync_system_ui(backend);
                return Ok(());
            }
            // The layout picker binds the same keys the cycle action does, so
            // holding the modifier and tapping space keeps stepping the strip
            // exactly as it did before the panel existed.
            if self.features.system_ui.is_layout_picker() {
                match keysym {
                    keys::KEY_Escape => self.cancel_layout_picker(backend),
                    keys::KEY_Return | keys::KEY_KP_Enter => self.confirm_layout_picker(backend),
                    keys::KEY_Left | keys::KEY_Up | keys::KEY_ISO_Left_Tab => {
                        self.layout_picker(backend, &WMArgEnum::Int(-1))?
                    }
                    keys::KEY_Tab if clean_state.contains(Mods::SHIFT) => {
                        self.layout_picker(backend, &WMArgEnum::Int(-1))?
                    }
                    keys::KEY_Right | keys::KEY_Down | keys::KEY_Tab => {
                        self.layout_picker(backend, &WMArgEnum::Int(1))?
                    }
                    keys::KEY_space => {
                        let delta = if clean_state.contains(Mods::SHIFT) {
                            -1
                        } else {
                            1
                        };
                        self.layout_picker(backend, &WMArgEnum::Int(delta))?
                    }
                    _ => {}
                }
                return Ok(());
            }
            // The tags overview answers its own keys here; the pointer paths
            // live in the dispatcher's motion and button branches. Unhandled
            // keys fall through so the panel's own toggle binding can still
            // close it.
            if self.features.system_ui.is_tags_overview() {
                let handled = match keysym {
                    keys::KEY_Escape => {
                        self.cancel_tags_overview(backend);
                        true
                    }
                    keys::KEY_Return | keys::KEY_KP_Enter => {
                        self.confirm_tags_overview(backend)?;
                        true
                    }
                    keys::KEY_Left => {
                        self.move_tags_overview_selection(backend, ExposeNavDirection::Left);
                        true
                    }
                    keys::KEY_Right => {
                        self.move_tags_overview_selection(backend, ExposeNavDirection::Right);
                        true
                    }
                    keys::KEY_Up => {
                        self.move_tags_overview_selection(backend, ExposeNavDirection::Up);
                        true
                    }
                    keys::KEY_Down => {
                        self.move_tags_overview_selection(backend, ExposeNavDirection::Down);
                        true
                    }
                    // A digit jumps straight to its tag and commits, with or
                    // without modifiers: the panel holds the keyboard grab,
                    // so the global Mod1+N bindings never see the key.
                    k if (keys::KEY_1..=keys::KEY_9).contains(&k) => {
                        self.jump_tags_overview(backend, (k - keys::KEY_1) as usize)?;
                        true
                    }
                    _ => false,
                };
                if handled {
                    return Ok(());
                }
            }
            // Every UI key binding is a toggle. The panel is modal and would
            // otherwise swallow the very key that opened it, so the binding
            // table is consulted here first: the opener sees its own panel on
            // screen and closes it (see `Jwm::toggle_off_system_ui`).
            //
            // Only bindings carrying a modifier qualify. A bare key belongs to
            // whatever the panel is typing into — the launcher query, a Wi-Fi
            // passphrase — and must not be stolen from it.
            if !locked && !(clean_state & key_mods).is_empty() {
                let toggle = self
                    .key_bindings
                    .iter()
                    .find(|kc| {
                        keysym == kc.key_sym
                            && (kc.mask & key_mods) == clean_state
                            && kc.func_opt.is_some_and(Self::opens_system_ui_panel)
                    })
                    .and_then(|kc| kc.func_opt.map(|func| (func, kc.arg.clone())));
                if let Some((func, arg)) = toggle {
                    if let Err(e) = func(self, backend, &arg) {
                        error!("Error toggling system UI panel: {:?}", e);
                    }
                    return Ok(());
                }
            }
            if keysym == keys::KEY_Escape && locked {
                self.features.system_ui.clear_lock_password();
                self.sync_system_ui(backend);
                return Ok(());
            }
            if keysym == keys::KEY_Escape && !locked {
                if self.features.system_ui_return_to_hub {
                    self.return_to_shell_hub(backend);
                } else {
                    self.close_system_ui(backend);
                }
                return Ok(());
            }
            if self.features.system_ui.is_monitor_layout() {
                let adjustment_step = if clean_state.contains(Mods::CONTROL) {
                    Some(1)
                } else if clean_state.contains(Mods::SHIFT) {
                    Some(10)
                } else {
                    None
                };
                let arrow_direction = match keysym {
                    keys::KEY_Left => Some(MonitorDirection::Left),
                    keys::KEY_Right => Some(MonitorDirection::Right),
                    keys::KEY_Up => Some(MonitorDirection::Above),
                    keys::KEY_Down => Some(MonitorDirection::Below),
                    _ => None,
                };
                if keysym == keys::KEY_Tab || keysym == keys::KEY_ISO_Left_Tab {
                    let backwards =
                        clean_state.contains(Mods::SHIFT) || keysym == keys::KEY_ISO_Left_Tab;
                    self.features
                        .system_ui
                        .cycle_monitor(if backwards { -1 } else { 1 });
                } else if keysym == keys::KEY_bracketleft {
                    self.features.system_ui.cycle_monitor_reference(-1);
                } else if keysym == keys::KEY_bracketright {
                    self.features.system_ui.cycle_monitor_reference(1);
                } else if let (Some(step), Some(direction)) = (adjustment_step, arrow_direction) {
                    self.features.system_ui.fine_tune_monitor(direction, step);
                } else if let Some(direction) = arrow_direction {
                    self.features.system_ui.place_monitor(direction);
                } else if keysym == keys::KEY_s {
                    self.features.system_ui.align_monitor_start();
                } else if keysym == keys::KEY_c {
                    self.features.system_ui.align_monitor_center();
                } else if keysym == keys::KEY_e {
                    self.features.system_ui.align_monitor_end();
                } else if keysym == keys::KEY_Return {
                    let args = self
                        .features
                        .system_ui
                        .monitor_layout_xrandr_args()
                        .unwrap_or_default();
                    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
                    match crate::jwm::features::external_command::output_with_limits(
                        "xrandr",
                        &arg_refs,
                        std::time::Duration::from_secs(5),
                        64 * 1024,
                    ) {
                        Ok(output) if output.status.success() => {
                            info!("Applied display layout with xrandr {args:?}");
                            self.close_system_ui(backend);
                            backend.output_ops().invalidate_output_cache();
                            self.updategeom(backend);
                            backend.compositor_force_full_redraw();
                            return Ok(());
                        }
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            let detail = stderr.trim();
                            let message = if detail.is_empty() {
                                format!("xrandr exited with {}", output.status)
                            } else {
                                let first_line = detail.lines().next().unwrap_or(detail);
                                format!(
                                    "xrandr: {}",
                                    first_line.chars().take(120).collect::<String>()
                                )
                            };
                            error!("Could not apply display layout: {message}");
                            self.features.system_ui.monitor_layout_error(message);
                        }
                        Err(err) => {
                            error!("Could not run xrandr: {err}");
                            self.features
                                .system_ui
                                .monitor_layout_error(format!("could not run xrandr: {err}"));
                        }
                    }
                }
                self.sync_system_ui(backend);
                return Ok(());
            }
            // Common list navigation works the same in the launcher, Shell
            // Hub, notification history and every picker. Handle it once
            // before their action-specific keys so long lists remain fast to
            // traverse and Shift+Tab never accidentally moves forward.
            if !self.features.system_ui.is_prompting() {
                let navigated = match keysym {
                    keys::KEY_Home => self.features.system_ui.jump_selection(false),
                    keys::KEY_End => self.features.system_ui.jump_selection(true),
                    keys::KEY_Page_Up => self.features.system_ui.page_selection(-1),
                    keys::KEY_Page_Down => self.features.system_ui.page_selection(1),
                    keys::KEY_ISO_Left_Tab => {
                        self.features.system_ui.move_selection(-1);
                        true
                    }
                    keys::KEY_Tab if clean_state.contains(Mods::SHIFT) => {
                        self.features.system_ui.move_selection(-1);
                        true
                    }
                    _ => false,
                };
                if navigated {
                    self.sync_system_ui(backend);
                    return Ok(());
                }
            }
            if let Some(control) = self.features.system_ui.selected_control() {
                self.handle_control_center_key(backend, control, keysym, clean_state);
                return Ok(());
            }
            if self.features.system_ui.is_notification_center() {
                self.handle_notification_center_key(backend, keysym);
                return Ok(());
            }
            if self.features.system_ui.is_session_menu() {
                self.handle_session_menu_key(backend, keysym);
                return Ok(());
            }
            if self.features.system_ui.is_wifi_picker() {
                self.handle_wifi_picker_key(backend, keysym, clean_state);
                return Ok(());
            }
            if self.features.system_ui.is_bluetooth_picker() {
                self.handle_bluetooth_picker_key(backend, keysym, clean_state);
                return Ok(());
            }
            if self.features.system_ui.audio_picker_direction().is_some() {
                if keysym == keys::KEY_Return || keysym == keys::KEY_space {
                    self.use_selected_audio_device(backend);
                } else {
                    if keysym == keys::KEY_Up {
                        self.features.system_ui.move_selection(-1);
                    } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
                        self.features.system_ui.move_selection(1);
                    }
                    self.sync_system_ui(backend);
                }
                return Ok(());
            }
            if self.features.system_ui.is_clipboard_picker() {
                // The picker is type-to-filter: printable characters narrow
                // the list and BackSpace edits the query. `d`, `c`, Delete,
                // space and Return keep their picker meanings and act on the
                // filtered selection — what you see is what they touch — so
                // those letters never land in the query itself.
                if keysym == keys::KEY_Return || keysym == keys::KEY_space {
                    self.copy_selected_clipboard(backend);
                } else if keysym == keys::KEY_d || keysym == keys::KEY_Delete {
                    self.forget_selected_clipboard(backend);
                } else if keysym == keys::KEY_c {
                    self.clear_clipboard_history();
                    self.sync_system_ui(backend);
                } else if keysym == keys::KEY_BackSpace {
                    self.features
                        .system_ui
                        .pop_clipboard_query(&self.features.clipboard);
                    self.sync_system_ui(backend);
                } else if keysym == keys::KEY_Up {
                    self.features.system_ui.move_selection(-1);
                    self.sync_system_ui(backend);
                } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
                    self.features.system_ui.move_selection(1);
                    self.sync_system_ui(backend);
                } else if let Some(ch) = Self::system_ui_char(keysym, clean_state) {
                    self.features
                        .system_ui
                        .push_clipboard_query(ch, &self.features.clipboard);
                    self.sync_system_ui(backend);
                } else {
                    self.sync_system_ui(backend);
                }
                return Ok(());
            }
            if self.features.system_ui.is_wallpaper_picker() {
                if keysym == keys::KEY_Return || keysym == keys::KEY_space {
                    self.apply_selected_wallpaper(backend);
                } else {
                    if keysym == keys::KEY_Up {
                        self.features.system_ui.move_selection(-1);
                    } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
                        self.features.system_ui.move_selection(1);
                    }
                    self.sync_system_ui(backend);
                }
                return Ok(());
            }
            if self.features.system_ui.is_calendar() {
                // Left/Right step months, Up/Down step years, t returns to
                // today; nothing here can leave the card in a bad state.
                let (months, years, today) = match keysym {
                    keys::KEY_Left => (-1, 0, false),
                    keys::KEY_Right => (1, 0, false),
                    keys::KEY_Up => (0, -1, false),
                    keys::KEY_Down => (0, 1, false),
                    keys::KEY_t | keys::KEY_Home => (0, 0, true),
                    _ => (0, 0, false),
                };
                self.features.system_ui.shift_calendar(months, years, today);
                self.sync_system_ui(backend);
                return Ok(());
            }
            if keysym == keys::KEY_BackSpace || keysym == keys::KEY_Delete {
                self.features.system_ui.backspace();
            } else if keysym == keys::KEY_Up {
                self.features.system_ui.move_selection(-1);
            } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
                self.features.system_ui.move_selection(1);
            } else if keysym == keys::KEY_Return {
                if locked {
                    self.submit_lock_password();
                } else if self.activate_launcher_selection(backend)? {
                    return Ok(());
                }
            } else if let Some(ch) = Self::system_ui_char(keysym, char_mods) {
                self.features.system_ui.push_char(ch);
            }
            self.sync_system_ui(backend);
            return Ok(());
        }

        // Recording region selection/adjustment mode.
        if self.features.recording.selecting_region {
            let ctrl = clean_state.contains(Mods::CONTROL);
            let shift = clean_state.contains(Mods::SHIFT);
            let capture_target = if !ctrl && keysym == keys::KEY_Tab {
                self.cycle_recording_capture_target(backend, shift);
                None
            } else if !ctrl && keysym == keys::KEY_g {
                Some(CaptureTarget::Region)
            } else if !ctrl && keysym == keys::KEY_w {
                Some(CaptureTarget::Window)
            } else if !ctrl && keysym == keys::KEY_m {
                Some(CaptureTarget::Monitor)
            } else if !ctrl && keysym == keys::KEY_d {
                Some(CaptureTarget::Desktop)
            } else {
                None
            };
            if let Some(target) = capture_target {
                self.set_recording_capture_target(backend, target);
                return Ok(());
            }
            if !ctrl && keysym == keys::KEY_Tab {
                return Ok(());
            }

            if keysym == keys::KEY_Escape {
                self.cancel_recording_region_interaction(backend);
            } else if keysym == keys::KEY_Return {
                self.finish_recording_region_interaction(backend)?;
            } else if matches!(
                keysym,
                keys::KEY_Left | keys::KEY_Right | keys::KEY_Up | keys::KEY_Down
            ) {
                let distance = if shift { 10 } else { 1 };
                let (dx, dy) = match keysym {
                    keys::KEY_Left => (-distance, 0),
                    keys::KEY_Right => (distance, 0),
                    keys::KEY_Up => (0, -distance),
                    keys::KEY_Down => (0, distance),
                    _ => (0, 0),
                };
                self.nudge_recording_capture_region(backend, dx, dy);
            }
            return Ok(());
        }

        // Screenshot region selection mode
        if self.features.screenshot.active {
            // A label under construction owns the keyboard: every printable
            // key is text, so no tool shortcut may fire while one is open.
            if self.features.screenshot.is_typing() {
                match keysym {
                    keys::KEY_Escape => {
                        self.features.screenshot.cancel_text_draft();
                    }
                    keys::KEY_Return | keys::KEY_KP_Enter => {
                        self.features.screenshot.commit_text_draft();
                    }
                    keys::KEY_BackSpace => self.features.screenshot.text_backspace(),
                    _ => {
                        // ASCII only: composing CJK needs an input method, and
                        // the window manager does not host one. The baked PNG
                        // renders whatever does arrive in the full UI font.
                        if let Some(ch) = Self::system_ui_char(keysym, clean_state) {
                            self.features.screenshot.text_input(ch);
                        }
                    }
                }
                self.sync_screenshot_annotation_overlay(backend, true);
                self.sync_screenshot_toolbar(backend);
                return Ok(());
            }

            if keysym == keys::KEY_Escape {
                self.cancel_screenshot_select(backend);
                return Ok(());
            }

            let ctrl = clean_state.contains(Mods::CONTROL);
            let shift = clean_state.contains(Mods::SHIFT);

            let capture_target = if !ctrl && keysym == keys::KEY_Tab {
                self.cycle_screenshot_capture_target(backend, shift);
                None
            } else if !ctrl && keysym == keys::KEY_g {
                Some(CaptureTarget::Region)
            } else if !ctrl && keysym == keys::KEY_w {
                Some(CaptureTarget::Window)
            } else if !ctrl && keysym == keys::KEY_m {
                Some(CaptureTarget::Monitor)
            } else if !ctrl && keysym == keys::KEY_d {
                Some(CaptureTarget::Desktop)
            } else {
                None
            };
            if let Some(target) = capture_target {
                self.set_screenshot_capture_target(backend, target);
                return Ok(());
            }
            if !ctrl && keysym == keys::KEY_Tab {
                return Ok(());
            }

            // Every tool has a letter, and the letters are the toolbar read
            // left to right wherever one was free.
            let requested_tool = if ctrl {
                None
            } else {
                match keysym {
                    keys::KEY_p | keys::KEY_f => Some(ScreenshotTool::Pencil),
                    keys::KEY_l => Some(ScreenshotTool::Line),
                    keys::KEY_a => Some(ScreenshotTool::Arrow),
                    keys::KEY_r => Some(ScreenshotTool::Rectangle),
                    keys::KEY_b => Some(ScreenshotTool::FilledRectangle),
                    keys::KEY_c | keys::KEY_o => Some(ScreenshotTool::Ellipse),
                    keys::KEY_h => Some(ScreenshotTool::Marker),
                    keys::KEY_t => Some(ScreenshotTool::Text),
                    keys::KEY_n => Some(ScreenshotTool::Counter),
                    keys::KEY_x => Some(ScreenshotTool::Pixelate),
                    keys::KEY_i => Some(ScreenshotTool::Invert),
                    _ => None,
                }
            };
            if let Some(tool) = requested_tool {
                self.features.screenshot.set_tool(tool);
                if backend.has_compositor() {
                    self.sync_screenshot_annotation_style(backend);
                    self.sync_screenshot_annotation_overlay(backend, true);
                }
                self.sync_screenshot_toolbar(backend);
                return Ok(());
            }

            if !ctrl && (keys::KEY_1..=keys::KEY_8).contains(&keysym) {
                self.features
                    .screenshot
                    .set_palette_color((keysym - keys::KEY_1) as usize);
                if backend.has_compositor() {
                    self.sync_screenshot_annotation_style(backend);
                    self.sync_screenshot_annotation_overlay(backend, true);
                }
                self.sync_screenshot_toolbar(backend);
                return Ok(());
            }

            if self.features.screenshot.committed {
                let nudge = if shift { 10.0 } else { 1.0 };

                if keysym == keys::KEY_Return
                    || keysym == keys::KEY_KP_Enter
                    || (ctrl && keysym == keys::KEY_s)
                {
                    self.finish_screenshot_select(backend, false);
                } else if ctrl && keysym == keys::KEY_c {
                    self.finish_screenshot_select(backend, true);
                } else if ctrl && (keysym == keys::KEY_y || (shift && keysym == keys::KEY_z)) {
                    self.features.screenshot.redo_annotation();
                    self.sync_screenshot_annotation_overlay(backend, false);
                    self.sync_screenshot_toolbar(backend);
                } else if ctrl && keysym == keys::KEY_z {
                    self.features.screenshot.undo_annotation();
                    self.sync_screenshot_annotation_overlay(backend, false);
                    self.sync_screenshot_toolbar(backend);
                } else if keysym == keys::KEY_BackSpace || keysym == keys::KEY_Delete {
                    self.features.screenshot.undo_annotation();
                    self.sync_screenshot_annotation_overlay(backend, false);
                    self.sync_screenshot_toolbar(backend);
                } else if (ctrl && keysym == keys::KEY_Up)
                    || keysym == keys::KEY_plus
                    || keysym == keys::KEY_equal
                {
                    self.features.screenshot.increase_line_width();
                    self.sync_screenshot_annotation_style(backend);
                    self.sync_screenshot_annotation_overlay(backend, true);
                    self.sync_screenshot_toolbar(backend);
                } else if (ctrl && keysym == keys::KEY_Down) || keysym == keys::KEY_minus {
                    self.features.screenshot.decrease_line_width();
                    self.sync_screenshot_annotation_style(backend);
                    self.sync_screenshot_annotation_overlay(backend, true);
                    self.sync_screenshot_toolbar(backend);
                } else if keysym == keys::KEY_Left
                    || keysym == keys::KEY_Right
                    || keysym == keys::KEY_Up
                    || keysym == keys::KEY_Down
                {
                    let (dx, dy) = match keysym {
                        keys::KEY_Left => (-nudge, 0.0),
                        keys::KEY_Right => (nudge, 0.0),
                        keys::KEY_Up => (0.0, -nudge),
                        keys::KEY_Down => (0.0, nudge),
                        _ => (0.0, 0.0),
                    };
                    self.features.screenshot.move_selection_within(
                        dx,
                        dy,
                        Rect::new(0, 0, self.s_w, self.s_h),
                    );
                    if backend.has_compositor() {
                        backend.compositor_set_snap_preview(
                            self.features
                                .screenshot
                                .get_selection_rect()
                                .map(|r| (r.x as f32, r.y as f32, r.w as f32, r.h as f32)),
                        );
                        backend.compositor_force_full_redraw();
                    }
                    // The strip follows the selection it belongs to.
                    self.sync_screenshot_toolbar(backend);
                }
                // Other keys are consumed silently
            }
            return Ok(());
        }

        if self.features.expose_active {
            if keysym == keys::KEY_Escape {
                return self.apply_expose_action(backend, expose_plan::plan_escape());
            }
            if keysym == keys::KEY_Left
                || keysym == keys::KEY_Right
                || keysym == keys::KEY_Up
                || keysym == keys::KEY_Down
            {
                let dir = match keysym {
                    keys::KEY_Left => ExposeNavDirection::Left,
                    keys::KEY_Right => ExposeNavDirection::Right,
                    keys::KEY_Up => ExposeNavDirection::Up,
                    _ => ExposeNavDirection::Down,
                };
                backend.compositor_expose_move(dir);
                return Ok(());
            }
            if keysym == keys::KEY_Return || keysym == keys::KEY_KP_Enter {
                let hit = backend.compositor_expose_selected();
                return self.apply_expose_action(backend, expose_plan::plan_click(hit));
            }
            if keysym == keys::KEY_Delete || keysym == keys::KEY_BackSpace {
                return self.close_expose_highlighted(backend);
            }
            // Fall through to normal keybinding dispatch so Alt+E can toggle off
        }

        if self.features.annotation_active {
            if keysym == keys::KEY_Escape {
                self.features.annotation_active = false;
                self.features.annotation_drawing = false;
                backend.compositor_set_annotation_mode(false);
                let _ = backend.key_ops().ungrab_keyboard();
                let _ = backend.input_ops().ungrab_pointer();
                return Ok(());
            }
            // Fall through to normal keybinding dispatch so Alt+Shift+A can toggle off
        }

        if self.features.overview.active {
            let overview_mods = clean_state
                & (Mods::SHIFT
                    | Mods::CONTROL
                    | Mods::ALT
                    | Mods::SUPER
                    | Mods::MOD2
                    | Mods::MOD3
                    | Mods::MOD5);

            // Tab / Shift+Tab / Alt+Tab / Alt+Shift+Tab → cycle forward / backward
            if keysym == keys::KEY_Tab && !overview_mods.contains(Mods::CONTROL) {
                let direction = if overview_mods.contains(Mods::SHIFT) {
                    -1
                } else {
                    1
                };
                if debug_keys {
                    info!(
                        "[overview] cycle via Tab keysym=0x{:x} mods=0x{:x} direction={}",
                        keysym,
                        overview_mods.bits(),
                        direction,
                    );
                }
                return self.cycle_overview(backend, &WMArgEnum::Int(direction));
            }
            // Alt+J → cycle forward, Alt+K → cycle backward
            if keysym == keys::KEY_j && overview_mods == Mods::ALT {
                return self.cycle_overview(backend, &WMArgEnum::Int(1));
            }
            if keysym == keys::KEY_k && overview_mods == Mods::ALT {
                return self.cycle_overview(backend, &WMArgEnum::Int(-1));
            }
            // Alt+Ctrl+Tab → confirm (close overview, focus selected)
            if keysym == keys::KEY_Tab
                && overview_mods.contains(Mods::ALT)
                && overview_mods.contains(Mods::CONTROL)
            {
                return self.toggle_overview(backend, &WMArgEnum::Int(0));
            }
            // Enter → confirm (close overview, focus selected)
            if keysym == keys::KEY_Return {
                return self.toggle_overview(backend, &WMArgEnum::Int(0));
            }
            // Escape → cancel (close overview, no focus change)
            if keysym == keys::KEY_Escape {
                self.features.overview.deactivate();
                backend.compositor_set_overview_mode(false, &[]);
                let _ = backend.key_ops().ungrab_keyboard();
                return Ok(());
            }
            // Consume all other keys while overview is active
            return Ok(());
        }

        // Chord state machine. The leader sets `chord_armed_until` and grabs
        // the keyboard so the WM gets the next keypress regardless of focus.
        // The next key either matches a chord binding (dispatch + ungrab) or
        // falls through to normal handling (also ungrab).
        if let Some(chord) = self.chord_compiled.clone() {
            // Expire stale arming.
            if let Some(deadline) = self.chord_armed_until {
                if std::time::Instant::now() >= deadline {
                    self.chord_armed_until = None;
                    let _ = backend.key_ops().ungrab_keyboard();
                }
            }

            if self.chord_armed_until.is_some() {
                // Find a matching second-key binding.
                let mut hit = None;
                for b in &chord.bindings {
                    if b.key_sym == keysym && (b.mask & key_mods) == clean_state {
                        hit = b.func_opt.map(|f| (f, b.arg.clone()));
                        break;
                    }
                }
                self.chord_armed_until = None;
                let _ = backend.key_ops().ungrab_keyboard();
                if let Some((func, arg)) = hit {
                    if let Err(e) = func(self, backend, &arg) {
                        error!("Error executing chord shortcut: {:?}", e);
                    }
                    return Ok(());
                }
                // Allow the leader itself to re-arm (Mod+Space then Mod+Space).
                if chord.leader == (clean_state, keysym) {
                    self.chord_armed_until = Some(std::time::Instant::now() + chord.timeout);
                    if let Some(root) = backend.root_window() {
                        let _ = backend.key_ops().grab_keyboard(root);
                    }
                    return Ok(());
                }
                // Otherwise fall through so the second key gets normal dispatch.
            } else if chord.leader == (clean_state, keysym) {
                // Arm the chord and capture next key.
                self.chord_armed_until = Some(std::time::Instant::now() + chord.timeout);
                if let Some(root) = backend.root_window() {
                    let _ = backend.key_ops().grab_keyboard(root);
                }
                if debug_keys {
                    info!("[chord] leader fired, armed for {:?}", chord.timeout);
                }
                return Ok(());
            }
        }

        // Find the first matching binding by immutable borrow; extract the
        // (Copy) fn pointer and clone only the matched arg instead of cloning
        // the whole key_bindings Vec on every keystroke.
        let found = self
            .key_bindings
            .iter()
            .find(|kc| keysym == kc.key_sym && (kc.mask & key_mods) == clean_state);
        let matched = found.is_some();
        let call = found.and_then(|kc| {
            if debug_keys {
                let func_name = kc.func_opt.map(Self::func_name).unwrap_or("<none>");
                info!(
                    "[key] matched keysym=0x{:x} mods=0x{:x} func={} arg={:?}",
                    keysym,
                    clean_state.bits(),
                    func_name,
                    kc.arg
                );
            }
            kc.func_opt.map(|func| (func, kc.arg.clone()))
        });
        if let Some((func, arg)) = call {
            if let Err(e) = func(self, backend, &arg) {
                error!("Error executing keyboard shortcut: {:?}", e);
            }
        }

        if debug_keys && !matched {
            info!(
                "[key] no match keysym=0x{:x} mods=0x{:x}",
                keysym,
                clean_state.bits()
            );
        }
        Ok(())
    }

    pub(crate) fn is_native_screenshot(func: WMFuncType) -> bool {
        std::ptr::fn_addr_eq(func, Jwm::take_screenshot as WMFuncType)
            || std::ptr::fn_addr_eq(func, Jwm::take_screenshot_fullscreen as WMFuncType)
    }

    /// The dedicated volume/brightness/media keysyms that stay live on the
    /// lock screen. Exactly the set the default keybinding table maps to the
    /// media actions; every other key remains swallowed by the modal lock.
    pub(crate) fn lock_media_keysym(keysym: u32) -> bool {
        matches!(
            keysym,
            keys::KEY_XF86AudioRaiseVolume
                | keys::KEY_XF86AudioLowerVolume
                | keys::KEY_XF86AudioMute
                | keys::KEY_XF86AudioPlay
                | keys::KEY_XF86AudioPause
                | keys::KEY_XF86AudioNext
                | keys::KEY_XF86AudioPrev
                | keys::KEY_XF86AudioStop
                | keys::KEY_XF86MonBrightnessUp
                | keys::KEY_XF86MonBrightnessDown
        )
    }

    /// The actions a lock-screen media key may invoke: exactly the volume,
    /// brightness and media transport functions — never a spawn, a panel
    /// opener, or anything else a user happened to bind to those keysyms.
    pub(crate) fn lock_media_func(func: WMFuncType) -> bool {
        std::ptr::fn_addr_eq(func, Jwm::volume_adjust as WMFuncType)
            || std::ptr::fn_addr_eq(func, Jwm::volume_mute as WMFuncType)
            || std::ptr::fn_addr_eq(func, Jwm::brightness_adjust as WMFuncType)
            || std::ptr::fn_addr_eq(func, Jwm::media_play_pause as WMFuncType)
            || std::ptr::fn_addr_eq(func, Jwm::media_next as WMFuncType)
            || std::ptr::fn_addr_eq(func, Jwm::media_previous as WMFuncType)
            || std::ptr::fn_addr_eq(func, Jwm::media_stop as WMFuncType)
    }

    pub(crate) fn on_button_press_internal(
        &mut self,
        backend: &mut dyn Backend,
        target: crate::backend::api::HitTarget,
        state_bits: u16,
        detail_btn: u8,
        time: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Recording source selection/adjustment intercept.
        if self.features.recording.selecting_region {
            let button = MouseButton::from_u8(detail_btn);
            if button == MouseButton::Left {
                if self.features.capture.recording == CaptureTarget::Region {
                    let (x, y) = self.last_mouse_root;
                    self.features
                        .recording
                        .begin_region_drag(x.round() as i32, y.round() as i32);
                } else {
                    self.commit_recording_capture_target(backend, target);
                }
            } else {
                self.features.capture.swallow_next_button_release();
                self.cancel_recording_region_interaction(backend);
            }
            return Ok(());
        }

        // Screenshot region selection intercept
        if self.features.screenshot.active {
            let btn = MouseButton::from_u8(detail_btn);
            let (px, py) = self.last_mouse_root;

            // The wheel adjusts the stroke instead of cancelling the capture,
            // which is what every other annotation tool does with it — and
            // losing a half-annotated selection to a stray scroll was a nasty
            // way to find out otherwise.
            if self.features.screenshot.committed && matches!(btn, MouseButton::Other(4 | 5)) {
                self.features.capture.swallow_next_button_release();
                if btn == MouseButton::Other(4) {
                    self.features.screenshot.increase_line_width();
                } else {
                    self.features.screenshot.decrease_line_width();
                }
                self.sync_screenshot_annotation_style(backend);
                self.sync_screenshot_annotation_overlay(backend, true);
                self.sync_screenshot_toolbar(backend);
                return Ok(());
            }

            // The toolbar floats over the canvas, so a left press inside it is
            // the toolbar's — including a press in the padding between two
            // buttons, which must not start a stroke through the strip. Other
            // buttons deliberately fall through, so right-click still cancels
            // the capture wherever the pointer happens to be.
            if btn == MouseButton::Left
                && self.features.screenshot.committed
                && self.screenshot_toolbar_contains(px, py)
            {
                self.features.capture.swallow_next_button_release();
                if let Some(command) = self
                    .screenshot_toolbar_hit(px, py)
                    .and_then(|index| self.features.screenshot.toolbar_command(index))
                {
                    self.apply_screenshot_toolbar_command(backend, command);
                }
                return Ok(());
            }

            if btn == MouseButton::Left && self.features.screenshot.committed {
                let (x, y) = (px, py);
                self.features
                    .screenshot
                    .begin_annotation(x as f32, y as f32);
                if matches!(
                    self.features.screenshot.tool,
                    ScreenshotTool::Pencil | ScreenshotTool::Marker
                ) && backend.has_compositor()
                {
                    let ink = if self.features.screenshot.tool == ScreenshotTool::Marker {
                        marker_ink(self.features.screenshot.color)
                    } else {
                        self.features.screenshot.color
                    };
                    Self::emit_screenshot_polyline(
                        backend,
                        ink,
                        self.features.screenshot.stroke_width(),
                        &[(x as f32, y as f32), (x as f32, y as f32)],
                    );
                    backend.compositor_force_full_redraw();
                } else if self.features.screenshot.tool.is_click_placed() {
                    // A click-placed mark is finished the moment it lands, so
                    // the overlay and the toolbar have to catch up now rather
                    // than on a motion that may never come.
                    self.sync_screenshot_annotation_overlay(backend, true);
                    self.sync_screenshot_toolbar(backend);
                }
            } else if btn == MouseButton::Left
                && self.features.capture.screenshot != CaptureTarget::Region
            {
                self.commit_screenshot_capture_target(backend, target);
            } else if btn == MouseButton::Left {
                self.features
                    .screenshot
                    .begin_drag(self.last_mouse_root.0, self.last_mouse_root.1);
                // Immediately show a 1x1 preview to avoid animation delay
                if backend.has_compositor() {
                    let (x, y) = self.last_mouse_root;
                    backend.compositor_set_snap_preview(Some((x as f32, y as f32, 1.0, 1.0)));
                    backend.compositor_force_full_redraw();
                }
            } else {
                // Right-click or other button → cancel without leaking the release.
                self.features.capture.swallow_next_button_release();
                self.cancel_screenshot_select(backend);
            }
            return Ok(());
        }

        // Expose mode intercept: route clicks to compositor. Every button
        // except middle commits exactly as it always has — a hit focuses the
        // clicked window; hit or miss, expose exits. A middle click instead
        // closes the clicked cell's window, browser-tab style, and never
        // commits: on a miss it is a no-op and the gesture stays up.
        if self.features.expose_active {
            let (rx, ry) = self.last_mouse_root;
            let hit = backend.compositor_expose_click(rx as f32, ry as f32);
            if MouseButton::from_u8(detail_btn) == MouseButton::Middle {
                return self.close_expose_clicked(backend, hit);
            }
            return self.apply_expose_action(backend, expose_plan::plan_click(hit));
        }

        // A click on a toast card dismisses it; a left click on an action
        // button additionally invokes the action against the notification
        // record. The click is swallowed here — before any window dispatch —
        // so it is never replayed to the client underneath the card, nor
        // resolved against the tab-strip cell the card is docked over.
        let press = toast_press(MouseButton::from_u8(detail_btn));
        if press != ToastPress::Ignored {
            let (rx, ry) = self.last_mouse_root;
            match backend.compositor_click_toast(rx as f32, ry as f32) {
                crate::backend::api::ToastClick::Miss => {}
                crate::backend::api::ToastClick::Dismissed => return Ok(()),
                crate::backend::api::ToastClick::Action {
                    notification_id,
                    action_key,
                } => {
                    // Invoking closes the record (freedesktop order:
                    // ActionInvoked, then NotificationClosed); the compositor
                    // already dismissed the card itself.
                    if press == ToastPress::Activate {
                        self.invoke_notification_action(notification_id, &action_key);
                    }
                    return Ok(());
                }
            }
        }

        let mut click_type = WMClickType::ClickRootWin;
        let clicked_win: Option<crate::backend::common_define::WindowId> = match target {
            HitTarget::Surface(wid) => Some(wid),
            HitTarget::Background { .. } => None,
        };
        let target_mon_key = self.target_to_monitor(
            backend,
            target,
            (self.last_mouse_root.0 as i32, self.last_mouse_root.1 as i32),
        );
        if target_mon_key != self.state.sel_mon {
            if let Some(cur) = self.get_selected_client_key() {
                self.unfocus_client(backend, cur, true)?;
            }
            self.state.sel_mon = target_mon_key;
            self.focus(backend, None)?;
        }
        // A click on a monitor's tab bar picks that window. The strip is space
        // the layout reserved, so nothing tiled can be under it; a floating
        // window that happens to cover it still keeps its own clicks.
        let clicked_a_managed_window = clicked_win
            .filter(|&wid| Some(wid) != backend.root_window())
            .and_then(|wid| self.wintoclient(wid))
            .is_some();
        if !clicked_a_managed_window {
            let (x, y) = backend
                .input_ops()
                .get_pointer_position()
                .unwrap_or(self.last_mouse_root);
            if MouseButton::from_u8(detail_btn) == MouseButton::Middle {
                // Middle-click closes the window the cell stands for.
                if self.close_window_tab(backend, x, y)? {
                    return Ok(());
                }
            } else if self.click_window_tab(backend, x, y)? {
                // A left press also arms a reorder drag, committed by the
                // release if the pointer crosses the drag threshold; any
                // other button stays a plain focus click.
                if MouseButton::from_u8(detail_btn) == MouseButton::Left {
                    self.arm_window_tab_drag(x, y);
                }
                return Ok(());
            }
        }

        let mut is_client_click = false;
        let mut clicked_client_key: Option<ClientKey> = None;
        if let Some(wid) = clicked_win {
            if Some(wid) != backend.root_window() {
                if let Some(client_key) = self.wintoclient(wid) {
                    is_client_click = true;
                    clicked_client_key = Some(client_key);
                    self.focus(backend, Some(client_key))?;
                    // Invalidate stacking cache so restack always applies the
                    // new z-order when clicking a partially-obscured window.
                    if let Some(mon_key) = self.state.sel_mon {
                        self.last_stacking.remove(mon_key);
                    }
                    let _ = self.restack(backend, self.state.sel_mon);
                    click_type = WMClickType::ClickClientWin;
                }
            }
        }

        let event_mask = self.clean_mask(backend, state_bits);
        let mouse_button = MouseButton::from_u8(detail_btn);

        let mut handled_by_wm = false;
        for config in CONFIG.load().get_buttons().iter() {
            let kc_mask = config.mask
                & (Mods::SHIFT
                    | Mods::CONTROL
                    | Mods::ALT
                    | Mods::SUPER
                    | Mods::MOD2
                    | Mods::MOD3
                    | Mods::MOD5);
            if config.click_type == click_type
                && config.func.is_some()
                && config.button == mouse_button
                && kc_mask == event_mask
            {
                handled_by_wm = true;
                if let Some(ref func) = config.func {
                    if Self::debug_drag_enabled()
                        && event_mask.contains(Mods::CONTROL)
                        && mouse_button == MouseButton::Left
                        && is_client_click
                    {
                        let (px, py) = backend
                            .input_ops()
                            .get_pointer_position()
                            .unwrap_or((self.last_mouse_root.0, self.last_mouse_root.1));

                        let (win, geom) = clicked_client_key
                            .and_then(|ck| {
                                self.state
                                    .clients
                                    .get(ck)
                                    .map(|c| (c.win, c.geometry.clone()))
                            })
                            .map(|(w, g)| (Some(w), Some(g)))
                            .unwrap_or((clicked_win, None));

                        let func_name = Self::func_name(*func);
                        info!(
                            "[drag] Ctrl+Left ButtonPress: click_type={:?} win={:?} client={:?} func={} mods=0x{:x} pointer=({:.1},{:.1}) geom={:?}",
                            click_type,
                            win,
                            clicked_client_key,
                            func_name,
                            event_mask.bits(),
                            px,
                            py,
                            geom
                        );
                    }
                    let _ = func(self, backend, &config.arg);
                }
                break;
            }
        }

        if is_client_click {
            let _ = if handled_by_wm {
                backend
                    .input_ops()
                    .allow_events(AllowMode::AsyncPointer, time)
            } else {
                backend
                    .input_ops()
                    .allow_events(AllowMode::ReplayPointer, time)
            };
        }
        Ok(())
    }

    pub(crate) fn on_motion_notify_internal(
        &mut self,
        backend: &mut dyn Backend,
        _window: Option<WindowId>,
        root_x: i16,
        root_y: i16,
        _time: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A left press on a tab cell turns into a reorder drag once the
        // pointer crosses the drag threshold; the release commits the new
        // slot. No live preview — the commit is a single arrange. Ahead of
        // the focus guard below: the guard exists to keep pointer motion
        // from moving focus for a moment after a keyboard focus change or a
        // new map, and a drag started inside that moment is not a focus
        // change — held behind it, a quick flick would silently stay a click.
        if let Some(drag) = self.tab_drag.as_mut() {
            if !drag.activated {
                let thr = CONFIG.load().drag_threshold_px() as f64;
                let (dx, dy) = (
                    root_x as f64 - drag.start_root.0,
                    root_y as f64 - drag.start_root.1,
                );
                if dx * dx + dy * dy >= thr * thr {
                    drag.activated = true;
                }
            }
        }

        // 1. 如果因为键盘操作等原因暂时阻塞了鼠标聚焦，直接返回
        if self.mouse_focus_blocked() {
            return Ok(());
        }

        // 3. 更新当前鼠标所在的显示器状态
        let new_monitor_key = self.recttomon(backend, root_x as i32, root_y as i32);
        if new_monitor_key != self.state.motion_mon {
            self.handle_monitor_switch_by_key(backend, new_monitor_key)?;
        }
        self.state.motion_mon = new_monitor_key;

        Ok(())
    }

    pub(crate) fn on_configure_request_internal(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        mask_bits: u16,
        changes: WindowChanges,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(client_key) = self.wintoclient(window) {
            return self
                .handle_regular_configure_request_params(backend, client_key, mask_bits, changes);
        }

        self.handle_unmanaged_configure_request_params(backend, window, mask_bits, changes)
    }

    pub(crate) fn handle_regular_configure_request_params(
        &mut self,
        backend: &mut dyn Backend,
        client_key: ClientKey,
        mask_bits: u16,
        req: WindowChanges,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A managed fullscreen rectangle and its pre-fullscreen `old_*`
        // return slot are WM-owned.  Fullscreen temporarily forces the client
        // into the floating group, so letting the ordinary floating branch
        // below consume a ConfigureRequest would both move/resize the live
        // fullscreen window and overwrite that return slot.  This is even
        // more dangerous while minimized: the live x is JWM's off-screen
        // parking coordinate, which could then become the fullscreen exit
        // target.
        //
        // ICCCM requires a WM that rejects requested geometry to report the
        // authoritative geometry back to the client.  `configure_client`
        // performs that response for both X11 transports.  Do it before the
        // border request or any current/old geometry field can be mutated.
        if self
            .state
            .clients
            .get(client_key)
            .is_some_and(|client| client.state.is_fullscreen)
        {
            return self.configure_client(backend, client_key);
        }

        let mask = ConfigWindowBits::from_bits_truncate(mask_bits);

        let (win, is_dock, no_decorations) = self
            .state
            .clients
            .get(client_key)
            .map(|client| {
                (
                    client.win,
                    client.state.is_dock,
                    client.state.no_decorations,
                )
            })
            .ok_or("Client not found")?;
        let window_types = backend.property_ops().get_window_types(win);
        let is_popup = RuleMatcher::types_are_popup_like(&window_types);
        let wm_owns_border =
            no_decorations || is_popup || RuleMatcher::is_structurally_borderless(&window_types);

        if is_dock {
            if let Some(client) = self.state.clients.get(client_key) {
                info!(
                    "[dock_configure_request] win={:?} mask=0x{:x} req={:?} current={}x{}+{}+{}",
                    client.win,
                    mask_bits,
                    req,
                    client.geometry.w,
                    client.geometry.h,
                    client.geometry.x,
                    client.geometry.y
                );
                let changes = WindowChanges {
                    x: Some(client.geometry.x),
                    y: Some(client.geometry.y),
                    width: Some(client.geometry.w as u32),
                    height: Some(client.geometry.h as u32),
                    border_width: Some(client.geometry.border_w.max(0) as u32),
                    ..Default::default()
                };
                backend
                    .window_ops()
                    .apply_window_changes(client.win, changes)?;
            }
            return Ok(());
        }

        if mask.contains(ConfigWindowBits::BORDER_WIDTH) {
            if let Some(border) = req.border_width {
                if !wm_owns_border {
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.geometry.border_w = bounded_configure_border(border);
                    }
                }
            }
        }

        let (is_floating, mon_key_opt) = if let Some(client) = self.state.clients.get(client_key) {
            (client.state.is_floating, client.mon)
        } else {
            return Err("Client not found".into());
        };

        if is_floating {
            let (mx, my, mw, mh) = if let Some(mon_key) = mon_key_opt {
                let monitor = self
                    .state
                    .monitors
                    .get(mon_key)
                    .ok_or("Monitor not found")?;
                (
                    monitor.geometry.m_x,
                    monitor.geometry.m_y,
                    monitor.geometry.m_w,
                    monitor.geometry.m_h,
                )
            } else {
                return Err("Client has no monitor assigned".into());
            };

            let mut popup_apply: Option<WindowId> = None;
            let mut popup_clamp_request: Option<(i32, i32, i32, i32)> = None;
            let mut popup_is_dialog = false;

            let mut clamp_request: Option<(i32, i32, i32, i32)> = None;

            if let Some(client) = self.state.clients.get_mut(client_key) {
                if mask.contains(ConfigWindowBits::X) {
                    if let Some(x) = req.x {
                        client.geometry.old_x = client.geometry.x;
                        // ConfigureRequest coordinates for a managed top-level
                        // X11 window are relative to its parent (the root), so
                        // they are already in global desktop coordinates.
                        client.geometry.x = x;
                    }
                }
                if mask.contains(ConfigWindowBits::Y) {
                    if let Some(y) = req.y {
                        client.geometry.old_y = client.geometry.y;
                        client.geometry.y = y;
                    }
                }
                if mask.contains(ConfigWindowBits::WIDTH) {
                    if let Some(w) = req.width {
                        client.geometry.old_w = client.geometry.w;
                        client.geometry.w = bounded_configure_dimension(w);
                    }
                }
                if mask.contains(ConfigWindowBits::HEIGHT) {
                    if let Some(h) = req.height {
                        client.geometry.old_h = client.geometry.h;
                        client.geometry.h = bounded_configure_dimension(h);
                    }
                }

                // Defer workarea clamping until after we release the mutable borrow.
                // Skip clamping for windows that cover the full monitor (e.g.
                // screenshot overlays that intentionally span strut areas).
                let covers_monitor = client.geometry.x <= mx
                    && client.geometry.y <= my
                    && client.total_width() >= mw
                    && client.total_height() >= mh;
                if client.state.is_floating && !client.state.is_fullscreen && !covers_monitor {
                    clamp_request = Some((
                        client.geometry.x,
                        client.geometry.y,
                        client.total_width(),
                        client.total_height(),
                    ));
                }

                if is_popup {
                    let types = backend.property_ops().get_window_types(client.win);
                    let should_clamp = types.contains(&WindowType::Notification)
                        || types.contains(&WindowType::Dialog);
                    popup_is_dialog = types.contains(&WindowType::Dialog);

                    if should_clamp {
                        popup_clamp_request = Some((
                            client.geometry.x,
                            client.geometry.y,
                            client.total_width(),
                            client.total_height(),
                        ));
                    }
                    popup_apply = Some(client.win);
                }
            }

            // Popup-like windows: apply workarea clamp for Dialog/Notification, then commit.
            if let Some(win) = popup_apply {
                if let (Some(mon_key), Some((x, y, total_w, total_h))) =
                    (mon_key_opt, popup_clamp_request)
                {
                    let mut clamp = self
                        .monitor_work_area(mon_key)
                        .unwrap_or(Rect::new(mx, my, mw, mh));

                    // For transient dialogs, intersect with parent bounds to avoid jumping
                    // across tiled columns.
                    if popup_is_dialog {
                        if let Some(parent_key) = self.parent_client_of(backend, client_key) {
                            if let Some(parent) = self.state.clients.get(parent_key) {
                                let parent_rect = Rect::new(
                                    parent.geometry.x,
                                    parent.geometry.y,
                                    parent.total_width(),
                                    parent.total_height(),
                                );

                                let left = i64::from(clamp.x.max(parent_rect.x));
                                let top = i64::from(clamp.y.max(parent_rect.y));
                                let right = (i64::from(clamp.x) + i64::from(clamp.w))
                                    .min(i64::from(parent_rect.x) + i64::from(parent_rect.w));
                                let bottom = (i64::from(clamp.y) + i64::from(clamp.h))
                                    .min(i64::from(parent_rect.y) + i64::from(parent_rect.h));
                                let w = (right - left).clamp(0, i64::from(i32::MAX)) as i32;
                                let h = (bottom - top).clamp(0, i64::from(i32::MAX)) as i32;
                                if w > 0 && h > 0 {
                                    clamp = Rect::new(left as i32, top as i32, w, h);
                                }
                            }
                        }
                    }

                    let clamped_x = clamp_configure_axis(x, total_w, clamp.x, clamp.w);
                    let clamped_y = clamp_configure_axis(y, total_h, clamp.y, clamp.h);

                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.geometry.x = clamped_x;
                        client.geometry.y = clamped_y;
                    }
                }

                if let Some(client) = self.state.clients.get(client_key) {
                    let border_width = if backend.has_compositor() {
                        0
                    } else {
                        client.geometry.border_w.max(0) as u32
                    };
                    let changes = WindowChanges {
                        x: Some(client.geometry.x),
                        y: Some(client.geometry.y),
                        width: Some(client.geometry.w as u32),
                        height: Some(client.geometry.h as u32),
                        border_width: Some(border_width),
                        ..Default::default()
                    };
                    backend.window_ops().apply_window_changes(win, changes)?;
                }

                return Ok(());
            }

            // Clamp floating (non-fullscreen) windows to the monitor workarea so they don't end
            // up under dock/statusbar reserved space.
            if let (Some(mon_key), Some((x, y, total_w, total_h))) = (mon_key_opt, clamp_request) {
                let clamp = self
                    .monitor_work_area(mon_key)
                    .unwrap_or(Rect::new(mx, my, mw, mh));

                let clamped_x = clamp_configure_axis(x, total_w, clamp.x, clamp.w);
                let clamped_y = clamp_configure_axis(y, total_h, clamp.y, clamp.h);

                if let Some(client) = self.state.clients.get_mut(client_key) {
                    if client.state.is_floating && !client.state.is_fullscreen {
                        client.geometry.x = clamped_x;
                        client.geometry.y = clamped_y;
                    }
                }
            }

            if mask.contains(ConfigWindowBits::X | ConfigWindowBits::Y)
                && !mask.contains(ConfigWindowBits::WIDTH | ConfigWindowBits::HEIGHT)
            {
                self.configure_client(backend, client_key)?;
            }

            if self.is_client_visible_by_key(client_key) {
                if let Some(client) = self.state.clients.get(client_key) {
                    let border_width = if backend.has_compositor() {
                        0
                    } else {
                        client.geometry.border_w.max(0) as u32
                    };
                    let changes = WindowChanges {
                        x: Some(client.geometry.x),
                        y: Some(client.geometry.y),
                        width: Some(client.geometry.w as u32),
                        height: Some(client.geometry.h as u32),
                        border_width: Some(border_width),
                        ..Default::default()
                    };
                    backend
                        .window_ops()
                        .apply_window_changes(client.win, changes)?;
                }
            }
        } else {
            self.configure_client(backend, client_key)?;
        }

        Ok(())
    }

    pub(crate) fn handle_unmanaged_configure_request_params(
        &mut self,
        backend: &mut dyn Backend,
        window: WindowId,
        mask_bits: u16,
        req: WindowChanges,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!(
            "[handle_unmanaged_configure_request] unmanaged window={:?}",
            window
        );

        let mask = ConfigWindowBits::from_bits_truncate(mask_bits);
        let mut changes = WindowChanges::default();

        if mask.contains(ConfigWindowBits::X) {
            changes.x = req.x;
        }
        if mask.contains(ConfigWindowBits::Y) {
            changes.y = req.y;
        }
        if mask.contains(ConfigWindowBits::WIDTH) {
            changes.width = req.width;
        }
        if mask.contains(ConfigWindowBits::HEIGHT) {
            changes.height = req.height;
        }
        if mask.contains(ConfigWindowBits::BORDER_WIDTH) {
            changes.border_width = req.border_width;
        }
        if mask.contains(ConfigWindowBits::SIBLING) {
            changes.sibling = req.sibling;
        }
        if mask.contains(ConfigWindowBits::STACK_MODE) {
            changes.stack_mode = req.stack_mode;
        }

        backend.window_ops().apply_window_changes(window, changes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{choose_system_ui_viewport, parse_direct_launcher_command};
    use crate::Jwm;
    use crate::backend::api::{
        Backend, BackendDiagnostics, Capabilities, CloseResult, ColorAllocator,
        CompositorAnnotation, CompositorBenchmark, CompositorControl, CompositorMedia,
        CompositorWindowEffects, CompositorWorkspaceEffects, DisplayControl, EventHandler,
        Geometry, HitTarget, RenderScheduler, WindowAttributes, WindowChanges, WindowOps,
    };
    use crate::backend::common_define::{ConfigWindowBits, MouseButton, Pixel, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps,
    };
    use crate::core::models::WMClient;
    use crate::core::types::Rect;
    use std::any::Any;
    use std::sync::Mutex;

    #[test]
    fn ordinary_system_ui_targets_the_selected_monitor_in_global_coordinates() {
        let viewport =
            choose_system_ui_viewport(false, Some((-1920, 160, 1920, 1080)), (3840, 1440));
        assert_eq!(viewport.rect(), [-1920.0, 160.0, 1920.0, 1080.0]);
    }

    #[test]
    fn lock_and_missing_selection_keep_the_full_virtual_desktop() {
        let selected = Some((1920, 0, 1920, 1080));
        assert_eq!(
            choose_system_ui_viewport(true, selected, (3840, 1440)).rect(),
            [0.0, 0.0, 3840.0, 1440.0]
        );
        assert_eq!(
            choose_system_ui_viewport(false, None, (3840, 1440)).rect(),
            [0.0, 0.0, 3840.0, 1440.0]
        );
    }

    /// Every writer of `do_not_disturb` broadcasts `dnd/toggle`, because a
    /// bar subscribed to it has no other way to learn the new state: the
    /// keybinding and the IPC command through `toggle_dnd`, a reload through
    /// `apply_config_changes`. The control-center row is the third writer and
    /// goes through the toggle rather than repeating the broadcast — exactly
    /// as its Caffeine neighbour does. The needles are built at runtime and
    /// the haystack is one arm of one function, well above this module, so
    /// this cannot match its own source.
    #[test]
    fn the_do_not_disturb_row_toggles_through_the_broadcasting_path() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let arm = SOURCE
            .split_once(&format!("fn {}(", "handle_control_center_key"))
            .expect("handle_control_center_key")
            .1
            .split_once(&format!("{}::{} =>", "ControlKind", "DoNotDisturb"))
            .expect("the do-not-disturb arm")
            .1
            .split_once(&format!("{}::{} =>", "ControlKind", "Caffeine"))
            .expect("the arm that follows it")
            .0;
        assert!(
            arm.contains(&format!("{}(", "toggle_dnd")),
            "the row flips do-not-disturb without the `dnd/toggle` broadcast"
        );
        assert!(
            !arm.contains(&format!("{}.{} = ", "self", "do_not_disturb")),
            "the row still assigns the field itself instead of using the toggle"
        );
        assert!(
            arm.contains(&format!("{}(", "update_control")),
            "the row no longer refreshes its own panel entry"
        );
    }

    #[test]
    fn system_ui_text_accepts_unicode_keysyms() {
        use crate::backend::common_define::Mods;

        // XKB's encoded-Unicode keysym for U+4E2D and the legacy Latin-1
        // keysym for e-acute both resolve through keysym_to_utf32.
        assert_eq!(Jwm::system_ui_char(0x0100_4e2d, Mods::empty()), Some('中'));
        assert_eq!(Jwm::system_ui_char(0x00e9, Mods::empty()), Some('é'));
        assert_eq!(
            Jwm::system_ui_char(crate::backend::common_define::keys::KEY_1, Mods::SHIFT,),
            Some('!')
        );
    }

    #[test]
    fn system_ui_char_applies_caps_lock_to_letters_only() {
        use crate::backend::common_define::{Mods, keys};

        // Caps XORs with shift on ASCII letters: alone it capitalizes, with
        // shift it lowers — the lock screen's password field behaves like
        // every real lock screen once `char_mods` carries the live reading.
        assert_eq!(Jwm::system_ui_char(keys::KEY_a, Mods::CAPS), Some('A'));
        assert_eq!(
            Jwm::system_ui_char(keys::KEY_a, Mods::CAPS | Mods::SHIFT),
            Some('a')
        );
        // Digits and punctuation follow the shift table only; caps alone is
        // not shift.
        assert_eq!(Jwm::system_ui_char(keys::KEY_1, Mods::CAPS), Some('1'));
        // The Caps Lock press itself is not a printable character: no
        // password character appears for the keystroke that toggles it.
        assert_eq!(Jwm::system_ui_char(keys::KEY_Caps_Lock, Mods::CAPS), None);
        // A non-ASCII keysym already names the layout's character; caps does
        // not reach it.
        assert_eq!(Jwm::system_ui_char(0x00e9, Mods::CAPS), Some('é'));
    }

    #[test]
    fn the_lock_screen_media_passthrough_covers_exactly_the_xf86_set() {
        use crate::backend::common_define::keys;

        for keysym in [
            keys::KEY_XF86AudioRaiseVolume,
            keys::KEY_XF86AudioLowerVolume,
            keys::KEY_XF86AudioMute,
            keys::KEY_XF86AudioPlay,
            keys::KEY_XF86AudioPause,
            keys::KEY_XF86AudioNext,
            keys::KEY_XF86AudioPrev,
            keys::KEY_XF86AudioStop,
            keys::KEY_XF86MonBrightnessUp,
            keys::KEY_XF86MonBrightnessDown,
        ] {
            assert!(
                Jwm::lock_media_keysym(keysym),
                "keysym 0x{keysym:x} fell out of the lock-screen set"
            );
        }
        // Password keys, navigation and ordinary function keys stay
        // swallowed.
        assert!(!Jwm::lock_media_keysym(keys::KEY_a));
        assert!(!Jwm::lock_media_keysym(keys::KEY_1));
        assert!(!Jwm::lock_media_keysym(keys::KEY_Return));
        assert!(!Jwm::lock_media_keysym(keys::KEY_Escape));
        assert!(!Jwm::lock_media_keysym(keys::KEY_F5));
    }

    #[test]
    fn the_lock_screen_media_passthrough_only_runs_media_actions() {
        use crate::jwm::types::WMFuncType;

        for func in [
            Jwm::volume_adjust as WMFuncType,
            Jwm::volume_mute as WMFuncType,
            Jwm::brightness_adjust as WMFuncType,
            Jwm::media_play_pause as WMFuncType,
            Jwm::media_next as WMFuncType,
            Jwm::media_previous as WMFuncType,
            Jwm::media_stop as WMFuncType,
        ] {
            assert!(Jwm::lock_media_func(func));
        }
        // A user binding one of those keysyms to anything else — a panel
        // opener, a lock action, a spawn — stays swallowed behind the lock.
        assert!(!Jwm::lock_media_func(Jwm::app_launcher as WMFuncType));
        assert!(!Jwm::lock_media_func(Jwm::lock_screen as WMFuncType));
        assert!(!Jwm::lock_media_func(Jwm::spawn as WMFuncType));
    }

    /// GNOME, KDE, macOS and Windows all answer volume and transport keys on
    /// their lock screens; jwm's locked arm swallowed them with every other
    /// key. The pin is textual because driving the modal branch needs a
    /// grabbing backend: the passthrough must be consulted inside the locked
    /// arm, before the fall-through that feeds the password field. Needles
    /// are assembled at runtime so this test's own source cannot match them.
    #[test]
    fn the_locked_arm_passes_media_keys_through_before_swallowing() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let body = SOURCE
            .split_once(&format!("fn {}(", "on_key_press_internal"))
            .expect("the key handler")
            .1
            .split_once(&format!("fn {}(", "is_native_screenshot"))
            .expect("the function after the key handler")
            .0;
        let passthrough = body
            .find(&format!("Self::{}(", "lock_media_keysym"))
            .expect("the locked arm never consults the media passthrough");
        let guard = body
            .find(&format!("Self::{}", "lock_media_func"))
            .expect("the passthrough lost its action guard");
        let swallow = body
            .find("system_ui.push_char")
            .expect("the password field feed");
        assert!(
            passthrough < swallow && guard < swallow,
            "media keys are swallowed with every other key again"
        );
    }

    /// The round-13 freeze shape on the auth path: a wrong password blocks
    /// `pam_authenticate` for ~2s on stock pam_unix, and pam_sss,
    /// fingerprint or faillock wait arbitrarily long. The pin is textual,
    /// needles assembled at runtime: this file must not name the blocking
    /// call at all — its only caller is the worker in system_ui.rs — and
    /// the locked Enter path must go through the worker submit.
    #[test]
    fn pam_never_runs_on_the_compositor_thread_from_the_key_handler() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let inline_call = format!("{}(", "authenticate_current_user");
        assert!(
            !SOURCE.contains(&inline_call),
            "Enter called PAM inline again; only the worker may"
        );
        let body = SOURCE
            .split_once(&format!("fn {}(", "on_key_press_internal"))
            .expect("the key handler")
            .1
            .split_once(&format!("fn {}(", "is_native_screenshot"))
            .expect("the function after the key handler")
            .0;
        assert!(
            body.contains(&format!("self.{}()", "submit_lock_password")),
            "the locked Enter path no longer submits the authentication worker"
        );
        const SYSTEM_UI: &str = include_str!("features/system_ui.rs");
        let worker = SYSTEM_UI
            .split_once(&format!("struct {}", "ZeroizingPassword"))
            .expect("the wiping password buffer")
            .1
            .split_once(&format!("fn {}(", "authenticate_pam"))
            .expect("the PAM client after it")
            .0;
        assert!(
            worker.contains(&inline_call),
            "the worker no longer performs the authentication"
        );
        assert!(
            worker.contains("BackgroundJob::spawn"),
            "the authentication is back on the calling thread"
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ConfigureReply {
        window: WindowId,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        border: u32,
    }

    #[derive(Default)]
    struct ConfigureReplyWindowOps {
        replies: Mutex<Vec<ConfigureReply>>,
        applied: Mutex<Vec<(WindowId, WindowChanges)>>,
        closed: Mutex<Vec<WindowId>>,
    }

    impl WindowOps for ConfigureReplyWindowOps {
        fn set_position(&self, _win: WindowId, _x: i32, _y: i32) -> Result<(), BackendError> {
            Ok(())
        }

        fn configure(
            &self,
            window: WindowId,
            x: i32,
            y: i32,
            width: u32,
            height: u32,
            border: u32,
        ) -> Result<(), BackendError> {
            self.replies
                .lock()
                .expect("configure reply lock")
                .push(ConfigureReply {
                    window,
                    x,
                    y,
                    width,
                    height,
                    border,
                });
            Ok(())
        }

        fn set_decoration_style(
            &self,
            _win: WindowId,
            _border_width: u32,
            _border_color: Pixel,
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn raise_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn map_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn unmap_window(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn close_window(&self, win: WindowId) -> Result<CloseResult, BackendError> {
            self.closed.lock().expect("closed windows lock").push(win);
            Ok(CloseResult::Graceful)
        }

        fn set_input_focus(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn get_window_attributes(&self, _win: WindowId) -> Result<WindowAttributes, BackendError> {
            Ok(WindowAttributes {
                override_redirect: false,
                map_state_viewable: true,
            })
        }

        fn get_geometry(&self, _win: WindowId) -> Result<Geometry, BackendError> {
            Ok(Geometry::default())
        }

        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            Ok(Vec::new())
        }

        fn flush(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn kill_client(&self, _win: WindowId) -> Result<(), BackendError> {
            Ok(())
        }

        fn apply_window_changes(
            &self,
            win: WindowId,
            changes: WindowChanges,
        ) -> Result<(), BackendError> {
            self.applied
                .lock()
                .expect("applied changes lock")
                .push((win, changes));
            Ok(())
        }
    }

    struct ConfigureReplyBackend {
        window_ops: ConfigureReplyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        /// Every OSD card the session asked for, in order.
        osd_log: std::sync::Arc<Mutex<Vec<(crate::backend::api::OsdKind, u8)>>>,
        /// Every toast pushed past the DND gate, in order (titles only).
        toast_log: std::sync::Arc<Mutex<Vec<String>>>,
        /// The expose grid the session last published, entry ids in order
        /// (empty while expose is off), and the highlighted entry.
        expose_windows: Vec<WindowId>,
        expose_selected: Option<WindowId>,
        /// What the next expose click hit-test answers.
        expose_click_hit: Option<WindowId>,
    }

    impl ConfigureReplyBackend {
        fn new() -> Self {
            Self {
                window_ops: ConfigureReplyWindowOps::default(),
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                osd_log: std::sync::Arc::new(Mutex::new(Vec::new())),
                toast_log: std::sync::Arc::new(Mutex::new(Vec::new())),
                expose_windows: Vec::new(),
                expose_selected: None,
                expose_click_hit: None,
            }
        }
    }

    impl CompositorBenchmark for ConfigureReplyBackend {}
    impl BackendDiagnostics for ConfigureReplyBackend {}
    impl CompositorControl for ConfigureReplyBackend {}
    impl CompositorMedia for ConfigureReplyBackend {}
    impl CompositorWorkspaceEffects for ConfigureReplyBackend {
        fn compositor_show_osd(&mut self, kind: crate::backend::api::OsdKind, percent: u8) {
            self.osd_log
                .lock()
                .expect("osd log lock")
                .push((kind, percent));
        }

        fn compositor_push_toast(&mut self, toast: crate::backend::api::ToastNotification) {
            self.toast_log
                .lock()
                .expect("toast log lock")
                .push(toast.title);
        }

        fn compositor_set_expose_mode(
            &mut self,
            active: bool,
            windows: Vec<(WindowId, i32, i32, u32, u32, String)>,
        ) {
            self.expose_windows = if active {
                windows.iter().map(|&(win, ..)| win).collect()
            } else {
                Vec::new()
            };
            if !active {
                self.expose_selected = None;
            }
        }

        fn compositor_expose_click(&mut self, _x: f32, _y: f32) -> Option<WindowId> {
            self.expose_click_hit
        }

        fn compositor_expose_selected(&mut self) -> Option<WindowId> {
            self.expose_selected
                .filter(|win| self.expose_windows.contains(win))
        }

        fn compositor_expose_select(&mut self, win: Option<WindowId>) {
            self.expose_selected = win;
        }
    }
    impl CompositorWindowEffects for ConfigureReplyBackend {}
    impl CompositorAnnotation for ConfigureReplyBackend {}
    impl DisplayControl for ConfigureReplyBackend {}
    impl RenderScheduler for ConfigureReplyBackend {}

    impl Backend for ConfigureReplyBackend {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn root_window(&self) -> Option<WindowId> {
            Some(WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn crate::backend::api::InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn crate::backend::api::PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn crate::backend::api::OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn crate::backend::api::KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn crate::backend::api::KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn crate::backend::api::CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
            &mut self.color_allocator
        }

        fn run(&mut self, _handler: &mut dyn EventHandler) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn add_floating_configure_client(
        jwm: &mut Jwm,
        window: WindowId,
        monitor: crate::core::models::MonitorKey,
        border_w: i32,
        no_decorations: bool,
    ) -> crate::core::models::ClientKey {
        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.state.no_decorations = no_decorations;
        client.geometry.x = jwm.state.monitors[monitor].geometry.m_x + 80;
        client.geometry.y = jwm.state.monitors[monitor].geometry.m_y + 120;
        client.geometry.w = 640;
        client.geometry.h = 480;
        client.geometry.border_w = border_w;
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        client_key
    }

    fn assert_fullscreen_configure_request_is_rejected(hidden: bool) {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let fullscreen = Rect::new(0, 0, 1920, 1080);
        let parked = Rect::new(-4096, fullscreen.y, fullscreen.w, fullscreen.h);
        let current = if hidden { parked } else { fullscreen };
        let pre_fullscreen = Rect::new(240, 130, 960, 700);
        let window = WindowId::from_raw(if hidden { 0x7f02 } else { 0x7f01 });

        let mut client = WMClient::new(window);
        client.mon = Some(monitor);
        client.state.tags = jwm.state.monitors[monitor].get_active_tags();
        client.state.is_floating = true;
        client.state.is_fullscreen = true;
        client.state.is_hidden = hidden;
        client.geometry.x = current.x;
        client.geometry.y = current.y;
        client.geometry.w = current.w;
        client.geometry.h = current.h;
        client.geometry.old_x = pre_fullscreen.x;
        client.geometry.old_y = pre_fullscreen.y;
        client.geometry.old_w = pre_fullscreen.w;
        client.geometry.old_h = pre_fullscreen.h;
        client.geometry.border_w = 0;
        client.geometry.old_border_w = 7;
        if hidden {
            client.geometry.hidden_x = Some(parked.x);
            client.geometry.hidden_restore_rect = Some(fullscreen);
        }
        let client_key = jwm.insert_client(client);
        jwm.attach_to_monitor(client_key, monitor);
        let before = jwm.state.clients[client_key].geometry.clone();

        let mask = (ConfigWindowBits::X
            | ConfigWindowBits::Y
            | ConfigWindowBits::WIDTH
            | ConfigWindowBits::HEIGHT
            | ConfigWindowBits::BORDER_WIDTH)
            .bits();
        jwm.handle_regular_configure_request_params(
            &mut backend,
            client_key,
            mask,
            WindowChanges {
                x: Some(333),
                y: Some(222),
                width: Some(640),
                height: Some(480),
                border_width: Some(19),
                ..Default::default()
            },
        )
        .unwrap();

        let after = &jwm.state.clients[client_key];
        assert_eq!(after.geometry, before);
        assert!(after.state.is_fullscreen);
        assert_eq!(
            backend
                .window_ops
                .replies
                .lock()
                .expect("configure reply lock")
                .as_slice(),
            &[ConfigureReply {
                window,
                x: current.x,
                y: current.y,
                width: current.w as u32,
                height: current.h as u32,
                border: 0,
            }]
        );
    }

    #[test]
    fn fullscreen_rejects_complete_client_configure_without_losing_return_slot() {
        assert_fullscreen_configure_request_is_rejected(false);
    }

    #[test]
    fn hidden_fullscreen_rejects_configure_without_losing_parked_geometry() {
        assert_fullscreen_configure_request_is_rejected(true);
    }

    #[test]
    fn floating_configure_coordinates_stay_root_relative_on_a_secondary_monitor() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        {
            let geometry = &mut jwm.state.monitors[monitor].geometry;
            geometry.m_x = 1920;
            geometry.m_y = 0;
            geometry.m_w = 1920;
            geometry.m_h = 1080;
            geometry.w_x = 1920;
            geometry.w_y = 0;
            geometry.w_w = 1920;
            geometry.w_h = 1080;
        }
        let window = WindowId::from_raw(0x7f03);
        let client_key = add_floating_configure_client(&mut jwm, window, monitor, 3, false);

        jwm.handle_regular_configure_request_params(
            &mut backend,
            client_key,
            (ConfigWindowBits::X | ConfigWindowBits::Y).bits(),
            WindowChanges {
                x: Some(2240),
                y: Some(180),
                ..Default::default()
            },
        )
        .unwrap();

        let client = &jwm.state.clients[client_key];
        assert_eq!((client.geometry.x, client.geometry.y), (2240, 180));
        let applied = backend
            .window_ops
            .applied
            .lock()
            .expect("applied changes lock");
        let (_, changes) = applied.last().expect("floating configure commit");
        assert_eq!((changes.x, changes.y), (Some(2240), Some(180)));
    }

    #[test]
    fn floating_border_request_commits_natively_but_csd_keeps_border_ownership() {
        for (no_decorations, initial, expected) in [(false, 3, 11), (true, 0, 0)] {
            let mut backend = ConfigureReplyBackend::new();
            let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
            let monitor = jwm.state.monitor_order[0];
            let window = WindowId::from_raw(if no_decorations { 0x7f05 } else { 0x7f04 });
            let client_key =
                add_floating_configure_client(&mut jwm, window, monitor, initial, no_decorations);

            jwm.handle_regular_configure_request_params(
                &mut backend,
                client_key,
                ConfigWindowBits::BORDER_WIDTH.bits(),
                WindowChanges {
                    border_width: Some(11),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(jwm.state.clients[client_key].geometry.border_w, expected);
            let applied = backend
                .window_ops
                .applied
                .lock()
                .expect("applied changes lock");
            let (_, changes) = applied.last().expect("floating configure commit");
            assert_eq!(changes.border_width, Some(expected as u32));
        }
    }

    #[test]
    fn hostile_configure_values_are_bounded_and_clamped_without_overflow() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let work_area = jwm.monitor_work_area(monitor).unwrap();
        let window = WindowId::from_raw(0x7f06);
        let client_key = add_floating_configure_client(&mut jwm, window, monitor, 3, false);

        jwm.handle_regular_configure_request_params(
            &mut backend,
            client_key,
            (ConfigWindowBits::X
                | ConfigWindowBits::Y
                | ConfigWindowBits::WIDTH
                | ConfigWindowBits::HEIGHT
                | ConfigWindowBits::BORDER_WIDTH)
                .bits(),
            WindowChanges {
                x: Some(i32::MAX),
                y: Some(i32::MIN),
                width: Some(u32::MAX),
                height: Some(u32::MAX),
                border_width: Some(u32::MAX),
                ..Default::default()
            },
        )
        .unwrap();

        let client = &jwm.state.clients[client_key];
        assert_eq!(
            (client.geometry.x, client.geometry.y),
            (work_area.x, work_area.y)
        );
        assert_eq!(
            (
                client.geometry.w,
                client.geometry.h,
                client.geometry.border_w
            ),
            (u16::MAX as i32, u16::MAX as i32, u16::MAX as i32)
        );
        let applied = backend
            .window_ops
            .applied
            .lock()
            .expect("applied changes lock");
        let (_, changes) = applied.last().expect("bounded configure commit");
        assert_eq!(changes.width, Some(u16::MAX as u32));
        assert_eq!(changes.height, Some(u16::MAX as u32));
        assert_eq!(changes.border_width, Some(u16::MAX as u32));
    }

    #[test]
    fn launcher_composite_command_preserves_quoted_arguments() {
        assert_eq!(
            parse_direct_launcher_command("flameshot gui --path '/tmp/capture area'", |program| {
                program == "flameshot"
            },),
            Some(vec![
                "flameshot".into(),
                "gui".into(),
                "--path".into(),
                "/tmp/capture area".into(),
            ])
        );
    }

    #[test]
    fn launcher_unknown_command_remains_a_search() {
        assert_eq!(
            parse_direct_launcher_command("not-a-program gui", |_| false),
            None
        );
    }

    #[test]
    fn explicit_command_prefix_allows_one_program() {
        assert_eq!(
            parse_direct_launcher_command("> flameshot", |program| program == "flameshot"),
            Some(vec!["flameshot".into()])
        );
        assert_eq!(
            parse_direct_launcher_command("flameshot", |program| program == "flameshot"),
            None
        );
    }

    #[test]
    fn launcher_does_not_interpret_shell_operators() {
        assert_eq!(
            crate::command_line::split_command_line("flameshot gui | sh -c 'echo unsafe'"),
            Ok(vec![
                "flameshot".into(),
                "gui".into(),
                "|".into(),
                "sh".into(),
                "-c".into(),
                "echo unsafe".into(),
            ])
        );
    }

    #[test]
    fn every_button_but_the_wheel_is_stopped_by_a_toast_card() {
        use super::{ToastPress, toast_press};
        use crate::backend::common_define::MouseButton;

        assert_eq!(toast_press(MouseButton::Left), ToastPress::Activate);
        // Middle and right dismiss without falling through to the tab-strip
        // cell (or window) under the card — and without invoking a chip.
        assert_eq!(toast_press(MouseButton::Middle), ToastPress::Dismiss);
        assert_eq!(toast_press(MouseButton::Right), ToastPress::Dismiss);
        // So do the side buttons: any real click on a card is the card's.
        assert_eq!(toast_press(MouseButton::from_u8(8)), ToastPress::Dismiss);
        assert_eq!(toast_press(MouseButton::from_u8(9)), ToastPress::Dismiss);
        // The wheel is not a click and never dismisses a card.
        for wheel in 4..=7 {
            assert_eq!(
                toast_press(MouseButton::from_u8(wheel)),
                ToastPress::Ignored
            );
        }
    }

    #[test]
    fn the_switcher_binding_steps_the_open_panel_whatever_its_key() {
        use super::switcher_binding_step;
        use crate::backend::common_define::{Mods, keys};
        use crate::jwm::types::{WMArgEnum, WMFuncType, WMKey};

        let bindings = vec![
            WMKey::new(
                Mods::SUPER,
                keys::KEY_j,
                Some(Jwm::window_switcher as WMFuncType),
                WMArgEnum::Int(1),
            ),
            WMKey::new(
                Mods::SUPER | Mods::SHIFT,
                keys::KEY_j,
                Some(Jwm::window_switcher as WMFuncType),
                WMArgEnum::Int(-1),
            ),
            // The same chord bound to something else is not a step.
            WMKey::new(
                Mods::ALT,
                keys::KEY_j,
                Some(Jwm::view as WMFuncType),
                WMArgEnum::UInt(1),
            ),
        ];
        assert_eq!(
            switcher_binding_step(&bindings, keys::KEY_j, Mods::SUPER),
            Some(WMArgEnum::Int(1))
        );
        assert_eq!(
            switcher_binding_step(&bindings, keys::KEY_j, Mods::SUPER | Mods::SHIFT),
            Some(WMArgEnum::Int(-1))
        );
        // Lock modifiers are not part of the chord.
        assert_eq!(
            switcher_binding_step(&bindings, keys::KEY_j, Mods::SUPER | Mods::CAPS),
            Some(WMArgEnum::Int(1))
        );
        assert_eq!(
            switcher_binding_step(&bindings, keys::KEY_j, Mods::ALT),
            None
        );
        assert_eq!(
            switcher_binding_step(&bindings, keys::KEY_k, Mods::SUPER),
            None
        );
        assert_eq!(switcher_binding_step(&[], keys::KEY_j, Mods::SUPER), None);
    }

    /// A switcher panel over `windows`, highlighting `selected`.
    fn open_switcher(jwm: &mut Jwm, windows: &[u64], selected: usize) {
        use crate::jwm::features::system_ui::{ListRow, RowData, SystemUiState};
        let rows = windows
            .iter()
            .map(|&window| ListRow {
                key: window.to_string(),
                text: format!("window {window:#x}"),
                data: RowData::WindowSwitcher { window },
            })
            .collect();
        jwm.features.system_ui = SystemUiState::window_switcher(rows, selected);
    }

    #[test]
    fn delete_with_the_switcher_up_closes_the_highlighted_window_mid_gesture() {
        use crate::backend::common_define::{Mods, keys};

        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let closing = WindowId::from_raw(0x7f10);
        let survivor = WindowId::from_raw(0x7f11);
        add_floating_configure_client(&mut jwm, closing, monitor, 0, false);
        add_floating_configure_client(&mut jwm, survivor, monitor, 0, false);
        open_switcher(&mut jwm, &[closing.raw(), survivor.raw()], 0);

        jwm.handle_window_switcher_key(&mut backend, keys::KEY_Delete, Mods::empty())
            .unwrap();

        // The close is the one killclient sends: window_ops().close_window on
        // the row's live window.
        assert_eq!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .as_slice(),
            &[closing]
        );
        // The highlight keeps its index over the shortened list — the
        // next-oldest window slides under it — and the gesture, grabs and
        // all, is still up.
        assert!(jwm.features.system_ui.is_window_switcher());
        assert_eq!(
            jwm.features.system_ui.selected_switcher_window(),
            Some(survivor.raw())
        );
    }

    #[test]
    fn backspace_drops_a_dead_row_without_a_close_request() {
        use crate::backend::common_define::{Mods, keys};

        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let survivor = WindowId::from_raw(0x7f12);
        add_floating_configure_client(&mut jwm, survivor, monitor, 0, false);
        // The tail row names no live client: the snapshot can outlive a
        // window, and the gesture must survive that.
        open_switcher(&mut jwm, &[survivor.raw(), 0x7f13], 1);

        jwm.handle_window_switcher_key(&mut backend, keys::KEY_BackSpace, Mods::empty())
            .unwrap();

        assert!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .is_empty(),
            "no live client, no close request"
        );
        // The tail's highlight clamped onto the new tail, and the gesture
        // goes on.
        assert!(jwm.features.system_ui.is_window_switcher());
        assert_eq!(
            jwm.features.system_ui.selected_switcher_window(),
            Some(survivor.raw())
        );
    }

    #[test]
    fn closing_the_last_switcher_row_ends_the_gesture() {
        use crate::backend::common_define::{Mods, keys};

        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x7f14);
        add_floating_configure_client(&mut jwm, window, monitor, 0, false);
        open_switcher(&mut jwm, &[window.raw()], 0);

        jwm.handle_window_switcher_key(&mut backend, keys::KEY_Delete, Mods::empty())
            .unwrap();

        assert_eq!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .as_slice(),
            &[window]
        );
        // The opener refuses an empty list, so the panel never shows one
        // either: the gesture is over, and a later modifier release finds no
        // switcher to commit.
        assert!(!jwm.features.system_ui.is_window_switcher());
        assert!(!jwm.features.system_ui.is_active());
    }

    /// An expose grid over `windows` (ids in entry order), highlighting
    /// `selected`, published to the recording backend.
    fn open_expose(
        jwm: &mut Jwm,
        backend: &mut ConfigureReplyBackend,
        windows: &[WindowId],
        selected: WindowId,
    ) {
        jwm.features.expose_active = true;
        backend.compositor_set_expose_mode(
            true,
            windows
                .iter()
                .map(|&win| (win, 0, 0, 640, 480, format!("{win:?}")))
                .collect(),
        );
        backend.compositor_expose_select(Some(selected));
    }

    #[test]
    fn delete_with_expose_up_closes_the_highlighted_cell_and_rebuilds_the_grid() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let closing = WindowId::from_raw(0x7f20);
        let survivor = WindowId::from_raw(0x7f21);
        add_floating_configure_client(&mut jwm, closing, monitor, 0, false);
        add_floating_configure_client(&mut jwm, survivor, monitor, 0, false);
        open_expose(&mut jwm, &mut backend, &[closing, survivor], closing);

        jwm.close_expose_highlighted(&mut backend).unwrap();

        // The close is the one killclient sends: window_ops().close_window
        // on the highlighted cell's live window.
        assert_eq!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .as_slice(),
            &[closing]
        );
        // The grid rebuilt in place from the survivors in their old order,
        // the next entry slid under the highlight, and the gesture, grabs
        // and all, is still up.
        assert!(jwm.features.expose_active);
        assert_eq!(backend.expose_windows, &[survivor]);
        assert_eq!(backend.expose_selected, Some(survivor));
    }

    #[test]
    fn closing_the_expose_tail_clamps_the_highlight_onto_the_new_tail() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let survivor = WindowId::from_raw(0x7f22);
        let closing = WindowId::from_raw(0x7f23);
        add_floating_configure_client(&mut jwm, survivor, monitor, 0, false);
        add_floating_configure_client(&mut jwm, closing, monitor, 0, false);
        open_expose(&mut jwm, &mut backend, &[survivor, closing], closing);

        jwm.close_expose_highlighted(&mut backend).unwrap();

        assert_eq!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .as_slice(),
            &[closing]
        );
        assert!(jwm.features.expose_active);
        assert_eq!(backend.expose_windows, &[survivor]);
        assert_eq!(backend.expose_selected, Some(survivor));
    }

    #[test]
    fn closing_the_last_expose_cell_ends_the_gesture() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x7f24);
        add_floating_configure_client(&mut jwm, window, monitor, 0, false);
        open_expose(&mut jwm, &mut backend, &[window], window);

        jwm.close_expose_highlighted(&mut backend).unwrap();

        assert_eq!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .as_slice(),
            &[window]
        );
        // The enter path refuses an empty grid, so the overlay never sits
        // open over one either: the gesture is over, and with
        // `expose_active` cleared no later key can commit the window just
        // closed (expose has no modifier-release commit to guard — its
        // commit is Return, and the branch is gated on the flag).
        assert!(!jwm.features.expose_active);
        assert!(backend.expose_windows.is_empty());
        assert_eq!(backend.expose_selected, None);
    }

    #[test]
    fn a_highlight_that_names_no_live_window_closes_nothing_in_expose() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let live = WindowId::from_raw(0x7f25);
        add_floating_configure_client(&mut jwm, live, monitor, 0, false);
        // The compositor's grid can still show a cell for a window that died
        // mid-expose; a Delete on it has nothing honest to close or re-clamp,
        // so the gesture and the grid stay exactly as they were.
        let dead = WindowId::from_raw(0x7f26);
        open_expose(&mut jwm, &mut backend, &[live, dead], dead);

        jwm.close_expose_highlighted(&mut backend).unwrap();

        assert!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .is_empty(),
            "no live client, no close request"
        );
        assert!(jwm.features.expose_active);
        assert_eq!(backend.expose_windows, &[live, dead]);
    }

    #[test]
    fn middle_click_closes_the_clicked_expose_cell_and_rebuilds_the_grid() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let before = WindowId::from_raw(0x7f30);
        let clicked = WindowId::from_raw(0x7f31);
        let highlighted = WindowId::from_raw(0x7f32);
        add_floating_configure_client(&mut jwm, before, monitor, 0, false);
        add_floating_configure_client(&mut jwm, clicked, monitor, 0, false);
        add_floating_configure_client(&mut jwm, highlighted, monitor, 0, false);
        // The highlight sits on a different cell than the click: browser-tab
        // semantics close the clicked one, not the highlighted one.
        open_expose(
            &mut jwm,
            &mut backend,
            &[before, clicked, highlighted],
            highlighted,
        );
        backend.expose_click_hit = Some(clicked);

        jwm.on_button_press_internal(
            &mut backend,
            HitTarget::Background { output: None },
            0,
            MouseButton::Middle.to_u8(),
            0,
        )
        .unwrap();

        // The close is the one killclient sends, on the clicked cell's
        // window — the highlighted one stays open.
        assert_eq!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .as_slice(),
            &[clicked]
        );
        // The grid rebuilt in place from the survivors in their old order
        // and the gesture, grabs and all, is still up; the highlight landed
        // on the survivor that slid into the clicked slot (index 1).
        assert!(jwm.features.expose_active);
        assert_eq!(backend.expose_windows, &[before, highlighted]);
        assert_eq!(backend.expose_selected, Some(highlighted));
    }

    #[test]
    fn middle_click_on_expose_empty_space_closes_nothing_and_stays_up() {
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = jwm.state.monitor_order[0];
        let window = WindowId::from_raw(0x7f33);
        add_floating_configure_client(&mut jwm, window, monitor, 0, false);
        open_expose(&mut jwm, &mut backend, &[window], window);
        // A middle click never commits the gesture, so missing every cell is
        // a plain no-op — unlike the other buttons, whose miss exits.
        backend.expose_click_hit = None;

        jwm.on_button_press_internal(
            &mut backend,
            HitTarget::Background { output: None },
            0,
            MouseButton::Middle.to_u8(),
            0,
        )
        .unwrap();

        assert!(
            backend
                .window_ops
                .closed
                .lock()
                .expect("closed windows lock")
                .is_empty(),
            "a middle click on empty space has nothing to close"
        );
        assert!(jwm.features.expose_active);
        assert_eq!(backend.expose_windows, &[window]);
        assert_eq!(backend.expose_selected, Some(window));
    }

    /// The expose pointer branch discriminates exactly one button: middle
    /// routes to the close helper (the clicked cell's index drives the same
    /// close execution the keyboard path uses); every other button — the
    /// left commit, right, scroll — falls through to the `plan_click` it
    /// always took. Round 16 deliberately shipped this branch
    /// button-agnostic; this round intentionally changes THAT and nothing
    /// else about it. The haystack is the shipped source, and the needles
    /// are built at runtime so this test cannot match its own.
    #[test]
    fn only_the_middle_button_routes_to_the_expose_close() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let compact: String = SOURCE.chars().filter(|c| !c.is_whitespace()).collect();

        let press = compact
            .split_once(concat!("fnon_button_press", "_internal("))
            .expect("the button-press handler")
            .1;
        let branch = press
            .split_once(concat!("ifself.features.expose", "_active{"))
            .expect("the expose pointer branch")
            .1
            .split_once(concat!("letpress=toast", "_press("))
            .expect("the end of the expose pointer branch")
            .0;
        // The one and only button check: middle goes to the close helper…
        let routing = concat!(
            "MouseButton::from_u8(detail",
            "_btn)==MouseButton::Middle{returnself.close_expose",
            "_clicked(backend,hit);}"
        );
        assert!(
            branch.contains(routing),
            "the expose pointer branch must route the middle button to the close helper"
        );
        // …and no other button is named in the branch: left, right and
        // scroll keep the plan_click fall-through they always had.
        for button in [
            "MouseButton::Left",
            "MouseButton::Right",
            "MouseButton::Other",
        ] {
            assert!(
                !branch.contains(button),
                "the expose pointer branch grew a {button} special case"
            );
        }
        assert!(
            branch.contains(concat!(
                "apply_expose_action(backend,expose_plan::plan",
                "_click(hit))"
            )),
            "the non-middle buttons no longer fall through to plan_click"
        );

        // The click-close helper resolves the clicked window to its grid
        // index, a miss is a no-op, and the close itself rides the shared
        // execution with the keyboard path.
        let helper = compact
            .split_once(concat!("fnclose_expose", "_clicked("))
            .expect("the expose click-close helper")
            .1
            .split_once(concat!("fnapply_expose", "_close("))
            .expect("the end of the expose click-close helper")
            .0;
        assert!(helper.contains(concat!("letSome(hit)=hitelse{returnOk(());}")));
        assert!(helper.contains(concat!("expose_plan::grid", "_index(&candidates,hit)")));
        assert!(helper.contains(concat!("expose_plan::plan", "_close_at(candidates,index)")));
        assert!(helper.contains(concat!("self.apply_expose", "_close(backend,action)")));
    }

    /// The expose Delete/BackSpace branch must close through the same call
    /// killclient uses — `window_ops().close_window`, graceful with its
    /// forced fallback — never a window-killing call of its own; the grid
    /// rebuild must ride the same entry-set rebuild that entering expose
    /// uses (the compositors rebuild the thumbnails' label textures with
    /// it); and closing down to an empty grid must end the gesture through
    /// the shared exit sequence. The haystack is the shipped source, and the
    /// needles are built at runtime so this test cannot match its own.
    #[test]
    fn expose_delete_routes_through_the_same_close_path_as_killclient() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let compact: String = SOURCE.chars().filter(|c| !c.is_whitespace()).collect();

        let branch = compact
            .split_once(concat!("ifself.features.expose", "_active{"))
            .expect("the expose key branch")
            .1
            .split_once(concat!("ifself.features.annotation", "_active{"))
            .expect("the end of the expose key branch")
            .0;
        let routing = concat!(
            "keysym==keys::KEY_Delete||keysym==keys::KEY_BackSpace{returnself.close_expose",
            "_highlighted(backend);}"
        );
        assert!(
            branch.contains(routing),
            "the expose key branch must route Delete/BackSpace to the close helper"
        );

        let helper = compact
            .split_once(concat!("fnclose_expose", "_highlighted("))
            .expect("the expose close helper")
            .1
            .split_once("fnhandle_notification_center_key")
            .expect("the end of the expose close helper")
            .0;
        // The highlighted cell — not the focused window — decides what closes.
        assert!(helper.contains(concat!("compositor_expose", "_selected()")));
        // The close is the one killclient sends…
        assert!(helper.contains(concat!("window_ops().close", "_window(client.win)")));
        // …and no new window-killing call appeared for it.
        assert!(
            !helper.contains("kill_client("),
            "expose grew its own window-killing call"
        );
        // The grid rebuilds in place through the same entry-set rebuild that
        // entered expose, and the highlight slides onto the planned survivor.
        assert!(helper.contains(concat!("compositor_set_expose_mode(true,", "survivors)")));
        assert!(helper.contains(concat!("compositor_expose_select(select", "_id)")));
        // Close-to-empty ends the gesture through the shared exit sequence.
        assert!(helper.contains(concat!(
            "apply_expose_action(backend,expose_plan::plan",
            "_escape())"
        )));
    }

    /// The slider paths used to shell out to the session's tools — plus a
    /// read-back — for every motion event of a drag. They queue the level on
    /// the controls worker now, and the worker folds the storm. The haystack
    /// is the control-center input region alone, and the needles are built
    /// at runtime so this test cannot match its own source.
    #[test]
    fn control_center_input_queues_instead_of_shelling_out() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let region = SOURCE
            .split_once("fn handle_control_center_key")
            .expect("handle_control_center_key")
            .1
            .split_once("fn dismiss_system_ui_from_pointer")
            .expect("the end of the control-center input region")
            .0;
        for primitive in [
            "volume_adjust",
            "volume_set",
            "volume_toggle_mute",
            "volume_state",
            "brightness_adjust",
            "brightness_set",
            "brightness_percent",
        ] {
            let needle = format!("system_controls::{primitive}(");
            assert!(
                !region.contains(&needle),
                "control-center input regained a blocking tool call: {needle}"
            );
        }
        for helper in ["queue_volume_request", "queue_brightness_request"] {
            let needle = format!("self.{helper}(");
            assert!(
                region.contains(&needle),
                "a slider path no longer queues on the controls worker ({needle})"
            );
        }
    }

    /// The radio rows used to run `nmcli`/`bluetoothctl` — up to the 10 s
    /// query timeout behind a wedged bus — on the event thread, freezing the
    /// whole session. They queue a `start_radio_set` worker now, like every
    /// other connectivity action. The haystack is the shipped source alone
    /// (the test modules are cut away), and the needles are built at runtime
    /// so this test cannot match its own source.
    #[test]
    fn radio_rows_never_set_the_radio_on_the_event_thread() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .expect("the first test module")
            .0;
        for primitive in ["set_wifi", "set_bluetooth"] {
            let needle = format!("{primitive}(");
            assert!(
                !shipped.contains(&needle),
                "input handling regained a synchronous radio set: {needle}"
            );
        }
        let region = SOURCE
            .split_once("fn handle_control_center_key")
            .expect("handle_control_center_key")
            .1
            .split_once("fn dismiss_system_ui_from_pointer")
            .expect("the end of the control-center input region")
            .0;
        let helper = format!("self.{}(", "request_radio_set");
        assert!(
            region.contains(&helper),
            "the radio rows no longer queue the flip on the connectivity worker ({helper})"
        );
    }

    /// Every key-bound toggle confirms the flip on screen. The OSD is the
    /// right surface — never a toast: toasts are DND-gated, so a "Do Not
    /// Disturb On" toast would be swallowed by the very state it announces.
    #[test]
    fn toggle_confirmations_raise_the_osd_and_never_a_toast() {
        use crate::backend::api::OsdKind;
        use crate::jwm::types::WMArgEnum;

        let mut backend = ConfigureReplyBackend::new();
        let osd_log = std::sync::Arc::clone(&backend.osd_log);
        let toast_log = std::sync::Arc::clone(&backend.toast_log);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();

        jwm.toggle_dnd(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(jwm.do_not_disturb);
        jwm.toggle_dnd(&mut backend, &WMArgEnum::Int(0)).unwrap();
        assert!(!jwm.do_not_disturb);
        jwm.toggle_night_light(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        let night_light = jwm.night_light_active();
        jwm.toggle_idle_inhibit(&mut backend, &WMArgEnum::Int(0))
            .unwrap();
        assert!(jwm.idle_inhibited);

        assert_eq!(
            osd_log.lock().expect("osd log lock").as_slice(),
            &[
                (OsdKind::DoNotDisturb(true), 0),
                (OsdKind::DoNotDisturb(false), 0),
                (OsdKind::NightLight(night_light), 0),
                (OsdKind::Caffeine(true), 0),
            ]
        );
        assert!(
            toast_log.lock().expect("toast log lock").is_empty(),
            "a toggle confirmation must not be a DND-gated toast"
        );
    }

    /// The pickers' `d` removes a bond or a saved profile — seconds behind a
    /// possibly wedged bus — so like every other connectivity action it goes
    /// through the workers, never a child process on the event thread. The
    /// haystack is the shipped source alone (the test modules are cut away)
    /// and the needles are built at runtime, so this test cannot match its
    /// own source.
    #[test]
    fn picker_forget_keys_submit_workers_and_never_spawn() {
        const SOURCE: &str = include_str!("input_handler.rs");
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .expect("the first test module")
            .0;
        for needle in [
            format!("{}{}", "Command", "::new"),
            format!("{}{}", "std::process", "::Command"),
            format!("{}{}", "connectivity", "_output("),
        ] {
            assert!(
                !shipped.contains(&needle),
                "input handling regained an event-thread spawn: {needle}"
            );
        }
        // Each handler's `d` branch routes to its forget helper...
        let wifi = SOURCE
            .split_once("fn handle_wifi_picker_key")
            .expect("handle_wifi_picker_key")
            .1
            .split_once("fn forget_selected_wifi")
            .expect("handle_wifi_picker_key is no longer followed by forget_selected_wifi")
            .0;
        let key = format!("keys::KEY_{}", "d");
        assert!(
            wifi.contains(&key),
            "the Wi-Fi picker no longer routes {key} to the forget"
        );
        // ...and each helper starts its worker: the profile delete for
        // Wi-Fi, the device-action slot carrying `remove` for Bluetooth.
        let wifi_forget = SOURCE
            .split_once("fn forget_selected_wifi")
            .expect("forget_selected_wifi")
            .1
            .split_once("fn handle_bluetooth_picker_key")
            .expect("forget_selected_wifi is no longer followed by handle_bluetooth_picker_key")
            .0;
        for helper in ["start_forget_profile", "track_wifi_forget"] {
            let needle = format!("connectivity::{}(", helper);
            assert!(
                wifi_forget.contains(&needle),
                "the Wi-Fi forget no longer queues the worker ({needle})"
            );
        }
        let bluetooth_forget = SOURCE
            .split_once("fn forget_selected_bluetooth")
            .expect("forget_selected_bluetooth")
            .1
            .split_once("fn activate_launcher_selection")
            .expect(
                "forget_selected_bluetooth is no longer followed by activate_launcher_selection",
            )
            .0;
        let action = format!("connectivity::{}(", "start_device_action");
        assert!(
            bluetooth_forget.contains(&action),
            "the Bluetooth forget no longer rides the device-action worker ({action})"
        );
        let remove = format!("{:?}", "remove");
        assert!(
            bluetooth_forget.contains(&remove),
            "the Bluetooth forget no longer asks bluetoothctl for {remove}"
        );
    }

    /// A finished profile delete is adopted by the connectivity poll: the
    /// picker's status line says so, success and failure alike. The fake
    /// jobs' closures are pure — no nmcli — so only the adoption path is
    /// under test; the delete itself is pinned by source scan above.
    ///
    /// The slot is process-wide (see `WIFI_FORGET` in connectivity.rs) and
    /// every test handler's tick polls it, so a concurrent test can take a
    /// job this test parked — the thief's adoption is inert (no Wi-Fi panel
    /// open, nothing marked dirty). Rather than serialize half the suite
    /// against the slot, this test parks afresh and polls again until one
    /// adoption lands on *this* handler.
    #[test]
    fn a_finished_wifi_forget_lands_on_the_picker_status_line() {
        use crate::jwm::features::connectivity;

        fn adopt_until(
            jwm: &mut Jwm,
            park: impl Fn() -> connectivity::BackgroundJob<Result<String, String>>,
            wanted: &str,
        ) -> bool {
            for _ in 0..400 {
                // An empty slot means the last park was adopted — by this
                // handler, or stolen by a concurrent test's tick.
                if !connectivity::wifi_forget_in_flight() {
                    connectivity::track_wifi_forget(park());
                }
                jwm.poll_connectivity_job();
                let parts = jwm.features.system_ui.overlay_parts();
                if parts.items.iter().any(|row| row.contains(wanted)) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        }

        let _serial = connectivity::FORGET_SLOT_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut backend = ConfigureReplyBackend::new();
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.features.system_ui = crate::jwm::features::SystemUiState::wifi_picker("");

        assert!(
            adopt_until(
                &mut jwm,
                || connectivity::BackgroundJob::spawn(|| Ok::<String, String>("TestNet".into())),
                "Forgot TestNet"
            ),
            "the finished forget never reached the picker's status line"
        );
        // The honest error surface is the same line: a delete that found no
        // profile says so rather than vanishing.
        assert!(
            adopt_until(
                &mut jwm,
                || connectivity::BackgroundJob::spawn(|| Err::<String, String>(
                    "no saved profile for Ghost".into()
                )),
                "no saved profile for Ghost"
            ),
            "the failed forget never reached the status line"
        );
    }
}
