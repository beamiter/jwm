// Input event handling: keyboard, mouse, and configure request processing

use crate::Jwm;
use crate::backend::api::{
    AllowMode, Backend, HitTarget, LayoutFilmCell, LayoutFilmstrip, SystemUiOverlay, WindowChanges,
    WindowType,
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
use crate::jwm::features::{CaptureTarget, MonitorDirection};
use crate::jwm::types::{WMArgEnum, WMClickType};
use log::{error, info};

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

/// Split one launcher command line into argv without involving a shell.
///
/// Quotes and backslash escapes are handled so arguments can contain spaces,
/// but operators such as `|`, `>`, `;` and `$()` remain ordinary characters.
fn split_launcher_command_line(input: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut token_started = false;

    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            token_started = true;
            continue;
        }

        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
                token_started = true;
            } else if ch == '\\' && active_quote == '"' {
                escaped = true;
            } else {
                current.push(ch);
                token_started = true;
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                token_started = true;
            }
            '\\' => {
                escaped = true;
                token_started = true;
            }
            ch if ch.is_whitespace() => {
                if token_started {
                    args.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            _ => {
                current.push(ch);
                token_started = true;
            }
        }
    }

    if escaped || quote.is_some() {
        return None;
    }
    if token_started {
        args.push(current);
    }
    Some(args)
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

    let command = split_launcher_command_line(command_line)?;
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
        if self.system_ui_dirty {
            self.sync_system_ui(backend);
        }
    }

    pub(crate) fn sync_system_ui(&mut self, backend: &mut dyn Backend) {
        self.system_ui_dirty = false;
        let active = self.features.system_ui.is_active();
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
                parts.hint = "Enter  run command    Esc  close".into();
            }
            SystemUiOverlay {
                title: parts.title,
                query: parts.query,
                items: parts.items,
                selected: parts.selected,
                hint: parts.hint,
                locked: self.features.system_ui.is_locked(),
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
            }
        }));
        backend.compositor_force_full_redraw();
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
        (!ch.is_control() && ch.is_ascii()).then_some(ch)
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
        use crate::jwm::features::{ControlKind, ShellHubRoute, system_controls};

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
            keys::KEY_Left => Some(-5),
            keys::KEY_Right => Some(5),
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
                    let state = if let Some(delta) = slider_delta {
                        system_controls::volume_adjust(delta)
                    } else if activate || keysym == keys::KEY_m {
                        system_controls::volume_toggle_mute()
                    } else {
                        None
                    };
                    if let Some(state) = state {
                        self.features.system_ui.update_control(
                            ControlKind::Volume,
                            state.percent,
                            state.muted,
                        );
                    }
                }
                ControlKind::Brightness => {
                    if let Some(delta) = slider_delta {
                        if let Some(percent) = system_controls::brightness_adjust(delta) {
                            self.features.system_ui.update_control(
                                ControlKind::Brightness,
                                percent,
                                false,
                            );
                        }
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
                                self.features.wifi_scan = Some(scan);
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
                            // Re-read rather than assume: the radio may be
                            // hard-blocked and refuse to come back on.
                            if connectivity::set_wifi(true) {
                                self.refresh_connectivity();
                            }
                        }
                        NetworkRowAction::SetRadio(enabled) => {
                            if connectivity::set_wifi(enabled) {
                                self.refresh_connectivity();
                            }
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
                                self.features.bluetooth_scan = Some(scan);
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
                            if connectivity::set_bluetooth(true) {
                                self.refresh_connectivity();
                            }
                        }
                        // Powering down can take a Bluetooth keyboard with it,
                        // so `activate_control` withholds it until a second
                        // press confirms.
                        BluetoothRowAction::SetPower(false) => {
                            if self.features.system_ui.activate_control().is_some()
                                && connectivity::set_bluetooth(false)
                            {
                                self.refresh_connectivity();
                            }
                        }
                        BluetoothRowAction::SetPower(true) => {
                            if connectivity::set_bluetooth(true) {
                                self.refresh_connectivity();
                            }
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
                    if let Some(delta) = slider_delta
                        && let Some((available, active)) = crate::jwm::features::power::profiles()
                        && let Some(next) = crate::jwm::features::power::cycle_profile(
                            &available,
                            &active,
                            delta.signum() as isize,
                        )
                        && crate::jwm::features::power::set_profile(&next)
                    {
                        // Rebuild so the row shows what actually took effect.
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
                        self.do_not_disturb = !self.do_not_disturb;
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
    /// rescans, and typing feeds the prompt.
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
                    self.features.wifi_scan = Some(scan);
                    self.features.system_ui.set_wifi_message("Scanning\u{2026}");
                }
                None => self
                    .features
                    .system_ui
                    .set_wifi_message("nmcli is not available"),
            }
        } else if prompting && let Some(ch) = Self::system_ui_char(keysym, mods) {
            self.features.system_ui.push_char(ch);
        }
        self.sync_system_ui(backend);
    }

    /// Key handling while the Bluetooth picker is open: Up/Down select,
    /// Return connects or disconnects, `r` re-reads the list.
    fn handle_bluetooth_picker_key(&mut self, backend: &mut dyn Backend, keysym: u32) {
        if keysym == keys::KEY_Return || keysym == keys::KEY_space {
            self.activate_selected_bluetooth(backend);
            return;
        }
        if keysym == keys::KEY_Up {
            self.features.system_ui.move_selection(-1);
        } else if keysym == keys::KEY_Down || keysym == keys::KEY_Tab {
            self.features.system_ui.move_selection(1);
        } else if keysym == keys::KEY_r {
            match crate::jwm::features::connectivity::start_device_scan() {
                Some(scan) => {
                    self.features.bluetooth_scan = Some(scan);
                    self.features
                        .system_ui
                        .set_bluetooth_message("Reading devices\u{2026}");
                }
                None => self
                    .features
                    .system_ui
                    .set_bluetooth_message("bluetoothctl is not available"),
            }
        }
        self.sync_system_ui(backend);
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

        // Built-in system UI is modal and consumes every key. This branch is
        // shared by X11rb, XCB and Wayland-udev, keeping behavior identical.
        if self.features.system_ui.is_active() {
            let locked = self.features.system_ui.is_locked();
            // Escape backs out of the passphrase prompt before it closes the
            // picker, so a typo does not cost the whole scan.
            if keysym == keys::KEY_Escape && self.features.system_ui.cancel_wifi_passphrase() {
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
                    match std::process::Command::new("xrandr").args(&args).output() {
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
                self.handle_bluetooth_picker_key(backend, keysym);
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
                if keysym == keys::KEY_Return || keysym == keys::KEY_space {
                    self.copy_selected_clipboard(backend);
                } else if keysym == keys::KEY_d || keysym == keys::KEY_Delete {
                    self.forget_selected_clipboard(backend);
                } else if keysym == keys::KEY_c {
                    self.clear_clipboard_history();
                    self.sync_system_ui(backend);
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
                    if let Some(mut password) = self.features.system_ui.take_password() {
                        let authenticated =
                            crate::jwm::features::system_ui::authenticate_current_user(&password);
                        unsafe { password.as_bytes_mut().fill(0) };
                        if authenticated {
                            self.close_system_ui(backend);
                            return Ok(());
                        }
                        self.features.system_ui.authentication_failed();
                    }
                } else if let Some(result) = self
                    .features
                    .system_ui
                    .computed_result()
                    .map(str::to_string)
                {
                    // An answer nobody can paste is half an answer.
                    if backend.set_clipboard_text(&result) {
                        self.record_clipboard(&result);
                        log::info!("Launcher: copied {result}");
                    } else {
                        log::warn!("Launcher: this backend cannot set the clipboard");
                    }
                    self.close_system_ui(backend);
                    return Ok(());
                } else if let Some(window) = self.features.system_ui.selected_window() {
                    // Focusing what already exists rather than starting a
                    // second copy of it.
                    self.close_system_ui(backend);
                    if let Err(error) = self.reveal_and_focus(
                        backend,
                        crate::backend::common_define::WindowId::from_raw(window),
                    ) {
                        log::warn!("Launcher: could not focus window: {error}");
                    }
                    return Ok(());
                } else if let Some(command) = direct_command_from_launcher(&self.features.system_ui)
                {
                    let id = command[0].clone();
                    self.features.system_ui.note_launch(&id);
                    log::info!("Launcher: running {id}");
                    self.close_system_ui(backend);
                    return self.spawn(backend, &WMArgEnum::StringVec(command));
                } else if let Some(choice) = self.features.system_ui.selected_launch() {
                    self.features.system_ui.note_launch(&choice.id);
                    let command = crate::jwm::features::launcher::launch_command(
                        &crate::config::Config::get_termcmd(),
                        &choice.command,
                        choice.terminal,
                    );
                    self.close_system_ui(backend);
                    return self.spawn(backend, &WMArgEnum::StringVec(command));
                }
            } else if let Some(ch) = Self::system_ui_char(keysym, clean_state) {
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

        // Expose mode intercept: route clicks to compositor. A hit focuses the
        // clicked window; hit or miss, expose exits.
        if self.features.expose_active {
            let (rx, ry) = self.last_mouse_root;
            let hit = backend.compositor_expose_click(rx as f32, ry as f32);
            return self.apply_expose_action(backend, expose_plan::plan_click(hit));
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
            if self.click_window_tab(backend, x, y)? {
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
        let is_popup = self.is_popup_like(backend, client_key);
        let mask = ConfigWindowBits::from_bits_truncate(mask_bits);

        let is_dock = self
            .state
            .clients
            .get(client_key)
            .map(|client| client.state.is_dock)
            .unwrap_or(false);

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
                if !is_popup {
                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.geometry.border_w = border as i32;
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
                        client.geometry.x = mx + x;
                    }
                }
                if mask.contains(ConfigWindowBits::Y) {
                    if let Some(y) = req.y {
                        client.geometry.old_y = client.geometry.y;
                        client.geometry.y = my + y;
                    }
                }
                if mask.contains(ConfigWindowBits::WIDTH) {
                    if let Some(w) = req.width {
                        client.geometry.old_w = client.geometry.w;
                        client.geometry.w = w as i32;
                    }
                }
                if mask.contains(ConfigWindowBits::HEIGHT) {
                    if let Some(h) = req.height {
                        client.geometry.old_h = client.geometry.h;
                        client.geometry.h = h as i32;
                    }
                }

                if (client.geometry.x + client.geometry.w) > mx + mw && client.state.is_floating {
                    client.geometry.x = mx + (mw / 2 - client.total_width() / 2);
                }
                if (client.geometry.y + client.geometry.h) > my + mh && client.state.is_floating {
                    client.geometry.y = my + (mh / 2 - client.total_height() / 2);
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

                                let left = clamp.x.max(parent_rect.x);
                                let top = clamp.y.max(parent_rect.y);
                                let right = (clamp.x + clamp.w).min(parent_rect.x + parent_rect.w);
                                let bottom = (clamp.y + clamp.h).min(parent_rect.y + parent_rect.h);
                                let w = (right - left).max(0);
                                let h = (bottom - top).max(0);
                                if w > 0 && h > 0 {
                                    clamp = Rect::new(left, top, w, h);
                                }
                            }
                        }
                    }

                    let min_x = clamp.x;
                    let max_x = clamp.x + clamp.w - total_w;
                    let clamped_x = if min_x <= max_x {
                        x.clamp(min_x, max_x)
                    } else {
                        min_x
                    };

                    let min_y = clamp.y;
                    let max_y = clamp.y + clamp.h - total_h;
                    let clamped_y = if min_y <= max_y {
                        y.clamp(min_y, max_y)
                    } else {
                        min_y
                    };

                    if let Some(client) = self.state.clients.get_mut(client_key) {
                        client.geometry.x = clamped_x;
                        client.geometry.y = clamped_y;
                    }
                }

                if let Some(client) = self.state.clients.get(client_key) {
                    let changes = WindowChanges {
                        x: Some(client.geometry.x),
                        y: Some(client.geometry.y),
                        width: Some(client.geometry.w as u32),
                        height: Some(client.geometry.h as u32),
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

                let min_x = clamp.x;
                let max_x = clamp.x + clamp.w - total_w;
                let clamped_x = if min_x <= max_x {
                    x.clamp(min_x, max_x)
                } else {
                    min_x
                };

                let min_y = clamp.y;
                let max_y = clamp.y + clamp.h - total_h;
                let clamped_y = if min_y <= max_y {
                    y.clamp(min_y, max_y)
                } else {
                    min_y
                };

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
                    let changes = WindowChanges {
                        x: Some(client.geometry.x),
                        y: Some(client.geometry.y),
                        width: Some(client.geometry.w as u32),
                        height: Some(client.geometry.h as u32),
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
    use super::{parse_direct_launcher_command, split_launcher_command_line};

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
            split_launcher_command_line("flameshot gui | sh -c 'echo unsafe'"),
            Some(vec![
                "flameshot".into(),
                "gui".into(),
                "|".into(),
                "sh".into(),
                "-c".into(),
                "echo unsafe".into(),
            ])
        );
    }
}
