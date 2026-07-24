use crate::backend::api::BackendEvent;
// src/backend/x11rb/backend.rs
use self::ids::X11IdRegistry;
use crate::backend::api::EventHandler;
use crate::backend::api::Geometry;
use crate::backend::api::InteractionAction;
use crate::backend::api::PropertyKind;
use crate::backend::api::ResizeEdge;
use crate::backend::common_define::EventMaskBits;
use crate::backend::common_define::StdCursorKind;
use crate::backend::common_define::WindowId;
use crate::backend::error::BackendError;
use calloop::signals::{Signal, Signals};
use std::any::Any;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::connection::RequestConnection;
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::randr::NotifyMask;
use x11rb::protocol::xproto::Screen;
use x11rb::rust_connection::RustConnection;

use calloop::{
    EventLoop,
    timer::{TimeoutAction, Timer},
};

use crate::backend::api::{
    Backend, Capabilities, ColorAllocator, CompositorBenchmark, CursorProvider, DisplayControl,
    EwmhFacade, InputOps, KeyOps, OutputOps, PropertyOps, RenderScheduler, VrrCapabilities,
    WindowOps,
};
use crate::backend::x11::wm::event_bridge::{CompositorEventSources, compositor_event_ops};
use crate::backend::x11::wm::{SUPPORTED_EWMH_FEATURES, primary_refresh};

use self::{
    color::X11ColorAllocator, cursor::X11CursorProvider, event_source::X11EventSource,
    ewmh_facade::X11EwmhFacade, input_ops::X11InputOps, key_ops::X11KeyOps,
    output_ops::X11OutputOps, property_ops::X11PropertyOps, window_ops::X11WindowOps,
};
use super::Atoms;

pub struct X11rbLoopData<'a> {
    pub backend: &'a mut X11rbBackend,
    pub handler: &'a mut dyn EventHandler,
    pub should_exit: bool,
}

#[allow(dead_code)]
pub struct X11rbBackend {
    conn: Arc<RustConnection>,
    screen: Screen,
    screen_num: usize,
    root: WindowId,
    root_x11: u32,
    ids: X11IdRegistry,
    atoms: Atoms,

    caps: Capabilities,

    window_ops: Box<dyn WindowOps>,
    input_ops: Box<dyn InputOps>,
    property_ops: Box<dyn PropertyOps>,
    output_ops: Box<dyn OutputOps>,
    key_ops: Box<dyn KeyOps>,
    ewmh_facade: Option<Box<dyn EwmhFacade>>,

    cursor_provider: Box<dyn CursorProvider>,
    color_allocator: Box<dyn ColorAllocator>,

    _init_event_source: Option<X11EventSource>,

    interaction: Option<X11Interaction>,

    compositor: Option<super::compositor::Compositor<RustConnection>>,
    compositor_loop_signal: Option<calloop::LoopSignal>,

    systray: Option<super::systray::SystemTray<RustConnection>>,

    benchmark_auto_exit: bool,

    /// Reused per-frame scratch buffer for the WindowId→x11 scene translation
    /// in `compositor_render_frame`, avoiding a Vec allocation every frame.
    scratch_x11_scene: Vec<(u32, i32, i32, u32, u32)>,
}

struct X11Interaction {
    win: WindowId,
    start_geom: Geometry,
    start_root_x: f64,
    start_root_y: f64,
    action: InteractionAction,
    /// Current geometry during drag, updated each handle_motion call.
    current_x: i32,
    current_y: i32,
    current_w: u32,
    current_h: u32,
}

impl X11rbBackend {
    fn debug_drag_enabled() -> bool {
        // Cached: this is read on every MotionNotify during a drag, so the
        // env lookup (process-wide env lock + alloc) must not run per-event.
        static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CACHE.get_or_init(|| {
            env::var("JWM_DEBUG_DRAG")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    fn systray_handle_event(&mut self, ev: &BackendEvent) -> bool {
        let systray = match self.systray.as_mut() {
            Some(s) => s,
            None => return false,
        };
        match ev {
            BackendEvent::ClientMessage {
                window: _,
                type_,
                data,
                ..
            } => {
                if *type_ == u32::from(self.atoms._NET_SYSTEM_TRAY_OPCODE) {
                    return systray.handle_client_message(self.root_x11, data);
                }
                false
            }
            BackendEvent::WindowDestroyed(win) => {
                let x11w = self.ids.x11(*win).unwrap_or(0);
                if systray.is_tray_icon(x11w) {
                    systray.handle_destroy(x11w);
                    return true;
                }
                false
            }
            BackendEvent::WindowUnmapped(win) => {
                let x11w = self.ids.x11(*win).unwrap_or(0);
                if systray.is_tray_icon(x11w) {
                    systray.handle_unmap(x11w);
                    return true;
                }
                false
            }
            BackendEvent::WindowMapped(win) => {
                let x11w = self.ids.x11(*win).unwrap_or(0);
                if systray.is_tray_icon(x11w) {
                    systray.handle_map(x11w);
                    return true;
                }
                false
            }
            BackendEvent::PropertyChanged { window, kind } => {
                if matches!(kind, PropertyKind::Other) {
                    let x11w = self.ids.x11(*window).unwrap_or(0);
                    if systray.is_tray_icon(x11w) {
                        systray.handle_xembed_info_change(x11w);
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn enrich_event_with_output(&self, mut ev: BackendEvent) -> BackendEvent {
        crate::backend::x11::wm::enrich_background_event(
            &mut ev,
            |x, y| self.output_ops.output_at(x, y),
            || self.output_ops.invalidate_output_cache(),
        );
        ev
    }
    pub fn new() -> Result<Self, BackendError> {
        let (raw_conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)?;
        let conn = Arc::new(raw_conn);
        use x11rb::connection::Connection;
        let screen = conn.setup().roots[screen_num].clone();
        let ids = X11IdRegistry::new(1);
        let root_x11 = screen.root;
        let root = ids.intern(root_x11);

        if conn
            .extension_information(x11rb::protocol::randr::X11_EXTENSION_NAME)?
            .is_some()
        {
            let mask =
                NotifyMask::SCREEN_CHANGE | NotifyMask::OUTPUT_CHANGE | NotifyMask::CRTC_CHANGE;
            conn.randr_select_input(screen.root, mask)?;
        }

        let numlock_mask = Arc::new(Mutex::new(0u16));
        let atoms = Atoms::new(conn.as_ref())?.reply()?;

        let window_ops: Box<dyn WindowOps> = Box::new(X11WindowOps::new(
            conn.clone(),
            atoms.clone(),
            numlock_mask.clone(),
            screen.root,
            ids.clone(),
        ));

        let x11_input_ops = X11InputOps::new(conn.clone(), screen.root, ids.clone());
        let input_ops: Box<dyn InputOps> = Box::new(x11_input_ops.clone());
        let property_ops: Box<dyn PropertyOps> = Box::new(X11PropertyOps::new(
            conn.clone(),
            atoms.clone(),
            ids.clone(),
        ));
        let output_ops: Box<dyn OutputOps> = Box::new(X11OutputOps::new(
            conn.clone(),
            screen.root,
            screen.width_in_pixels as i32,
            screen.height_in_pixels as i32,
        ));
        let key_ops: Box<dyn KeyOps> = Box::new(X11KeyOps::new(
            conn.clone(),
            numlock_mask.clone(),
            ids.clone(),
        ));
        let ewmh_facade: Option<Box<dyn EwmhFacade>> = Some(Box::new(X11EwmhFacade::new(
            conn.clone(),
            root,
            atoms.clone(),
            ids.clone(),
        )));
        let cursor_provider: Box<dyn CursorProvider> =
            Box::new(X11CursorProvider::new(conn.clone(), root_x11, ids.clone())?);
        let color_allocator: Box<dyn ColorAllocator> = Box::new(X11ColorAllocator::new(
            conn.clone(),
            screen.default_colormap,
        ));

        let caps = Capabilities {
            can_warp_pointer: true,
            supports_client_list: true,
            ..Default::default()
        };

        // Try to initialize compositor (GPU compositing)
        // The shared compositor uses MANUAL redirect and imports named X pixmaps
        // through the configured GLX/OpenGL or EGL/GLES platform. GLX still requires
        // direct rendering; `compositor_api = "auto"` can probe EGL before GLX.
        // Enabled via config.toml [behavior] compositor = true, or env JWM_COMPOSITOR=1.
        let compositor_enabled = env::var("JWM_COMPOSITOR")
            .map(|v| v == "1")
            .unwrap_or_else(|_| crate::config::CONFIG.load().compositor_enabled());

        // P4: Query primary monitor refresh rate before compositor init
        let refresh = primary_refresh(&OutputOps::enumerate_outputs(output_ops.as_ref()));
        log::info!(
            "x11rb backend: primary monitor refresh rate: {:.3}Hz ({}Hz compositor policy)",
            refresh.millihz as f64 / 1000.0,
            refresh.hz
        );

        let compositor = if compositor_enabled {
            match super::compositor::Compositor::new(
                conn.clone(),
                root_x11,
                screen.width_in_pixels as u32,
                screen.height_in_pixels as u32,
                refresh.hz,
            ) {
                Ok(c) => {
                    log::info!("GPU compositor initialized successfully");
                    Some(c)
                }
                Err(e) => {
                    let error = BackendError::Message(e).with_context(
                        crate::backend::error::BackendErrorContext::new(
                            "x11rb",
                            crate::backend::error::ErrorBoundary::Renderer,
                            "initialize GPU compositor",
                        ),
                    );
                    log::warn!("falling back to non-composited mode: {error}");
                    None
                }
            }
        } else {
            log::info!("Compositor disabled (set JWM_COMPOSITOR=1 to enable)");
            None
        };

        let overlay_x11 = compositor.as_ref().map(|c| c.overlay_window());
        let event_source = X11EventSource::new(
            conn.clone(),
            atoms.clone(),
            screen.root,
            overlay_x11,
            ids.clone(),
        );

        let mut backend = Self {
            conn,
            screen,
            screen_num,
            root,
            root_x11,
            ids,
            atoms,
            caps,
            window_ops,
            input_ops,
            property_ops,
            output_ops,
            key_ops,
            ewmh_facade,
            cursor_provider,
            color_allocator,
            _init_event_source: Some(event_source),
            interaction: None,
            compositor,
            compositor_loop_signal: None,
            systray: None,
            benchmark_auto_exit: false,
            scratch_x11_scene: Vec::new(),
        };

        // Initialize system tray
        let screen_num = backend.screen_num;
        match super::systray::SystemTray::new(
            Arc::clone(&backend.conn),
            backend.atoms,
            backend.root_x11,
            screen_num,
        ) {
            Ok(mut tray) => match tray.acquire_selection() {
                Ok(true) => {
                    log::info!("[systray] Acquired system tray selection");
                    backend.systray = Some(tray);
                }
                Ok(false) => {
                    log::info!("[systray] Another tray owner exists, skipping");
                }
                Err(e) => {
                    log::warn!("[systray] Failed to acquire selection: {}", e);
                }
            },
            Err(e) => {
                log::warn!("[systray] Failed to create system tray: {}", e);
            }
        }

        backend.compositor_auto_configure_hdr();
        Ok(backend)
    }

    fn compositor_handle_event(&mut self, event: &BackendEvent) {
        let Some(compositor) = self.compositor.as_mut() else {
            return;
        };
        let overlay = compositor.overlay_window();
        let ids = &self.ids;
        let window_ops = &self.window_ops;
        let property_ops = &self.property_ops;
        let sources = CompositorEventSources {
            resolve: &|win| ids.x11(win).ok(),
            geometry: &|win| {
                window_ops
                    .get_geometry(win)
                    .ok()
                    .map(|geometry| (geometry.x, geometry.y, geometry.w, geometry.h))
            },
            class: &|win| property_ops.get_class(win).1,
            override_redirect: &|win| {
                window_ops
                    .get_window_attributes(win)
                    .is_ok_and(|attributes| attributes.override_redirect)
            },
        };
        for op in compositor_event_ops(event, self.root_x11, overlay, &sources) {
            compositor.apply_event_op(self.root_x11, op);
        }
    }

    pub fn atoms(&self) -> &Atoms {
        &self.atoms
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    fn compositor_auto_configure_hdr(&mut self) {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        if !behavior.hdr_enabled {
            return;
        }

        if let Some(output_id) = self.query_primary_randr_output() {
            if let Some(caps) = super::edid::query_edid_hdr(&self.conn, output_id) {
                log::info!(
                    "HDR EDID: max={:.0} nits, min={:.2} nits, PQ={}, HLG={}, BT.2020={}",
                    caps.max_luminance_nits,
                    caps.min_luminance_nits,
                    caps.supports_pq,
                    caps.supports_hlg,
                    caps.supports_bt2020
                );

                let plan = crate::backend::edid::hdr_compositor_plan(&caps);
                if let Some(c) = self.compositor.as_mut() {
                    c.apply_hdr_plan(&plan);
                }

                self.set_output_hdr_properties(output_id, true);
            } else {
                log::info!("HDR enabled but display EDID has no HDR metadata; using SDR EOTF");
            }
        }
    }

    fn set_output_hdr_properties(&self, output: u32, enable: bool) {
        use x11rb::protocol::xproto::ConnectionExt as _;
        use x11rb::protocol::xproto::PropMode;

        if enable {
            if let Ok(atom_cookie) = self.conn.intern_atom(false, b"max_bpc") {
                if let Ok(atom_reply) = atom_cookie.reply() {
                    let max_bpc_atom = atom_reply.atom;
                    let value: u32 = 10;
                    let _ = self.conn.randr_change_output_property(
                        output,
                        max_bpc_atom,
                        x11rb::protocol::xproto::AtomEnum::INTEGER.into(),
                        32,
                        PropMode::REPLACE,
                        1,
                        &value.to_le_bytes(),
                    );
                    log::info!("HDR: set max_bpc=10 on output 0x{:x}", output);
                }
            }

            if let Ok(cs_atom_cookie) = self.conn.intern_atom(false, b"Colorspace") {
                if let Ok(cs_atom_reply) = cs_atom_cookie.reply() {
                    let cs_atom = cs_atom_reply.atom;
                    if let Ok(val_cookie) = self.conn.intern_atom(false, b"BT2020_RGB") {
                        if let Ok(val_reply) = val_cookie.reply() {
                            let val_atom = val_reply.atom;
                            let _ = self.conn.randr_change_output_property(
                                output,
                                cs_atom,
                                x11rb::protocol::xproto::AtomEnum::ATOM.into(),
                                32,
                                PropMode::REPLACE,
                                1,
                                &val_atom.to_le_bytes(),
                            );
                            log::info!("HDR: set Colorspace=BT2020_RGB on output 0x{:x}", output);
                        }
                    }
                }
            }
        } else {
            if let Ok(atom_cookie) = self.conn.intern_atom(false, b"max_bpc") {
                if let Ok(atom_reply) = atom_cookie.reply() {
                    let max_bpc_atom = atom_reply.atom;
                    let value: u32 = 8;
                    let _ = self.conn.randr_change_output_property(
                        output,
                        max_bpc_atom,
                        x11rb::protocol::xproto::AtomEnum::INTEGER.into(),
                        32,
                        PropMode::REPLACE,
                        1,
                        &value.to_le_bytes(),
                    );
                    log::info!("HDR: restored max_bpc=8 on output 0x{:x}", output);
                }
            }
        }

        let _ = self.conn.flush();
    }

    fn query_output_vrr_capable(&self, output: u32) -> bool {
        use x11rb::protocol::xproto::AtomEnum;
        use x11rb::protocol::xproto::ConnectionExt as _;

        let atom = match self.conn.intern_atom(false, b"vrr_capable") {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply.atom,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        let reply = match self.conn.randr_get_output_property(
            output,
            atom,
            AtomEnum::ANY,
            0,
            1,
            false,
            false,
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        match reply.format {
            8 => reply.data.first().copied().unwrap_or(0) != 0,
            32 => {
                reply
                    .data
                    .get(..4)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u32::from_ne_bytes)
                    .unwrap_or(0)
                    != 0
            }
            _ => false,
        }
    }

    fn query_primary_randr_output(&self) -> Option<u32> {
        let resources = self
            .conn
            .randr_get_screen_resources(self.root_x11)
            .ok()?
            .reply()
            .ok()?;
        for &output in resources.outputs.iter() {
            let oi = self
                .conn
                .randr_get_output_info(output, 0)
                .ok()?
                .reply()
                .ok()?;
            if oi.crtc != 0 && oi.connection == x11rb::protocol::randr::Connection::CONNECTED {
                return Some(output);
            }
        }
        None
    }
}

crate::backend::x11::wm::compositor_delegation::delegate_compositor_capabilities!(
    X11rbBackend,
    intern_raw = intern
);

impl DisplayControl for X11rbBackend {
    fn query_vrr_capabilities(
        &self,
        output: crate::backend::common_define::OutputId,
    ) -> Option<VrrCapabilities> {
        let config = crate::config::CONFIG.load();
        if !config.behavior().vrr_enabled {
            return None;
        }
        let outputs = OutputOps::enumerate_outputs(self.output_ops.as_ref());
        outputs
            .iter()
            .any(|candidate| candidate.id == output)
            .then(|| VrrCapabilities {
                supported: self.query_output_vrr_capable(output.0 as u32),
                current_enabled: false,
                min_refresh_hz: config.behavior().vrr_min_fps,
                max_refresh_hz: config.behavior().vrr_max_fps,
            })
    }
    fn set_vrr_enabled(
        &mut self,
        _output: crate::backend::common_define::OutputId,
        _enabled: bool,
    ) -> Result<(), BackendError> {
        Err(BackendError::Unsupported(
            "X11 set_vrr_enabled not implemented",
        ))
    }
    fn set_hdr_metadata(
        &mut self,
        output: crate::backend::common_define::OutputId,
        enabled: bool,
    ) -> Result<(), BackendError> {
        self.set_output_hdr_properties(output.0 as u32, enabled);
        Ok(())
    }
}

impl RenderScheduler for X11rbBackend {
    fn request_render(&mut self) {
        let _ = self.conn.flush();
    }
    fn has_compositor(&self) -> bool {
        self.compositor.is_some()
    }
    fn compositor_needs_render(&self) -> bool {
        self.compositor.as_ref().is_some_and(|c| c.needs_render())
    }
    fn compositor_overlay_window(&self) -> Option<WindowId> {
        self.compositor
            .as_ref()
            .map(|c| self.ids.intern(c.overlay_window()))
    }
}

impl Backend for X11rbBackend {
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn root_window(&self) -> Option<WindowId> {
        Some(self.root)
    }

    fn check_existing_wm(&self) -> Result<(), BackendError> {
        let mask_bits = EventMaskBits::SUBSTRUCTURE_REDIRECT.bits();
        self.window_ops
            .change_event_mask(self.root, mask_bits)
            .map_err(|e| {
                BackendError::Message(format!(
                    "Another window manager is already running: {:?}",
                    e
                ))
            })
    }

    fn set_compositor_enabled(&mut self, enabled: bool) -> Result<bool, BackendError> {
        let currently_enabled = self.compositor.is_some();
        if enabled == currently_enabled {
            return Ok(false);
        }
        if enabled {
            let refresh = primary_refresh(&OutputOps::enumerate_outputs(self.output_ops.as_ref()));
            match super::compositor::Compositor::new(
                self.conn.clone(),
                self.root_x11,
                self.screen.width_in_pixels as u32,
                self.screen.height_in_pixels as u32,
                refresh.hz,
            ) {
                Ok(mut compositor) => {
                    if let Some(signal) = self.compositor_loop_signal.clone() {
                        compositor.set_waterlily_loop_signal(signal);
                    }
                    // Phase 1.3: Use batched geometry requests for all windows (single round-trip!)
                    let overlay = compositor.overlay_window();
                    let all_windows = self.ids.all_x11_windows();
                    let windows: Vec<_> = all_windows
                        .into_iter()
                        .filter(|(x11w, _)| *x11w != self.root_x11 && *x11w != overlay)
                        .collect();

                    if !windows.is_empty() {
                        use crate::backend::x11rb::batch::BatchedGeometryRequest;
                        let mut batch = BatchedGeometryRequest::new(&*self.conn);

                        for &(x11w, _) in &windows {
                            batch.queue_geometry(x11w);
                        }

                        match batch.flush_and_collect() {
                            Ok(geometries) => {
                                for (x11w, _) in windows {
                                    if let Some((x, y, w, h)) = geometries.get(&x11w) {
                                        compositor.add_window(
                                            x11w, *x as i32, *y as i32, *w as u32, *h as u32,
                                        );
                                    }
                                }
                                log::info!(
                                    "Compositor enabled at runtime (batched {} windows)",
                                    geometries.len()
                                );
                            }
                            Err(e) => {
                                log::warn!(
                                    "Batched geometry request failed: {:?}, falling back to individual queries",
                                    e
                                );
                                // Fallback to individual queries
                                for (x11w, wid) in self.ids.all_x11_windows() {
                                    if x11w == self.root_x11 || x11w == overlay {
                                        continue;
                                    }
                                    if let Ok(geom) = self.window_ops.get_geometry(wid) {
                                        compositor.add_window(x11w, geom.x, geom.y, geom.w, geom.h);
                                    }
                                }
                            }
                        }
                    }

                    self.compositor = Some(compositor);
                    Ok(true)
                }
                Err(e) => {
                    log::warn!("Failed to enable compositor at runtime: {e}");
                    Err(BackendError::Message(format!(
                        "compositor init failed: {e}"
                    )))
                }
            }
        } else {
            log::info!("Compositor disabled at runtime");
            self.compositor.take(); // Drop triggers cleanup
            Ok(true)
        }
    }

    fn has_partial_damage(&self) -> bool {
        self.compositor
            .as_ref()
            .is_some_and(|c| c.has_partial_damage())
    }

    fn set_partial_damage(&mut self, enabled: bool) -> Result<bool, BackendError> {
        Ok(self
            .compositor
            .as_mut()
            .is_some_and(|c| c.set_partial_damage(enabled)))
    }

    fn compositor_render_frame(
        &mut self,
        scene: &[(u64, i32, i32, u32, u32)],
        focused_window: Option<u64>,
    ) -> Result<bool, BackendError> {
        if self.compositor.is_none() {
            return Ok(false);
        }
        // Convert WindowId raw u64 to x11 window u32 via ids registry.
        // Reuse a persistent scratch Vec (detached via take) so this runs
        // allocation-free every frame.
        let mut x11_scene = std::mem::take(&mut self.scratch_x11_scene);
        x11_scene.clear();
        x11_scene.reserve(scene.len());
        x11_scene.extend(scene.iter().filter_map(|&(wid_raw, x, y, w, h)| {
            let wid = WindowId::from_raw(wid_raw);
            self.ids.x11(wid).ok().map(|x11w| (x11w, x, y, w, h))
        }));
        let focused_x11 = focused_window.and_then(|raw| {
            let wid = WindowId::from_raw(raw);
            self.ids.x11(wid).ok()
        });
        let compositor = self.compositor.as_mut().unwrap();
        compositor.ensure_scene_windows_tracked(&x11_scene, self.root_x11, scene.len());

        let _ = self.conn.flush();
        let rendered = compositor.render_frame(&x11_scene, focused_x11);
        // Return the buffer to its home for reuse next frame.
        self.scratch_x11_scene = x11_scene;
        Ok(rendered)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
    fn window_ops(&self) -> &dyn WindowOps {
        &*self.window_ops
    }
    fn input_ops(&self) -> &dyn InputOps {
        &*self.input_ops
    }
    fn property_ops(&self) -> &dyn PropertyOps {
        &*self.property_ops
    }
    fn output_ops(&self) -> &dyn OutputOps {
        &*self.output_ops
    }
    fn key_ops(&self) -> &dyn KeyOps {
        &*self.key_ops
    }
    fn key_ops_mut(&mut self) -> &mut dyn KeyOps {
        &mut *self.key_ops
    }
    fn register_wm(&self, wm_name: &str) -> Result<(), BackendError> {
        if let Some(facade) = self.ewmh_facade.as_ref() {
            let _support_win = facade.setup_supporting_wm_check(wm_name)?;
            facade.declare_supported(SUPPORTED_EWMH_FEATURES)?;
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<(), BackendError> {
        // Drop compositor before other X11 resources
        self.compositor.take();

        // Clean up system tray
        if let Some(ref tray) = self.systray {
            tray.cleanup();
        }
        self.systray.take();

        // Free X11 resources
        let _ = self.color_allocator.free_all_theme_pixels();
        let _ = self.cursor_provider.cleanup();

        if let Some(facade) = self.ewmh_facade.as_ref() {
            let _ = facade.reset_root_properties();
        }
        Ok(())
    }

    fn on_focused_client_changed(&mut self, win: Option<WindowId>) -> Result<(), BackendError> {
        if let Some(w) = win {
            // ICCCM focus model: only call SetInputFocus when the client accepts
            // input. The WM_HINTS input flag is absent → assume true (Passive),
            // Some(true) → Passive/Locally Active, Some(false) → Globally Active or
            // No Input, where the client manages its own focus and a forced
            // SetInputFocus is incorrect (mirrors dwm's `neverfocus`).
            let wants_input = self
                .property_ops
                .get_wm_hints(w)
                .and_then(|h| h.input)
                .unwrap_or(true);
            if wants_input {
                self.window_ops.set_input_focus(w)?;
            }

            // Always offer WM_TAKE_FOCUS; send_take_focus is a no-op unless the
            // client advertises it in WM_PROTOCOLS (Locally/Globally Active).
            let _ = self.window_ops.send_take_focus(w);

            // 3. 更新 EWMH 属性
            if let Some(facade) = self.ewmh_facade.as_ref() {
                facade.set_active_window(w)?;
            }
        } else {
            // 清除焦点到 Root
            self.window_ops.set_input_focus_root()?;
            if let Some(facade) = self.ewmh_facade.as_ref() {
                facade.clear_active_window()?;
            }
        }

        if let Some(compositor) = self.compositor.as_mut() {
            // Focus affects active/inactive visuals and can invalidate more than
            // the target window's last damage region, so redraw the full scene.
            compositor.force_full_redraw();
        }

        Ok(())
    }

    fn on_client_list_changed(
        &mut self,
        clients: &[WindowId],
        stack: &[WindowId],
    ) -> Result<(), BackendError> {
        if let Some(facade) = self.ewmh_facade.as_ref() {
            facade.set_client_list(clients)?;
            facade.set_client_list_stacking(stack)?;
        }
        Ok(())
    }

    fn on_desktop_changed(
        &mut self,
        current: u32,
        total: u32,
        names: &[&str],
    ) -> Result<(), BackendError> {
        if let Some(facade) = self.ewmh_facade.as_ref() {
            facade.set_desktop_info(current, total, names)?;
        }
        Ok(())
    }

    fn set_workarea(&mut self, areas: &[(i32, i32, u32, u32)]) -> Result<(), BackendError> {
        if let Some(facade) = self.ewmh_facade.as_ref() {
            facade.set_workarea(areas)?;
        }
        Ok(())
    }
    // [实现] 开始移动
    fn begin_move(&mut self, win: WindowId) -> Result<(), BackendError> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry) = self.input_ops.get_pointer_position()?;
        if Self::debug_drag_enabled() {
            log::info!(
                "[drag] begin_move win={:?} geom={:?} pointer=({:.1},{:.1})",
                win,
                geom,
                rx,
                ry
            );
        }

        // 1. 设置光标
        self.cursor_provider.get(StdCursorKind::Hand)?; // 预加载
        self.input_ops.set_cursor(StdCursorKind::Hand)?;

        // 2. 抓取指针
        let mask = (EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION).bits();
        // X11 Cursor handle 是一层封装，这里需要解开
        let cursor_handle = self.cursor_provider.get(StdCursorKind::Hand)?.0;

        if self.input_ops.grab_pointer(mask, Some(cursor_handle))? {
            self.interaction = Some(X11Interaction {
                win,
                current_x: geom.x,
                current_y: geom.y,
                current_w: geom.w,
                current_h: geom.h,
                start_geom: geom,
                start_root_x: rx,
                start_root_y: ry,
                action: InteractionAction::Move,
            });
        } else if Self::debug_drag_enabled() {
            log::info!("[drag] begin_move grab_pointer failed win={:?}", win);
        }
        Ok(())
    }

    // [实现] 开始调整大小
    fn begin_resize(&mut self, win: WindowId, edge: ResizeEdge) -> Result<(), BackendError> {
        let geom = self.window_ops.get_geometry(win)?;
        let (rx, ry) = self.input_ops.get_pointer_position()?;

        if Self::debug_drag_enabled() {
            log::info!(
                "[drag] begin_resize win={:?} edge={:?} geom={:?}",
                win,
                edge,
                geom
            );
        }

        // Do not warp pointer: resizemouse already picked an edge based on current cursor.
        let cursor_kind = match edge {
            ResizeEdge::Top | ResizeEdge::Bottom => StdCursorKind::VDoubleArrow,
            ResizeEdge::Left | ResizeEdge::Right => StdCursorKind::HDoubleArrow,
            ResizeEdge::TopLeft => StdCursorKind::TopLeftCorner,
            ResizeEdge::TopRight => StdCursorKind::TopRightCorner,
            ResizeEdge::BottomLeft => StdCursorKind::BottomLeftCorner,
            ResizeEdge::BottomRight => StdCursorKind::BottomRightCorner,
        };

        self.cursor_provider.get(cursor_kind)?; // 预加载
        self.input_ops.set_cursor(cursor_kind)?;
        let cursor_handle = self.cursor_provider.get(cursor_kind)?.0;
        let mask = (EventMaskBits::BUTTON_RELEASE | EventMaskBits::POINTER_MOTION).bits();

        if self.input_ops.grab_pointer(mask, Some(cursor_handle))? {
            self.interaction = Some(X11Interaction {
                win,
                current_x: geom.x,
                current_y: geom.y,
                current_w: geom.w,
                current_h: geom.h,
                start_geom: geom,
                start_root_x: rx,
                start_root_y: ry,
                action: InteractionAction::Resize(edge),
            });
        } else if Self::debug_drag_enabled() {
            log::info!("[drag] begin_resize grab_pointer failed win={:?}", win);
        }
        Ok(())
    }

    // [实现] 处理 Motion
    fn handle_motion(&mut self, x: f64, y: f64, _time: u32) -> Result<bool, BackendError> {
        if let Some(ref mut state) = self.interaction {
            let dx = (x - state.start_root_x) as i32;
            let dy = (y - state.start_root_y) as i32;

            match state.action {
                InteractionAction::Move => {
                    let new_x = state.start_geom.x + dx;
                    let new_y = state.start_geom.y + dy;
                    if Self::debug_drag_enabled() {
                        log::debug!(
                            "[drag] motion(move) win={:?} start=({},{}) dxdy=({},{}) -> pos=({},{}) keep_size=({}x{})",
                            state.win,
                            state.start_geom.x,
                            state.start_geom.y,
                            dx,
                            dy,
                            new_x,
                            new_y,
                            state.start_geom.w,
                            state.start_geom.h
                        );
                    }
                    state.current_x = new_x;
                    state.current_y = new_y;
                    self.window_ops.set_position(state.win, new_x, new_y)?;
                }
                InteractionAction::Resize(_) => {
                    let new_w = (state.start_geom.w as i32 + dx).max(1) as u32;
                    let new_h = (state.start_geom.h as i32 + dy).max(1) as u32;

                    if Self::debug_drag_enabled() {
                        log::debug!(
                            "[drag] motion(resize) win={:?} start_size=({}x{}) dxdy=({},{}) -> size=({}x{}) pos=({},{}) border={}",
                            state.win,
                            state.start_geom.w,
                            state.start_geom.h,
                            dx,
                            dy,
                            new_w,
                            new_h,
                            state.start_geom.x,
                            state.start_geom.y,
                            state.start_geom.border
                        );
                    }

                    state.current_w = new_w;
                    state.current_h = new_h;
                    self.window_ops.configure(
                        state.win,
                        state.start_geom.x,
                        state.start_geom.y,
                        new_w,
                        new_h,
                        state.start_geom.border,
                    )?;
                }
            }
            // 告诉 Jwm 这个事件被处理了
            return Ok(true);
        }
        Ok(false)
    }

    fn interaction_geometry(&self) -> Option<(WindowId, i32, i32, u32, u32)> {
        let state = self.interaction.as_ref()?;
        Some((
            state.win,
            state.current_x,
            state.current_y,
            state.current_w,
            state.current_h,
        ))
    }

    // [实现] 处理 ButtonRelease
    fn handle_button_release(&mut self, _time: u32) -> Result<bool, BackendError> {
        if self.interaction.is_some() {
            if Self::debug_drag_enabled() {
                if let Some(state) = self.interaction.as_ref() {
                    log::info!(
                        "[drag] end_interaction win={:?} action={:?}",
                        state.win,
                        state.action
                    );
                } else {
                    log::info!("[drag] end_interaction");
                }
            }
            self.input_ops.ungrab_pointer()?;
            self.input_ops.set_cursor(StdCursorKind::LeftPtr)?;
            self.interaction = None;
            return Ok(true);
        }
        Ok(false)
    }
    fn cursor_provider(&mut self) -> &mut dyn CursorProvider {
        &mut *self.cursor_provider
    }
    fn color_allocator(&mut self) -> &mut dyn ColorAllocator {
        &mut *self.color_allocator
    }

    fn run(&mut self, handler: &mut dyn EventHandler) -> Result<(), BackendError> {
        let mut event_loop: EventLoop<X11rbLoopData> = EventLoop::try_new()?;
        let loop_signal = event_loop.get_signal();
        self.compositor_loop_signal = Some(loop_signal.clone());
        if let Some(compositor) = self.compositor.as_ref() {
            compositor.set_waterlily_loop_signal(loop_signal);
        }
        let handle = event_loop.handle();

        // 1. 注册 X11 事件源
        let x11_source = if let Some(src) = self._init_event_source.take() {
            src
        } else {
            X11EventSource::new(
                self.conn.clone(),
                self.atoms.clone(),
                self.screen.root,
                self.compositor.as_ref().map(|c| c.overlay_window()),
                self.ids.clone(),
            )
        };

        handle
            .insert_source(x11_source, |event, _, data| {
                // Compositor hooks: track window lifecycle and damage
                data.backend.compositor_handle_event(&event);
                // Systray intercept: handle tray-related events internally
                if data.backend.systray_handle_event(&event) {
                    return;
                }
                let event = data.backend.enrich_event_with_output(event);
                if let Err(e) = data.handler.handle_event(data.backend, event) {
                    log::error!("Error handling X11 event: {:?}", e);
                }
            })
            .map_err(|e| BackendError::Message(format!("Failed to insert X11 source: {}", e)))?;

        // 2. 注册 Signals
        let signals = Signals::new(&[Signal::SIGCHLD])?;
        handle
            .insert_source(signals, |event, _, data| {
                if event.signal() == Signal::SIGCHLD {
                    if let Err(e) = data.handler.handle_event(
                        data.backend,
                        crate::backend::api::BackendEvent::ChildProcessExited,
                    ) {
                        log::error!("Error handling SIGCHLD: {:?}", e);
                    }
                }
            })
            .map_err(|e| BackendError::Message(format!("Failed to insert Signal source: {}", e)))?;

        // 3. 注册 Timer
        // Timer 绝对不是 Send/Sync 的，必须转 String
        let update_interval = Duration::from_millis(20);
        let timer = Timer::from_duration(update_interval);
        handle
            .insert_source(timer, move |_, _, data| {
                if let Err(e) = data.handler.update(data.backend) {
                    log::error!("Error in update loop: {:?}", e);
                }
                if data.handler.should_exit() {
                    data.should_exit = true;
                }
                TimeoutAction::ToDuration(update_interval)
            })
            .map_err(|e| BackendError::Message(format!("Failed to insert Timer source: {}", e)))?;

        // 4. 注册 inotify 配置文件监听
        //
        // 监听父目录而非配置文件本身:编辑器普遍以"写临时文件 + rename 覆盖"的
        // 原子保存方式落盘,这会使针对文件 inode 的 watch 收到 IN_IGNORED 而被
        // 内核自动移除,此后热重载静默失效。目录 inode 稳定,可持续捕获保存事件。
        // Generic 源拥有 Inotify(实现 AsFd),回调中用 read_events() 解析并按
        // 文件名过滤,避免目录内其它文件的无关事件。
        let setup_inotify = || -> Result<(), BackendError> {
            use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

            let config_path = crate::config::Config::get_default_config_path();
            let watch_dir = config_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| config_path.clone());
            let config_file_name = config_path.file_name().map(|n| n.to_os_string());

            let inotify = Inotify::init(InitFlags::IN_NONBLOCK)
                .map_err(|e| BackendError::Message(format!("Failed to init inotify: {}", e)))?;

            inotify
                .add_watch(
                    &watch_dir,
                    AddWatchFlags::IN_CLOSE_WRITE
                        | AddWatchFlags::IN_MOVED_TO
                        | AddWatchFlags::IN_CREATE,
                )
                .map_err(|e| {
                    BackendError::Message(format!(
                        "Failed to watch config dir {:?}: {}",
                        watch_dir, e
                    ))
                })?;

            handle
                .insert_source(
                    calloop::generic::Generic::new(
                        inotify,
                        calloop::Interest::READ,
                        calloop::Mode::Level,
                    ),
                    move |_, inotify, data| {
                        let events = inotify.read_events().unwrap_or_default();
                        // 仅当配置文件本身发生写入/移入时才触发重载。
                        let relevant =
                            events.iter().any(|ev| match (&config_file_name, &ev.name) {
                                (Some(want), Some(got)) => got == want,
                                // 无文件名(理论上不应出现在目录 watch)时保守地触发一次。
                                _ => true,
                            });
                        if relevant {
                            if let Err(e) = data.handler.handle_event(
                                data.backend,
                                crate::backend::api::BackendEvent::ConfigChanged,
                            ) {
                                log::error!("Error handling ConfigChanged: {:?}", e);
                            }
                        }
                        Ok(calloop::PostAction::Continue)
                    },
                )
                .map_err(|e| {
                    BackendError::Message(format!("Failed to insert inotify source: {}", e))
                })?;

            Ok(())
        };

        if let Err(e) = setup_inotify() {
            log::warn!(
                "Failed to set up config file watching: {}. Falling back to polling.",
                e
            );
        } else {
            log::info!("Config file hot-reload enabled via inotify");
        }

        // 5. 运行事件循环
        let mut loop_data = X11rbLoopData {
            backend: self,
            handler,
            should_exit: false,
        };
        loop {
            // When animations or overview are active, use a very short timeout so
            // the event loop doesn't block between frames.  With vsync-enabled
            // glXSwapBuffers (swap interval=1) the ~16.6ms vblank wait already
            // provides natural frame pacing; we just need dispatch to return
            // promptly after the swap completes so we can start the next frame.
            // Without this, dispatch(None) only wakes on the 20ms calloop timer,
            // which drifts against the vblank period and produces severe stutter
            // (the exact symptom: smooth when mouse moves, choppy when still).
            let timeout =
                if loop_data.handler.needs_tick() || loop_data.backend.compositor_needs_render() {
                    Some(Duration::from_millis(1))
                } else {
                    None
                };
            event_loop
                .dispatch(timeout, &mut loop_data)
                .map_err(|e| BackendError::Other(Box::new(e)))?;

            // Immediate compositor render: after processing X events (including
            // DamageNotify), render without waiting for the 20ms timer.
            // This dramatically reduces visual latency for rapidly-updating
            // overlay windows (e.g. flameshot screenshot selection).
            if !loop_data.should_exit {
                loop_data
                    .handler
                    .render_compositor_immediate(loop_data.backend);
            }

            // Benchmark auto-exit: check if benchmark completed
            if loop_data.backend.benchmark_auto_exit {
                if loop_data.backend.compositor_benchmark_is_complete() {
                    if let Some(report) = loop_data.backend.compositor_benchmark_report() {
                        println!("{}", report);
                    }
                    loop_data.should_exit = true;
                }
            }

            if loop_data.should_exit {
                break;
            }
        }

        Ok(())
    }
}

mod ids {
    use crate::backend::common_define::WindowId;
    use crate::backend::error::BackendError;
    use crate::sync_ext::RwLockExt;
    use std::collections::HashMap;
    use std::sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    };

    #[derive(Clone, Default)]
    pub(super) struct X11IdRegistry {
        next: Arc<AtomicU64>,
        x11_to_wid: Arc<RwLock<HashMap<u32, WindowId>>>,
        wid_to_x11: Arc<RwLock<HashMap<WindowId, u32>>>,
    }

    impl X11IdRegistry {
        pub(super) fn new(start: u64) -> Self {
            Self {
                next: Arc::new(AtomicU64::new(start)),
                x11_to_wid: Arc::new(RwLock::new(HashMap::new())),
                wid_to_x11: Arc::new(RwLock::new(HashMap::new())),
            }
        }

        /// X11 window(u32) intern  WindowId
        pub(super) fn intern(&self, x11: u32) -> WindowId {
            if let Some(id) = self.x11_to_wid.read_safe().get(&x11).copied() {
                return id;
            }
            // 
            let mut w = self.x11_to_wid.write_safe();
            if let Some(id) = w.get(&x11).copied() {
                return id;
            }

            let id = WindowId::from_raw(self.next.fetch_add(1, Ordering::Relaxed));
            w.insert(x11, id);
            self.wid_to_x11.write_safe().insert(id, x11);
            id
        }

        pub(super) fn x11(&self, id: WindowId) -> Result<u32, BackendError> {
            self.wid_to_x11
                .read_safe()
                .get(&id)
                .copied()
                .ok_or(BackendError::NotFound("WindowId not mapped to X11 window"))
        }

        pub(super) fn remove_x11(&self, x11: u32) {
            if let Some(id) = self.x11_to_wid.write_safe().remove(&x11) {
                self.wid_to_x11.write_safe().remove(&id);
            }
        }

        /// Return a snapshot of all known (x11_window, WindowId) pairs.
        pub(super) fn all_x11_windows(&self) -> Vec<(u32, WindowId)> {
            self.x11_to_wid
                .read_safe()
                .iter()
                .map(|(&x, &w)| (x, w))
                .collect()
        }
    }
}

mod adapter {
    use crate::backend::common_define::MouseButton;
    use crate::backend::common_define::{EventMaskBits, Mods};
    use x11rb::protocol::xproto::{ButtonIndex, EventMask, KeyButMask};

    pub(super) fn mods_from_x11(mask: KeyButMask, numlock_mask: KeyButMask) -> Mods {
        let mut m = Mods::empty();
        let raw = mask.bits();

        if raw & KeyButMask::SHIFT.bits() != 0 {
            m |= Mods::SHIFT;
        }
        if raw & KeyButMask::CONTROL.bits() != 0 {
            m |= Mods::CONTROL;
        }
        if raw & KeyButMask::MOD1.bits() != 0 {
            m |= Mods::ALT;
        }
        if raw & KeyButMask::MOD2.bits() != 0 {
            // If NumLock is mapped to Mod2, don't treat it as a regular modifier.
            if !numlock_mask.contains(KeyButMask::MOD2) {
                m |= Mods::MOD2;
            }
        }
        if raw & KeyButMask::MOD3.bits() != 0 {
            if !numlock_mask.contains(KeyButMask::MOD3) {
                m |= Mods::MOD3;
            }
        }
        if raw & KeyButMask::MOD4.bits() != 0 {
            m |= Mods::SUPER;
        }
        if raw & KeyButMask::MOD5.bits() != 0 {
            if !numlock_mask.contains(KeyButMask::MOD5) {
                m |= Mods::MOD5;
            }
        }
        if raw & KeyButMask::LOCK.bits() != 0 {
            m |= Mods::CAPS;
        }
        if raw & numlock_mask.bits() != 0 {
            m |= Mods::NUMLOCK;
        }
        m
    }

    pub(super) fn mods_to_x11(mods: Mods, numlock_mask: KeyButMask) -> KeyButMask {
        let mut m = KeyButMask::default();
        if mods.contains(Mods::SHIFT) {
            m |= KeyButMask::SHIFT;
        }
        if mods.contains(Mods::CONTROL) {
            m |= KeyButMask::CONTROL;
        }
        if mods.contains(Mods::ALT) {
            m |= KeyButMask::MOD1;
        }
        if mods.contains(Mods::MOD2) {
            m |= KeyButMask::MOD2;
        }
        if mods.contains(Mods::MOD3) {
            m |= KeyButMask::MOD3;
        }
        if mods.contains(Mods::SUPER) {
            m |= KeyButMask::MOD4;
        }
        if mods.contains(Mods::MOD5) {
            m |= KeyButMask::MOD5;
        }
        if mods.contains(Mods::CAPS) {
            m |= KeyButMask::LOCK;
        }
        if mods.contains(Mods::NUMLOCK) {
            m |= numlock_mask;
        }
        m
    }

    #[allow(dead_code)]
    pub(super) fn button_from_x11(detail: u8) -> MouseButton {
        MouseButton::from_u8(detail)
    }

    #[allow(dead_code)]
    pub(super) fn button_to_x11(btn: MouseButton) -> ButtonIndex {
        ButtonIndex::from(btn.to_u8())
    }

    pub(super) fn event_mask_from_generic(bits: u32) -> EventMask {
        let mut m = EventMask::default();
        if (bits & EventMaskBits::BUTTON_PRESS.bits()) != 0 {
            m |= EventMask::BUTTON_PRESS;
        }
        if (bits & EventMaskBits::BUTTON_RELEASE.bits()) != 0 {
            m |= EventMask::BUTTON_RELEASE;
        }
        if (bits & EventMaskBits::POINTER_MOTION.bits()) != 0 {
            m |= EventMask::POINTER_MOTION;
        }
        if (bits & EventMaskBits::ENTER_WINDOW.bits()) != 0 {
            m |= EventMask::ENTER_WINDOW;
        }
        if (bits & EventMaskBits::LEAVE_WINDOW.bits()) != 0 {
            m |= EventMask::LEAVE_WINDOW;
        }
        if (bits & EventMaskBits::PROPERTY_CHANGE.bits()) != 0 {
            m |= EventMask::PROPERTY_CHANGE;
        }
        if (bits & EventMaskBits::STRUCTURE_NOTIFY.bits()) != 0 {
            m |= EventMask::STRUCTURE_NOTIFY;
        }
        if (bits & EventMaskBits::SUBSTRUCTURE_REDIRECT.bits()) != 0 {
            m |= EventMask::SUBSTRUCTURE_REDIRECT;
        }
        if (bits & EventMaskBits::FOCUS_CHANGE.bits()) != 0 {
            m |= EventMask::FOCUS_CHANGE;
        }
        if (bits & EventMaskBits::SUBSTRUCTURE_NOTIFY.bits()) != 0 {
            m |= EventMask::SUBSTRUCTURE_NOTIFY;
        }
        if (bits & EventMaskBits::KEY_RELEASE.bits()) != 0 {
            m |= EventMask::KEY_RELEASE;
        }
        m
    }
}

mod color {
    use crate::backend::api::ColorAllocator;
    use crate::backend::common_define::{ArgbColor, ColorScheme, Pixel, SchemeType};
    use crate::backend::error::BackendError;
    use std::collections::HashMap;
    use std::sync::Arc;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::Colormap;

    pub(super) struct X11ColorAllocator<C: Connection> {
        conn: Arc<C>,
        colormap: Colormap,

        pixel_cache: HashMap<u32, Pixel>,
        schemes: HashMap<SchemeType, ColorScheme>,
    }

    impl<C: Connection> X11ColorAllocator<C> {
        pub(super) fn new(conn: Arc<C>, colormap: Colormap) -> Self {
            Self {
                conn,
                colormap,
                pixel_cache: HashMap::new(),
                schemes: HashMap::new(),
            }
        }

        fn ensure_pixel(&mut self, color: ArgbColor) -> Result<Pixel, BackendError> {
            if let Some(p) = self.pixel_cache.get(&color.value).copied() {
                return Ok(p);
            }
            let (_, r, g, b) = color.components();
            let pix = self.alloc_rgb(r, g, b)?;
            self.pixel_cache.insert(color.value, pix);
            Ok(pix)
        }

        fn alloc_rgb(&mut self, r: u8, g: u8, b: u8) -> Result<Pixel, BackendError> {
            use x11rb::protocol::xproto::ConnectionExt;
            let reply = (*self.conn)
                .alloc_color(
                    self.colormap,
                    (r as u16) << 8,
                    (g as u16) << 8,
                    (b as u16) << 8,
                )?
                .reply()?;
            Ok(Pixel(reply.pixel))
        }

        fn free_pixels(&mut self, pixels: &[Pixel]) -> Result<(), BackendError> {
            if pixels.is_empty() {
                return Ok(());
            }
            use x11rb::protocol::xproto::ConnectionExt;
            let raw: Vec<u32> = pixels.iter().map(|p| p.0).collect();
            (*self.conn).free_colors(self.colormap, 0, &raw)?;
            Ok(())
        }
    }

    impl<C: Connection + Send + Sync + 'static> ColorAllocator for X11ColorAllocator<C> {
        fn set_scheme(&mut self, t: SchemeType, s: ColorScheme) {
            self.schemes.insert(t, s);
        }

        fn get_border_pixel_of(&mut self, t: SchemeType) -> Result<Pixel, BackendError> {
            let s = self
                .schemes
                .get(&t)
                .ok_or(BackendError::NotFound("scheme not found"))?
                .clone();
            self.ensure_pixel(s.border)
        }

        fn allocate_schemes_pixels(&mut self) -> Result<(), BackendError> {
            let mut colors: Vec<ArgbColor> = Vec::new();
            for s in self.schemes.values() {
                colors.push(s.fg);
                colors.push(s.bg);
                colors.push(s.border);
            }
            colors.sort_by_key(|c| c.value);
            colors.dedup();
            for c in colors {
                let _ = self.ensure_pixel(c)?;
            }
            Ok(())
        }

        fn free_all_theme_pixels(&mut self) -> Result<(), BackendError> {
            if self.pixel_cache.is_empty() {
                return Ok(());
            }
            let pixels: Vec<Pixel> = self.pixel_cache.values().copied().collect();
            self.free_pixels(&pixels)?;
            self.pixel_cache.clear();
            Ok(())
        }
    }
}

mod cursor {
    use super::ids::X11IdRegistry;
    use crate::backend::api::CursorProvider;
    use crate::backend::common_define::{CursorHandle, StdCursorKind, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::xcursor_theme::{ResolvedCursor, XcursorImages};
    use std::collections::HashMap;
    use std::sync::Arc;
    use x11rb::connection::Connection;
    use x11rb::protocol::render::{
        self, ConnectionExt as RenderConnectionExt, PictType, Pictformat,
    };
    use x11rb::protocol::xproto::*;

    #[allow(dead_code)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) enum X11StdCursor {
        XCursor = 0,
        Arrow = 2,
        BasedArrowDown = 4,
        BasedArrowUp = 6,
        Boat = 8,
        Bogosity = 10,
        BottomLeftCorner = 12,
        BottomRightCorner = 14,
        BottomSide = 16,
        BottomTee = 18,
        BoxSpiral = 20,
        CenterPtr = 22,
        Circle = 24,
        Clock = 26,
        CoffeeMug = 28,
        Cross = 30,
        CrossReverse = 32,
        Crosshair = 34,
        DiamondCross = 36,
        Dot = 38,
        Dotbox = 40,
        DoubleArrow = 42,
        DraftLarge = 44,
        DraftSmall = 46,
        DrapedBox = 48,
        Exchange = 50,
        Fleur = 52,
        Gobbler = 54,
        Gumby = 56,
        Hand1 = 58,
        Hand2 = 60,
        Heart = 62,
        Icon = 64,
        IronCross = 66,
        LeftPtr = 68,
        LeftSide = 70,
        LeftTee = 72,
        Leftbutton = 74,
        LlAngle = 76,
        LrAngle = 78,
        Man = 80,
        Middlebutton = 82,
        Mouse = 84,
        Pencil = 86,
        Pirate = 88,
        Plus = 90,
        QuestionArrow = 92,
        RightPtr = 94,
        RightSide = 96,
        RightTee = 98,
        Rightbutton = 100,
        RtlLogo = 102,
        Sailboat = 104,
        SbDownArrow = 106,
        SbHDoubleArrow = 108,
        SbLeftArrow = 110,
        SbRightArrow = 112,
        SbUpArrow = 114,
        SbVDoubleArrow = 116,
        Shuttle = 118,
        Sizing = 120,
        Spider = 122,
        Spraycan = 124,
        Star = 126,
        Target = 128,
        Tcross = 130,
        TopLeftArrow = 132,
        TopLeftCorner = 134,
        TopRightCorner = 136,
        TopSide = 138,
        TopTee = 140,
        Trek = 142,
        UlAngle = 144,
        Umbrella = 146,
        UrAngle = 148,
        Watch = 150,
        Xterm = 152,
    }

    impl X11StdCursor {
        pub(super) fn create(
            &self,
            conn: &impl Connection,
            font: Font,
        ) -> Result<Cursor, BackendError> {
            let cursor_id = conn.generate_id()?;
            let glyph = *self as u16;
            conn.create_glyph_cursor(
                cursor_id,
                font,
                font,
                glyph,
                glyph + 1,
                0,
                0,
                0, // 
                65535,
                65535,
                65535, // 
            )?;
            Ok(cursor_id)
        }

        #[allow(dead_code)]
        pub(super) fn create_colored(
            &self,
            conn: &impl Connection,
            font: Font,
            fg_r: u16,
            fg_g: u16,
            fg_b: u16,
            bg_r: u16,
            bg_g: u16,
            bg_b: u16,
        ) -> Result<Cursor, BackendError> {
            let cursor_id = conn.generate_id()?;
            let glyph = *self as u16;
            conn.create_glyph_cursor(
                cursor_id,
                font,
                font,
                glyph,
                glyph + 1,
                fg_r,
                fg_g,
                fg_b,
                bg_r,
                bg_g,
                bg_b,
            )?;
            Ok(cursor_id)
        }

        #[allow(dead_code)]
        pub(super) fn description(&self) -> &'static str {
            match self {
                Self::XCursor => "Default X cursor",
                Self::Arrow => "Standard arrow",
                Self::BasedArrowDown => "Down arrow",
                Self::BasedArrowUp => "Up arrow",
                Self::Boat => "Boat shape",
                Self::Bogosity => "Error/invalid indicator",
                Self::BottomLeftCorner => "Bottom-left corner resize",
                Self::BottomRightCorner => "Bottom-right corner resize",
                Self::BottomSide => "Bottom side resize",
                Self::BottomTee => "Bottom T shape",
                Self::BoxSpiral => "Box spiral",
                Self::CenterPtr => "Center pointer",
                Self::Circle => "Circle",
                Self::Clock => "Clock/waiting",
                Self::CoffeeMug => "Coffee mug",
                Self::Cross => "Cross",
                Self::CrossReverse => "Reverse cross",
                Self::Crosshair => "Crosshair",
                Self::DiamondCross => "Diamond cross",
                Self::Dot => "Dot",
                Self::Dotbox => "Dotted box",
                Self::DoubleArrow => "Double arrow",
                Self::DraftLarge => "Large draft",
                Self::DraftSmall => "Small draft",
                Self::DrapedBox => "Draped box",
                Self::Exchange => "Exchange",
                Self::Fleur => "Four-way move",
                Self::Gobbler => "Pac-man",
                Self::Gumby => "Gumby character",
                Self::Hand1 => "Hand pointer 1",
                Self::Hand2 => "Hand pointer 2",
                Self::Heart => "Heart shape",
                Self::Icon => "Icon",
                Self::IronCross => "Iron cross",
                Self::LeftPtr => "Left pointer (standard)",
                Self::LeftSide => "Left side resize",
                Self::LeftTee => "Left T shape",
                Self::Leftbutton => "Left button",
                Self::LlAngle => "Lower-left angle",
                Self::LrAngle => "Lower-right angle",
                Self::Man => "Man figure",
                Self::Middlebutton => "Middle button",
                Self::Mouse => "Mouse",
                Self::Pencil => "Pencil",
                Self::Pirate => "Pirate",
                Self::Plus => "Plus sign",
                Self::QuestionArrow => "Question arrow",
                Self::RightPtr => "Right pointer",
                Self::RightSide => "Right side resize",
                Self::RightTee => "Right T shape",
                Self::Rightbutton => "Right button",
                Self::RtlLogo => "RTL logo",
                Self::Sailboat => "Sailboat",
                Self::SbDownArrow => "Scrollbar down arrow",
                Self::SbHDoubleArrow => "Horizontal double arrow",
                Self::SbLeftArrow => "Scrollbar left arrow",
                Self::SbRightArrow => "Scrollbar right arrow",
                Self::SbUpArrow => "Scrollbar up arrow",
                Self::SbVDoubleArrow => "Vertical double arrow",
                Self::Shuttle => "Shuttle",
                Self::Sizing => "Sizing",
                Self::Spider => "Spider",
                Self::Spraycan => "Spray can",
                Self::Star => "Star",
                Self::Target => "Target",
                Self::Tcross => "T cross",
                Self::TopLeftArrow => "Top-left arrow",
                Self::TopLeftCorner => "Top-left corner resize",
                Self::TopRightCorner => "Top-right corner resize",
                Self::TopSide => "Top side resize",
                Self::TopTee => "Top T shape",
                Self::Trek => "Star Trek",
                Self::UlAngle => "Upper-left angle",
                Self::Umbrella => "Umbrella",
                Self::UrAngle => "Upper-right angle",
                Self::Watch => "Watch/waiting",
                Self::Xterm => "Text cursor",
            }
        }

        #[allow(dead_code)]
        pub(super) fn common_cursors() -> &'static [X11StdCursor] {
            &[
                Self::LeftPtr,           // 
                Self::Hand1,             // 
                Self::Xterm,             // 
                Self::Watch,             // 
                Self::Crosshair,         // 
                Self::Fleur,             // 
                Self::SbHDoubleArrow,    // 
                Self::SbVDoubleArrow,    // 
                Self::TopLeftCorner,     // 
                Self::TopRightCorner,    // 
                Self::BottomLeftCorner,  // 
                Self::BottomRightCorner, // 
                Self::Sizing,            // 
            ]
        }

        #[allow(dead_code)]
        pub(super) fn all_cursors() -> &'static [X11StdCursor] {
            &[
                Self::XCursor,
                Self::Arrow,
                Self::BasedArrowDown,
                Self::BasedArrowUp,
                Self::Boat,
                Self::Bogosity,
                Self::BottomLeftCorner,
                Self::BottomRightCorner,
                Self::BottomSide,
                Self::BottomTee,
                Self::BoxSpiral,
                Self::CenterPtr,
                Self::Circle,
                Self::Clock,
                Self::CoffeeMug,
                Self::Cross,
                Self::CrossReverse,
                Self::Crosshair,
                Self::DiamondCross,
                Self::Dot,
                Self::Dotbox,
                Self::DoubleArrow,
                Self::DraftLarge,
                Self::DraftSmall,
                Self::DrapedBox,
                Self::Exchange,
                Self::Fleur,
                Self::Gobbler,
                Self::Gumby,
                Self::Hand1,
                Self::Hand2,
                Self::Heart,
                Self::Icon,
                Self::IronCross,
                Self::LeftPtr,
                Self::LeftSide,
                Self::LeftTee,
                Self::Leftbutton,
                Self::LlAngle,
                Self::LrAngle,
                Self::Man,
                Self::Middlebutton,
                Self::Mouse,
                Self::Pencil,
                Self::Pirate,
                Self::Plus,
                Self::QuestionArrow,
                Self::RightPtr,
                Self::RightSide,
                Self::RightTee,
                Self::Rightbutton,
                Self::RtlLogo,
                Self::Sailboat,
                Self::SbDownArrow,
                Self::SbHDoubleArrow,
                Self::SbLeftArrow,
                Self::SbRightArrow,
                Self::SbUpArrow,
                Self::SbVDoubleArrow,
                Self::Shuttle,
                Self::Sizing,
                Self::Spider,
                Self::Spraycan,
                Self::Star,
                Self::Target,
                Self::Tcross,
                Self::TopLeftArrow,
                Self::TopLeftCorner,
                Self::TopRightCorner,
                Self::TopSide,
                Self::TopTee,
                Self::Trek,
                Self::UlAngle,
                Self::Umbrella,
                Self::UrAngle,
                Self::Watch,
                Self::Xterm,
            ]
        }
    }

    pub(super) struct X11CursorProvider<C: Connection> {
        conn: Arc<C>,
        /// A drawable on the target screen used to allocate the ARGB pixmaps.
        root: u32,
        /// Legacy core "cursor" font, used as a fallback when the Xcursor theme
        /// has no image for a given kind (or RENDER is unavailable).
        cursor_font: Font,
        /// XRENDER a8r8g8b8 picture format id; `None` disables themed cursors.
        argb_format: Option<Pictformat>,
        /// Whether the server's image byte order is LSB-first (the common case).
        lsb_first: bool,
        /// Live Xcursor theme/size loader shared with the other backends.
        images: XcursorImages,
        cache: HashMap<StdCursorKind, Cursor>,
        ids: X11IdRegistry,
    }

    impl<C: Connection> X11CursorProvider<C> {
        pub(super) fn new(
            conn: Arc<C>,
            root: u32,
            ids: X11IdRegistry,
        ) -> Result<Self, BackendError> {
            use x11rb::protocol::xproto::ConnectionExt;
            let font = conn.generate_id()?;
            conn.open_font(font, b"cursor")?;
            let lsb_first = conn.setup().image_byte_order == ImageOrder::LSB_FIRST;
            let argb_format = Self::query_argb_format(&*conn);
            if argb_format.is_none() {
                log::info!(
                    "[cursor] XRENDER ARGB32 format unavailable; using legacy glyph cursors"
                );
            }
            let images = XcursorImages::from_config();
            log::info!(
                "[cursor] x11rb theme={:?} size={}px",
                images.theme_name(),
                images.size()
            );
            Ok(Self {
                conn,
                root,
                cursor_font: font,
                argb_format,
                lsb_first,
                images,
                cache: HashMap::new(),
                ids,
            })
        }

        /// Locate the standard a8r8g8b8 (premultiplied) picture format, if the
        /// RENDER extension is present.
        fn query_argb_format(conn: &C) -> Option<Pictformat> {
            let reply = conn.render_query_pict_formats().ok()?.reply().ok()?;
            reply
                .formats
                .iter()
                .find(|f| {
                    f.type_ == PictType::DIRECT
                        && f.depth == 32
                        && f.direct.alpha_mask == 0xff
                        && f.direct.alpha_shift == 24
                        && f.direct.red_shift == 16
                        && f.direct.green_shift == 8
                        && f.direct.blue_shift == 0
                })
                .map(|f| f.id)
        }

        /// Build a themed ARGB cursor for `kind`, or `None` to fall back to the
        /// glyph font (no RENDER, no theme image, or an upload error).
        fn build_themed_cursor(&mut self, kind: StdCursorKind) -> Option<Cursor> {
            let format = self.argb_format?;
            let img = self.images.resolve(kind, 1)?;
            match self.upload_cursor(format, &img) {
                Ok(cursor) => Some(cursor),
                Err(e) => {
                    log::warn!(
                        "[cursor] themed upload failed for {kind:?}: {e}; using glyph fallback"
                    );
                    None
                }
            }
        }

        /// Upload a resolved ARGB image as an XRENDER cursor and return its id.
        fn upload_cursor(
            &self,
            format: Pictformat,
            img: &ResolvedCursor,
        ) -> Result<Cursor, BackendError> {
            use x11rb::protocol::xproto::ConnectionExt;
            let conn = &*self.conn;
            let w = img.width as u16;
            let h = img.height as u16;

            // The image is little-endian ARGB ([B,G,R,A]); on an MSB-first
            // server each pixel must be byte-reversed to [A,R,G,B].
            let data: std::borrow::Cow<'_, [u8]> = if self.lsb_first {
                std::borrow::Cow::Borrowed(&img.pixels_argb_le)
            } else {
                let mut swapped = img.pixels_argb_le.clone();
                for px in swapped.chunks_exact_mut(4) {
                    px.reverse();
                }
                std::borrow::Cow::Owned(swapped)
            };

            let pixmap = conn.generate_id()?;
            conn.create_pixmap(32, pixmap, self.root, w, h)?;

            let gc = conn.generate_id()?;
            conn.create_gc(gc, pixmap, &CreateGCAux::new())?;
            conn.put_image(ImageFormat::Z_PIXMAP, pixmap, gc, w, h, 0, 0, 0, 32, &data)?;
            conn.free_gc(gc)?;

            let picture = conn.generate_id()?;
            conn.render_create_picture(picture, pixmap, format, &render::CreatePictureAux::new())?;

            let cursor = conn.generate_id()?;
            conn.render_create_cursor(cursor, picture, img.xhot as u16, img.yhot as u16)?;

            conn.render_free_picture(picture)?;
            conn.free_pixmap(pixmap)?;
            Ok(cursor)
        }

        fn map_kind(kind: StdCursorKind) -> X11StdCursor {
            match kind {
                StdCursorKind::LeftPtr => X11StdCursor::LeftPtr,
                StdCursorKind::Hand => X11StdCursor::Hand1,
                StdCursorKind::XTerm => X11StdCursor::Xterm,
                StdCursorKind::Watch => X11StdCursor::Watch,
                StdCursorKind::Crosshair => X11StdCursor::Crosshair,
                StdCursorKind::Fleur => X11StdCursor::Fleur,
                StdCursorKind::HDoubleArrow => X11StdCursor::SbHDoubleArrow,
                StdCursorKind::VDoubleArrow => X11StdCursor::SbVDoubleArrow,
                StdCursorKind::TopLeftCorner => X11StdCursor::TopLeftCorner,
                StdCursorKind::TopRightCorner => X11StdCursor::TopRightCorner,
                StdCursorKind::BottomLeftCorner => X11StdCursor::BottomLeftCorner,
                StdCursorKind::BottomRightCorner => X11StdCursor::BottomRightCorner,
                StdCursorKind::Sizing => X11StdCursor::Sizing,
            }
        }
    }

    impl<C: Connection + Send + Sync + 'static> CursorProvider for X11CursorProvider<C> {
        fn preload_common(&mut self) -> Result<(), BackendError> {
            for kind in [
                StdCursorKind::LeftPtr,
                StdCursorKind::Hand,
                StdCursorKind::XTerm,
                StdCursorKind::Watch,
                StdCursorKind::Crosshair,
                StdCursorKind::Fleur,
                StdCursorKind::HDoubleArrow,
                StdCursorKind::VDoubleArrow,
                StdCursorKind::TopLeftCorner,
                StdCursorKind::TopRightCorner,
                StdCursorKind::BottomLeftCorner,
                StdCursorKind::BottomRightCorner,
                StdCursorKind::Sizing,
            ] {
                let _ = self.get(kind)?;
            }
            Ok(())
        }

        fn get(&mut self, kind: StdCursorKind) -> Result<CursorHandle, BackendError> {
            if let Some(&c) = self.cache.get(&kind) {
                return Ok(CursorHandle(c as u64));
            }
            let cursor = match self.build_themed_cursor(kind) {
                Some(c) => c,
                None => Self::map_kind(kind).create(&*self.conn, self.cursor_font)?,
            };
            self.cache.insert(kind, cursor);
            Ok(CursorHandle(cursor as u64))
        }

        fn reload_theme(&mut self) -> Result<bool, BackendError> {
            use x11rb::protocol::xproto::ConnectionExt;
            if !self.images.reload_from_config() {
                return Ok(false);
            }
            // Drop cursors built from the previous theme/size; they are rebuilt
            // lazily on the next get(). Freeing a cursor still installed on a
            // window is safe — the server keeps displaying it until replaced.
            for &cursor in self.cache.values() {
                let _ = (*self.conn).free_cursor(cursor);
            }
            self.cache.clear();
            log::info!(
                "[cursor] x11rb reloaded theme={:?} size={}px",
                self.images.theme_name(),
                self.images.size()
            );
            Ok(true)
        }

        fn apply(&mut self, window: WindowId, kind: StdCursorKind) -> Result<(), BackendError> {
            use x11rb::protocol::xproto::ConnectionExt;
            let c = match self.get(kind) {
                Ok(h) => h.0 as u32,
                Err(e) => return Err(e),
            };
            (*self.conn).change_window_attributes(
                self.ids.x11(window)?,
                &ChangeWindowAttributesAux::new().cursor(c),
            )?;
            Ok(())
        }

        fn cleanup(&mut self) -> Result<(), BackendError> {
            use x11rb::protocol::xproto::ConnectionExt;
            for &cursor in self.cache.values() {
                let _ = (*self.conn).free_cursor(cursor);
            }
            let _ = (*self.conn).close_font(self.cursor_font);
            Ok(())
        }
    }
}

mod event_source {
    use std::collections::VecDeque;
    use std::os::unix::io::{AsRawFd, BorrowedFd};
    use std::sync::Arc;
    use x11rb::connection::Connection;
    use x11rb::protocol::{Event as XEvent, xproto};
    use x11rb::rust_connection::RustConnection;

    use super::ids::X11IdRegistry;
    use crate::backend::api::{BackendEvent, NetWmState};
    use crate::backend::api::{HitTarget, NotifyMode};
    use crate::backend::error::BackendError;
    use crate::backend::x11::wm::{
        ClientMessageAtoms, ClientMessageKind, PropertyKindAtoms, classify_client_message,
        expand_net_wm_state_requests, net_wm_state_from_atom, property_kind_from_atom,
        stack_mode_from_index, window_changes_from_configure_request_parts,
    };
    use crate::backend::x11rb::Atoms;

    use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};

    pub(super) struct X11EventSource {
        conn: Arc<RustConnection>,
        atoms: Atoms,
        root_x11: u32,
        overlay_x11: Option<u32>,
        ids: X11IdRegistry,
        // Events produced beyond the single one returned by map_event (e.g. a
        // _NET_WM_STATE ClientMessage carrying two state atoms). Drained first.
        pending: VecDeque<BackendEvent>,
    }

    impl X11EventSource {
        pub(super) fn new(
            conn: Arc<RustConnection>,
            atoms: Atoms,
            root_x11: u32,
            overlay_x11: Option<u32>,
            ids: X11IdRegistry,
        ) -> Self {
            Self {
                conn,
                atoms,
                root_x11,
                overlay_x11,
                ids,
                pending: VecDeque::new(),
            }
        }

        fn hit_target_from_event_window(&self, event_window: u32) -> HitTarget {
            if event_window == self.root_x11 || self.overlay_x11 == Some(event_window) {
                HitTarget::Background { output: None }
            } else {
                HitTarget::Surface(self.ids.intern(event_window))
            }
        }

        fn hit_target_from_pointer_event(&self, event_window: u32, child: u32) -> HitTarget {
            if (event_window == self.root_x11 || self.overlay_x11 == Some(event_window))
                && child != 0
                && self.overlay_x11 != Some(child)
            {
                HitTarget::Surface(self.ids.intern(child))
            } else {
                self.hit_target_from_event_window(event_window)
            }
        }

        fn property_kind_atoms(&self) -> PropertyKindAtoms<u32> {
            PropertyKindAtoms {
                wm_transient_for: self.atoms.WM_TRANSIENT_FOR,
                wm_normal_hints: u32::from(xproto::AtomEnum::WM_NORMAL_HINTS),
                wm_hints: u32::from(xproto::AtomEnum::WM_HINTS),
                wm_name: u32::from(xproto::AtomEnum::WM_NAME),
                net_wm_name: self.atoms._NET_WM_NAME,
                wm_class: u32::from(xproto::AtomEnum::WM_CLASS),
                net_wm_window_type: self.atoms._NET_WM_WINDOW_TYPE,
                wm_protocols: self.atoms.WM_PROTOCOLS,
                net_wm_strut: self.atoms._NET_WM_STRUT,
                net_wm_strut_partial: self.atoms._NET_WM_STRUT_PARTIAL,
                motif_wm_hints: self.atoms._MOTIF_WM_HINTS,
                gtk_frame_extents: self.atoms._GTK_FRAME_EXTENTS,
                net_wm_bypass_compositor: self.atoms._NET_WM_BYPASS_COMPOSITOR,
                net_wm_opaque_region: self.atoms._NET_WM_OPAQUE_REGION,
                net_wm_icon: self.atoms._NET_WM_ICON,
                net_wm_user_time: self.atoms._NET_WM_USER_TIME,
            }
        }

        fn client_message_atoms(&self) -> ClientMessageAtoms<u32> {
            ClientMessageAtoms {
                net_wm_state: self.atoms._NET_WM_STATE,
                net_active_window: self.atoms._NET_ACTIVE_WINDOW,
                net_close_window: self.atoms._NET_CLOSE_WINDOW,
                net_wm_moveresize: self.atoms._NET_WM_MOVERESIZE,
                wm_protocols: self.atoms.WM_PROTOCOLS,
                net_wm_ping: self.atoms._NET_WM_PING,
            }
        }

        fn atom_to_net_wm_state(&self, atom: u32) -> Option<NetWmState> {
            net_wm_state_from_atom(atom, self.net_wm_state_atoms())
        }

        fn net_wm_state_atoms(&self) -> crate::backend::x11::wm::NetWmStateAtoms<u32> {
            crate::backend::x11::wm::NetWmStateAtoms {
                fullscreen: self.atoms._NET_WM_STATE_FULLSCREEN,
                maximized_vert: self.atoms._NET_WM_STATE_MAXIMIZED_VERT,
                maximized_horz: self.atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                hidden: self.atoms._NET_WM_STATE_HIDDEN,
                above: self.atoms._NET_WM_STATE_ABOVE,
                below: self.atoms._NET_WM_STATE_BELOW,
                demands_attention: self.atoms._NET_WM_STATE_DEMANDS_ATTENTION,
                sticky: self.atoms._NET_WM_STATE_STICKY,
                skip_taskbar: self.atoms._NET_WM_STATE_SKIP_TASKBAR,
                skip_pager: self.atoms._NET_WM_STATE_SKIP_PAGER,
            }
        }

        fn map_event(&mut self, ev: XEvent) -> Option<BackendEvent> {
            match ev {
                XEvent::ButtonPress(e) => {
                    log::debug!(
                        "[X11] ButtonPress: event=0x{:x} child=0x{:x} root=0x{:x} root_xy=({},{}) detail={}",
                        e.event,
                        e.child,
                        self.root_x11,
                        e.root_x,
                        e.root_y,
                        e.detail
                    );
                    Some(BackendEvent::ButtonPress {
                        target: self.hit_target_from_pointer_event(e.event, e.child),
                        state: e.state.bits(),
                        detail: e.detail,
                        time: e.time,
                        root_x: e.root_x as f64,
                        root_y: e.root_y as f64,
                    })
                }
                XEvent::MotionNotify(e) => Some(BackendEvent::MotionNotify {
                    target: self.hit_target_from_pointer_event(e.event, e.child),
                    root_x: e.root_x as f64,
                    root_y: e.root_y as f64,
                    time: e.time,
                }),
                XEvent::ButtonRelease(e) => Some(BackendEvent::ButtonRelease {
                    target: self.hit_target_from_pointer_event(e.event, e.child),
                    time: e.time,
                }),
                XEvent::RandrScreenChangeNotify(_) => Some(BackendEvent::ScreenLayoutChanged),
                XEvent::RandrNotify(_) => Some(BackendEvent::ScreenLayoutChanged),
                XEvent::KeyPress(e) => Some(BackendEvent::KeyPress {
                    keycode: e.detail,
                    state: e.state.bits(),
                    time: e.time,
                }),
                XEvent::KeyRelease(e) => Some(BackendEvent::KeyRelease {
                    keycode: e.detail,
                    state: e.state.bits(),
                    time: e.time,
                }),
                XEvent::MapRequest(e) => {
                    Some(BackendEvent::WindowCreated(self.ids.intern(e.window)))
                }
                XEvent::MapNotify(e) => Some(BackendEvent::WindowMapped(self.ids.intern(e.window))),
                XEvent::UnmapNotify(e) => {
                    Some(BackendEvent::WindowUnmapped(self.ids.intern(e.window)))
                }
                XEvent::DestroyNotify(e) => {
                    let id = self.ids.intern(e.window);
                    self.ids.remove_x11(e.window);
                    Some(BackendEvent::WindowDestroyed(id))
                }
                XEvent::ConfigureNotify(e) => Some(BackendEvent::WindowConfigured {
                    window: self.ids.intern(e.window),
                    x: e.x as i32,
                    y: e.y as i32,
                    width: e.width as u32,
                    height: e.height as u32,
                }),
                XEvent::EnterNotify(e) => {
                    let mode = match e.mode {
                        xproto::NotifyMode::NORMAL => NotifyMode::Normal,
                        xproto::NotifyMode::GRAB => NotifyMode::Grab,
                        xproto::NotifyMode::UNGRAB => NotifyMode::Ungrab,
                        _ => NotifyMode::Grab,
                    };
                    Some(BackendEvent::EnterNotify {
                        window: self.ids.intern(e.event),
                        subwindow: if e.child != 0 {
                            Some(self.ids.intern(e.child))
                        } else {
                            None
                        },
                        mode,
                        root_x: e.root_x as f64,
                        root_y: e.root_y as f64,
                    })
                }
                XEvent::LeaveNotify(e) => {
                    let mode = match e.mode {
                        xproto::NotifyMode::NORMAL => NotifyMode::Normal,
                        xproto::NotifyMode::GRAB => NotifyMode::Grab,
                        xproto::NotifyMode::UNGRAB => NotifyMode::Ungrab,
                        _ => NotifyMode::Grab,
                    };
                    Some(BackendEvent::LeaveNotify {
                        window: self.ids.intern(e.event),
                        mode,
                    })
                }
                XEvent::FocusIn(e) => Some(BackendEvent::FocusIn {
                    window: self.ids.intern(e.event),
                }),
                XEvent::FocusOut(e) => Some(BackendEvent::FocusOut {
                    window: self.ids.intern(e.event),
                }),
                XEvent::ConfigureRequest(e) => {
                    let changes = window_changes_from_configure_request_parts(
                        e.value_mask
                            .contains(xproto::ConfigWindow::X)
                            .then_some(e.x as i32),
                        e.value_mask
                            .contains(xproto::ConfigWindow::Y)
                            .then_some(e.y as i32),
                        e.value_mask
                            .contains(xproto::ConfigWindow::WIDTH)
                            .then_some(e.width as u32),
                        e.value_mask
                            .contains(xproto::ConfigWindow::HEIGHT)
                            .then_some(e.height as u32),
                        e.value_mask
                            .contains(xproto::ConfigWindow::BORDER_WIDTH)
                            .then_some(e.border_width as u32),
                        e.value_mask
                            .contains(xproto::ConfigWindow::SIBLING)
                            .then_some(self.ids.intern(e.sibling)),
                        e.value_mask
                            .contains(xproto::ConfigWindow::STACK_MODE)
                            .then(|| {
                                stack_mode_from_index(match e.stack_mode {
                                    xproto::StackMode::ABOVE => 0,
                                    xproto::StackMode::BELOW => 1,
                                    xproto::StackMode::TOP_IF => 2,
                                    xproto::StackMode::BOTTOM_IF => 3,
                                    xproto::StackMode::OPPOSITE => 4,
                                    _ => 255,
                                })
                            })
                            .flatten(),
                    );
                    Some(BackendEvent::ConfigureRequest {
                        window: self.ids.intern(e.window),
                        mask_bits: e.value_mask.bits(),
                        changes,
                    })
                }
                XEvent::PropertyNotify(e) => {
                    if e.state == xproto::Property::DELETE.into() {
                        return None;
                    }
                    let kind = property_kind_from_atom(e.atom, self.property_kind_atoms());
                    Some(BackendEvent::PropertyChanged {
                        window: self.ids.intern(e.window),
                        kind,
                    })
                }
                XEvent::ClientMessage(e) => {
                    let data32 = e.data.as_data32();
                    let data = [
                        data32.first().copied().unwrap_or(0),
                        data32.get(1).copied().unwrap_or(0),
                        data32.get(2).copied().unwrap_or(0),
                        data32.get(3).copied().unwrap_or(0),
                        data32.get(4).copied().unwrap_or(0),
                    ];
                    match classify_client_message(
                        e.type_,
                        e.format,
                        data,
                        self.client_message_atoms(),
                    ) {
                        ClientMessageKind::WindowState {
                            action,
                            first,
                            second,
                        } => {
                            let window = self.ids.intern(e.window);
                            let mut events = expand_net_wm_state_requests(
                                window,
                                action,
                                first,
                                second,
                                |atom| self.atom_to_net_wm_state(atom),
                            );
                            if events.is_empty() {
                                Some(BackendEvent::ClientMessage {
                                    window,
                                    type_: e.type_,
                                    data,
                                    format: e.format,
                                })
                            } else {
                                for event in events.drain(1..) {
                                    self.pending.push_back(event);
                                }
                                events.into_iter().next()
                            }
                        }
                        ClientMessageKind::ActiveWindow => {
                            Some(BackendEvent::ActiveWindowMessage {
                                window: self.ids.intern(e.window),
                            })
                        }
                        ClientMessageKind::CloseWindow => Some(BackendEvent::CloseWindowRequest {
                            window: self.ids.intern(e.window),
                        }),
                        ClientMessageKind::MoveResize { direction, button } => {
                            Some(BackendEvent::MoveResizeRequest {
                                window: self.ids.intern(e.window),
                                direction,
                                button,
                            })
                        }
                        ClientMessageKind::PingResponse { window } => {
                            Some(BackendEvent::PingResponse {
                                window: self.ids.intern(window),
                            })
                        }
                        ClientMessageKind::Other => Some(BackendEvent::ClientMessage {
                            window: self.ids.intern(e.window),
                            type_: e.type_,
                            data,
                            format: e.format,
                        }),
                    }
                }
                XEvent::MappingNotify(_) => Some(BackendEvent::MappingNotify),
                XEvent::Expose(e) => Some(BackendEvent::Expose {
                    window: self.ids.intern(e.window),
                }),
                XEvent::DamageNotify(e) => Some(BackendEvent::DamageNotify {
                    drawable: self.ids.intern(e.drawable),
                }),
                XEvent::PresentCompleteNotify(e) => Some(BackendEvent::PresentComplete {
                    window: self.ids.intern(e.window),
                    serial: e.serial,
                    msc: e.msc,
                    ust: e.ust,
                }),
                XEvent::PresentIdleNotify(e) => Some(BackendEvent::PresentIdle {
                    window: self.ids.intern(e.window),
                    serial: e.serial,
                    pixmap: e.pixmap,
                }),
                XEvent::ShapeNotify(e) => Some(BackendEvent::ShapeChanged {
                    window: self.ids.intern(e.affected_window),
                    shaped: e.shaped,
                }),
                XEvent::Unknown(_) => None,
                _ => None,
            }
        }

        pub(super) fn poll_event(
            &mut self,
        ) -> Result<Option<BackendEvent>, Box<dyn std::error::Error>> {
            if let Some(ev) = self.pending.pop_front() {
                return Ok(Some(ev));
            }
            let ev = self.conn.poll_for_event()?;
            Ok(ev.and_then(|e| self.map_event(e)))
        }
    }

    impl EventSource for X11EventSource {
        type Event = BackendEvent;
        type Metadata = ();
        type Ret = ();
        type Error = BackendError;

        fn process_events<F>(
            &mut self,
            _readiness: Readiness,
            _token: Token,
            mut callback: F,
        ) -> Result<PostAction, Self::Error>
        where
            F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
        {
            // Batch events and coalesce motion events for better performance
            let mut pending_motion: Option<BackendEvent> = None;

            loop {
                match self.poll_event() {
                    Ok(Some(event)) => {
                        match &event {
                            BackendEvent::MotionNotify { target, .. } => {
                                // Coalesce motion events - only keep the latest one.
                                // This dramatically reduces processing overhead during
                                // drags. But coalescing must not collapse motion across
                                // different targets: if the new event hits a different
                                // window/background than the one pending, flush the
                                // pending one first so its target isn't silently lost.
                                if let Some(BackendEvent::MotionNotify {
                                    target: prev_target,
                                    ..
                                }) = &pending_motion
                                {
                                    if prev_target != target {
                                        if let Some(m) = pending_motion.take() {
                                            callback(m, &mut ());
                                        }
                                    }
                                }
                                pending_motion = Some(event);
                            }
                            _ => {
                                // For non-motion events, first flush any pending motion
                                if let Some(m) = pending_motion.take() {
                                    callback(m, &mut ());
                                }
                                callback(event, &mut ());
                            }
                        }
                    }
                    Ok(None) => {
                        // No more events - flush any pending motion event
                        if let Some(m) = pending_motion.take() {
                            callback(m, &mut ());
                        }
                        break;
                    }
                    Err(e) => {
                        log::error!("X11 poll error: {:?}", e);
                        let err_msg = format!("X11 poll error: {}", e);
                        return Err(BackendError::from(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            err_msg,
                        )));
                    }
                }
            }
            Ok(PostAction::Continue)
        }

        fn register(
            &mut self,
            poll: &mut Poll,
            token_factory: &mut TokenFactory,
        ) -> calloop::Result<()> {
            let raw_fd = self.conn.stream().as_raw_fd();
            unsafe {
                let fd = BorrowedFd::borrow_raw(raw_fd);
                poll.register(fd, Interest::READ, Mode::Level, token_factory.token())
            }
        }

        fn reregister(
            &mut self,
            poll: &mut Poll,
            token_factory: &mut TokenFactory,
        ) -> calloop::Result<()> {
            let raw_fd = self.conn.stream().as_raw_fd();
            let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
            poll.reregister(fd, Interest::READ, Mode::Level, token_factory.token())
        }

        fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
            let raw_fd = self.conn.stream().as_raw_fd();
            let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
            poll.unregister(fd)
        }
    }
}

mod ewmh_facade {
    use super::ids::X11IdRegistry;
    use crate::backend::api::{EwmhFacade, EwmhFeature};
    use crate::backend::common_define::WindowId;
    use crate::backend::error::BackendError;
    use crate::backend::x11::wm::{EwmhFeatureAtoms, atom_for_ewmh_feature};
    use crate::backend::x11rb::Atoms;
    use std::sync::Arc;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::xproto::CreateWindowAux;
    use x11rb::protocol::xproto::*;
    use x11rb::protocol::xproto::{AtomEnum, PropMode};
    use x11rb::wrapper::ConnectionExt as _;

    pub(super) struct X11EwmhFacade<C: Connection> {
        conn: Arc<C>,
        root: WindowId,
        atoms: Atoms,
        ids: X11IdRegistry,
    }

    impl<C: Connection + Send + Sync + 'static> X11EwmhFacade<C> {
        pub(super) fn new(conn: Arc<C>, root: WindowId, atoms: Atoms, ids: X11IdRegistry) -> Self {
            Self {
                conn,
                root,
                atoms,
                ids,
            }
        }
        fn feature_to_atom(&self, f: EwmhFeature) -> u32 {
            atom_for_ewmh_feature(f, self.feature_atoms())
        }

        fn feature_atoms(&self) -> EwmhFeatureAtoms<u32> {
            EwmhFeatureAtoms {
                active_window: self.atoms._NET_ACTIVE_WINDOW,
                supported: self.atoms._NET_SUPPORTED,
                wm_name: self.atoms._NET_WM_NAME,
                wm_state: self.atoms._NET_WM_STATE,
                supporting_wm_check: self.atoms._NET_SUPPORTING_WM_CHECK,
                wm_state_fullscreen: self.atoms._NET_WM_STATE_FULLSCREEN,
                wm_state_maximized_vert: self.atoms._NET_WM_STATE_MAXIMIZED_VERT,
                wm_state_maximized_horz: self.atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                wm_state_hidden: self.atoms._NET_WM_STATE_HIDDEN,
                wm_state_above: self.atoms._NET_WM_STATE_ABOVE,
                wm_state_below: self.atoms._NET_WM_STATE_BELOW,
                wm_state_demands_attention: self.atoms._NET_WM_STATE_DEMANDS_ATTENTION,
                wm_state_sticky: self.atoms._NET_WM_STATE_STICKY,
                wm_state_skip_taskbar: self.atoms._NET_WM_STATE_SKIP_TASKBAR,
                wm_state_skip_pager: self.atoms._NET_WM_STATE_SKIP_PAGER,
                client_list: self.atoms._NET_CLIENT_LIST,
                client_info: self.atoms._NET_CLIENT_INFO,
                wm_window_type: self.atoms._NET_WM_WINDOW_TYPE,
                wm_window_type_dialog: self.atoms._NET_WM_WINDOW_TYPE_DIALOG,
                current_desktop: self.atoms._NET_CURRENT_DESKTOP,
                number_of_desktops: self.atoms._NET_NUMBER_OF_DESKTOPS,
                desktop_names: self.atoms._NET_DESKTOP_NAMES,
                desktop_viewport: self.atoms._NET_DESKTOP_VIEWPORT,
                wm_moveresize: self.atoms._NET_WM_MOVERESIZE,
                frame_extents: self.atoms._NET_FRAME_EXTENTS,
                wm_allowed_actions: self.atoms._NET_WM_ALLOWED_ACTIONS,
                workarea: self.atoms._NET_WORKAREA,
                close_window: self.atoms._NET_CLOSE_WINDOW,
                restack_window: self.atoms._NET_RESTACK_WINDOW,
                wm_ping: self.atoms._NET_WM_PING,
                wm_user_time: self.atoms._NET_WM_USER_TIME,
                wm_icon: self.atoms._NET_WM_ICON,
                wm_bypass_compositor: self.atoms._NET_WM_BYPASS_COMPOSITOR,
                wm_opaque_region: self.atoms._NET_WM_OPAQUE_REGION,
            }
        }
    }

    impl<C: Connection + Send + Sync + 'static> EwmhFacade for X11EwmhFacade<C> {
        fn declare_supported(&self, features: &[EwmhFeature]) -> Result<(), BackendError> {
            let atoms: Vec<u32> = features.iter().map(|f| self.feature_to_atom(*f)).collect();
            let r = self.ids.x11(self.root)?;
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_SUPPORTED,
                AtomEnum::ATOM,
                &atoms,
            )?;
            Ok(())
        }

        fn reset_root_properties(&self) -> Result<(), BackendError> {
            for &prop in [
                self.atoms._NET_ACTIVE_WINDOW,
                self.atoms._NET_CLIENT_LIST,
                self.atoms._NET_SUPPORTED,
                self.atoms._NET_CLIENT_LIST_STACKING,
                self.atoms._NET_SUPPORTING_WM_CHECK,
                self.atoms._NET_CURRENT_DESKTOP,
                self.atoms._NET_NUMBER_OF_DESKTOPS,
                self.atoms._NET_DESKTOP_NAMES,
                self.atoms._NET_DESKTOP_VIEWPORT,
            ]
            .iter()
            {
                let r = self.ids.x11(self.root)?;
                let _ = self.conn.delete_property(r, prop);
            }
            Ok(())
        }
        fn setup_supporting_wm_check(&self, wm_name: &str) -> Result<WindowId, BackendError> {
            let frame_win = self.conn.generate_id()?;
            let aux = CreateWindowAux::new()
                .event_mask(EventMask::EXPOSURE | EventMask::KEY_PRESS)
                .override_redirect(1);
            let r = self.ids.x11(self.root)?;
            self.conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                frame_win,
                r,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &aux,
            )?;
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_SUPPORTING_WM_CHECK,
                AtomEnum::WINDOW,
                &[frame_win],
            )?;
            self.conn.change_property32(
                PropMode::REPLACE,
                frame_win,
                self.atoms._NET_SUPPORTING_WM_CHECK,
                AtomEnum::WINDOW,
                &[frame_win],
            )?;
            // WM_NAME (legacy STRING)
            x11rb::wrapper::ConnectionExt::change_property8(
                &*self.conn,
                PropMode::REPLACE,
                frame_win,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                wm_name.as_bytes(),
            )?;
            // _NET_WM_NAME (UTF8) — EWMH-compliant pagers read the WM name from
            // the check window via this property, not the legacy WM_NAME.
            x11rb::wrapper::ConnectionExt::change_property8(
                &*self.conn,
                PropMode::REPLACE,
                frame_win,
                self.atoms._NET_WM_NAME,
                self.atoms.UTF8_STRING,
                wm_name.as_bytes(),
            )?;
            Ok(self.ids.intern(frame_win))
        }

        fn set_active_window(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let r = self.ids.x11(self.root)?;
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_ACTIVE_WINDOW,
                AtomEnum::WINDOW,
                &[w],
            )?;
            Ok(())
        }

        fn clear_active_window(&self) -> Result<(), BackendError> {
            use x11rb::protocol::xproto::ConnectionExt as RawExt;
            let r = self.ids.x11(self.root)?;
            self.conn
                .delete_property(r, self.atoms._NET_ACTIVE_WINDOW)?;
            Ok(())
        }

        fn set_client_list(&self, list: &[WindowId]) -> Result<(), BackendError> {
            let r = self.ids.x11(self.root)?;
            let raw: Vec<u32> = list.iter().filter_map(|&w| self.ids.x11(w).ok()).collect();
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_CLIENT_LIST,
                AtomEnum::WINDOW,
                &raw,
            )?;
            Ok(())
        }

        fn set_client_list_stacking(&self, list: &[WindowId]) -> Result<(), BackendError> {
            let r = self.ids.x11(self.root)?;
            let raw: Vec<u32> = list.iter().filter_map(|&w| self.ids.x11(w).ok()).collect();
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_CLIENT_LIST_STACKING,
                AtomEnum::WINDOW,
                &raw,
            )?;
            Ok(())
        }

        fn set_desktop_info(
            &self,
            current: u32,
            total: u32,
            names: &[&str],
        ) -> Result<(), BackendError> {
            let r = self.ids.x11(self.root)?;

            // _NET_NUMBER_OF_DESKTOPS
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_NUMBER_OF_DESKTOPS,
                AtomEnum::CARDINAL,
                &[total],
            )?;

            // _NET_CURRENT_DESKTOP
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_CURRENT_DESKTOP,
                AtomEnum::CARDINAL,
                &[current],
            )?;

            // _NET_DESKTOP_NAMES (null-separated UTF8 strings)
            let mut name_bytes: Vec<u8> = Vec::new();
            for name in names {
                name_bytes.extend_from_slice(name.as_bytes());
                name_bytes.push(0);
            }
            x11rb::wrapper::ConnectionExt::change_property8(
                &*self.conn,
                PropMode::REPLACE,
                r,
                self.atoms._NET_DESKTOP_NAMES,
                self.atoms.UTF8_STRING,
                &name_bytes,
            )?;

            // _NET_DESKTOP_VIEWPORT (all zeros for single-screen virtual desktops)
            let viewports: Vec<u32> = vec![0; total as usize * 2];
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_DESKTOP_VIEWPORT,
                AtomEnum::CARDINAL,
                &viewports,
            )?;

            Ok(())
        }

        fn set_workarea(&self, areas: &[(i32, i32, u32, u32)]) -> Result<(), BackendError> {
            let r = self.ids.x11(self.root)?;
            let data: Vec<u32> = areas
                .iter()
                .flat_map(|&(x, y, w, h)| [x as u32, y as u32, w, h])
                .collect();
            self.conn.change_property32(
                PropMode::REPLACE,
                r,
                self.atoms._NET_WORKAREA,
                AtomEnum::CARDINAL,
                &data,
            )?;
            Ok(())
        }
    }
}

mod input_ops {
    use crate::backend::error::BackendError;
    use std::sync::Arc;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;

    use super::ids::X11IdRegistry;
    use crate::backend::api::AllowMode;
    use crate::backend::api::InputOps as InputOpsTrait;
    use crate::backend::common_define::StdCursorKind;
    use crate::backend::common_define::WindowId;

    pub(super) struct X11InputOps<C: Connection> {
        conn: Arc<C>,
        root_x11: u32,
        ids: X11IdRegistry,
    }

    impl<C: Connection> Clone for X11InputOps<C> {
        fn clone(&self) -> Self {
            Self {
                conn: self.conn.clone(),
                root_x11: self.root_x11,
                ids: self.ids.clone(),
            }
        }
    }

    impl<C: Connection + Send + Sync + 'static> X11InputOps<C> {
        pub(super) fn new(conn: Arc<C>, root_x11: u32, ids: X11IdRegistry) -> Self {
            Self {
                conn,
                root_x11,
                ids,
            }
        }

        fn map_allow_mode(mode: AllowMode) -> Allow {
            match mode {
                AllowMode::AsyncPointer => Allow::ASYNC_POINTER,
                AllowMode::ReplayPointer => Allow::REPLAY_POINTER,
                AllowMode::SyncPointer => Allow::SYNC_POINTER,
                AllowMode::AsyncKeyboard => Allow::ASYNC_KEYBOARD,
                AllowMode::SyncKeyboard => Allow::SYNC_KEYBOARD,
                AllowMode::ReplayKeyboard => Allow::REPLAY_KEYBOARD,
                AllowMode::AsyncBoth => Allow::ASYNC_BOTH,
                AllowMode::SyncBoth => Allow::SYNC_BOTH,
            }
        }

        pub(super) fn allow_events_raw(&self, mode: Allow, time: u32) -> Result<(), BackendError> {
            self.conn.allow_events(mode, time)?;
            Ok(())
        }

        pub(super) fn query_pointer(&self) -> Result<QueryPointerReply, BackendError> {
            Ok(self.conn.query_pointer(self.root_x11)?.reply()?)
        }

        #[allow(dead_code)]
        pub(super) fn flush(&self) -> Result<(), BackendError> {
            self.conn.flush()?;
            Ok(())
        }
    }

    impl<C: Connection + Send + Sync + 'static> InputOpsTrait for X11InputOps<C> {
        fn get_pointer_position(&self) -> Result<(f64, f64), BackendError> {
            let reply = self.query_pointer()?;
            // X11  f64
            Ok((reply.root_x as f64, reply.root_y as f64))
        }

        fn window_under_pointer(&self) -> Result<Option<WindowId>, BackendError> {
            let reply = self.query_pointer()?;
            Ok((reply.child != 0).then(|| self.ids.intern(reply.child)))
        }

        fn grab_pointer(&self, mask_bits: u32, cursor: Option<u64>) -> Result<bool, BackendError> {
            let cursor_id = cursor.map(|c| c as u32).unwrap_or(0);
            //  Grab Pointer  ButtonRelease  Motion
            let mask = if mask_bits != 0 {
                // mask_bits uses EventMaskBits (custom bit layout), must convert
                // to X11 EventMask via the adapter — the bit positions differ.
                super::adapter::event_mask_from_generic(mask_bits)
            } else {
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION
            };

            let reply = self
                .conn
                .grab_pointer(
                    false,
                    self.root_x11,
                    mask,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    0u32, // None confine_to
                    cursor_id,
                    0u32, // Current time
                )?
                .reply()?;

            Ok(reply.status == GrabStatus::SUCCESS)
        }

        fn set_cursor(&self, _kind: StdCursorKind) -> Result<(), BackendError> {
            Ok(())
        }

        fn ungrab_pointer(&self) -> Result<(), BackendError> {
            self.conn.ungrab_pointer(0u32)?;
            Ok(())
        }

        fn allow_events(&self, mode: AllowMode, time: u32) -> Result<(), BackendError> {
            let allow = Self::map_allow_mode(mode);
            self.allow_events_raw(allow, time)
        }

        fn query_pointer_root(&self) -> Result<(i32, i32, u16, u16), BackendError> {
            let reply = self.query_pointer()?;
            Ok((
                reply.root_x as i32,
                reply.root_y as i32,
                reply.mask.bits() as u16,
                0,
            ))
        }

        fn warp_pointer_to_window(
            &self,
            win: WindowId,
            x: i16,
            y: i16,
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn.warp_pointer(0u32, w, 0, 0, 0, 0, x, y)?;
            Ok(())
        }
    }
}

mod key_ops {
    use crate::sync_ext::MutexExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    use log::warn;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;

    use super::adapter::mods_to_x11;
    use super::ids::X11IdRegistry;
    use crate::backend::api::KeyOps;
    use crate::backend::common_define::WindowId;
    use crate::backend::common_define::{KeySym, Mods};
    use crate::backend::error::BackendError;
    use crate::backend::x11::wm::lock_modifier_combinations;

    pub(super) struct X11KeyOps<C: Connection> {
        conn: Arc<C>,
        cache: HashMap<u8, u32>,
        numlock_mask: Arc<Mutex<u16>>,
        ids: X11IdRegistry,
        /// Cached full keyboard mapping (keysyms array + keysyms_per_keycode)
        /// Populated on first use, invalidated by clear_cache().
        full_keymap: Option<(Vec<u32>, u8, u8)>, // (keysyms, per_keycode, min_keycode)
    }

    impl<C: Connection> X11KeyOps<C> {
        pub(super) fn new(conn: Arc<C>, numlock_mask: Arc<Mutex<u16>>, ids: X11IdRegistry) -> Self {
            let mut ops = Self {
                conn: conn.clone(),
                cache: HashMap::new(),
                numlock_mask,
                ids,
                full_keymap: None,
            };
            let _ = ops.detect_and_store_numlock();
            ops
        }

        /// Ensure the full keyboard mapping is cached
        fn ensure_keymap_cached(&mut self) -> Result<(&[u32], u8, u8), BackendError> {
            if self.full_keymap.is_none() {
                let setup = self.conn.setup();
                let min = setup.min_keycode;
                let max = setup.max_keycode;
                let mapping = self
                    .conn
                    .get_keyboard_mapping(min, (max - min) + 1)?
                    .reply()?;
                let per = mapping.keysyms_per_keycode;
                self.full_keymap = Some((mapping.keysyms, per, min));

                // Pre-populate the keysym cache for all keycodes
                if let Some(ref km) = self.full_keymap {
                    let per_usize = km.1 as usize;
                    for offset in 0..((km.0.len()) / per_usize.max(1)) {
                        let kc = km.2 + offset as u8;
                        if let Some(&ks) = km.0.get(offset * per_usize) {
                            if ks != 0 {
                                self.cache.insert(kc, ks);
                            }
                        }
                    }
                }
            }
            let km = self
                .full_keymap
                .as_ref()
                .ok_or_else(|| BackendError::Message("keymap not initialized".into()))?;
            Ok((&km.0, km.1, km.2))
        }

        fn detect_and_store_numlock(&mut self) -> Result<(), BackendError> {
            // Populate the keymap cache during initialization so we
            // don't need to query it again later
            let (keysyms, per, min) = self.ensure_keymap_cached()?;
            let per_usize = per as usize;

            const XK_NUM_LOCK: u32 = 0xFF7F;
            let mut numkc: u8 = 0;
            let max = min as usize + keysyms.len() / per_usize.max(1);
            for kc_usize in (min as usize)..max {
                let idx = (kc_usize - min as usize) * per_usize;
                if idx < keysyms.len() {
                    for i in 0..per_usize {
                        if keysyms[idx + i] == XK_NUM_LOCK {
                            numkc = kc_usize as u8;
                            break;
                        }
                    }
                }
                if numkc != 0 {
                    break;
                }
            }

            let mask = if numkc == 0 {
                0
            } else {
                self.find_modifier_mask(numkc)? as u16
            };

            *self.numlock_mask.lock_safe() = mask;
            Ok(())
        }

        fn find_modifier_mask(&self, target_keycode: u8) -> Result<u8, BackendError> {
            let mm = self.conn.get_modifier_mapping()?.reply()?;
            let per = mm.keycodes_per_modifier() as usize;
            for mod_index in 0..8 {
                let start = mod_index * per;
                let end = start + per;
                if end <= mm.keycodes.len() {
                    for &kc in &mm.keycodes[start..end] {
                        if kc == target_keycode && kc != 0 {
                            return Ok(1 << mod_index);
                        }
                    }
                }
            }
            Ok(0)
        }
    }

    impl<C: Connection + Send + Sync + 'static> KeyOps for X11KeyOps<C> {
        fn clean_mods(&self, raw: u16) -> Mods {
            let numlock = *self.numlock_mask.lock_safe();
            let raw_mask = x11rb::protocol::xproto::KeyButMask::from(raw);
            let numlock_mask = x11rb::protocol::xproto::KeyButMask::from(numlock);
            super::adapter::mods_from_x11(raw_mask, numlock_mask)
        }

        fn clear_key_grabs(&self, root: WindowId) -> Result<(), BackendError> {
            let r = self.ids.x11(root)?;
            self.conn.ungrab_key(Grab::ANY, r, ModMask::ANY.into())?;
            Ok(())
        }

        fn grab_keys(
            &self,
            root: WindowId,
            bindings: &[(Mods, KeySym)],
        ) -> Result<(), BackendError> {
            let numlock_local = *self.numlock_mask.lock_safe();
            let r = self.ids.x11(root)?;

            // Query keyboard mapping once for all bindings
            let setup = self.conn.setup();
            let min = setup.min_keycode;
            let max = setup.max_keycode;
            let mapping = self
                .conn
                .get_keyboard_mapping(min, (max - min) + 1)?
                .reply()?;
            let per = mapping.keysyms_per_keycode as usize;

            use x11rb::protocol::xproto::{KeyButMask as KBM, ModMask};
            let numlock_mask_obj = KBM::from(numlock_local);

            for (mods, keysym) in bindings {
                for (offset, keysyms_for_keycode) in mapping.keysyms.chunks(per).enumerate() {
                    let keycode = min + offset as u8;
                    let matched = keysyms_for_keycode.first() == Some(keysym);
                    if matched {
                        let base = mods_to_x11(*mods, numlock_mask_obj);
                        let combos = lock_modifier_combinations(base, KBM::LOCK, numlock_mask_obj);
                        for mm in combos {
                            let cookie = self.conn.grab_key(
                                false,
                                r,
                                ModMask::from(mm.bits()),
                                keycode,
                                GrabMode::ASYNC,
                                GrabMode::ASYNC,
                            )?;
                            if let Err(e) = cookie.check() {
                                // If another client grabbed the same key, X11 will typically
                                // report BadAccess asynchronously. Surface it for debugging.
                                warn!(
                                    "X11 grab_key failed (keysym=0x{:x}, keycode={}, mods=0x{:x}): {:?}",
                                    *keysym,
                                    keycode,
                                    mm.bits(),
                                    e
                                );
                            }
                        }
                    }
                }
            }

            self.conn.flush()?;
            Ok(())
        }

        fn keysym_from_keycode(&mut self, keycode: u8) -> Result<KeySym, BackendError> {
            if let Some(&ks) = self.cache.get(&keycode) {
                return Ok(ks);
            }

            // Ensure full keymap is cached and use it
            let (keysyms, per, min) = self.ensure_keymap_cached()?;
            let per_usize = per as usize;
            if keycode >= min && per_usize > 0 {
                let offset = (keycode - min) as usize;
                if let Some(&ks) = keysyms.get(offset * per_usize) {
                    return Ok(ks);
                }
            }

            Ok(0)
        }

        fn clear_cache(&mut self) {
            self.cache.clear();
            self.full_keymap = None;
        }

        fn grab_keyboard(&self, root: WindowId) -> Result<(), BackendError> {
            let r = self.ids.x11(root)?;
            self.conn
                .grab_keyboard(
                    false,
                    r,
                    x11rb::CURRENT_TIME,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )?
                .reply()?;
            self.conn.flush()?;
            Ok(())
        }

        fn ungrab_keyboard(&self) -> Result<(), BackendError> {
            self.conn.ungrab_keyboard(x11rb::CURRENT_TIME)?;
            self.conn.flush()?;
            Ok(())
        }
    }
}

mod output_ops {
    use crate::backend::api::{OutputInfo, OutputOps, ScreenInfo};
    use crate::backend::common_define::OutputId;
    use crate::backend::x11::wm::{
        DEFAULT_OUTPUT_REFRESH_MHZ, build_output_info, fallback_output, output_at,
    };
    use std::sync::{Arc, Mutex};
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
    use x11rb::protocol::xproto::ConnectionExt as XprotoExt;

    /// Calculate refresh rate in millihertz from a RandR ModeInfo.
    fn calc_refresh_mhz(mode: &randr::ModeInfo) -> u32 {
        if mode.htotal == 0 || mode.vtotal == 0 {
            return DEFAULT_OUTPUT_REFRESH_MHZ;
        }
        let mut vtotal = mode.vtotal as u64;
        let flags = u32::from(mode.mode_flags);
        if flags & (1 << 4) != 0 {
            vtotal *= 2; // DoubleScan
        }
        if flags & (1 << 0) != 0 {
            vtotal /= 2; // Interlace (fields per second → frames)
        }
        let denom = mode.htotal as u64 * vtotal;
        if denom == 0 {
            return DEFAULT_OUTPUT_REFRESH_MHZ;
        }
        ((mode.dot_clock as u64 * 1000) / denom) as u32
    }

    pub(super) struct X11OutputOps<C: Connection> {
        conn: Arc<C>,
        root: u32,
        sw: i32,
        sh: i32,
        /// Cached output layout - invalidated on RandR events
        /// None = cache miss, Some(vec) = cached outputs
        cached_outputs: Arc<Mutex<Option<Vec<OutputInfo>>>>,
        /// VRR-capable outputs: output_id -> true if VRR supported
        vrr_capable_outputs: Arc<Mutex<std::collections::HashMap<u32, bool>>>,
    }

    impl<C: Connection> X11OutputOps<C> {
        pub(super) fn new(conn: Arc<C>, root: u32, sw: i32, sh: i32) -> Self {
            Self {
                conn,
                root,
                sw,
                sh,
                cached_outputs: Arc::new(Mutex::new(None)),
                vrr_capable_outputs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            }
        }

        /// Invalidate output cache - call on RandR events
        fn invalidate_cache(&self) {
            if let Ok(mut cache) = self.cached_outputs.lock() {
                *cache = None;
            }
        }

        /// Check if output supports VRR (Variable Refresh Rate)
        /// Queries for "vrr_capable" property on the output
        fn query_output_vrr_capable(&self, output: u32) -> bool {
            // Try to get VRR property via atom lookup
            if let Ok(atom_cookie) = self.conn.intern_atom(false, b"vrr_capable") {
                if let Ok(atom_reply) = atom_cookie.reply() {
                    let vrr_atom = atom_reply.atom;
                    if vrr_atom > 0 {
                        // Try to read the property
                        if let Ok(prop_cookie) = self.conn.randr_get_output_property(
                            output,
                            vrr_atom,
                            x11rb::protocol::xproto::AtomEnum::ANY,
                            0,     // offset
                            1,     // length (single value)
                            false, // delete
                            false, // pending
                        ) {
                            if let Ok(prop) = prop_cookie.reply() {
                                if prop.format == 8 && prop.num_items > 0 {
                                    if !prop.data.is_empty() {
                                        return prop.data[0] != 0;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            false
        }

        /// Read the connector's EDID blob via RandR and parse HDR Static Metadata.
        fn query_output_edid_hdr(
            &self,
            output: u32,
        ) -> Option<crate::backend::edid::EdidHdrCapabilities> {
            let edid_atom = self
                .conn
                .intern_atom(false, b"EDID")
                .ok()?
                .reply()
                .ok()?
                .atom;
            let prop = self
                .conn
                .randr_get_output_property(output, edid_atom, 0u32, 0, 256, false, false)
                .ok()?
                .reply()
                .ok()?;
            if prop.data.len() < 128 {
                return None;
            }
            crate::backend::edid::parse_edid_hdr_from_bytes(&prop.data)
        }

        /// Check if output supports HDR
        /// Queries for "max_bpc" property on the output (>= 10 indicates HDR support)
        fn query_output_hdr_capable(&self, output: u32) -> bool {
            // Try to get max_bpc property via atom lookup
            if let Ok(atom_cookie) = self.conn.intern_atom(false, b"max_bpc") {
                if let Ok(atom_reply) = atom_cookie.reply() {
                    let max_bpc_atom = atom_reply.atom;
                    if max_bpc_atom > 0 {
                        // Try to read the property
                        if let Ok(prop_cookie) = self.conn.randr_get_output_property(
                            output,
                            max_bpc_atom,
                            x11rb::protocol::xproto::AtomEnum::INTEGER,
                            0,     // offset
                            1,     // length (single value)
                            false, // delete
                            false, // pending
                        ) {
                            if let Ok(prop) = prop_cookie.reply() {
                                // x11rb un-swaps format=32 property data to host
                                // byte order at the connection layer; reading as
                                // little-endian was wrong on big-endian hosts.
                                // RandR's reply has no value32() helper so we
                                // call from_ne_bytes directly (the format=32
                                // codepath that xproto's value32 takes).
                                if prop.format == 32 && prop.data.len() >= 4 {
                                    let value = u32::from_ne_bytes([
                                        prop.data[0],
                                        prop.data[1],
                                        prop.data[2],
                                        prop.data[3],
                                    ]);
                                    return value >= 10;
                                }
                            }
                        }
                    }
                }
            }
            // Fallback: assume HDR capable (let config decide)
            true
        }

        /// Get cached outputs or query if cache miss
        fn get_cached_or_query(&self) -> Vec<OutputInfo> {
            // Fast path: check cache
            if let Ok(cache) = self.cached_outputs.lock() {
                if let Some(ref outputs) = *cache {
                    return outputs.clone();
                }
            }

            // Cache miss: query X11
            let outputs = self.query_outputs_internal();

            // Update cache
            if let Ok(mut cache) = self.cached_outputs.lock() {
                *cache = Some(outputs.clone());
            }

            outputs
        }

        /// Internal query method (does the actual X11 round-trips)
        fn query_outputs_internal(&self) -> Vec<OutputInfo> {
            // Try RandR 1.5 first (monitors API)
            if let Ok(ver) = self.conn.randr_query_version(1, 5) {
                if let Ok(v) = ver.reply() {
                    if (v.major_version > 1) || (v.major_version == 1 && v.minor_version >= 5) {
                        if let Ok(cookie) = self.conn.randr_get_monitors(self.root, true) {
                            if let Ok(reply) = cookie.reply() {
                                // Pre-fetch screen resources for mode info lookup
                                let modes: Vec<randr::ModeInfo> = self
                                    .conn
                                    .randr_get_screen_resources(self.root)
                                    .ok()
                                    .and_then(|c| c.reply().ok())
                                    .map(|r| r.modes)
                                    .unwrap_or_default();

                                let mut out = Vec::with_capacity(4);
                                for (i, m) in reply.monitors.into_iter().enumerate() {
                                    if m.width > 0 && m.height > 0 {
                                        // Resolve refresh rate: output → output_info → crtc → crtc_info → mode
                                        let refresh = m
                                            .outputs
                                            .first()
                                            .and_then(|&output| {
                                                self.conn
                                                    .randr_get_output_info(output, 0)
                                                    .ok()?
                                                    .reply()
                                                    .ok()
                                            })
                                            .and_then(|oi| {
                                                if oi.crtc == 0 {
                                                    return None;
                                                }
                                                self.conn
                                                    .randr_get_crtc_info(oi.crtc, 0)
                                                    .ok()?
                                                    .reply()
                                                    .ok()
                                            })
                                            .and_then(|ci| {
                                                modes
                                                    .iter()
                                                    .find(|mode| mode.id == ci.mode)
                                                    .map(calc_refresh_mhz)
                                            })
                                            .unwrap_or(DEFAULT_OUTPUT_REFRESH_MHZ);

                                        let (hdr_capable, hdr_metadata) =
                                            if let Some(&first_output) = m.outputs.first() {
                                                let caps = self.query_output_edid_hdr(first_output);
                                                let bpc_capable =
                                                    self.query_output_hdr_capable(first_output);
                                                (bpc_capable || caps.is_some(), caps)
                                            } else {
                                                (false, None)
                                            };

                                        let id = m
                                            .outputs
                                            .first()
                                            .copied()
                                            .map(|output| OutputId(output as u64))
                                            .unwrap_or(OutputId(i as u64));
                                        let name = m
                                            .outputs
                                            .first()
                                            .and_then(|&output| {
                                                self.conn
                                                    .randr_get_output_info(output, 0)
                                                    .ok()?
                                                    .reply()
                                                    .ok()
                                                    .and_then(|info| {
                                                        String::from_utf8(info.name).ok()
                                                    })
                                            })
                                            .filter(|name| !name.is_empty())
                                            .unwrap_or_else(|| format!("Monitor-{}", i));

                                        out.push(build_output_info(
                                            id,
                                            name,
                                            m.x as i32,
                                            m.y as i32,
                                            m.width as i32,
                                            m.height as i32,
                                            refresh,
                                            hdr_capable,
                                            hdr_metadata,
                                        ));

                                        // Check for VRR support on this output
                                        if let Some(&first_output) = m.outputs.first() {
                                            if self.query_output_vrr_capable(first_output) {
                                                if let Ok(mut vrr_map) =
                                                    self.vrr_capable_outputs.lock()
                                                {
                                                    vrr_map.insert(first_output, true);
                                                    log::info!(
                                                        "backend: Output {} supports VRR",
                                                        first_output
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    return out;
                                }
                            }
                        }
                    }
                }
            }

            // Fallback: RandR 1.2 CRTC enumeration
            if let Ok(cookie) = self.conn.randr_get_screen_resources(self.root) {
                if let Ok(resources) = cookie.reply() {
                    let modes = &resources.modes;
                    let mut out = Vec::with_capacity(4);
                    for (i, crtc) in resources.crtcs.iter().enumerate() {
                        if let Ok(cookie) = self.conn.randr_get_crtc_info(*crtc, 0) {
                            if let Ok(ci) = cookie.reply() {
                                if ci.width > 0 && ci.height > 0 {
                                    let refresh = modes
                                        .iter()
                                        .find(|m| m.id == ci.mode)
                                        .map(calc_refresh_mhz)
                                        .unwrap_or(DEFAULT_OUTPUT_REFRESH_MHZ);
                                    let first_output = ci.outputs.first().copied();
                                    let id = first_output
                                        .map(|output| OutputId(output as u64))
                                        .unwrap_or(OutputId((*crtc) as u64));
                                    let name = first_output
                                        .and_then(|output| {
                                            self.conn
                                                .randr_get_output_info(output, 0)
                                                .ok()?
                                                .reply()
                                                .ok()
                                                .and_then(|info| String::from_utf8(info.name).ok())
                                        })
                                        .filter(|name| !name.is_empty())
                                        .unwrap_or_else(|| format!("CRTC-{}", i));
                                    out.push(build_output_info(
                                        id,
                                        name,
                                        ci.x as i32,
                                        ci.y as i32,
                                        ci.width as i32,
                                        ci.height as i32,
                                        refresh,
                                        false,
                                        None,
                                    ));
                                }
                            }
                        }
                    }
                    if !out.is_empty() {
                        return out;
                    }
                }
            }

            // Ultimate fallback: single screen
            vec![fallback_output("Default", self.sw, self.sh)]
        }
    }

    impl<C: Connection + Send + Sync + 'static> OutputOps for X11OutputOps<C> {
        fn screen_info(&self) -> ScreenInfo {
            ScreenInfo {
                width: self.sw,
                height: self.sh,
            }
        }

        fn output_at(&self, x: i32, y: i32) -> Option<OutputId> {
            let outputs = self.get_cached_or_query();
            output_at(&outputs, x, y)
        }

        fn enumerate_outputs(&self) -> Vec<OutputInfo> {
            self.get_cached_or_query()
        }

        fn invalidate_output_cache(&self) {
            self.invalidate_cache();
        }

        fn set_gamma_ramp(
            &self,
            output: OutputId,
            red: &[u16],
            green: &[u16],
            blue: &[u16],
        ) -> Result<(), crate::backend::error::BackendError> {
            let crtc = self.output_to_crtc(output.0 as u32);
            if let Some(crtc_id) = crtc {
                self.conn
                    .randr_set_crtc_gamma(crtc_id, red, green, blue)
                    .map_err(|e| crate::backend::error::BackendError::Message(e.to_string()))?;
                self.conn
                    .flush()
                    .map_err(|e| crate::backend::error::BackendError::Message(e.to_string()))?;
            }
            Ok(())
        }

        fn get_gamma_ramp(&self, output: OutputId) -> Option<(Vec<u16>, Vec<u16>, Vec<u16>)> {
            let crtc = self.output_to_crtc(output.0 as u32)?;
            let gamma_size = self
                .conn
                .randr_get_crtc_gamma_size(crtc)
                .ok()?
                .reply()
                .ok()?;
            let reply = self.conn.randr_get_crtc_gamma(crtc).ok()?.reply().ok()?;
            let _ = gamma_size;
            Some((
                reply.red.to_vec(),
                reply.green.to_vec(),
                reply.blue.to_vec(),
            ))
        }
    }

    impl<C: Connection + Send + Sync + 'static> X11OutputOps<C> {
        fn output_to_crtc(&self, output_id: u32) -> Option<u32> {
            let resources = self
                .conn
                .randr_get_screen_resources(self.root)
                .ok()?
                .reply()
                .ok()?;
            for &output in &resources.outputs {
                if output == output_id {
                    let info = self
                        .conn
                        .randr_get_output_info(output, 0)
                        .ok()?
                        .reply()
                        .ok()?;
                    if info.crtc != 0 {
                        return Some(info.crtc);
                    }
                }
            }
            None
        }
    }
}

mod property_ops {
    use super::ids::X11IdRegistry;
    use crate::backend::api::{
        AllowedAction, IconData, MotifWmHints, NetWmState, NormalHints,
        PropertyOps as PropertyOpsTrait, StrutPartial, WindowType, WmHints,
    };
    use crate::backend::common_define::WindowId;
    use crate::backend::error::BackendError;
    use crate::backend::x11::wm::{
        AllowedActionAtoms, NetWmStateAtoms, WindowTypeAtoms, atom_for_allowed_action,
        atom_for_net_wm_state, decode_text_property, net_wm_ping_message,
        net_wm_sync_request_message, parse_gtk_frame_extents, parse_icon_data, parse_motif_hints,
        parse_normal_hints, parse_opaque_region, parse_strut, parse_strut_partial, parse_wm_class,
        parse_wm_hints, protocol_supported, window_type_from_atom,
    };
    use crate::backend::x11rb::Atoms;
    use std::sync::Arc;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::wrapper::ConnectionExt as _;
    use x11rb::x11_utils::Serialize;

    // Caps for client-supplied property fetches. A hostile or buggy client can
    // set an arbitrarily large property on its own windows; reading without a
    // cap pulls it wholesale into the WM's address space. Each cap is generous
    // for legitimate use but bounds the worst case:
    //   - Icon: 16 MB worth of u32 (~one 2048×2048 icon, or several 256×256).
    //   - Opaque region: 1 M u32 = 256 K rects = 4 MB.
    //   - Text property (UTF-8 / Latin-1, format=8): 256 KB. EWMH titles are
    //     conventionally well under 4 KB.
    //   - Atom list (e.g. _NET_WM_STATE): 4 K atoms = 16 KB. There aren't that
    //     many distinct states per window in practice.
    pub(super) const MAX_ICON_ITEMS_U32: u32 = 4 * 1024 * 1024;
    pub(super) const MAX_OPAQUE_REGION_ITEMS_U32: u32 = 1024 * 1024;
    pub(super) const MAX_TEXT_PROPERTY_BYTES: u32 = 256 * 1024;
    pub(super) const MAX_ATOM_LIST_ITEMS: u32 = 4096;

    pub(super) struct X11PropertyOps<C: Connection> {
        conn: Arc<C>,
        atoms: Atoms,
        ids: X11IdRegistry,
    }

    impl<C: Connection> X11PropertyOps<C> {
        pub(super) fn new(conn: Arc<C>, atoms: Atoms, ids: X11IdRegistry) -> Self {
            Self { conn, atoms, ids }
        }
    }

    impl<C: Connection + Send + Sync + 'static> X11PropertyOps<C> {
        fn get_text_property(&self, win: WindowId, atom: Atom) -> Option<String> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(false, w, atom, AtomEnum::ANY, 0, MAX_TEXT_PROPERTY_BYTES)
                .ok()?
                .reply()
                .ok()?;

            if reply.value.is_empty() || reply.format != 8 {
                return None;
            }

            decode_text_property(
                &reply.value,
                reply.type_,
                self.atoms.UTF8_STRING,
                u32::from(AtomEnum::STRING),
            )
        }

        fn get_net_wm_state_atoms(&self, win: WindowId) -> Result<Vec<u32>, BackendError> {
            let w = self.ids.x11(win)?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._NET_WM_STATE,
                    AtomEnum::ATOM,
                    0,
                    MAX_ATOM_LIST_ITEMS,
                )?
                .reply()?;
            if reply.format != 32 {
                return Ok(Vec::new());
            }
            Ok(reply.value32().into_iter().flatten().collect())
        }

        fn set_net_wm_state_atoms(&self, win: WindowId, atoms: &[u32]) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms._NET_WM_STATE,
                AtomEnum::ATOM,
                atoms,
            )?;
            Ok(())
        }

        fn add_net_wm_state_atom(&self, win: WindowId, atom: u32) -> Result<(), BackendError> {
            let mut states = self.get_net_wm_state_atoms(win)?;
            if !states.contains(&atom) {
                states.push(atom);
                self.set_net_wm_state_atoms(win, &states)?;
            }
            Ok(())
        }

        fn remove_net_wm_state_atom(&self, win: WindowId, atom: u32) -> Result<(), BackendError> {
            let mut states = self.get_net_wm_state_atoms(win)?;
            let len_before = states.len();
            states.retain(|&a| a != atom);
            if states.len() != len_before {
                self.set_net_wm_state_atoms(win, &states)?;
            }
            Ok(())
        }

        fn window_type_atoms(&self) -> WindowTypeAtoms<u32> {
            WindowTypeAtoms {
                desktop: self.atoms._NET_WM_WINDOW_TYPE_DESKTOP,
                dock: self.atoms._NET_WM_WINDOW_TYPE_DOCK,
                toolbar: self.atoms._NET_WM_WINDOW_TYPE_TOOLBAR,
                menu: self.atoms._NET_WM_WINDOW_TYPE_MENU,
                utility: self.atoms._NET_WM_WINDOW_TYPE_UTILITY,
                splash: self.atoms._NET_WM_WINDOW_TYPE_SPLASH,
                dialog: self.atoms._NET_WM_WINDOW_TYPE_DIALOG,
                dropdown_menu: self.atoms._NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
                popup_menu: self.atoms._NET_WM_WINDOW_TYPE_POPUP_MENU,
                tooltip: self.atoms._NET_WM_WINDOW_TYPE_TOOLTIP,
                notification: self.atoms._NET_WM_WINDOW_TYPE_NOTIFICATION,
                combo: self.atoms._NET_WM_WINDOW_TYPE_COMBO,
            }
        }

        fn net_wm_state_atoms(&self) -> NetWmStateAtoms<u32> {
            NetWmStateAtoms {
                fullscreen: self.atoms._NET_WM_STATE_FULLSCREEN,
                maximized_vert: self.atoms._NET_WM_STATE_MAXIMIZED_VERT,
                maximized_horz: self.atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                hidden: self.atoms._NET_WM_STATE_HIDDEN,
                above: self.atoms._NET_WM_STATE_ABOVE,
                below: self.atoms._NET_WM_STATE_BELOW,
                demands_attention: self.atoms._NET_WM_STATE_DEMANDS_ATTENTION,
                sticky: self.atoms._NET_WM_STATE_STICKY,
                skip_taskbar: self.atoms._NET_WM_STATE_SKIP_TASKBAR,
                skip_pager: self.atoms._NET_WM_STATE_SKIP_PAGER,
            }
        }

        fn allowed_action_atoms(&self) -> AllowedActionAtoms<u32> {
            AllowedActionAtoms {
                move_: self.atoms._NET_WM_ACTION_MOVE,
                resize: self.atoms._NET_WM_ACTION_RESIZE,
                minimize: self.atoms._NET_WM_ACTION_MINIMIZE,
                maximize_horz: self.atoms._NET_WM_ACTION_MAXIMIZE_HORZ,
                maximize_vert: self.atoms._NET_WM_ACTION_MAXIMIZE_VERT,
                fullscreen: self.atoms._NET_WM_ACTION_FULLSCREEN,
                close: self.atoms._NET_WM_ACTION_CLOSE,
                stick: self.atoms._NET_WM_ACTION_STICK,
                above: self.atoms._NET_WM_ACTION_ABOVE,
                below: self.atoms._NET_WM_ACTION_BELOW,
            }
        }
    }

    impl<C: Connection + Send + Sync + 'static> PropertyOpsTrait for X11PropertyOps<C> {
        fn get_title(&self, win: WindowId) -> String {
            if let Some(title) = self.get_text_property(win, self.atoms._NET_WM_NAME) {
                return title;
            }
            if let Some(title) = self.get_text_property(win, AtomEnum::WM_NAME.into()) {
                return title;
            }
            "".to_string()
        }

        fn get_class(&self, win: WindowId) -> (String, String) {
            let w = match self.ids.x11(win) {
                Ok(w) => w,
                Err(_) => return (String::new(), String::new()),
            };
            let reply =
                match self
                    .conn
                    .get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
                {
                    Ok(cookie) => cookie.reply().ok(),
                    Err(_) => None,
                };

            if let Some(reply) = reply {
                if reply.type_ == u32::from(AtomEnum::STRING) && reply.format == 8 {
                    if !reply.value.is_empty() {
                        return parse_wm_class(&reply.value);
                    }
                }
            }
            (String::new(), String::new())
        }

        fn get_window_types(&self, win: WindowId) -> Vec<WindowType> {
            let w = match self.ids.x11(win) {
                Ok(w) => w,
                Err(_) => return Vec::new(),
            };
            let mut result = Vec::new();
            if let Ok(reply) = self.conn.get_property(
                false,
                w,
                self.atoms._NET_WM_WINDOW_TYPE,
                AtomEnum::ATOM,
                0,
                u32::MAX,
            ) {
                if let Ok(rep) = reply.reply() {
                    if rep.format == 32 {
                        for atom in rep.value32().into_iter().flatten() {
                            let wt = window_type_from_atom(atom, self.window_type_atoms());
                            if wt != WindowType::Unknown {
                                result.push(wt);
                            }
                        }
                    }
                }
            }
            if result.is_empty() {
                // EWMH: a window with WM_TRANSIENT_FOR but no explicit type is a
                // DIALOG (not DND, which was a copy-paste bug that mislabeled every
                // typeless transient as a drag-and-drop surface).
                if self.transient_for(win).is_some() {
                    result.push(WindowType::Dialog);
                } else {
                    result.push(WindowType::Normal);
                }
            }
            result
        }

        fn is_fullscreen(&self, win: WindowId) -> bool {
            let states = self.get_net_wm_state_atoms(win).unwrap_or_default();
            states.contains(&self.atoms._NET_WM_STATE_FULLSCREEN)
        }

        fn set_fullscreen_state(&self, win: WindowId, on: bool) -> Result<(), BackendError> {
            if on {
                self.add_net_wm_state_atom(win, self.atoms._NET_WM_STATE_FULLSCREEN)
            } else {
                self.remove_net_wm_state_atom(win, self.atoms._NET_WM_STATE_FULLSCREEN)
            }
        }

        fn get_wm_hints(&self, win: WindowId) -> Option<WmHints> {
            let w = self.ids.x11(win).ok()?;
            let prop = self
                .conn
                .get_property(false, w, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, 0, 20)
                .ok()?
                .reply()
                .ok()?;
            let values: Vec<u32> = prop.value32()?.collect();
            parse_wm_hints(&values)
        }

        fn set_urgent_hint(&self, win: WindowId, urgent: bool) -> Result<(), BackendError> {
            const X_URGENCY_HINT: u32 = 1 << 8;
            let w = self.ids.x11(win)?;
            let cookie =
                self.conn
                    .get_property(false, w, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, 0, 20)?;

            let mut data = Vec::new();
            if let Ok(reply) = cookie.reply() {
                data = reply.value32().into_iter().flatten().collect();
            }
            if data.is_empty() {
                data.push(0);
            }

            if urgent {
                data[0] |= X_URGENCY_HINT;
            } else {
                data[0] &= !X_URGENCY_HINT;
            }

            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                AtomEnum::WM_HINTS,
                AtomEnum::WM_HINTS,
                &data,
            )?;
            Ok(())
        }

        fn transient_for(&self, win: WindowId) -> Option<WindowId> {
            let w = self.ids.x11(win).ok()?;
            let r = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms.WM_TRANSIENT_FOR,
                    AtomEnum::WINDOW,
                    0,
                    1,
                )
                .ok()?
                .reply()
                .ok()?;

            if r.format == 32 {
                if let Some(t) = r.value32()?.next() {
                    if t != 0 && t != w {
                        return Some(self.ids.intern(t));
                    }
                }
            }
            None
        }

        fn fetch_normal_hints(&self, win: WindowId) -> Result<Option<NormalHints>, BackendError> {
            let w = self.ids.x11(win)?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    AtomEnum::WM_NORMAL_HINTS,
                    AtomEnum::WM_SIZE_HINTS,
                    0,
                    18,
                )?
                .reply()?;
            if reply.format != 32 {
                return Ok(None);
            }
            let values: Vec<u32> = reply.value32().into_iter().flatten().collect();
            Ok(parse_normal_hints(&values))
        }

        fn set_window_strut_top(
            &self,
            win: WindowId,
            top: u32,
            start_x: u32,
            end_x: u32,
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let strut = [0, 0, top, 0];
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms._NET_WM_STRUT,
                AtomEnum::CARDINAL,
                &strut,
            )?;
            let partial = [0, 0, top, 0, 0, 0, 0, 0, start_x, end_x, 0, 0];
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms._NET_WM_STRUT_PARTIAL,
                AtomEnum::CARDINAL,
                &partial,
            )?;
            Ok(())
        }

        fn set_window_type_dock(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms._NET_WM_WINDOW_TYPE,
                AtomEnum::ATOM,
                &[self.atoms._NET_WM_WINDOW_TYPE_DOCK],
            )?;
            Ok(())
        }

        fn clear_window_strut(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let _ = self.conn.delete_property(w, self.atoms._NET_WM_STRUT);
            let _ = self
                .conn
                .delete_property(w, self.atoms._NET_WM_STRUT_PARTIAL);
            Ok(())
        }

        fn get_window_strut_partial(&self, win: WindowId) -> Option<StrutPartial> {
            let w = self.ids.x11(win).ok()?;
            // Try _NET_WM_STRUT_PARTIAL first (12 CARDINAL values)
            if let Ok(reply) = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._NET_WM_STRUT_PARTIAL,
                    AtomEnum::CARDINAL,
                    0,
                    12,
                )
                .ok()?
                .reply()
            {
                if reply.format == 32 {
                    let vals: Vec<u32> = reply.value32()?.collect();
                    if let Some(strut) = parse_strut_partial(&vals) {
                        return Some(strut);
                    }
                }
            }
            // Fallback: _NET_WM_STRUT (4 CARDINAL values, no start/end ranges)
            if let Ok(reply) = self
                .conn
                .get_property(false, w, self.atoms._NET_WM_STRUT, AtomEnum::CARDINAL, 0, 4)
                .ok()?
                .reply()
            {
                if reply.format == 32 {
                    let vals: Vec<u32> = reply.value32()?.collect();
                    if let Some(strut) = parse_strut(&vals) {
                        return Some(strut);
                    }
                }
            }
            None
        }

        fn set_client_info_props(
            &self,
            win: WindowId,
            tags: u32,
            monitor_num: u32,
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let data = [tags, monitor_num];
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms._NET_CLIENT_INFO,
                AtomEnum::CARDINAL,
                &data,
            )?;
            Ok(())
        }

        fn get_wm_state(&self, win: WindowId) -> Result<i64, BackendError> {
            let w = self.ids.x11(win)?;
            let reply = self
                .conn
                .get_property(false, w, self.atoms.WM_STATE, self.atoms.WM_STATE, 0, 2)?
                .reply()?;
            if reply.format != 32 {
                return Ok(-1);
            }
            Ok(reply
                .value32()
                .into_iter()
                .flatten()
                .next()
                .map(|v| v as i64)
                .unwrap_or(-1))
        }

        fn set_wm_state(&self, win: WindowId, state: i64) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let data: [u32; 2] = [state as u32, 0];
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms.WM_STATE,
                self.atoms.WM_STATE,
                &data,
            )?;
            Ok(())
        }

        fn get_window_pid(&self, win: WindowId) -> Option<u32> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(false, w, self.atoms._NET_WM_PID, AtomEnum::CARDINAL, 0, 1)
                .ok()?
                .reply()
                .ok()?;

            if reply.format == 32 {
                reply.value32()?.next()
            } else {
                None
            }
        }

        fn set_net_wm_state_flag(
            &self,
            win: WindowId,
            state: NetWmState,
            on: bool,
        ) -> Result<(), BackendError> {
            let atom = self.net_wm_state_to_atom(state);
            if on {
                self.add_net_wm_state_atom(win, atom)
            } else {
                self.remove_net_wm_state_atom(win, atom)
            }
        }

        fn set_frame_extents(
            &self,
            win: WindowId,
            left: u32,
            right: u32,
            top: u32,
            bottom: u32,
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms._NET_FRAME_EXTENTS,
                AtomEnum::CARDINAL,
                &[left, right, top, bottom],
            )?;
            Ok(())
        }

        fn set_allowed_actions(
            &self,
            win: WindowId,
            actions: &[AllowedAction],
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let atoms: Vec<u32> = actions
                .iter()
                .map(|a| self.allowed_action_to_atom(*a))
                .collect();
            self.conn.change_property32(
                PropMode::REPLACE,
                w,
                self.atoms._NET_WM_ALLOWED_ACTIONS,
                AtomEnum::ATOM,
                &atoms,
            )?;
            Ok(())
        }

        fn send_ping(&self, win: WindowId, timestamp: u32) -> Result<bool, BackendError> {
            let w = self.ids.x11(win)?;
            let reply = self
                .conn
                .get_property(false, w, self.atoms.WM_PROTOCOLS, AtomEnum::ATOM, 0, 1024)?
                .reply()?;
            let supports_ping = reply.value32().into_iter().flatten().collect::<Vec<_>>();
            if protocol_supported(&supports_ping, self.atoms._NET_WM_PING) {
                let event = ClientMessageEvent::new(
                    32,
                    w,
                    self.atoms.WM_PROTOCOLS,
                    net_wm_ping_message(self.atoms._NET_WM_PING, timestamp, w),
                );
                self.conn
                    .send_event(false, w, EventMask::NO_EVENT, event.serialize())?;
                self.conn.flush()?;
                return Ok(true);
            }
            Ok(false)
        }

        fn get_user_time(&self, win: WindowId) -> Option<u32> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._NET_WM_USER_TIME,
                    AtomEnum::CARDINAL,
                    0,
                    1,
                )
                .ok()?
                .reply()
                .ok()?;
            if reply.format == 32 {
                reply.value32()?.next()
            } else {
                None
            }
        }

        fn get_net_wm_icon(&self, win: WindowId) -> Option<Vec<IconData>> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._NET_WM_ICON,
                    AtomEnum::CARDINAL,
                    0,
                    MAX_ICON_ITEMS_U32,
                )
                .ok()?
                .reply()
                .ok()?;
            if reply.format != 32 {
                return None;
            }
            let data: Vec<u32> = reply.value32()?.collect();
            parse_icon_data(&data)
        }

        fn get_bypass_compositor(&self, win: WindowId) -> Option<u32> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._NET_WM_BYPASS_COMPOSITOR,
                    AtomEnum::CARDINAL,
                    0,
                    1,
                )
                .ok()?
                .reply()
                .ok()?;
            if reply.format == 32 {
                reply.value32()?.next()
            } else {
                None
            }
        }

        fn get_opaque_region(&self, win: WindowId) -> Option<Vec<(i32, i32, u32, u32)>> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._NET_WM_OPAQUE_REGION,
                    AtomEnum::CARDINAL,
                    0,
                    MAX_OPAQUE_REGION_ITEMS_U32,
                )
                .ok()?
                .reply()
                .ok()?;
            if reply.format != 32 {
                return None;
            }
            let data: Vec<u32> = reply.value32()?.collect();
            parse_opaque_region(&data)
        }

        fn get_motif_hints(&self, win: WindowId) -> Option<MotifWmHints> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(false, w, self.atoms._MOTIF_WM_HINTS, AtomEnum::ANY, 0, 5)
                .ok()?
                .reply()
                .ok()?;
            if reply.format != 32 {
                return None;
            }
            let data: Vec<u32> = reply.value32()?.collect();
            parse_motif_hints(&data)
        }

        fn get_gtk_frame_extents(&self, win: WindowId) -> Option<[u32; 4]> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._GTK_FRAME_EXTENTS,
                    AtomEnum::CARDINAL,
                    0,
                    4,
                )
                .ok()?
                .reply()
                .ok()?;
            if reply.format != 32 {
                return None;
            }
            let data: Vec<u32> = reply.value32()?.collect();
            parse_gtk_frame_extents(&data)
        }

        fn get_sync_counter(&self, win: WindowId) -> Option<u32> {
            let w = self.ids.x11(win).ok()?;
            let reply = self
                .conn
                .get_property(
                    false,
                    w,
                    self.atoms._NET_WM_SYNC_REQUEST_COUNTER,
                    AtomEnum::CARDINAL,
                    0,
                    1,
                )
                .ok()?
                .reply()
                .ok()?;
            if reply.format != 32 {
                return None;
            }
            let data: Vec<u32> = reply.value32()?.collect();
            data.first().copied()
        }

        fn send_sync_request(
            &self,
            win: WindowId,
            _counter: u32,
            value: u64,
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let event = ClientMessageEvent::new(
                32,
                w,
                self.atoms.WM_PROTOCOLS,
                net_wm_sync_request_message(
                    self.atoms._NET_WM_SYNC_REQUEST,
                    x11rb::CURRENT_TIME,
                    value,
                ),
            );
            self.conn
                .send_event(false, w, EventMask::NO_EVENT, event.serialize())?;
            self.conn.flush()?;
            Ok(())
        }
    }

    impl<C: Connection + Send + Sync + 'static> X11PropertyOps<C> {
        fn net_wm_state_to_atom(&self, state: NetWmState) -> u32 {
            atom_for_net_wm_state(state, self.net_wm_state_atoms())
        }

        fn allowed_action_to_atom(&self, action: AllowedAction) -> u32 {
            atom_for_allowed_action(action, self.allowed_action_atoms())
        }
    }
}

mod window_ops {
    use super::adapter::{event_mask_from_generic, mods_to_x11};
    use super::ids::X11IdRegistry;
    use crate::backend::api::{CloseResult, Geometry, WindowAttributes, WindowOps};
    use crate::backend::api::{StackMode, WindowChanges};
    use crate::backend::common_define::{Mods, Pixel, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::x11::wm::{
        lock_modifier_combinations, protocol_supported, restack_window_changes,
        wm_delete_window_message, wm_take_focus_message,
    };
    use crate::backend::x11rb::Atoms;
    use crate::backend::x11rb::batch::X11RequestBatcher;
    use crate::sync_ext::MutexExt;
    use log::debug;
    use std::env;
    use std::sync::Arc;
    use std::sync::Mutex;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::*;
    use x11rb::x11_utils::Serialize;

    pub(super) struct X11WindowOps<C: Connection> {
        conn: Arc<C>,
        atoms: Atoms,
        numlock_mask: Arc<Mutex<u16>>,
        root_x11: u32,
        ids: X11IdRegistry,
        batcher: X11RequestBatcher,
    }

    impl<C: Connection> X11WindowOps<C> {
        fn debug_drag_enabled() -> bool {
            // Cached: read per MotionNotify during a drag (see X11rbBackend copy).
            static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *CACHE.get_or_init(|| {
                env::var("JWM_DEBUG_DRAG")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(true)
            })
        }

        pub(super) fn new(
            conn: Arc<C>,
            atoms: Atoms,
            numlock_mask: Arc<Mutex<u16>>,
            root_x11: u32,
            ids: X11IdRegistry,
        ) -> Self {
            Self {
                conn,
                atoms,
                numlock_mask,
                root_x11,
                ids,
                batcher: X11RequestBatcher::new(),
            }
        }

        fn send_configure_notify_internal(
            &self,
            win: WindowId,
            x: i16,
            y: i16,
            width: u16,
            height: u16,
            border: u16,
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let event = ConfigureNotifyEvent {
                response_type: CONFIGURE_NOTIFY_EVENT,
                sequence: 0,
                event: w,
                window: w,
                x,
                y,
                width,
                height,
                border_width: border,
                above_sibling: 0,
                override_redirect: false,
            };
            self.conn
                .send_event(false, w, EventMask::STRUCTURE_NOTIFY, event)?;
            self.batcher.mark_op(&*self.conn)?;
            Ok(())
        }
    }

    impl<C: Connection + Send + Sync + 'static> WindowOps for X11WindowOps<C> {
        fn set_position(&self, win: WindowId, x: i32, y: i32) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            if Self::debug_drag_enabled() {
                debug!("[drag] x11 set_position win={:?} x={} y={}", win, x, y);
            }
            let aux = ConfigureWindowAux::new().x(x).y(y);
            self.conn.configure_window(w, &aux)?;
            Ok(())
        }

        fn configure(
            &self,
            win: WindowId,
            x: i32,
            y: i32,
            w: u32,
            h: u32,
            border: u32,
        ) -> Result<(), BackendError> {
            let wid = self.ids.x11(win)?;

            if Self::debug_drag_enabled() {
                debug!(
                    "[drag] x11 configure win={:?} x={} y={} w={} h={} border={}",
                    win, x, y, w, h, border
                );
            }

            // 1. 
            let aux = ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(w)
                .height(h)
                .border_width(border);
            self.conn.configure_window(wid, &aux)?;

            // 2.  ConfigureNotify (ICCCM )
            self.send_configure_notify_internal(
                win,
                x as i16,
                y as i16,
                w as u16,
                h as u16,
                border as u16,
            )?;

            Ok(())
        }

        fn set_decoration_style(
            &self,
            win: WindowId,
            border_width: u32,
            border_color: Pixel,
        ) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            // 
            let aux_attr = ChangeWindowAttributesAux::new().border_pixel(border_color.0);
            self.conn.change_window_attributes(w, &aux_attr)?;
            // 
            let aux_conf = ConfigureWindowAux::new().border_width(border_width);
            self.conn.configure_window(w, &aux_conf)?;
            Ok(())
        }

        fn raise_window(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            let aux =
                ConfigureWindowAux::new().stack_mode(x11rb::protocol::xproto::StackMode::ABOVE);
            self.conn.configure_window(w, &aux)?;
            Ok(())
        }

        fn restack_windows(&self, windows: &[WindowId]) -> Result<(), BackendError> {
            for (window, changes) in restack_window_changes(windows) {
                if self.ids.x11(window).is_ok() {
                    self.apply_window_changes(window, changes)?;
                }
            }
            Ok(())
        }

        fn close_window(&self, win: WindowId) -> Result<CloseResult, BackendError> {
            let w = self.ids.x11(win)?;
            let protocols = {
                let reply = self
                    .conn
                    .get_property(false, w, self.atoms.WM_PROTOCOLS, AtomEnum::ATOM, 0, 1024)?
                    .reply()?;
                reply.value32().into_iter().flatten().collect::<Vec<_>>()
            };

            if protocol_supported(&protocols, self.atoms.WM_DELETE_WINDOW) {
                let event = ClientMessageEvent::new(
                    32,
                    w,
                    self.atoms.WM_PROTOCOLS,
                    wm_delete_window_message(self.atoms.WM_DELETE_WINDOW),
                );
                self.conn.send_event(
                    false,
                    w,
                    EventMask::NO_EVENT,
                    event.serialize(), // 
                )?;
                // 
                self.conn.flush()?;
                return Ok(CloseResult::Graceful);
            }

            self.conn.kill_client(w)?;
            Ok(CloseResult::Forced)
        }

        fn scan_windows(&self) -> Result<Vec<WindowId>, BackendError> {
            let tree = self
                .conn
                .query_tree(self.conn.setup().roots[0].root)?
                .reply()?;
            Ok(tree.children.iter().map(|&w| self.ids.intern(w)).collect())
        }

        fn change_event_mask(&self, win: WindowId, mask: u32) -> Result<(), BackendError> {
            debug!("[change_event_mask]");
            let w = self.ids.x11(win)?;
            let x_mask = event_mask_from_generic(mask);
            let aux = ChangeWindowAttributesAux::new().event_mask(x_mask);
            self.conn.change_window_attributes(w, &aux)?;
            Ok(())
        }

        fn grab_button_any_anymod(
            &self,
            win: WindowId,
            event_mask_bits: u32,
        ) -> Result<(), BackendError> {
            let x_mask = event_mask_from_generic(event_mask_bits);
            let w = self.ids.x11(win)?;
            log::info!(
                "[grab_button_any_anymod] WindowId={:?} -> X11 window=0x{:x}",
                win,
                w
            );
            self.conn.grab_button(
                false,
                w,
                x_mask,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                0u32,
                0u32,
                ButtonIndex::ANY,
                ModMask::ANY.into(),
            )?;
            Ok(())
        }

        fn grab_button(
            &self,
            win: WindowId,
            button: u8,
            event_mask_bits: u32,
            mods: Mods,
        ) -> Result<(), BackendError> {
            let x_mask = event_mask_from_generic(event_mask_bits);
            let bi = ButtonIndex::from(button);
            let numlock_val = *self.numlock_mask.lock_safe();
            let numlock_obj = KeyButMask::from(numlock_val);
            let x_mods = mods_to_x11(mods, numlock_obj);
            let w = self.ids.x11(win)?;
            let combos = lock_modifier_combinations(x_mods, KeyButMask::LOCK, numlock_obj);
            for combo in combos {
                self.conn.grab_button(
                    false,
                    w,
                    x_mask,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    0u32,
                    0u32,
                    bi,
                    ModMask::from(combo.bits()),
                )?;
            }
            Ok(())
        }

        fn map_window(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn.map_window(w)?;
            Ok(())
        }

        fn apply_window_changes(
            &self,
            win: WindowId,
            changes: WindowChanges,
        ) -> Result<(), BackendError> {
            let mut aux = ConfigureWindowAux::new();
            if let Some(x) = changes.x {
                aux = aux.x(x);
            }
            if let Some(y) = changes.y {
                aux = aux.y(y);
            }
            if let Some(w) = changes.width {
                aux = aux.width(w);
            }
            if let Some(h) = changes.height {
                aux = aux.height(h);
            }
            if let Some(b) = changes.border_width {
                aux = aux.border_width(b);
            }
            if let Some(sibling) = changes.sibling {
                aux = aux.sibling(self.ids.x11(sibling)?);
            }
            if let Some(mode) = changes.stack_mode {
                let x_mode = match mode {
                    StackMode::Above => x11rb::protocol::xproto::StackMode::ABOVE,
                    StackMode::Below => x11rb::protocol::xproto::StackMode::BELOW,
                    StackMode::TopIf => x11rb::protocol::xproto::StackMode::TOP_IF,
                    StackMode::BottomIf => x11rb::protocol::xproto::StackMode::BOTTOM_IF,
                    StackMode::Opposite => x11rb::protocol::xproto::StackMode::OPPOSITE,
                };
                aux = aux.stack_mode(x_mode);
            }

            let w = self.ids.x11(win)?;
            self.conn.configure_window(w, &aux)?;
            Ok(())
        }

        fn set_input_focus_root(&self) -> Result<(), BackendError> {
            self.conn.set_input_focus(
                InputFocus::POINTER_ROOT,
                self.root_x11,
                x11rb::CURRENT_TIME,
            )?;
            Ok(())
        }

        fn unmap_window(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn.unmap_window(w)?;
            Ok(())
        }

        fn set_input_focus(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn
                .set_input_focus(InputFocus::PARENT, w, x11rb::CURRENT_TIME)?;
            Ok(())
        }

        fn send_take_focus(&self, win: WindowId) -> Result<bool, BackendError> {
            let w = self.ids.x11(win)?;
            let reply = self
                .conn
                .get_property(false, w, self.atoms.WM_PROTOCOLS, AtomEnum::ATOM, 0, 1024)?
                .reply()?;
            let protocols = reply.value32().into_iter().flatten().collect::<Vec<_>>();
            if protocol_supported(&protocols, self.atoms.WM_TAKE_FOCUS) {
                let event = ClientMessageEvent::new(
                    32,
                    w,
                    self.atoms.WM_PROTOCOLS,
                    wm_take_focus_message(self.atoms.WM_TAKE_FOCUS, x11rb::CURRENT_TIME as u32),
                );
                self.conn
                    .send_event(false, w, EventMask::NO_EVENT, event.serialize())?;
                self.conn.flush()?;
                return Ok(true);
            }
            Ok(false)
        }

        fn get_geometry(&self, win: WindowId) -> Result<Geometry, BackendError> {
            let w = self.ids.x11(win)?;
            let reply = self.conn.get_geometry(w)?.reply()?;
            Ok(Geometry {
                x: reply.x as i32,
                y: reply.y as i32,
                w: reply.width as u32,
                h: reply.height as u32,
                border: reply.border_width as u32,
            })
        }

        fn flush(&self) -> Result<(), BackendError> {
            self.batcher.flush(&*self.conn)?;
            Ok(())
        }

        fn kill_client(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn.kill_client(w)?;
            Ok(())
        }

        fn get_window_attributes(&self, win: WindowId) -> Result<WindowAttributes, BackendError> {
            let w = self.ids.x11(win)?;
            let r = self.conn.get_window_attributes(w)?.reply()?;
            Ok(WindowAttributes {
                override_redirect: r.override_redirect,
                map_state_viewable: r.map_state == MapState::VIEWABLE,
            })
        }

        fn get_tree_child(&self, win: WindowId) -> Result<Vec<WindowId>, BackendError> {
            let w = self.ids.x11(win)?;
            let tree_reply = self.conn.query_tree(w)?.reply()?;
            Ok(tree_reply
                .children
                .iter()
                .map(|&c| self.ids.intern(c))
                .collect())
        }

        fn ungrab_all_buttons(&self, win: WindowId) -> Result<(), BackendError> {
            let w = self.ids.x11(win)?;
            self.conn
                .ungrab_button(ButtonIndex::ANY, w, ModMask::ANY.into())?;
            Ok(())
        }

        fn shape_select_input(&self, win: WindowId) -> Result<(), BackendError> {
            use x11rb::protocol::shape;
            let w = self.ids.x11(win)?;
            shape::select_input(&*self.conn, w, true)?;
            Ok(())
        }

        fn get_window_shaped(&self, win: WindowId) -> bool {
            use x11rb::protocol::shape;
            let w = match self.ids.x11(win) {
                Ok(w) => w,
                Err(_) => return false,
            };
            match shape::query_extents(&*self.conn, w) {
                Ok(cookie) => match cookie.reply() {
                    Ok(reply) => reply.bounding_shaped,
                    Err(_) => false,
                },
                Err(_) => false,
            }
        }
    }
}
