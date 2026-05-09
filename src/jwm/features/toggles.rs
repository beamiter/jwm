//! 特性切换功能
//!
//! 这个模块包含所有窗口管理器特性的切换函数（toggle* 系列）

use crate::backend::api::Backend;
use crate::backend::common_define::{EventMaskBits, WindowId};
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use crate::jwm::types::WMArgEnum;
use crate::jwm::Jwm;
use log::{error, info, warn};
use std::process::Command;

impl Jwm {
    /// 切换当前选中窗口的浮动状态
    pub fn togglefloating(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // info!("[togglefloating]");
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };
        let geom = if let Some(client) = self.state.clients.get_mut(sel_client_key) {
            client.state.is_floating = !client.state.is_floating;
            if client.state.is_floating {
                if client.geometry.floating_w <= 0 || client.geometry.floating_h <= 0 {
                    client.geometry.floating_x = client.geometry.x;
                    client.geometry.floating_y = client.geometry.y;
                    client.geometry.floating_w = client.geometry.w;
                    client.geometry.floating_h = client.geometry.h;
                }
                Some((
                    client.geometry.floating_x,
                    client.geometry.floating_y,
                    client.geometry.floating_w,
                    client.geometry.floating_h,
                ))
            } else {
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
                None
            }
        } else {
            return Ok(());
        };

        if let Some((x, y, w, h)) = geom {
            self.resize_client(backend, sel_client_key, x, y, w, h, false);
        }

        self.reorder_client_in_monitor_groups(sel_client_key);

        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }

    /// 切换当前选中窗口的粘性状态（sticky: 显示在所有标签）
    pub fn togglesticky(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };
        if let Some(client) = self.state.clients.get_mut(sel_client_key) {
            client.state.is_sticky = !client.state.is_sticky;
            if client.state.is_sticky {
                // Ensure sticky client has current monitor tags
                if let Some(monitor) = self.state.monitors.get(sel_mon_key) {
                    let current_tags = monitor.get_active_tags();
                    if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                        client.state.tags = current_tags;
                    }
                }
            }
        }
        self.arrange(backend, Some(sel_mon_key));
        Ok(())
    }

    /// 切换合成器开关
    pub fn togglecompositor(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enable = !backend.has_compositor();
        match backend.set_compositor_enabled(enable) {
            Ok(true) => {
                log::info!(
                    "Compositor toggled: now {}",
                    if enable { "ON" } else { "OFF" }
                );
            }
            Ok(false) => {
                log::info!("Compositor state unchanged");
            }
            Err(e) => {
                log::warn!("Failed to toggle compositor: {e}");
            }
        }
        Ok(())
    }

    /// 切换 Overview 模式（3D 窗口切换器）
    pub fn toggle_overview(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.overview.active {
            // End overview: focus selected window and promote it to master
            if let Some(&client_key) = self
                .features
                .overview
                .clients
                .get(self.features.overview.index)
            {
                if let Some(mon_key) = self.state.sel_mon {
                    self.detach(client_key);
                    self.attach_front(client_key);
                    self.focus(backend, Some(client_key))?;
                    self.arrange(backend, Some(mon_key));
                } else {
                    self.focus(backend, Some(client_key))?;
                }
            }
            self.features.overview.active = false;
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
        } else {
            // Start overview: collect visible windows on current monitor
            let sel_mon_key = match self.state.sel_mon {
                Some(k) => k,
                None => return Ok(()),
            };
            let visible: Vec<ClientKey> = {
                let mon_clients = self.state.monitor_clients.get(sel_mon_key);
                match mon_clients {
                    Some(clients) => clients
                        .iter()
                        .copied()
                        .filter(|&ck| self.is_client_visible_by_key(ck))
                        .collect(),
                    None => Vec::new(),
                }
            };

            if visible.is_empty() {
                return Ok(());
            }

            // Tell compositor which monitor to render the prism on.
            if let Some(mon) = self.state.monitors.get(sel_mon_key) {
                backend.compositor_set_overview_monitor(
                    mon.geometry.w_x as i32,
                    mon.geometry.w_y as i32,
                    mon.geometry.w_w as u32,
                    mon.geometry.w_h as u32,
                );
            }

            // Build simple client list; the compositor handles all 3D positioning.
            let layout = self.build_overview_layout(&visible);

            self.features.overview.active = true;
            self.features.overview.index = 0;
            self.features.overview.slide_offset = 0;
            self.features.overview.clients = visible;
            backend.compositor_set_overview_mode(true, &layout);
            if let Some(root) = backend.root_window() {
                let _ = backend.key_ops().grab_keyboard(root);
            }
        }
        Ok(())
    }

    /// 在 Overview 模式中循环切换窗口选择
    pub fn cycle_overview(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.overview.active || self.features.overview.clients.is_empty() {
            return Ok(());
        }

        let direction = match arg {
            WMArgEnum::Int(d) => *d,
            _ => 1,
        };

        let len = self.features.overview.clients.len();
        if direction > 0 {
            self.features.overview.index = (self.features.overview.index + 1) % len;
        } else {
            self.features.overview.index = (self.features.overview.index + len - 1) % len;
        }

        if len <= 6 {
            // All clients fit on the prism; just rotate to selection.
            if let Some(&ck) = self
                .features
                .overview
                .clients
                .get(self.features.overview.index)
            {
                if let Some(client) = self.state.clients.get(ck) {
                    backend.compositor_set_overview_selection(client.win);
                }
            }
        } else {
            // Sliding window: keep selected index near center of 6-window view.
            let half = 3usize;
            let new_start = if self.features.overview.index < half {
                0
            } else if self.features.overview.index + half >= len {
                len.saturating_sub(6)
            } else {
                self.features.overview.index - half
            };
            let window_end = (new_start + 6).min(len);

            if new_start != self.features.overview.slide_offset {
                // Window shifted: refresh prism with new 6-client subset.
                self.features.overview.slide_offset = new_start;
                let subset: Vec<ClientKey> =
                    self.features.overview.clients[new_start..window_end].to_vec();
                let mut layout = self.build_overview_layout(&subset);
                // Mark the correct entry as selected.
                let sel_in_window = self.features.overview.index - new_start;
                for (i, entry) in layout.iter_mut().enumerate() {
                    entry.5 = i == sel_in_window;
                }
                backend.compositor_set_overview_mode(true, &layout);
            }
            // Set selection (rotation) to the face within the current window.
            let sel_in_window = self.features.overview.index - new_start;
            if let Some(&ck) = self
                .features
                .overview
                .clients
                .get(self.features.overview.index)
            {
                if let Some(client) = self.state.clients.get(ck) {
                    backend.compositor_set_overview_selection(client.win);
                }
            }
            let _ = sel_in_window; // used implicitly via set_overview_selection face_index
        }
        Ok(())
    }

    /// 切换放大镜功能
    pub fn toggle_magnifier(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.magnifier.enabled = !self.features.magnifier.enabled;
        backend.compositor_set_magnifier(self.features.magnifier.enabled);
        Ok(())
    }

    /// 切换 Peek 模式（Boss Key - 所有窗口淡出）
    pub fn toggle_peek(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.peek_active = !self.features.peek_active;
        backend.compositor_set_peek_mode(self.features.peek_active);
        Ok(())
    }

    /// 切换屏幕录制
    pub fn toggle_recording(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.recording.active = !self.features.recording.active;
        if self.features.recording.active {
            let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let cfg_dir = CONFIG.load().behavior().recording_output_dir.clone();
            let videos_dir = if !cfg_dir.is_empty() {
                cfg_dir
            } else {
                std::env::var("XDG_VIDEOS_DIR")
                    .or_else(|_| std::env::var("HOME").map(|h| format!("{}/Videos", h)))
                    .unwrap_or_else(|_| "/tmp".to_string())
            };
            let mut output_dir = std::path::PathBuf::from(&videos_dir);
            if let Err(e) = std::fs::create_dir_all(&output_dir) {
                warn!(
                    "[toggle_recording] cannot create output dir '{}': {}, fallback to /tmp",
                    output_dir.display(),
                    e
                );
                output_dir = std::path::PathBuf::from("/tmp");
            }
            let output_path = output_dir
                .join(format!("recording-{}.mp4", timestamp))
                .to_string_lossy()
                .to_string();
            let seg_path = format!("/tmp/jwm-rec-{}-seg0.mp4", timestamp);

            self.features.recording.output_path = Some(output_path.clone());
            self.features.recording.segments = Vec::new();
            self.features.recording.current_segment = Some(seg_path.clone());
            Self::save_recording_state(&output_path, &[]);

            info!(
                "[toggle_recording] start → {} (segment: {})",
                output_path, seg_path
            );
            backend.compositor_start_recording(&seg_path);
        } else {
            backend.compositor_stop_recording();
            // Collect current segment
            if let Some(seg) = self.features.recording.current_segment.take() {
                self.features.recording.segments.push(seg);
            }
            let segments = std::mem::take(&mut self.features.recording.segments);
            let output_path = self
                .features
                .recording
                .output_path
                .take()
                .unwrap_or_default();
            info!(
                "[toggle_recording] stop → {} ({} segments)",
                output_path,
                segments.len()
            );
            Self::finalize_recording(segments, output_path);
        }
        Ok(())
    }

    pub(crate) const RECORDING_STATE_FILE: &'static str = "/tmp/jwm-recording-state";

    pub(crate) fn save_recording_state(output_path: &str, segments: &[String]) {
        let mut content = output_path.to_string();
        for seg in segments {
            content.push('\n');
            content.push_str(seg);
        }
        if let Err(e) = std::fs::write(Self::RECORDING_STATE_FILE, &content) {
            warn!("[recording] failed to save state: {e}");
        }
    }

    fn load_recording_state() -> Option<(String, Vec<String>)> {
        let content = std::fs::read_to_string(Self::RECORDING_STATE_FILE).ok()?;
        let mut lines = content.lines();
        let output_path = lines.next()?.to_string();
        if output_path.is_empty() {
            return None;
        }
        let segments: Vec<String> = lines
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect();
        Some((output_path, segments))
    }

    fn clear_recording_state() {
        let _ = std::fs::remove_file(Self::RECORDING_STATE_FILE);
    }

    /// Concatenate segments into final output, or rename if single segment.
    fn finalize_recording(segments: Vec<String>, output_path: String) {
        std::thread::spawn(move || {
            if segments.is_empty() {
                Self::clear_recording_state();
                return;
            }
            if segments.len() == 1 {
                // Single segment: just move it to the final path
                if std::fs::rename(&segments[0], &output_path).is_err() {
                    let _ = std::fs::copy(&segments[0], &output_path);
                    let _ = std::fs::remove_file(&segments[0]);
                }
            } else {
                // Multiple segments: concat with ffmpeg -c copy
                let list_path = "/tmp/jwm-recording-concat.txt";
                let list_content: String = segments
                    .iter()
                    .map(|s| format!("file '{}'", s))
                    .collect::<Vec<_>>()
                    .join("\n");
                if std::fs::write(list_path, &list_content).is_ok() {
                    let _ = std::process::Command::new("ffmpeg")
                        .args([
                            "-f",
                            "concat",
                            "-safe",
                            "0",
                            "-i",
                            list_path,
                            "-c",
                            "copy",
                            "-y",
                            &output_path,
                        ])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    let _ = std::fs::remove_file(list_path);
                }
                for seg in &segments {
                    let _ = std::fs::remove_file(seg);
                }
            }
            Self::clear_recording_state();
            log::info!("[recording] finalized → {output_path}");
        });
    }

    /// Auto-resume recording after restart if state file exists.
    pub fn resume_recording_if_needed(&mut self, backend: &mut dyn Backend) {
        if let Some((output_path, segments)) = Self::load_recording_state() {
            let seg_index = segments.len();
            // Derive timestamp from output path for consistent naming
            let base = std::path::Path::new(&output_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .trim_start_matches("recording-");
            let seg_path = format!("/tmp/jwm-rec-{}-seg{}.mp4", base, seg_index);

            self.features.recording.output_path = Some(output_path);
            self.features.recording.segments = segments;
            self.features.recording.current_segment = Some(seg_path.clone());
            self.features.recording.active = true;

            backend.compositor_start_recording(&seg_path);
            info!("[recording] auto-resumed from restart (segment {seg_index}: {seg_path})");
        }
    }

    /// 切换 Expose / Mission Control 模式（显示所有窗口缩略图）
    pub fn toggle_expose(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.expose_active {
            self.features.expose_active = false;
            backend.compositor_set_expose_mode(false, vec![]);
            let _ = backend.key_ops().ungrab_keyboard();
            let _ = backend.input_ops().ungrab_pointer();
        } else {
            // Collect visible windows across all monitors
            let mut windows: Vec<(WindowId, i32, i32, u32, u32)> = Vec::new();
            for &mon_key in &self.state.monitor_order.clone() {
                if let Some(clients) = self.state.monitor_clients.get(mon_key) {
                    for &ck in clients {
                        if !self.is_client_visible_on_monitor(ck, mon_key) {
                            continue;
                        }
                        if let Some(client) = self.state.clients.get(ck) {
                            let g = &client.geometry;
                            if g.w > 0 && g.h > 0 {
                                windows.push((client.win, g.x, g.y, g.w as u32, g.h as u32));
                            }
                        }
                    }
                }
            }
            if windows.is_empty() {
                return Ok(());
            }
            self.features.expose_active = true;
            backend.compositor_set_expose_mode(true, windows);
            if let Some(root) = backend.root_window() {
                let _ = backend.key_ops().grab_keyboard(root);
            }
            let pointer_mask = (EventMaskBits::BUTTON_PRESS
                | EventMaskBits::BUTTON_RELEASE
                | EventMaskBits::POINTER_MOTION)
                .bits();
            let _ = backend.input_ops().grab_pointer(pointer_mask, None);
        }
        Ok(())
    }

    /// 更新粘性窗口的标签（当显示器切换标签时调用）
    pub(crate) fn update_sticky_tags(&mut self, mon_key: crate::core::models::MonitorKey) {
        let new_tags = if let Some(monitor) = self.state.monitors.get(mon_key) {
            monitor.get_active_tags()
        } else {
            return;
        };
        let client_keys: Vec<ClientKey> = self
            .state
            .monitor_clients
            .get(mon_key)
            .map(|keys| keys.clone())
            .unwrap_or_default();
        for ck in client_keys {
            if let Some(client) = self.state.clients.get_mut(ck) {
                if client.state.is_sticky {
                    client.state.tags = new_tags;
                }
            }
        }
    }

    /// Toggle a named scratchpad.
    ///
    /// Argument encoding (via `StringVec`):
    ///   `["name", "cmd", "arg1", ...]`  — name + spawn command
    ///   `["name"]`                      — name only (uses default scratchpad terminal)
    ///
    /// Legacy `Int(0)` falls back to the default name `"term"`.
    pub fn togglescratchpad(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cfg = CONFIG.load();
        // Parse name and optional command from argument
        let (name, spawn_cmd) = match arg {
            WMArgEnum::StringVec(v) if !v.is_empty() => {
                let name = v[0].clone();
                let cmd = if v.len() > 1 {
                    v[1..].to_vec()
                } else {
                    crate::config::Config::get_scratchpad_termcmd()
                };
                (name, cmd)
            }
            _ => (
                "term".to_string(),
                crate::config::Config::get_scratchpad_termcmd(),
            ),
        };

        // Check if the scratchpad's client still exists
        if let Some(&sp_key) = self.scratchpads.get(&name) {
            if self.state.clients.get(sp_key).is_none() {
                self.scratchpads.remove(&name);
            }
        }

        if let Some(&sp_key) = self.scratchpads.get(&name) {
            // Scratchpad exists — toggle visibility
            let is_visible = self.is_client_visible_by_key(sp_key);
            if is_visible {
                // Hide: animate upward then hide
                if let Some(client) = self.state.clients.get(sp_key) {
                    let current_rect = Rect::new(
                        client.geometry.x,
                        client.geometry.y,
                        client.geometry.w,
                        client.geometry.h,
                    );
                    // Target: move up by window height
                    let hidden_y = current_rect.y - current_rect.h - 100;
                    let hidden_rect =
                        Rect::new(current_rect.x, hidden_y, current_rect.w, current_rect.h);

                    if cfg.animation_enabled() {
                        self.animations.start(
                            sp_key,
                            current_rect,
                            hidden_rect,
                            cfg.animation_duration(),
                            cfg.animation_easing(),
                            AnimationKind::Hide,
                        );
                    } else {
                        // If animations disabled, immediately hide
                        if let Some(c) = self.state.clients.get_mut(sp_key) {
                            c.state.tags = 0;
                        }
                    }
                }

                // Mark for deferred hiding after animation completes
                if let Some(c) = self.state.clients.get_mut(sp_key) {
                    c.state.tags = 0;
                }

                let mon_key = self.state.clients.get(sp_key).and_then(|c| c.mon);
                self.focus(backend, None)?;
                if let Some(mk) = mon_key {
                    self.arrange(backend, Some(mk));
                }
            } else {
                // Show: animate downward from top
                let sel_mon_key = self.state.sel_mon;
                if let Some(mon_key) = sel_mon_key {
                    let current_tags = self
                        .state
                        .monitors
                        .get(mon_key)
                        .map(|m| m.get_active_tags())
                        .unwrap_or(1);

                    if let Some(client) = self.state.clients.get_mut(sp_key) {
                        client.state.tags = current_tags;
                        client.mon = Some(mon_key);
                        client.state.is_floating = true;
                    }

                    self.reorder_client_in_monitor_groups(sp_key);

                    // Center at 80% of monitor work area
                    if let Some(area) = self.monitor_work_area(mon_key) {
                        let w = (area.w as f32 * 0.8) as i32;
                        let h = (area.h as f32 * 0.8) as i32;
                        let x = area.x + (area.w - w) / 2;
                        let y = area.y + (area.h - h) / 2;

                        // Suppress animation during resize to set target position
                        let suppress_flag = self.suppress_layout_animation;
                        self.suppress_layout_animation = true;
                        self.resize_client(backend, sp_key, x, y, w, h, false);
                        self.suppress_layout_animation = suppress_flag;
                    }

                    self.focus(backend, Some(sp_key))?;
                    self.arrange(backend, Some(mon_key));

                    // After arrange, get actual position and start downward animation
                    if let Some(area) = self.monitor_work_area(mon_key) {
                        let w = (area.w as f32 * 0.8) as i32;
                        let h = (area.h as f32 * 0.8) as i32;
                        let x = area.x + (area.w - w) / 2;
                        let y = area.y + (area.h - h) / 2;

                        if cfg.animation_enabled() {
                            // Animate from above screen to target position
                            // from_y: window top is at (area.y - h), so window is completely above visible area
                            let from_y = area.y - h;
                            let from_rect = Rect::new(x, from_y, w, h);
                            let to_rect = Rect::new(x, y, w, h);

                            info!(
                                "[togglescratchpad] scratchpad show animation from y={} to y={}",
                                from_y, y
                            );

                            self.animations.start(
                                sp_key,
                                from_rect,
                                to_rect,
                                cfg.animation_duration(),
                                cfg.animation_easing(),
                                AnimationKind::Appear,
                            );
                        }
                    }
                }
            }
        } else {
            // No scratchpad with this name — spawn command, mark pending
            if let Some(prog) = spawn_cmd.first() {
                let mut command = Command::new(prog);
                command.args(&spawn_cmd[1..]);

                Self::setup_smithay_child_env(&mut command, backend);
                command
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit());
                Self::apply_child_pre_exec(&mut command);

                match command.spawn() {
                    Ok(child) => {
                        info!("[togglescratchpad] spawned '{}' PID: {}", name, child.id());
                        self.scratchpad_pending_name = Some(name);
                    }
                    Err(e) => {
                        error!("[togglescratchpad] failed to spawn '{}': {}", name, e);
                    }
                }
            }
        }
        Ok(())
    }

    /// 切换 Picture-in-Picture (PIP) 模式
    ///
    /// 将当前选中的窗口变为小窗悬浮在所有工作区右下角
    pub fn togglepip(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sel_mon_key) = self.state.sel_mon else {
            return Ok(());
        };
        let Some(sel_client_key) = self.state.monitors.get(sel_mon_key).and_then(|m| m.sel) else {
            return Ok(());
        };

        let is_pip = self
            .state
            .clients
            .get(sel_client_key)
            .map(|c| c.state.is_pip)
            .unwrap_or(false);

        if is_pip {
            // Exit PiP: restore state
            if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                client.state.is_pip = false;
                client.state.is_floating = client.state.old_state;
                client.state.is_sticky = false;
            }
            self.reorder_client_in_monitor_groups(sel_client_key);
            let (fx, fy, fw, fh) = if let Some(client) = self.state.clients.get(sel_client_key) {
                (
                    client.geometry.floating_x,
                    client.geometry.floating_y,
                    client.geometry.floating_w,
                    client.geometry.floating_h,
                )
            } else {
                return Ok(());
            };
            if fw > 0 && fh > 0 {
                self.resize_client(backend, sel_client_key, fx, fy, fw, fh, false);
            }
            self.arrange(backend, Some(sel_mon_key));
        } else {
            // Enter PiP: save state, shrink to bottom-right
            if let Some(client) = self.state.clients.get_mut(sel_client_key) {
                client.state.old_state = client.state.is_floating;
                client.geometry.floating_x = client.geometry.x;
                client.geometry.floating_y = client.geometry.y;
                client.geometry.floating_w = client.geometry.w;
                client.geometry.floating_h = client.geometry.h;
                client.state.is_pip = true;
                client.state.is_floating = true;
                client.state.is_sticky = true;
            }

            self.reorder_client_in_monitor_groups(sel_client_key);

            // Position at bottom-right, 25% of monitor, 10px padding
            if let Some(area) = self.monitor_work_area(sel_mon_key) {
                let w = (area.w as f32 * 0.25) as i32;
                let h = (area.h as f32 * 0.25) as i32;
                let x = area.x + area.w - w - 10;
                let y = area.y + area.h - h - 10;
                self.resize_client(backend, sel_client_key, x, y, w, h, false);
            }

            self.arrange(backend, Some(sel_mon_key));
            self.restack(backend, Some(sel_mon_key))?;
        }

        // Notify compositor of PiP state change
        if backend.has_compositor() {
            if let Some(client) = self.state.clients.get(sel_client_key) {
                backend.compositor_set_window_pip(client.win, client.state.is_pip);
            }
        }

        Ok(())
    }
}
