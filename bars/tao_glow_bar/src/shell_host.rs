//! Native host for one supervised, four-panel desktop shell per JWM output.
//! One runtime owns the transport and providers; auxiliary windows have no
//! separate model or command reader and disappear with their owning process.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use glow::HasContext as _;
use glutin::context::PossiblyCurrentGlContext as _;
use glutin::prelude::GlSurface as _;
use gtk::prelude::*;
use log::{info, warn};
use raw_window_handle::HasWindowHandle as _;
use tao::dpi::{PhysicalPosition, PhysicalSize};
use tao::event::{ElementState, Event, MouseButton, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn as _;
use tao::platform::unix::{WindowBuilderExtUnix as _, WindowExtUnix as _};
use tao::window::{Window, WindowBuilder};
use x11rb::connection::Connection as _;
use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ClipOrdering, ConnectionExt as _, Rectangle,
};
use x11rb::rust_connection::RustConnection;
use xbar_core::glass::{GlassBackdrop, GlassParams, fallback_rgb};
use xbar_core::presentation::Point;
use xbar_core::render::cairo::{CpuFrame, PointerInput};
use xbar_core::{
    AlignedWakeThread, BarEdge, BarPlacement, BarRuntime, BarSnapshot, DockItemGeometry,
    FourEdgeGeometry, MonitorGeometry, RuntimeUpdate, ScrollAccumulator, ScrollMode, ScrollStep,
    TransportRecoveryConfig, TransportWakeSlot, UserAction, WindowToken,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};

use crate::shell_actions::{self, ShellAction, SliderKind};
use crate::shell_scene::{self, ShellRect, ShellScene};
use crate::{OpaqueWallpaper, RootPixmapSource, UserEvent};

pub fn requested() -> bool {
    std::env::var("JWM_BAR_LAYOUT").is_ok_and(|value| value == "four-edge")
}

#[derive(Debug, Clone, Copy)]
struct Press {
    action: ShellAction,
    rect: ShellRect,
    button: MouseButton,
}

struct Panel {
    edge: BarEdge,
    window: Rc<Window>,
    presenter: crate::Presenter,
    placement: BarPlacement,
    canvas: Option<cairo::ImageSurface>,
    glass: GlassBackdrop<OpaqueWallpaper>,
    /// A non-composited X11 server cannot display alpha. Keep the sampled
    /// wallpaper sharp outside the capsule instead of exposing black pixels.
    opaque_backdrop: Option<GlassBackdrop<OpaqueWallpaper>>,
    scene: Option<ShellScene>,
    scale: f64,
    pointer: Option<(f64, f64)>,
    hovered: Option<ShellAction>,
    pressed: Option<Press>,
    dirty: bool,
    visible: bool,
    shaped_size: Option<(u32, u32, u32)>,
}

impl Panel {
    fn request_redraw(&mut self) {
        if !self.dirty {
            self.dirty = true;
            if self.visible {
                self.window.request_redraw();
            }
        }
    }

    fn global_rect(&self, rect: ShellRect) -> DockItemGeometry {
        DockItemGeometry::new(
            self.placement
                .x
                .saturating_add((rect.x * self.scale).round() as i32),
            self.placement
                .y
                .saturating_add((rect.y * self.scale).round() as i32),
            (rect.width * self.scale)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32,
            (rect.height * self.scale)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32,
        )
    }

    fn hit(&self, x: f64, y: f64) -> Option<(ShellAction, ShellRect)> {
        self.scene
            .as_ref()?
            .hit(x, y)
            .map(|hit| (hit.action, hit.rect))
    }

    fn redraw(&mut self, snapshot: &BarSnapshot, connection: &RustConnection) -> Result<()> {
        if !self.visible {
            return Ok(());
        }
        self.dirty = false;
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        crate::bgra_frame_len(size.width, size.height)?;
        if self.canvas.as_ref().is_none_or(|surface| {
            surface.width() != size.width as i32 || surface.height() != size.height as i32
        }) {
            self.canvas = Some(cairo::ImageSurface::create(
                cairo::Format::ARgb32,
                size.width as i32,
                size.height as i32,
            )?);
        }
        let logical_width = f64::from(size.width) / self.scale;
        let logical_height = f64::from(size.height) / self.scale;
        let radius = (shell_scene::panel_radius(self.edge, logical_width, logical_height)
            * self.scale)
            .round() as u32;
        if self.shaped_size != Some((size.width, size.height, radius)) {
            if let Err(error) = set_round_shape(connection, &self.window, size, radius) {
                warn!(
                    "native rounded shape unavailable; retaining Cairo alpha clipping: {error:#}"
                );
            }
            self.shaped_size = Some((size.width, size.height, radius));
        }
        let backdrop =
            self.glass
                .ensure(self.placement.x, self.placement.y, size.width, size.height);
        let surface = self.canvas.as_mut().expect("allocated Cairo canvas");
        {
            let context = cairo::Context::new(&*surface)?;
            context.scale(self.scale, self.scale);
            self.scene = Some(shell_scene::render(
                &context,
                self.edge,
                logical_width,
                logical_height,
                snapshot,
                backdrop,
                self.hovered,
                self.pressed.map(|press| press.action),
            )?);
            if let Some(sharp) = self.opaque_backdrop.as_mut().and_then(|source| {
                source.ensure(self.placement.x, self.placement.y, size.width, size.height)
            }) {
                context.identity_matrix();
                context.set_operator(cairo::Operator::DestOver);
                context.set_source_surface(sharp, 0.0, 0.0)?;
                context.paint()?;
            }
        }
        present_surface(&mut self.presenter, surface)
    }
}

fn present_surface(
    presenter: &mut crate::Presenter,
    image: &mut cairo::ImageSurface,
) -> Result<()> {
    image.flush();
    let width = image.width() as u32;
    let height = image.height() as u32;
    let stride = image.stride() as u32;
    let data = image.data()?;
    match presenter {
        crate::Presenter::OpenGl(presenter) => {
            // Each panel owns a context. GTK may deliver redraws in any order.
            presenter.context.make_current(&presenter.surface)?;
            unsafe {
                presenter.gl.viewport(0, 0, width as i32, height as i32);
            }
            presenter.upload.upload(
                &presenter.gl,
                presenter.texture,
                &CpuFrame {
                    data: &data,
                    width,
                    height,
                    stride,
                    damage: None,
                },
            )?;
            crate::draw_fullscreen_quad(
                &presenter.gl,
                presenter.vao,
                presenter.program,
                presenter.texture,
            );
            presenter.surface.swap_buffers(&presenter.context)?;
        }
        crate::Presenter::Software(presenter) => {
            let mut buffer = presenter
                .surface
                .buffer_mut()
                .map_err(|error| anyhow::anyhow!("software frame: {error}"))?;
            anyhow::ensure!(
                buffer.len() >= width as usize * height as usize,
                "undersized software frame"
            );
            for y in 0..height as usize {
                let source: &[u32] = bytemuck::cast_slice(
                    &data[y * stride as usize..y * stride as usize + width as usize * 4],
                );
                let target = y * width as usize;
                buffer[target..target + width as usize].copy_from_slice(source);
            }
            buffer
                .present()
                .map_err(|error| anyhow::anyhow!("software present: {error}"))?;
        }
    }
    Ok(())
}

fn set_round_shape(
    connection: &RustConnection,
    window: &Window,
    size: PhysicalSize<u32>,
    radius: u32,
) -> Result<()> {
    let Some(xid) = crate::raw_x11_window_id(window.window_handle()?.as_raw()) else {
        return Ok(());
    };
    // X11 rectangle fields are 16-bit. Oversized native windows retain their
    // Cairo alpha clip rather than wrapping an XShape request.
    if size.width > i16::MAX as u32 || size.height > i16::MAX as u32 {
        return Ok(());
    }
    let r = f64::from(radius.min(size.width / 2).min(size.height / 2));
    let rectangles: Vec<_> = (0..size.height)
        .map(|y| {
            let center_y = f64::from(y) + 0.5;
            let distance = if center_y < r {
                r - center_y
            } else if center_y > f64::from(size.height) - r {
                center_y - (f64::from(size.height) - r)
            } else {
                0.0
            };
            let inset = (r - (r * r - distance * distance).max(0.0).sqrt())
                .floor()
                .max(0.0) as u32;
            Rectangle {
                x: inset as i16,
                y: y as i16,
                width: size.width.saturating_sub(2 * inset).max(1) as u16,
                height: 1,
            }
        })
        .collect();
    // Keep the drawable rectangular: a bounding-shape change can replace its
    // redirected pixmap, leaving undefined corner pixels in a compositor's
    // texture import. Cairo clips the visible glass; only input is shaped.
    connection
        .shape_rectangles(
            SO::SET,
            SK::INPUT,
            ClipOrdering::UNSORTED,
            xid,
            0,
            0,
            &rectangles,
        )?
        .check()?;
    connection.flush()?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Preview {
    window: WindowToken,
    session: u64,
    generation: u64,
    geometry: DockItemGeometry,
}

struct Host {
    panels: Vec<Panel>,
    runtime: BarRuntime,
    effects: EffectRouter,
    connection: Rc<RustConnection>,
    output: MonitorGeometry,
    padding: u32,
    thickness: u32,
    wake: TransportWakeSlot,
    proxy: EventLoopProxy<UserEvent>,
    dock_epoch: Option<(u64, u64, u64)>,
    dock_geometry: HashMap<Option<WindowToken>, DockItemGeometry>,
    dock_withdrawals: Vec<Option<WindowToken>>,
    dock_retry_after: Option<Instant>,
    preview: Option<Preview>,
    preview_ack: Option<Preview>,
    preview_sent: Instant,
    scroll: ScrollAccumulator<(usize, u8)>,
    last_slider: Option<(SliderKind, u8, Instant)>,
    visible: bool,
}

impl Host {
    fn redraw_all(&mut self) {
        for panel in &mut self.panels {
            panel.request_redraw();
        }
    }

    fn update(&mut self, update: RuntimeUpdate) {
        let mut geometry = None;
        let redraw = self
            .effects
            .route::<_, std::convert::Infallible>(update, |request| {
                if let GeometryRequest::Apply(value) = request {
                    geometry = Some(value);
                }
                Ok(())
            })
            .expect("geometry collection is infallible");
        if let Some(geometry) = geometry {
            // The transport's horizontal rectangle already contains padding.
            let next = MonitorGeometry {
                x: geometry.x.saturating_sub(self.padding as i32),
                y: geometry.y.saturating_sub(self.padding as i32),
                width: geometry.width.saturating_add(2 * self.padding),
                height: geometry.height,
            };
            if next != self.output {
                self.output = next;
                self.reposition();
            }
        }
        if redraw {
            self.redraw_all();
        }
    }

    fn reposition(&mut self) {
        let geometry = FourEdgeGeometry::new(self.output, self.thickness, self.padding);
        for panel in &mut self.panels {
            panel.placement = geometry.panel(panel.edge);
            let p = panel.placement;
            panel
                .window
                .set_outer_position(PhysicalPosition::new(p.x, p.y));
            panel
                .window
                .set_inner_size(PhysicalSize::new(p.width, p.height));
            panel.glass.cache_mut().invalidate();
            panel.request_redraw();
        }
        self.dock_retry_after = None;
    }

    fn synchronize_visibility(&mut self) {
        let main = &self.panels[0];
        let Some(position) = main.window.outer_position().ok() else {
            return;
        };
        let visible = (i64::from(position.x) - i64::from(main.placement.x)).abs() <= 2
            && (i64::from(position.y) - i64::from(main.placement.y)).abs() <= 2;
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        for (index, panel) in self.panels.iter_mut().enumerate() {
            panel.visible = visible;
            // JWM owns mapping and parking of the main status-bar window.
            if index != 0 {
                panel.window.set_visible(visible);
            }
            panel.pressed = None;
            panel.hovered = None;
            panel.pointer = None;
            if visible {
                panel.dirty = false;
                panel.request_redraw();
            }
        }
        self.last_slider = None;
        if !visible {
            self.set_preview(None);
            self.report_dock();
        }
    }

    fn tick(&mut self) {
        let mut update = self.runtime.tick();
        update.merge(self.runtime.poll_transport());
        self.update(update);
        self.sync_wake();
        self.synchronize_visibility();
        // Wallpaper sampling is cached per panel. A root-pixmap replacement
        // or animated wallpaper is observed without a continuously running UI.
        self.redraw_all();
        if !self.visible {
            self.report_dock();
        }
        if (self.preview.is_some() || self.preview_ack.is_some())
            && self.preview_sent.elapsed() >= Duration::from_secs(1)
        {
            self.send_preview(true);
        }
    }

    fn sync_wake(&mut self) {
        let proxy = self.proxy.clone();
        if let Err(error) = self.wake.sync(&self.runtime, move |ack| {
            proxy.send_event(UserEvent::SharedUpdated(ack))
        }) {
            warn!("four-edge transport wake: {error}");
        }
    }

    fn dispatch(&mut self, index: usize, action: ShellAction, rect: ShellRect) {
        let geometry = self.panels[index].global_rect(rect);
        let update = shell_actions::dispatch_with_geometry(action, geometry, &mut self.runtime);
        self.update(update);
    }

    fn slider(&mut self, kind: SliderKind, rect: ShellRect, y: f64, final_event: bool) {
        let Some(percent) =
            shell_actions::slider_percent_from_y(y as f32, rect.y as f32, rect.height as f32)
        else {
            return;
        };
        if self.last_slider.is_some_and(|(previous, value, when)| {
            !final_event
                && previous == kind
                && (value == percent || when.elapsed() < Duration::from_millis(35))
        }) {
            return;
        }
        self.last_slider = Some((kind, percent, Instant::now()));
        let update = shell_actions::slider(kind, percent, &mut self.runtime);
        self.update(update);
    }

    fn pointer_move(&mut self, index: usize, x: f64, y: f64) {
        let panel = &mut self.panels[index];
        panel.pointer = Some((x, y));
        let hit = panel.hit(x, y);
        let next = hit.map(|(action, _)| action);
        if panel.hovered != next {
            panel.hovered = next;
            panel.request_redraw();
        }
        let pressed = panel.pressed;
        let preview = hit.and_then(|(action, rect)| match action {
            ShellAction::Restore {
                window,
                wm_session_id,
                minimized_generation,
            } => Some(Preview {
                window,
                session: wm_session_id,
                generation: minimized_generation,
                geometry: panel.global_rect(rect),
            }),
            _ => None,
        });
        self.set_preview(preview);
        if let Some(Press {
            action: ShellAction::Slider(kind),
            rect,
            button: MouseButton::Left,
        }) = pressed
        {
            self.slider(kind, rect, y, false);
        }
    }

    fn pointer_button(&mut self, index: usize, state: ElementState, button: MouseButton) {
        let panel = &mut self.panels[index];
        // GTK can coalesce motion when the pointer enters another panel and
        // immediately presses a button. Resolve the native local coordinates
        // on button events instead of activating a stale last-motion target.
        if let Ok(handle) = panel.window.window_handle()
            && let Some(xid) = crate::raw_x11_window_id(handle.as_raw())
            && let Ok(cookie) = self.connection.query_pointer(xid)
            && let Ok(pointer) = cookie.reply()
        {
            panel.pointer = Some((
                f64::from(pointer.win_x) / panel.scale,
                f64::from(pointer.win_y) / panel.scale,
            ));
        }
        let Some((x, y)) = panel.pointer else {
            return;
        };
        let hit = panel.hit(x, y);
        log::debug!(
            "four-edge pointer {:?} {:?} {:?} at {x:.1},{y:.1} ({} targets): hit={hit:?}, press={:?}",
            panel.edge,
            state,
            button,
            panel.scene.as_ref().map_or(0, |scene| scene.targets.len()),
            panel.pressed,
        );
        match state {
            ElementState::Pressed => {
                if !matches!(button, MouseButton::Left | MouseButton::Right) {
                    return;
                }
                panel.pressed = hit.map(|(action, rect)| Press {
                    action,
                    rect,
                    button,
                });
                panel.request_redraw();
                if button == MouseButton::Left
                    && let Some((ShellAction::Slider(kind), rect)) = hit
                {
                    self.last_slider = None;
                    self.slider(kind, rect, y, false);
                }
            }
            ElementState::Released => {
                let pressed = panel.pressed.take();
                panel.request_redraw();
                let Some(press) = pressed.filter(|press| press.button == button) else {
                    return;
                };
                if let ShellAction::Slider(kind) = press.action
                    && button == MouseButton::Left
                {
                    self.slider(kind, press.rect, y, true);
                    self.last_slider = None;
                    return;
                }
                let action = released_action(
                    press.action,
                    press.button,
                    button,
                    hit.map(|(action, _)| action),
                );
                if let Some(action) = action {
                    self.dispatch(index, action, press.rect);
                }
            }
            _ => {}
        }
    }

    fn wheel(
        &mut self,
        index: usize,
        delta: tao::event::MouseScrollDelta,
        phase: tao::event::TouchPhase,
    ) {
        let panel = &self.panels[index];
        let Some((x, y)) = panel.pointer else {
            return;
        };
        let Some((action, rect)) = panel.hit(x, y) else {
            return;
        };
        let PointerInput::Wheel { delta, phase, .. } = crate::tao_scroll::pointer_scroll(
            Point::new(x as f32, y as f32),
            delta,
            phase,
            panel.scale,
        ) else {
            return;
        };
        let adjustment = matches!(
            action,
            ShellAction::Slider(_) | ShellAction::User(UserAction::OpenAudioControl)
        );
        let group = match action {
            ShellAction::Slider(SliderKind::Volume)
            | ShellAction::User(UserAction::OpenAudioControl) => 1,
            ShellAction::Slider(SliderKind::Brightness) => 2,
            ShellAction::CycleLayout => 3,
            _ => 0,
        };
        let Some(step) = self.scroll.step(
            (index, group),
            delta,
            phase,
            if adjustment {
                ScrollMode::Adjustment
            } else {
                ScrollMode::Navigation
            },
            true,
            Instant::now(),
        ) else {
            return;
        };
        let user = match action {
            ShellAction::Slider(SliderKind::Volume)
            | ShellAction::User(UserAction::OpenAudioControl) => Some(if step == ScrollStep::Up {
                UserAction::VolumeUp
            } else {
                UserAction::VolumeDown
            }),
            ShellAction::Slider(SliderKind::Brightness) => Some(if step == ScrollStep::Up {
                UserAction::BrightnessUp
            } else {
                UserAction::BrightnessDown
            }),
            ShellAction::User(UserAction::ViewTagOn { .. } | UserAction::ViewTag(_))
            | ShellAction::NextWorkspace => {
                let snapshot = self.runtime.snapshot();
                let count = snapshot.tags.len();
                if count == 0 {
                    None
                } else {
                    let current = snapshot.active_tag.map_or(0, |tag| tag.index());
                    let next = if step == ScrollStep::Up {
                        (current + count - 1) % count
                    } else {
                        (current + 1) % count
                    };
                    xbar_core::TagId::new(next).map(|tag| UserAction::ViewTagOn {
                        tag,
                        monitor: snapshot.monitor,
                    })
                }
            }
            ShellAction::CycleLayout => {
                self.dispatch(index, ShellAction::CycleLayout, rect);
                return;
            }
            _ => None,
        };
        if let Some(action) = user {
            let update = self.runtime.dispatch(action);
            self.update(update);
        }
    }

    fn set_preview(&mut self, next: Option<Preview>) {
        if self.preview == next {
            return;
        }
        self.preview = next;
        self.send_preview(false);
    }

    fn send_preview(&mut self, renewal: bool) {
        let snapshot = self.runtime.snapshot();
        self.preview = self
            .preview
            .filter(|preview| self.visible && preview_is_current(*preview, &snapshot));
        self.preview_ack = self
            .preview_ack
            .filter(|preview| preview_is_current(*preview, &snapshot));
        // An acknowledgement is retained until a leave is delivered. If the
        // ring is full, a later Tick retries it even after the pointer left.
        if self.preview_ack != self.preview {
            if let Some(previous) = self.preview_ack {
                if !self.deliver_preview(previous, false, false) {
                    return;
                }
                self.preview_ack = None;
            }
            if let Some(preview) = self.preview
                && self.deliver_preview(preview, true, false)
            {
                self.preview_ack = Some(preview);
            }
        } else if renewal && let Some(preview) = self.preview_ack {
            self.deliver_preview(preview, true, true);
        }
    }

    fn deliver_preview(&mut self, preview: Preview, visible: bool, renewal: bool) -> bool {
        let update = self.runtime.dispatch(UserAction::PreviewWindow {
            window: preview.window,
            wm_session_id: preview.session,
            minimized_generation: preview.generation,
            visible,
            renewal,
            geometry: preview.geometry,
        });
        let delivered = update.issues.is_empty() && update.platform_effects.is_empty();
        self.preview_sent = Instant::now();
        self.update(update);
        delivered
    }

    fn report_dock(&mut self) {
        let snapshot = self.runtime.snapshot();
        if !snapshot.wm_available {
            self.dock_epoch = None;
            self.dock_geometry.clear();
            self.dock_withdrawals.clear();
            self.dock_retry_after = None;
            self.preview = None;
            self.preview_ack = None;
            return;
        }
        if self
            .dock_retry_after
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }
        let Some(generation) = snapshot.wm_sequence else {
            return;
        };
        let key = (
            snapshot.wm_session_id,
            generation,
            self.runtime.transport_generation(),
        );
        let Some(panel) = self
            .panels
            .iter()
            .find(|panel| panel.edge == BarEdge::Bottom)
        else {
            return;
        };
        let Some(scene) = panel.scene.as_ref() else {
            return;
        };
        let mut reports = Vec::new();
        if self.visible {
            reports.push((
                None,
                DockItemGeometry::new(
                    panel.placement.x,
                    panel.placement.y,
                    panel.placement.width,
                    panel.placement.height,
                ),
            ));
            reports.extend(
                scene
                    .targets
                    .iter()
                    .filter_map(|hit| match hit.action {
                        ShellAction::Restore {
                            window,
                            wm_session_id,
                            minimized_generation,
                        } if wm_session_id == key.0 && minimized_generation == key.1 => {
                            Some((Some(window), panel.global_rect(hit.rect)))
                        }
                        _ => None,
                    })
                    .take(xbar_core::MAX_MODEL_MINIMIZED_WINDOWS),
            );
        }
        // Only a freshly painted target may bind a window in the new epoch.
        let hovered_preview = self
            .visible
            .then_some(panel.pointer)
            .flatten()
            .and_then(|(x, y)| {
                panel.hit(x, y).and_then(|(action, rect)| match action {
                    ShellAction::Restore {
                        window,
                        wm_session_id,
                        minimized_generation,
                    } => Some(Preview {
                        window,
                        session: wm_session_id,
                        generation: minimized_generation,
                        geometry: panel.global_rect(rect),
                    }),
                    _ => None,
                })
            });
        let same_session = self.dock_epoch.is_some_and(|previous| previous.0 == key.0);
        if !same_session {
            self.dock_withdrawals.clear();
        }
        if self.dock_epoch != Some(key) {
            if self.dock_epoch.is_some_and(|previous| previous.2 != key.2)
                && hovered_preview.is_some()
            {
                // A lease may have expired while the channel was absent.
                // Re-enter from the fresh scene rather than sending a renewal
                // which cannot create a compositor preview by itself.
                self.preview_ack = None;
            }
            if same_session {
                for &window in self.dock_geometry.keys() {
                    if !reports.iter().any(|(desired, _)| *desired == window)
                        && !self.dock_withdrawals.contains(&window)
                    {
                        self.dock_withdrawals.push(window);
                    }
                }
            }
            self.dock_geometry.clear();
            self.dock_epoch = Some(key);
        }
        self.dock_withdrawals
            .retain(|window| !reports.iter().any(|(desired, _)| desired == window));
        for &window in self.dock_geometry.keys() {
            if !reports.iter().any(|(desired, _)| *desired == window)
                && !self.dock_withdrawals.contains(&window)
            {
                self.dock_withdrawals.push(window);
            }
        }
        self.dock_withdrawals
            .truncate(xbar_core::MAX_MODEL_MINIMIZED_WINDOWS + 1);
        for window in self.dock_withdrawals.clone() {
            let update = self.runtime.dispatch(UserAction::SetDockGeometry {
                window,
                wm_session_id: key.0,
                minimized_generation: key.1,
                geometry: DockItemGeometry::default(),
            });
            let delivered = update.issues.is_empty() && update.platform_effects.is_empty();
            self.update(update);
            if delivered {
                self.dock_geometry.remove(&window);
                self.dock_withdrawals.retain(|pending| *pending != window);
            } else {
                self.dock_retry_after = Some(Instant::now() + Duration::from_millis(100));
                return;
            }
        }
        for (window, geometry) in reports {
            if self.dock_geometry.get(&window) == Some(&geometry) {
                continue;
            }
            let update = self.runtime.dispatch(UserAction::SetDockGeometry {
                window,
                wm_session_id: key.0,
                minimized_generation: key.1,
                geometry,
            });
            let delivered = update.issues.is_empty() && update.platform_effects.is_empty();
            if delivered {
                self.dock_geometry.insert(window, geometry);
            }
            self.update(update);
            if !delivered {
                self.dock_retry_after = Some(Instant::now() + Duration::from_millis(100));
                return;
            }
        }
        self.dock_retry_after = None;
        self.set_preview(hovered_preview);
    }
}

fn preview_is_current(preview: Preview, snapshot: &BarSnapshot) -> bool {
    snapshot.wm_available
        && snapshot.wm_session_id == preview.session
        && snapshot.wm_sequence == Some(preview.generation)
        && snapshot.minimized_windows.iter().any(|window| {
            window.token == preview.window
                && window.flags & xbar_core::MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE != 0
        })
}

fn released_action(
    pressed: ShellAction,
    pressed_button: MouseButton,
    released_button: MouseButton,
    hit: Option<ShellAction>,
) -> Option<ShellAction> {
    if pressed_button != released_button || hit != Some(pressed) {
        return None;
    }
    if released_button == MouseButton::Right {
        secondary_action(pressed)
    } else {
        Some(pressed)
    }
}

fn secondary_action(action: ShellAction) -> Option<ShellAction> {
    Some(match action {
        ShellAction::User(UserAction::ViewTagOn { tag, monitor }) => {
            ShellAction::User(UserAction::ToggleTagOn { tag, monitor })
        }
        ShellAction::User(UserAction::ViewTag(tag)) => {
            ShellAction::User(UserAction::ToggleTag(tag))
        }
        ShellAction::User(UserAction::OpenShellHub(xbar_core::ShellRoute::Calendar)) => {
            ShellAction::User(UserAction::ToggleSeconds)
        }
        ShellAction::User(UserAction::OpenAudioControl)
        | ShellAction::Slider(SliderKind::Volume) => ShellAction::User(UserAction::ToggleMute),
        _ => return None,
    })
}

fn env_u32(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}
fn env_i32(name: &str, fallback: i32) -> i32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xbar_core::{BarModel, MinimizedWindow, MonitorId, ShellRoute, TagId};

    #[test]
    fn right_slider_click_mutes_but_mismatched_release_does_not_activate() {
        let slider = ShellAction::Slider(SliderKind::Volume);
        assert_eq!(
            released_action(slider, MouseButton::Right, MouseButton::Right, Some(slider)),
            Some(ShellAction::User(UserAction::ToggleMute)),
        );
        assert_eq!(
            released_action(slider, MouseButton::Left, MouseButton::Right, Some(slider)),
            None,
        );
        assert_eq!(
            released_action(slider, MouseButton::Right, MouseButton::Right, None),
            None
        );
    }

    #[test]
    fn secondary_workspace_and_calendar_actions_keep_their_semantics() {
        let tag = TagId::new(2).unwrap();
        assert_eq!(
            secondary_action(ShellAction::User(UserAction::ViewTagOn {
                tag,
                monitor: MonitorId(4)
            })),
            Some(ShellAction::User(UserAction::ToggleTagOn {
                tag,
                monitor: MonitorId(4)
            })),
        );
        assert_eq!(
            secondary_action(ShellAction::User(UserAction::OpenShellHub(
                ShellRoute::Calendar
            ))),
            Some(ShellAction::User(UserAction::ToggleSeconds)),
        );
        let old = ShellAction::Restore {
            window: WindowToken(12),
            wm_session_id: 7,
            minimized_generation: 11,
        };
        let replaced = ShellAction::Restore {
            window: WindowToken(12),
            wm_session_id: 7,
            minimized_generation: 12,
        };
        assert_eq!(
            released_action(old, MouseButton::Left, MouseButton::Left, Some(replaced)),
            None
        );
    }

    #[test]
    fn preview_lease_requires_the_same_session_generation_and_available_token() {
        let mut snapshot = BarModel::default().snapshot();
        snapshot.wm_available = true;
        snapshot.wm_session_id = 7;
        snapshot.wm_sequence = Some(11);
        snapshot.minimized_windows = vec![MinimizedWindow {
            token: WindowToken(12),
            flags: xbar_core::MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
            ..MinimizedWindow::default()
        }];
        let preview = Preview {
            window: WindowToken(12),
            session: 7,
            generation: 11,
            geometry: DockItemGeometry::new(128, 712, 144, 52),
        };
        assert!(preview_is_current(preview, &snapshot));
        snapshot.wm_sequence = Some(12);
        assert!(!preview_is_current(preview, &snapshot));
        snapshot.wm_sequence = Some(11);
        snapshot.wm_session_id = 8;
        assert!(!preview_is_current(preview, &snapshot));
        snapshot.wm_session_id = 7;
        snapshot.minimized_windows[0].flags = 0;
        assert!(!preview_is_current(preview, &snapshot));
        snapshot.minimized_windows[0].flags = xbar_core::MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE;
        snapshot.wm_available = false;
        assert!(!preview_is_current(preview, &snapshot));
    }
}

pub fn run(config: xbar_core::config::BarConfig, shared_path: String) -> Result<()> {
    let (connection, screen_number) =
        x11rb::connect(None).context("four-edge shell requires an X11 display")?;
    let connection = Rc::new(connection);
    let compositor_active = crate::compositor_active(&connection, screen_number);
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let tick_proxy = proxy.clone();
    let _ticks = AlignedWakeThread::spawn(move || tick_proxy.send_event(UserEvent::Tick))?;
    let monitor = event_loop
        .primary_monitor()
        .or_else(|| event_loop.available_monitors().next());
    let fallback_size = monitor
        .as_ref()
        .map(|m| m.size())
        .unwrap_or(PhysicalSize::new(1280, 800));
    let fallback_origin = monitor
        .as_ref()
        .map(|m| m.position())
        .unwrap_or(PhysicalPosition::new(0, 0));
    let output = MonitorGeometry {
        x: env_i32("JWM_BAR_OUTPUT_X", fallback_origin.x),
        y: env_i32("JWM_BAR_OUTPUT_Y", fallback_origin.y),
        width: env_u32("JWM_BAR_OUTPUT_WIDTH", fallback_size.width).max(1),
        height: env_u32("JWM_BAR_OUTPUT_HEIGHT", fallback_size.height).max(1),
    };
    let thickness = env_u32("JWM_BAR_THICKNESS", 42).max(1);
    let padding =
        env_u32("JWM_BAR_PADDING", 5).min((output.width.min(output.height).max(1) - 1) / 2);
    let geometry = FourEdgeGeometry::new(output, thickness, padding);
    let runtime = if shared_path.is_empty() {
        BarRuntime::new(config.model_config())?
    } else {
        BarRuntime::with_managed_transport(
            config.model_config(),
            TransportRecoveryConfig::new(shared_path, crate::TRANSPORT_RETRY_INTERVAL)?,
        )?
    };
    let mut panels = Vec::new();
    for edge in [BarEdge::Top, BarEdge::Left, BarEdge::Right, BarEdge::Bottom] {
        let placement = geometry.panel(edge);
        let title = if edge == BarEdge::Top {
            "tao_glow_bar".to_owned()
        } else {
            format!("tao_glow_bar-aux-{}", edge.as_str())
        };
        let window = Rc::new(
            WindowBuilder::new()
                .with_title(&title)
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(false)
                .with_transparent(true)
                .with_skip_taskbar(true)
                .with_inner_size(PhysicalSize::new(placement.width, placement.height))
                .with_position(PhysicalPosition::new(placement.x, placement.y))
                .build(&event_loop)?,
        );
        window.gtk_window().set_accept_focus(false);
        window.gtk_window().realize();
        let xid = crate::raw_x11_window_id(window.window_handle()?.as_raw())
            .context("four-edge shell requires X11 native windows")?;
        if edge != BarEdge::Top {
            connection
                .change_window_attributes(
                    xid,
                    &ChangeWindowAttributesAux::new().override_redirect(1),
                )?
                .check()?;
        }
        let mut hints = crate::native_x11::DockHints::default();
        hints.sync(window.as_ref(), placement, edge, &title)?;
        connection.flush()?;
        window.set_visible(true);
        let physical_size = window.inner_size();
        let presenter = match crate::GlPresenter::new(&window, physical_size) {
            Ok(gl) => crate::Presenter::OpenGl(Box::new(gl)),
            Err(error) => {
                warn!("four-edge GL unavailable: {error:#}; using software");
                crate::Presenter::Software(crate::SoftwarePresenter::new(&window, physical_size)?)
            }
        };
        let tint = fallback_rgb(config.theme);
        let opaque_backdrop = if !compositor_active || !crate::window_is_argb(&connection, xid) {
            let source =
                match config
                    .glass
                    .file_source(fallback_size.width, fallback_size.height, tint)
                {
                    Some(file) => OpaqueWallpaper::FileThenRoot {
                        file,
                        root: RootPixmapSource::new(Rc::clone(&connection), screen_number),
                    },
                    None => OpaqueWallpaper::Root(RootPixmapSource::new(
                        Rc::clone(&connection),
                        screen_number,
                    )),
                };
            Some(GlassBackdrop::new(source, GlassParams::clear()).with_fallback(tint))
        } else {
            None
        };
        let source = match config
            .glass
            .file_source(fallback_size.width, fallback_size.height, tint)
        {
            Some(file) => OpaqueWallpaper::FileThenRoot {
                file,
                root: RootPixmapSource::new(Rc::clone(&connection), screen_number),
            },
            None => {
                OpaqueWallpaper::Root(RootPixmapSource::new(Rc::clone(&connection), screen_number))
            }
        };
        panels.push(Panel {
            edge,
            scale: window.scale_factor(),
            window,
            presenter,
            placement,
            canvas: None,
            glass: GlassBackdrop::new(source, config.glass.params()).with_fallback(tint),
            opaque_backdrop,
            scene: None,
            pointer: None,
            hovered: None,
            pressed: None,
            dirty: false,
            visible: true,
            shaped_size: None,
        });
    }
    let mut host = Host {
        panels,
        runtime,
        effects: EffectRouter::default(),
        connection,
        output,
        padding,
        thickness,
        wake: TransportWakeSlot::new(true),
        proxy,
        dock_epoch: None,
        dock_geometry: HashMap::new(),
        dock_withdrawals: Vec::new(),
        dock_retry_after: None,
        preview: None,
        preview_ack: None,
        preview_sent: Instant::now(),
        scroll: ScrollAccumulator::with_policy(config.presentation.scroll),
        last_slider: None,
        visible: true,
    };
    host.tick();
    info!(
        "four-edge desktop shell ready on output {}x{} at {},{}",
        output.width, output.height, output.x, output.y
    );
    let exit = event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Tick) => host.tick(),
            Event::UserEvent(UserEvent::SharedUpdated(ack)) => {
                let update = host.runtime.poll_transport();
                ack.ack();
                host.update(update);
                host.sync_wake();
            }
            Event::WindowEvent {
                window_id, event, ..
            } => {
                let Some(index) = host
                    .panels
                    .iter()
                    .position(|panel| panel.window.id() == window_id)
                else {
                    return;
                };
                match event {
                    WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                        *control_flow = ControlFlow::Exit
                    }
                    WindowEvent::Resized(size) => {
                        if size.width > 0
                            && size.height > 0
                            && crate::bgra_frame_len(size.width, size.height).is_ok()
                        {
                            // GL viewport must be changed with this panel's context current.
                            if let crate::Presenter::OpenGl(gl) = &host.panels[index].presenter {
                                let _ = gl.context.make_current(&gl.surface);
                            }
                            host.panels[index].presenter.resize(size);
                            host.panels[index].request_redraw();
                        }
                    }
                    WindowEvent::Moved(_) if index == 0 => host.synchronize_visibility(),
                    WindowEvent::ScaleFactorChanged {
                        scale_factor,
                        new_inner_size,
                        ..
                    } => {
                        let placement = host.panels[index].placement;
                        *new_inner_size = PhysicalSize::new(placement.width, placement.height);
                        host.panels[index].scale = scale_factor;
                        host.panels[index].shaped_size = None;
                        host.panels[index].pressed = None;
                        host.panels[index].request_redraw();
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        let position = position.to_logical::<f64>(host.panels[index].scale);
                        host.pointer_move(index, position.x, position.y);
                    }
                    WindowEvent::CursorLeft { .. } => {
                        let panel = &mut host.panels[index];
                        log::debug!(
                            "four-edge pointer left {:?}: press={:?}",
                            panel.edge,
                            panel.pressed
                        );
                        panel.hovered = None;
                        if !panel
                            .pressed
                            .is_some_and(|press| matches!(press.action, ShellAction::Slider(_)))
                        {
                            panel.pointer = None;
                            panel.pressed = None;
                        }
                        panel.request_redraw();
                        host.set_preview(None);
                    }
                    WindowEvent::Focused(false) => {
                        log::debug!(
                            "four-edge focus left {:?}: press={:?}",
                            host.panels[index].edge,
                            host.panels[index].pressed
                        );
                        host.panels[index].pressed = None;
                        host.panels[index].request_redraw();
                    }
                    WindowEvent::MouseInput { state, button, .. } => {
                        host.pointer_button(index, state, button)
                    }
                    WindowEvent::MouseWheel { delta, phase, .. } => host.wheel(index, delta, phase),
                    _ => {}
                }
            }
            Event::RedrawRequested(window_id) => {
                if let Some(index) = host
                    .panels
                    .iter()
                    .position(|panel| panel.window.id() == window_id)
                {
                    let snapshot = host.runtime.snapshot();
                    if let Err(error) = host.panels[index].redraw(&snapshot, &host.connection) {
                        warn!("four-edge redraw: {error:#}");
                    }
                    if host.panels[index].edge == BarEdge::Bottom {
                        host.report_dock();
                    }
                }
            }
            _ => {}
        }
    });
    anyhow::ensure!(exit == 0, "four-edge event loop exited with status {exit}");
    Ok(())
}
