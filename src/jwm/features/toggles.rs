//! 特性切换功能
//!
//! 这个模块包含所有窗口管理器特性的切换函数（toggle* 系列）

use crate::backend::api::Backend;
use crate::backend::common_define::{EventMaskBits, StdCursorKind, WindowId};
use crate::config::CONFIG;
use crate::core::animation::AnimationKind;
use crate::core::models::ClientKey;
use crate::core::types::Rect;
use crate::jwm::Jwm;
use crate::jwm::types::WMArgEnum;
use log::{error, info};
use std::process::Command;

impl Jwm {
    pub(crate) fn app_launcher(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.system_ui.is_active() {
            return Ok(());
        }
        if !backend.has_compositor() {
            return Err("built-in launcher requires the JWM compositor".into());
        }
        if let Some(root) = backend.root_window() {
            backend.key_ops().grab_keyboard(root)?;
            if !backend.input_ops().grab_pointer(
                (EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE).bits(),
                None,
            )? {
                let _ = backend.key_ops().ungrab_keyboard();
                return Err("could not grab pointer for application launcher".into());
            }
        }
        self.features.system_ui = crate::jwm::features::SystemUiState::open_launcher();
        self.sync_system_ui(backend);
        Ok(())
    }

    pub(crate) fn monitor_layout(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.system_ui.is_active() {
            return Ok(());
        }
        if !backend.has_compositor() {
            return Err("display layout requires the JWM compositor".into());
        }
        let is_x11 = backend
            .as_any()
            .is::<crate::backend::x11rb::backend::X11rbBackend>()
            || backend
                .as_any()
                .is::<crate::backend::xcb::backend::XcbBackend>();
        if !is_x11 {
            return Err("display layout via xrandr is only available on an X11 backend".into());
        }

        let entries: Vec<_> = backend
            .output_ops()
            .enumerate_outputs()
            .into_iter()
            .filter(|output| !output.name.is_empty() && output.width > 0 && output.height > 0)
            .map(|output| crate::jwm::features::MonitorLayoutEntry {
                name: output.name,
                x: output.x,
                y: output.y,
                width: output.width,
                height: output.height,
            })
            .collect();
        if entries.len() < 2 {
            return Err("display layout requires at least two active outputs".into());
        }

        if let Some(root) = backend.root_window() {
            backend.key_ops().grab_keyboard(root)?;
            if !backend.input_ops().grab_pointer(
                (EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE).bits(),
                None,
            )? {
                let _ = backend.key_ops().ungrab_keyboard();
                return Err("could not grab pointer for display layout".into());
            }
        }
        self.features.system_ui = crate::jwm::features::SystemUiState::monitor_layout(entries);
        self.sync_system_ui(backend);
        Ok(())
    }

    pub(crate) fn lock_screen(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.system_ui.is_active() {
            return Ok(());
        }
        if !backend.has_compositor() {
            return Err("built-in lock screen requires the JWM compositor".into());
        }
        // On X11, never display a pretend lock if the exclusive keyboard grab
        // failed. Wayland-udev performs interception in its input pipeline.
        if let Some(root) = backend.root_window() {
            backend.key_ops().grab_keyboard(root)?;
            if !backend.input_ops().grab_pointer(
                (EventMaskBits::BUTTON_PRESS | EventMaskBits::BUTTON_RELEASE).bits(),
                None,
            )? {
                let _ = backend.key_ops().ungrab_keyboard();
                return Err("could not grab pointer for lock screen".into());
            }
        }
        self.features.system_ui = crate::jwm::features::SystemUiState::lock();
        self.sync_system_ui(backend);
        Ok(())
    }
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

    /// Toggle do-not-disturb. Broadcasts `dnd/toggle` so bars can update.
    pub fn toggle_dnd(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.do_not_disturb = !self.do_not_disturb;
        log::info!("DND {}", if self.do_not_disturb { "ON" } else { "OFF" });
        self.broadcast_ipc_event(
            "dnd/toggle",
            serde_json::json!({ "enabled": self.do_not_disturb }),
        );
        Ok(())
    }

    /// 切换 debug 看板(HUD): 显示 FPS / 帧周期 / 内存 / CPU / 渲染分区耗时
    pub fn toggle_debug_hud(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.debug_hud_on = !self.debug_hud_on;
        backend.compositor_set_debug_hud(self.debug_hud_on);
        backend.compositor_set_debug_hud_extended(self.debug_hud_on);
        log::info!("Debug HUD {}", if self.debug_hud_on { "ON" } else { "OFF" });
        Ok(())
    }

    /// Toggle the full-screen WaterLily simulation rendered by the compositor.
    pub fn toggle_waterlily(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match backend.compositor_toggle_waterlily_effect() {
            Some(enabled) => log::info!("WaterLily effect {}", if enabled { "ON" } else { "OFF" }),
            None => log::warn!("WaterLily effect is unavailable on this backend"),
        }
        Ok(())
    }

    /// 切换部分重绘(scissor 局部刷新,实验性,默认关)
    pub fn togglepartialdamage(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let enable = !backend.has_partial_damage();
        match backend.set_partial_damage(enable) {
            Ok(true) => log::info!(
                "Partial-damage redraw toggled: now {}",
                if enable { "ON" } else { "OFF" }
            ),
            Ok(false) => log::info!("Partial-damage toggle ignored (no compositor active)"),
            Err(e) => log::warn!("Failed to toggle partial-damage: {e}"),
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
            let visible = {
                let is_scrolling = self
                    .state
                    .monitors
                    .get(sel_mon_key)
                    .map(|monitor| {
                        *monitor.lt[monitor.sel_lt] == crate::core::layout::LayoutEnum::SCROLLING
                    })
                    .unwrap_or(false);
                if is_scrolling {
                    self.scrolling_state_for_monitor(sel_mon_key)
                        .map(|state| state.ordered_visible_clients(&visible))
                        .unwrap_or(visible)
                } else {
                    visible
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

    /// 切换屏幕标注（Annotation）模式
    pub fn toggle_annotation(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.annotation_active = !self.features.annotation_active;
        backend.compositor_set_annotation_mode(self.features.annotation_active);
        if self.features.annotation_active {
            // Grab keyboard (Escape to exit) and pointer (draw over all windows).
            if let Some(root) = backend.root_window() {
                let _ = backend.key_ops().grab_keyboard(root);
            }
            let pointer_mask = (EventMaskBits::BUTTON_PRESS
                | EventMaskBits::BUTTON_RELEASE
                | EventMaskBits::POINTER_MOTION)
                .bits();
            let _ = backend.input_ops().grab_pointer(pointer_mask, None);
        } else {
            self.features.annotation_drawing = false;
            let _ = backend.key_ops().ungrab_keyboard();
            let _ = backend.input_ops().ungrab_pointer();
        }
        Ok(())
    }

    /// 切换屏幕录制
    pub fn toggle_recording(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.recording.active {
            if self.features.recording.selecting_region {
                return Ok(());
            }
            let output_path = self.prepare_recording_output_path()?;
            self.begin_recording_region_selection(backend, output_path)?;
        } else {
            self.stop_recording(backend)?;
        }
        Ok(())
    }

    /// Enter interactive move/resize mode while keeping the encoder running.
    pub fn adjust_recording_region(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.recording.active {
            return Err("recording region adjustment requires an active recording".into());
        }
        if !self.features.recording.begin_region_adjustment() {
            return Ok(());
        }
        if let Err(error) = self.grab_recording_region_input(backend) {
            self.features.recording.cancel_region_selection();
            return Err(error);
        }
        self.sync_recording_region_overlay(backend);
        info!("[recording] interactive region adjustment started");
        Ok(())
    }

    fn prepare_recording_output_path(&self) -> Result<String, Box<dyn std::error::Error>> {
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let cfg_dir = CONFIG.load().behavior().recording_output_dir.clone();
        let output_dir = if !cfg_dir.is_empty() {
            std::path::PathBuf::from(cfg_dir)
        } else {
            std::env::var("XDG_VIDEOS_DIR")
                .ok()
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(dirs::video_dir)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|home| !home.is_empty())
                        .map(std::path::PathBuf::from)
                        .map(|home| home.join("Videos"))
                })
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "cannot resolve the Videos directory; set behavior.recording_output_dir",
                    )
                })?
        };
        std::fs::create_dir_all(&output_dir).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "cannot create recording output directory '{}': {error}",
                    output_dir.display()
                ),
            )
        })?;
        Ok(output_dir
            .join(format!("recording-{timestamp}.mp4"))
            .to_string_lossy()
            .to_string())
    }

    fn grab_recording_region_input(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(root) = backend.root_window() {
            backend.key_ops().grab_keyboard(root)?;
        }
        let crosshair = backend
            .cursor_provider()
            .get(StdCursorKind::Crosshair)
            .ok()
            .map(|cursor| cursor.0);
        let pointer_mask = (EventMaskBits::BUTTON_PRESS
            | EventMaskBits::BUTTON_RELEASE
            | EventMaskBits::POINTER_MOTION)
            .bits();
        if !backend.input_ops().grab_pointer(pointer_mask, crosshair)? {
            let _ = backend.key_ops().ungrab_keyboard();
            return Err("could not grab pointer for recording region selection".into());
        }
        Ok(())
    }

    fn release_recording_region_input(&mut self, backend: &mut dyn Backend) {
        let _ = backend.key_ops().ungrab_keyboard();
        let _ = backend.input_ops().ungrab_pointer();
        if let Some(root) = backend.root_window() {
            let _ = backend
                .cursor_provider()
                .apply(root, StdCursorKind::LeftPtr);
        }
    }

    fn begin_recording_region_selection(
        &mut self,
        backend: &mut dyn Backend,
        output_path: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !backend.has_compositor() {
            return Err("screen recording requires an active compositor".into());
        }
        self.features
            .recording
            .begin_initial_region_selection(output_path.clone());
        if let Err(error) = self.grab_recording_region_input(backend) {
            self.features.recording.cancel_region_selection();
            return Err(error);
        }
        backend.compositor_set_recording_region_overlay(None);
        backend.compositor_force_full_redraw();
        info!("[recording] select a region, then press Enter to start → {output_path}");
        Ok(())
    }

    pub(crate) fn sync_recording_region_overlay(&mut self, backend: &mut dyn Backend) {
        let region = self
            .features
            .recording
            .region
            .and_then(Self::recording_region_tuple);
        backend.compositor_set_recording_region_overlay(region);
        backend.compositor_force_full_redraw();
    }

    pub(crate) fn finish_recording_region_interaction(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(region) = self.features.recording.region else {
            return Ok(());
        };
        let Some(region_tuple) = Self::recording_region_tuple(region) else {
            return Ok(());
        };
        let adjusting = self.features.recording.adjusting_region;
        let pending_path = self.features.recording.pending_output_path.clone();
        self.features.recording.finish_region_selection();
        self.release_recording_region_input(backend);
        backend.compositor_set_recording_region_overlay(None);

        if adjusting {
            backend.compositor_set_recording_region(region_tuple);
            backend.compositor_force_full_redraw();
            info!(
                "[recording] region adjustment committed: {}x{}+{}+{}",
                region.w, region.h, region.x, region.y
            );
            return Ok(());
        }

        let Some(output_path) = pending_path else {
            return Err("recording selection lost its output path".into());
        };
        self.start_recording_region(backend, &output_path, region)?;
        Ok(())
    }

    pub(crate) fn cancel_recording_region_interaction(&mut self, backend: &mut dyn Backend) {
        let was_adjusting = self.features.recording.adjusting_region;
        let restored = self.features.recording.cancel_region_selection();
        self.release_recording_region_input(backend);
        backend.compositor_set_recording_region_overlay(None);
        if was_adjusting {
            if let Some(region) = restored.and_then(Self::recording_region_tuple) {
                backend.compositor_set_recording_region(region);
            }
        }
        backend.compositor_force_full_redraw();
        info!(
            "[recording] region {} cancelled",
            if was_adjusting {
                "adjustment"
            } else {
                "selection"
            }
        );
    }

    pub(crate) fn recording_region_tuple(region: Rect) -> Option<(i32, i32, u32, u32)> {
        Some((
            region.x,
            region.y,
            u32::try_from(region.w).ok()?,
            u32::try_from(region.h).ok()?,
        ))
    }

    /// Toggle the built-in microphone recorder (Alt+Ctrl+M by default).
    pub fn toggle_audio_recording(
        &mut self,
        _backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.features.audio_recording.refresh();
        if self.features.audio_recording.active {
            self.stop_audio_recording()?;
        } else {
            let behavior = CONFIG.load().behavior().clone();
            let output_dir = if !behavior.audio_recording_output_dir.is_empty() {
                std::path::PathBuf::from(&behavior.audio_recording_output_dir)
            } else {
                std::env::var("XDG_MUSIC_DIR")
                    .map(std::path::PathBuf::from)
                    .or_else(|_| {
                        std::env::var("HOME")
                            .map(|home| std::path::PathBuf::from(home).join("Music"))
                    })
                    .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
            };
            let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let format = behavior.audio_recording_format.as_str();
            if !matches!(format, "wav" | "flac" | "opus" | "mp3") {
                return Err(format!("unsupported audio recording format: {format}").into());
            }
            let path = output_dir.join(format!("jwm-recording-{timestamp}.{format}"));
            self.start_audio_recording(&path)?;
        }
        Ok(())
    }

    pub(crate) fn start_audio_recording(
        &mut self,
        output_path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let behavior = CONFIG.load().behavior().clone();
        if self.features.recording.active && behavior.recording_audio_enabled {
            return Err(
                "screen recording is already using the configured microphone; stop it first".into(),
            );
        }
        self.features.audio_recording.start(
            output_path,
            &behavior.audio_recording_device,
            behavior.audio_recording_sample_rate,
            behavior.audio_recording_channels,
            &behavior.audio_recording_backend,
            &behavior.audio_recording_bitrate,
        )?;
        info!(
            "[audio-recording] start → {} (backend={}, format={}, device={}, {} Hz, {} channel(s))",
            output_path.display(),
            self.features.audio_recording.backend,
            self.features.audio_recording.format,
            self.features.audio_recording.device,
            self.features.audio_recording.sample_rate,
            self.features.audio_recording.channels
        );
        self.broadcast_ipc_event(
            "audio_recording/started",
            serde_json::json!({"output_path": output_path}),
        );
        Ok(())
    }

    pub(crate) fn stop_audio_recording(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let was_active = self.features.audio_recording.active;
        let path = self.features.audio_recording.output_path.clone();
        self.features.audio_recording.stop()?;
        if was_active {
            info!(
                "[audio-recording] stop → {}",
                path.as_deref().unwrap_or("(unset)")
            );
            self.broadcast_ipc_event(
                "audio_recording/stopped",
                serde_json::json!({"output_path": path}),
            );
        }
        Ok(())
    }

    /// Start a recording from a source rectangle. The encoded dimensions are
    /// fixed from this initial rectangle while later region updates are scaled
    /// into the same video canvas.
    pub(crate) fn start_recording_region(
        &mut self,
        backend: &mut dyn Backend,
        output_path: &str,
        region: Rect,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.recording.active {
            return Err("recording is already active".into());
        }
        if !backend.has_compositor() {
            return Err("screen recording requires an active compositor".into());
        }
        let output = std::path::Path::new(output_path);
        if !output.is_absolute() {
            return Err("recording output path must be absolute".into());
        }
        if output.extension().and_then(|v| v.to_str()) != Some("mp4") {
            return Err("recording output path must end in .mp4".into());
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if output.exists() {
            return Err(format!("recording output already exists: {output_path}").into());
        }
        let region = self.normalize_initial_recording_region(region)?;

        // A standalone WAV recording and the synchronized screen audio track
        // must not race for the same capture device. Finalize the standalone
        // file before handing the microphone to the screen recorder.
        if CONFIG.load().behavior().recording_audio_enabled && self.features.audio_recording.active
        {
            info!("[recording] stopping standalone audio before synchronized capture");
            self.stop_audio_recording()?;
        }

        self.features.recording.start(output_path.to_string());
        self.features.recording.set_region(region);
        self.features.recording.set_output_size_from_region();
        self.features
            .recording
            .start_segment(output_path.to_string());
        let region_tuple = Self::recording_region_tuple(region)
            .ok_or("recording region dimensions are invalid")?;
        info!(
            "[recording] start → {output_path} ({}x{}+{}+{})",
            region.w, region.h, region.x, region.y
        );
        backend.compositor_start_recording_region(output_path, region_tuple);
        Ok(())
    }

    pub(crate) fn normalize_initial_recording_region(
        &self,
        region: Rect,
    ) -> Result<Rect, Box<dyn std::error::Error>> {
        if self.s_w < 16 || self.s_h < 16 {
            return Err("screen is too small for region recording".into());
        }
        let x = region.x.clamp(0, self.s_w - 16);
        let y = region.y.clamp(0, self.s_h - 16);
        let max_width = self.s_w - x;
        let max_height = self.s_h - y;
        let width = region.w.clamp(16, max_width) & !1;
        let height = region.h.clamp(16, max_height) & !1;
        if width < 16 || height < 16 {
            return Err("recording region must be at least 16x16".into());
        }
        Ok(Rect::new(x, y, width, height))
    }

    /// Stop the active recording. This operation is intentionally idempotent.
    pub(crate) fn stop_recording(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.features.recording.active {
            if self.features.recording.selecting_region {
                self.cancel_recording_region_interaction(backend);
            }
            return Ok(());
        }
        if self.features.recording.selecting_region {
            self.cancel_recording_region_interaction(backend);
        }
        backend.compositor_stop_recording();
        self.features.recording.stop();
        let segments = std::mem::take(&mut self.features.recording.segments);
        let output_path = self
            .features
            .recording
            .output_path
            .clone()
            .unwrap_or_default();
        info!(
            "[recording] stop → {output_path} ({} segments)",
            segments.len()
        );
        Self::finalize_recording(segments, output_path);
        Ok(())
    }

    /// Validate direct output, or concatenate legacy multi-segment recordings.
    fn finalize_recording(segments: Vec<String>, output_path: String) {
        std::thread::spawn(move || {
            if segments.is_empty() {
                return;
            }
            if segments.len() == 1 {
                // The Wayland compositor closes ffmpeg on its next GL frame.
                // Do not move its MP4 before ffmpeg has written the moov atom,
                // otherwise the final path can point at an unplayable file.
                let segment = &segments[0];
                let ready = (0..100).any(|_| {
                    let status = std::process::Command::new("ffprobe")
                        .args([
                            "-v",
                            "error",
                            "-show_entries",
                            "format=duration",
                            "-of",
                            "default=nw=1",
                            segment,
                        ])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|status| status.success());
                    if !status {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    status
                });
                if !ready {
                    log::error!(
                        "[recording] output was not finalized within 5s; leaving it at {segment}"
                    );
                    return;
                }
                // New recordings are encoded directly at output_path. Keep the
                // move for old callers that may still pass a separate segment.
                if segment != &output_path && std::fs::rename(segment, &output_path).is_err() {
                    if std::fs::copy(segment, &output_path).is_ok() {
                        let _ = std::fs::remove_file(segment);
                    }
                }
            } else {
                // Multiple segments: concat with ffmpeg -c copy
                let list_path = std::path::Path::new(&output_path).with_extension("concat.txt");
                let list_content: String = segments
                    .iter()
                    .map(|s| format!("file '{}'", s))
                    .collect::<Vec<_>>()
                    .join("\n");
                if std::fs::write(&list_path, &list_content).is_ok() {
                    let _ = std::process::Command::new("ffmpeg")
                        .args(["-f", "concat", "-safe", "0", "-i"])
                        .arg(&list_path)
                        .args(["-c", "copy", "-y", &output_path])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    let _ = std::fs::remove_file(&list_path);
                }
                for seg in &segments {
                    let _ = std::fs::remove_file(seg);
                }
            }
            log::info!("[recording] finalized → {output_path}");
        });
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
