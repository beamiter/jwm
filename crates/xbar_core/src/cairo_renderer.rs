//! Cairo/Pango renderer for the backend-neutral presentation scene.
//!
//! Scene coordinates are logical units.  A frontend that needs HiDPI output
//! should configure the Cairo context transform before calling [`CairoRenderer::render`].

use anyhow::Result;
use cairo::{Context, Extend, Filter, Format, ImageSurface, LineCap, LineJoin, Operator};
use pango::prelude::FontMapExt as _;
use pango::{FontDescription, Layout};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use crate::presentation::{
    Damage, ImageSource, InteractionState, LayoutEngine, Point, PointerAction, Rect, Rgba, Scene,
    SceneNode, Size, Stroke, TextAlign, TextMeasurer,
};
use crate::{
    BarModel, BarRuntime, BarSnapshot, DockReporter, DockReporterInput, RuntimeUpdate, UserAction,
};

/// Renders a [`Scene`] into an existing Cairo context.
#[derive(Debug, Clone)]
pub struct CairoRenderer {
    font: FontDescription,
    background_opacity: Option<f64>,
    /// Decoded scene images, keyed by [`ImageSource::key`].
    ///
    /// Rendering takes `&self`, and an icon is drawn on every frame while the
    /// window stays focused, so the decode has to be remembered behind shared
    /// mutability rather than repeated. Failures are cached as `None`: a file
    /// this build cannot decode will not start decoding between two frames,
    /// and retrying it every frame would put image decoding in the hot path
    /// for exactly the icons that never appear.
    images: RefCell<HashMap<u64, Option<Rc<ImageSurface>>>>,
}

impl CairoRenderer {
    #[must_use]
    pub fn new(font: FontDescription) -> Self {
        Self {
            font,
            background_opacity: None,
            images: RefCell::new(HashMap::new()),
        }
    }

    /// Construct a renderer whose font description also names an icon family.
    ///
    /// `configured` is the host's explicit choice; `None` lets
    /// [`crate::icon_font`] pick an installed patched font. Either way the
    /// private-use glyphs in bar labels stop depending on which font the
    /// generic fontconfig sort happens to hand this session.
    #[must_use]
    pub fn with_icon_font(font: FontDescription, configured: Option<&str>) -> Self {
        Self::new(crate::icon_font::with_icon_fallback(&font, configured))
    }

    /// Multiply background-node alpha by `opacity`.
    ///
    /// Other scene primitives retain their own alpha.  Non-finite values fall
    /// back to fully opaque and finite values are clamped to `0..=1`.
    #[must_use]
    pub fn with_background_opacity(mut self, opacity: f64) -> Self {
        self.background_opacity = Some(normalize_opacity(opacity));
        self
    }

    #[must_use]
    pub const fn font(&self) -> &FontDescription {
        &self.font
    }

    #[must_use]
    pub const fn background_opacity(&self) -> Option<f64> {
        self.background_opacity
    }

    pub fn set_background_opacity(&mut self, opacity: Option<f64>) {
        self.background_opacity = opacity.map(normalize_opacity);
    }

    /// Create a text measurer using the exact Pango context associated with
    /// this Cairo context.
    #[must_use]
    pub fn text_measurer(&self, context: &Context) -> PangoTextMeasurer {
        PangoTextMeasurer::from_cairo_context(context, self.font.clone())
    }

    /// Render every node in display-list order, clipped to the intersection
    /// of the scene viewport and scene clip.
    pub fn render(&self, context: &Context, scene: &Scene) -> Result<()> {
        self.render_scene(context, scene, None)
    }

    /// Render `scene` over `backdrop` — a frosted wallpaper strip, typically
    /// from [`glass::GlassBackdrop`](crate::glass).
    ///
    /// Since translucency became compositor-coupled the native bars call
    /// [`render`](Self::render) directly; the baked-strip path survives for
    /// the Tauri bridge, and `None` renders exactly like `render`, so
    /// retained call sites keep compiling either way.
    ///
    /// With a backdrop it is painted first and the scene's background node
    /// then blends *over* it instead of replacing it, so the bar's background
    /// opacity decides how much of the backdrop shows through.
    ///
    /// The backdrop is mapped onto the scene viewport, so an image built in
    /// device pixels lands one-to-one under any frontend transform.
    pub fn render_over(
        &self,
        context: &Context,
        scene: &Scene,
        backdrop: Option<&ImageSurface>,
    ) -> Result<()> {
        self.render_scene(context, scene, backdrop)
    }

    fn render_scene(
        &self,
        context: &Context,
        scene: &Scene,
        backdrop: Option<&ImageSurface>,
    ) -> Result<()> {
        let Some(viewport) = valid_rect(Rect::new(
            0.0,
            0.0,
            scene.viewport.width,
            scene.viewport.height,
        )) else {
            return Ok(());
        };
        let Some(scene_clip) = valid_rect(scene.clip).and_then(|clip| clip.intersection(viewport))
        else {
            return Ok(());
        };
        // Without a backdrop the background node owns the backing store and
        // replaces it; with one it must preserve what was just painted.
        let background_operator = if backdrop.is_some() {
            Operator::Over
        } else {
            Operator::Source
        };

        with_preserved_path(context, || {
            with_saved(context, || {
                clip_to_rect(context, scene_clip)?;
                if let Some(backdrop) = backdrop {
                    paint_backdrop(context, backdrop, scene_clip)?;
                }
                let layout = pangocairo::functions::create_layout(context);
                layout.set_single_paragraph_mode(true);

                for node in &scene.nodes {
                    self.draw_node(context, &layout, node, background_operator)?;
                }
                context.status()?;
                Ok(())
            })
        })
    }

    fn draw_node(
        &self,
        context: &Context,
        layout: &Layout,
        node: &SceneNode,
        background_operator: Operator,
    ) -> Result<()> {
        let Some(bounds) = valid_rect(node.bounds()) else {
            return Ok(());
        };

        with_saved(context, || {
            // A node is never allowed to paint outside its declared damage
            // bounds.  The scene-level clip remains active as an outer clip.
            clip_to_rect(context, bounds)?;
            match node {
                SceneNode::Background { fill, .. } => {
                    let opacity = self.background_opacity.unwrap_or(1.0);
                    // A full-frame scene can be rendered repeatedly into the
                    // same backing store. Source replacement keeps a
                    // translucent background from accumulating opacity; a
                    // backdrop re-establishes the base itself and is blended
                    // over instead.
                    context.set_operator(background_operator);
                    context.rectangle(
                        f64::from(bounds.x),
                        f64::from(bounds.y),
                        f64::from(bounds.width),
                        f64::from(bounds.height),
                    );
                    set_source(context, *fill, opacity);
                    context.fill()?;
                }
                SceneNode::RoundedRect {
                    radius,
                    fill,
                    stroke,
                    ..
                } => self.draw_rounded_rect(context, bounds, *radius, *fill, *stroke)?,
                SceneNode::Text {
                    text,
                    size,
                    color,
                    align,
                    ..
                } => self.draw_text(
                    context,
                    layout,
                    bounds,
                    text,
                    TextStyle {
                        size: *size,
                        color: *color,
                        align: *align,
                    },
                )?,
                SceneNode::Polyline {
                    points,
                    color,
                    width,
                    ..
                } => draw_polyline(context, points, *color, *width)?,
                SceneNode::Image { source, .. } => self.draw_image(context, bounds, source)?,
            }
            context.status()?;
            Ok(())
        })
    }

    fn draw_rounded_rect(
        &self,
        context: &Context,
        bounds: Rect,
        radius: f32,
        fill: Rgba,
        stroke: Option<Stroke>,
    ) -> Result<()> {
        let outer_radius = finite_non_negative(radius)
            .min(bounds.width * 0.5)
            .min(bounds.height * 0.5);
        let stroke = stroke.and_then(|stroke| {
            let width = finite_non_negative(stroke.width)
                .min(bounds.width)
                .min(bounds.height);
            (width > 0.0).then_some((stroke.color, width))
        });
        let inset = stroke.map_or(0.0, |(_, width)| width * 0.5);
        let path_bounds = Rect::new(
            bounds.x + inset,
            bounds.y + inset,
            (bounds.width - inset * 2.0).max(0.0),
            (bounds.height - inset * 2.0).max(0.0),
        );
        if path_bounds.is_empty() {
            return Ok(());
        }

        rounded_rect_path(context, path_bounds, (outer_radius - inset).max(0.0));
        set_source(context, fill, 1.0);
        if let Some((stroke_color, stroke_width)) = stroke {
            context.fill_preserve()?;
            context.set_line_width(f64::from(stroke_width));
            set_source(context, stroke_color, 1.0);
            context.stroke()?;
        } else {
            context.fill()?;
        }
        Ok(())
    }

    fn draw_text(
        &self,
        context: &Context,
        layout: &Layout,
        bounds: Rect,
        text: &str,
        style: TextStyle,
    ) -> Result<()> {
        let size = finite_non_negative(style.size);
        if text.is_empty() || size <= 0.0 {
            return Ok(());
        }

        configure_layout(layout, &self.font, text, size);
        // Place the ink/logical union so glyphs whose ink overflows their
        // advance (Nerd Font icons) are centered on what is actually drawn
        // and fit the width the measurer reported for them.
        let extents = layout_paint_extents(layout);
        let (x, y) = text_origin(bounds, extents.width(), extents.height(), style.align);
        set_source(context, style.color, 1.0);
        context.move_to(x - f64::from(extents.x()), y - f64::from(extents.y()));
        pangocairo::functions::show_layout(context, layout);
        context.status()?;
        Ok(())
    }
}

impl CairoRenderer {
    /// Draw a scene image scaled to fit `bounds`, keeping its aspect ratio and
    /// centred on what is left over.
    ///
    /// An image that cannot be decoded draws nothing at all. That is the same
    /// result the bar shows for an application whose icon was never found, so
    /// a broken file degrades into the plain title instead of a placeholder
    /// the user cannot act on.
    fn draw_image(&self, context: &Context, bounds: Rect, source: &ImageSource) -> Result<()> {
        let Some(surface) = self.image_surface(source) else {
            return Ok(());
        };
        let (image_width, image_height) = (f64::from(surface.width()), f64::from(surface.height()));
        if image_width <= 0.0 || image_height <= 0.0 || bounds.is_empty() {
            return Ok(());
        }

        let scale =
            (f64::from(bounds.width) / image_width).min(f64::from(bounds.height) / image_height);
        let (width, height) = (image_width * scale, image_height * scale);
        let x = f64::from(bounds.x) + (f64::from(bounds.width) - width) * 0.5;
        let y = f64::from(bounds.y) + (f64::from(bounds.height) - height) * 0.5;

        with_saved(context, || {
            context.translate(x, y);
            context.scale(scale, scale);
            context.set_source_surface(surface.as_ref(), 0.0, 0.0)?;
            let pattern = context.source();
            pattern.set_extend(Extend::Pad);
            // Icons are almost always downscaled from a larger theme size, and
            // Good is the filter cairo picks for that without the cost of
            // Best on every frame.
            pattern.set_filter(Filter::Good);
            context.paint()?;
            context.status()?;
            Ok(())
        })
    }

    fn image_surface(&self, source: &ImageSource) -> Option<Rc<ImageSurface>> {
        if let Some(cached) = self.images.borrow().get(&source.key) {
            return cached.clone();
        }
        let decoded = decode_image(&source.path).map(Rc::new);
        if decoded.is_none() {
            log_image_failure(&source.path);
        }
        self.images.borrow_mut().insert(source.key, decoded.clone());
        decoded
    }
}

#[cfg(feature = "logging-flexi")]
fn log_image_failure(path: &Path) {
    log::debug!("[bar] no drawable image at {}", path.display());
}

#[cfg(not(feature = "logging-flexi"))]
fn log_image_failure(_path: &Path) {}

/// Decode a raster file into a Cairo-native premultiplied surface.
fn decode_image(path: &Path) -> Option<ImageSurface> {
    let rgba = image::open(path).ok()?.into_rgba8();
    let (width, height) = rgba.dimensions();
    let (width, height) = (i32::try_from(width).ok()?, i32::try_from(height).ok()?);
    if width <= 0 || height <= 0 {
        return None;
    }
    let mut surface = ImageSurface::create(Format::ARgb32, width, height).ok()?;
    let stride = usize::try_from(surface.stride()).ok()?;
    {
        let mut destination = surface.data().ok()?;
        for (y, row) in rgba.rows().enumerate() {
            let line = y * stride;
            for (x, pixel) in row.enumerate() {
                let [red, green, blue, alpha] = pixel.0;
                let offset = line + x * 4;
                let Some(target) = destination.get_mut(offset..offset + 4) else {
                    continue;
                };
                // Format::ARgb32 is a native-endian 0xAARRGGBB word with
                // premultiplied colour, which on a little-endian host is these
                // four bytes in this order.
                target[0] = premultiply(blue, alpha);
                target[1] = premultiply(green, alpha);
                target[2] = premultiply(red, alpha);
                target[3] = alpha;
            }
        }
    }
    Some(surface)
}

fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((u32::from(channel) * u32::from(alpha) + 127) / 255) as u8
}

/// Union of a layout's ink and logical pixel extents, relative to the layout
/// origin. Logical extents alone under-report icon glyphs whose ink extends
/// past their advance, which used to clip them inside their pill.
fn layout_paint_extents(layout: &Layout) -> pango::Rectangle {
    let (ink, logical) = layout.pixel_extents();
    if ink.width() <= 0 || ink.height() <= 0 {
        return logical;
    }
    let x = ink.x().min(logical.x());
    let y = ink.y().min(logical.y());
    let right = (ink.x() + ink.width()).max(logical.x() + logical.width());
    let bottom = (ink.y() + ink.height()).max(logical.y() + logical.height());
    pango::Rectangle::new(x, y, right - x, bottom - y)
}

/// Semantic pointer buttons shared by native window-system frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
}

impl PointerButton {
    #[must_use]
    pub const fn action(self) -> PointerAction {
        match self {
            Self::Primary => PointerAction::Primary,
            Self::Secondary => PointerAction::Secondary,
        }
    }
}

/// Framework-neutral pointer input accepted by [`CairoBar::handle_pointer`].
/// Frontends only translate coordinates and their native button enum; hover,
/// press/release matching, scroll direction, hit testing, and dispatch remain
/// core-owned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerInput {
    Move(Point),
    Leave,
    Press { point: Point, button: PointerButton },
    Release { point: Point, button: PointerButton },
    Scroll { point: Point, delta_y: f64 },
}

/// Result of processing one [`PointerInput`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointerUpdate {
    pub runtime: RuntimeUpdate,
    pub redraw: bool,
}

impl PointerUpdate {
    #[must_use]
    pub fn needs_redraw(&self) -> bool {
        self.redraw || self.runtime.needs_redraw()
    }

    #[must_use]
    pub fn into_runtime(self) -> RuntimeUpdate {
        self.runtime
    }
}

/// High-level Cairo bar facade for native frontends.
///
/// `CairoBar` owns the semantic runtime, presentation configuration, pointer
/// state, last built scene, and renderer. A frontend only needs to translate
/// native input into logical [`Point`]s and call [`Self::render`] whenever a
/// returned update (or hover transition) requests another frame.
pub struct CairoBar {
    runtime: BarRuntime,
    config: crate::presentation::PresentationConfig,
    interaction: InteractionState,
    last_pointer: Option<Point>,
    pressed_button: Option<PointerButton>,
    scene: Scene,
    renderer: CairoRenderer,
    last_damage: Damage,
    pending_runtime: RuntimeUpdate,
    dock_reporter: DockReporter,
}

impl CairoBar {
    #[must_use]
    pub fn new(
        runtime: BarRuntime,
        config: crate::presentation::PresentationConfig,
        font: FontDescription,
    ) -> Self {
        // The icon family is resolved here rather than by the caller so that
        // every Cairo frontend gets deterministic private-use glyphs from the
        // same configuration field, without threading a second font argument
        // through ten `main.rs` files.
        let renderer = CairoRenderer::with_icon_font(font, config.icon_font.as_deref());
        Self {
            runtime,
            config,
            interaction: InteractionState::default(),
            last_pointer: None,
            pressed_button: None,
            scene: empty_scene(),
            renderer,
            last_damage: Damage::default(),
            pending_runtime: RuntimeUpdate::default(),
            dock_reporter: DockReporter::new(),
        }
    }

    #[must_use]
    pub const fn runtime(&self) -> &BarRuntime {
        &self.runtime
    }

    /// Mutable runtime access is primarily useful for installing an optional
    /// transport. Any resulting model change is reflected on the next render.
    pub const fn runtime_mut(&mut self) -> &mut BarRuntime {
        &mut self.runtime
    }

    #[must_use]
    pub const fn model(&self) -> &BarModel {
        self.runtime.model()
    }

    #[must_use]
    pub fn snapshot(&self) -> BarSnapshot {
        self.runtime.snapshot()
    }

    /// The scene most recently built by [`Self::render`].
    #[must_use]
    pub const fn scene(&self) -> &Scene {
        &self.scene
    }

    /// Toolkit-neutral Dock delivery state owned by this presenter.
    ///
    /// Native adapters that do not use [`CairoBar`] can own a
    /// [`DockReporter`] directly and feed it their shared presentation scene.
    #[must_use]
    pub const fn dock_reporter(&self) -> &DockReporter {
        &self.dock_reporter
    }

    /// Earliest time a native event loop should service Dock backpressure or
    /// renew an active compositor preview lease.
    #[must_use]
    pub fn next_dock_deadline(&self, now: Instant) -> Option<Instant> {
        self.dock_reporter.next_retry_deadline(now)
    }

    #[must_use]
    pub const fn config(&self) -> &crate::presentation::PresentationConfig {
        &self.config
    }

    /// Configuration changes are picked up by the next render.
    pub const fn config_mut(&mut self) -> &mut crate::presentation::PresentationConfig {
        &mut self.config
    }

    #[must_use]
    pub const fn renderer(&self) -> &CairoRenderer {
        &self.renderer
    }

    pub const fn renderer_mut(&mut self) -> &mut CairoRenderer {
        &mut self.renderer
    }

    /// Rebuild and render a complete scene using text metrics from this exact
    /// Cairo context. This keeps layout and drawing on the same Pango font map,
    /// resolution, transform, and font options.
    pub fn render(&mut self, context: &Context, viewport: Size) -> Result<()> {
        self.render_frame(context, viewport, None, 1.0)
    }

    /// Render over a frosted backdrop; see
    /// [`CairoRenderer::render_over`].  `None` behaves exactly like
    /// [`render`](Self::render), so a frontend keeps one call site whether or
    /// not glass is available this frame.
    pub fn render_over(
        &mut self,
        context: &Context,
        viewport: Size,
        backdrop: Option<&ImageSurface>,
    ) -> Result<()> {
        self.render_frame(context, viewport, backdrop, 1.0)
    }

    fn render_frame(
        &mut self,
        context: &Context,
        viewport: Size,
        backdrop: Option<&ImageSurface>,
        scale_factor: f64,
    ) -> Result<()> {
        let measurer = self.renderer.text_measurer(context);
        let layout = LayoutEngine::new(self.config.clone(), measurer);
        let mut next_interaction = self.interaction;
        let mut next_scene = layout.build(self.runtime.view(), viewport, &next_interaction);
        if let Some(point) = self.last_pointer
            && next_interaction.update_hover(&next_scene, point)
        {
            next_scene = layout.build(self.runtime.view(), viewport, &next_interaction);
        }
        self.renderer.render_over(context, &next_scene, backdrop)?;
        self.last_damage = next_scene.damage_from(&self.scene);
        self.interaction = next_interaction;
        self.scene = next_scene;
        self.synchronize_dock_scene(scale_factor);
        self.flush_dock_commands(Instant::now());
        Ok(())
    }

    /// Logical-coordinate damage between the two most recent rendered scenes.
    ///
    /// Empty damage after a render means the scene was visually identical.
    #[must_use]
    pub const fn last_damage(&self) -> &Damage {
        &self.last_damage
    }

    /// Render into a caller-owned premultiplied Cairo `ARgb32` byte buffer
    /// (BGRA on little-endian) covering `physical_width x physical_height`
    /// device pixels with `stride` bytes per row.
    ///
    /// The scene is laid out in logical units derived from `scale_factor`, so
    /// softbuffer/pixels frontends pass their surface buffer and window scale
    /// without repeating stride, bounds, or transform arithmetic.
    pub fn render_into_bgra(
        &mut self,
        frame: &mut [u8],
        physical_width: u32,
        physical_height: u32,
        stride: u32,
        scale_factor: f64,
    ) -> Result<()> {
        self.render_into_bgra_over(
            frame,
            physical_width,
            physical_height,
            stride,
            scale_factor,
            None,
        )
    }

    /// [`render_into_bgra`](Self::render_into_bgra) over a frosted backdrop.
    ///
    /// The backdrop is expected in the same device pixels as `frame`.
    pub fn render_into_bgra_over(
        &mut self,
        frame: &mut [u8],
        physical_width: u32,
        physical_height: u32,
        stride: u32,
        scale_factor: f64,
        backdrop: Option<&ImageSurface>,
    ) -> Result<()> {
        if physical_width == 0 || physical_height == 0 {
            anyhow::bail!("CPU frame dimensions must be non-zero");
        }
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            anyhow::bail!("scale factor must be finite and greater than zero");
        }
        let width = i32::try_from(physical_width)
            .map_err(|_| anyhow::anyhow!("CPU frame width does not fit Cairo"))?;
        let height = i32::try_from(physical_height)
            .map_err(|_| anyhow::anyhow!("CPU frame height does not fit Cairo"))?;
        let tight_row = physical_width
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("CPU frame row size overflow"))?;
        if stride < tight_row || !stride.is_multiple_of(4) {
            anyhow::bail!("stride {stride} cannot hold a {physical_width}-pixel ARgb32 row");
        }
        let required = usize::try_from(stride)
            .ok()
            .and_then(|stride| stride.checked_mul(physical_height as usize))
            .ok_or_else(|| anyhow::anyhow!("CPU frame size overflow"))?;
        if frame.len() < required {
            anyhow::bail!(
                "CPU frame is too small: expected at least {required} bytes, got {}",
                frame.len()
            );
        }

        let stride_i32 =
            i32::try_from(stride).map_err(|_| anyhow::anyhow!("stride does not fit Cairo"))?;
        // SAFETY: the surface borrows `frame` only inside this scope, the
        // buffer length was validated against `stride * height`, and the
        // surface is flushed and dropped before the borrow ends.
        let surface = unsafe {
            cairo::ImageSurface::create_for_data_unsafe(
                frame.as_mut_ptr(),
                cairo::Format::ARgb32,
                width,
                height,
                stride_i32,
            )?
        };
        let context = Context::new(&surface)?;
        context.scale(scale_factor, scale_factor);
        let viewport = Size::new(
            (f64::from(physical_width) / scale_factor) as f32,
            (f64::from(physical_height) / scale_factor) as f32,
        );
        self.render_frame(&context, viewport, backdrop, scale_factor)?;
        drop(context);
        surface.flush();
        Ok(())
    }

    fn transport_generation(&self) -> u64 {
        #[cfg(feature = "transport-shared")]
        {
            self.runtime.transport_generation()
        }
        #[cfg(not(feature = "transport-shared"))]
        {
            0
        }
    }

    fn synchronize_dock_scene(&mut self, scale_factor: f64) {
        let transport_generation = self.transport_generation();
        let view = self.runtime.view();
        self.dock_reporter.synchronize(DockReporterInput {
            wm_available: view.wm_available,
            wm_session_id: view.wm_session_id,
            minimized_generation: view.wm_sequence.unwrap_or_default(),
            transport_generation,
            monitor_geometry: view.geometry,
            minimized_windows: view.minimized_windows,
            scene: &self.scene,
            presentation: &self.config,
            hovered: self.interaction.hovered(),
            scale_factor,
        });
    }

    fn synchronize_dock_model(&mut self) {
        let transport_generation = self.transport_generation();
        let view = self.runtime.view();
        self.dock_reporter.synchronize_model(
            view.wm_available,
            view.wm_session_id,
            view.wm_sequence.unwrap_or_default(),
            transport_generation,
            view.minimized_windows,
            self.interaction.hovered(),
        );
    }

    fn dispatch_internal(&mut self, action: UserAction) -> bool {
        let update = self.runtime.dispatch(action);
        let accepted = !update.has_issues();
        self.pending_runtime.merge(update);
        // Pointer storms must never turn repeated backpressure into an
        // unbounded diagnostics allocation before the next service turn.
        if self.pending_runtime.issues.len() > 64 {
            let excess = self.pending_runtime.issues.len() - 64;
            self.pending_runtime.issues.drain(..excess);
        }
        if !self.runtime.view().wm_available {
            self.dock_reporter.reset();
        }
        accepted
    }

    fn flush_dock_commands(&mut self, now: Instant) {
        self.synchronize_dock_model();
        let actions = self.dock_reporter.pending_actions(now);
        for action in actions {
            if self.dispatch_internal(action) {
                let _ = self.dock_reporter.acknowledge(action, now);
            } else {
                if self.runtime.view().wm_available {
                    self.dock_reporter.record_failure(now);
                }
                return;
            }
        }
    }

    fn enrich_scene_action(&self, action: UserAction) -> UserAction {
        match action {
            UserAction::RestoreWindow {
                window,
                wm_session_id,
                minimized_generation,
                geometry,
            } if geometry.is_empty() => UserAction::RestoreWindow {
                window,
                wm_session_id,
                minimized_generation,
                geometry: self.dock_reporter.geometry_for(window),
            },
            action => action,
        }
    }

    fn dispatch_scene_action(&mut self, action: UserAction) -> RuntimeUpdate {
        let action = self.enrich_scene_action(action);
        let mut update = std::mem::take(&mut self.pending_runtime);
        let dispatched = self.runtime.dispatch(action);
        let model_rejected = dispatched
            .issues
            .iter()
            .any(|issue| matches!(issue, crate::RuntimeIssue::Model(_)));
        let action_delivered = !dispatched.has_issues();
        update.merge(dispatched);
        if matches!(action, UserAction::RestoreWindow { .. }) && !model_rejected && action_delivered
        {
            // A successful restore owns this pointer gesture. Do not let the
            // stationary cursor immediately reacquire the old scene node and
            // reopen its preview before the next WM snapshot removes it.
            self.last_pointer = None;
            let _ = self.interaction.clear_hover();
            self.dock_reporter.clear_preview();
            self.flush_dock_commands(Instant::now());
            update.merge(std::mem::take(&mut self.pending_runtime));
        }
        update
    }

    fn finish_runtime_turn(&mut self, mut update: RuntimeUpdate) -> RuntimeUpdate {
        self.synchronize_dock_model();
        self.flush_dock_commands(Instant::now());
        update.merge(std::mem::take(&mut self.pending_runtime));
        update
    }

    /// Update the semantic hover target. `true` means the frontend should
    /// schedule a render so node visual states can be rebuilt.
    pub fn pointer_motion(&mut self, point: Point) -> bool {
        self.last_pointer = Some(point);
        let transition = self.interaction.update_hover_transition(&self.scene, point);
        self.synchronize_dock_model();
        self.flush_dock_commands(Instant::now());
        transition.needs_redraw()
    }

    /// Clear all pointer state. `true` means the visible hover state changed.
    pub fn pointer_leave(&mut self) -> bool {
        self.last_pointer = None;
        self.pressed_button = None;
        let changed = self.interaction.clear_hover();
        self.synchronize_dock_model();
        self.flush_dock_commands(Instant::now());
        changed
    }

    /// Hit-test the last rendered scene and dispatch its semantic action.
    pub fn pointer_action(&mut self, point: Point, input: PointerAction) -> RuntimeUpdate {
        self.last_pointer = Some(point);
        match self.scene.action_at(point, input) {
            Some(action) => self.dispatch_scene_action(action),
            None => std::mem::take(&mut self.pending_runtime),
        }
    }

    /// Process a complete pointer state transition.
    ///
    /// Button activation occurs only when press and release use the same
    /// button and resolve to the same stable scene node. Scroll actions are
    /// immediate and zero/non-finite deltas are ignored.
    pub fn handle_pointer(&mut self, input: PointerInput) -> PointerUpdate {
        match input {
            PointerInput::Move(point) => PointerUpdate {
                redraw: self.pointer_motion(point),
                runtime: std::mem::take(&mut self.pending_runtime),
            },
            PointerInput::Leave => PointerUpdate {
                redraw: self.pointer_leave(),
                runtime: std::mem::take(&mut self.pending_runtime),
            },
            PointerInput::Press { point, button } => {
                self.last_pointer = Some(point);
                let hover_changed = self.interaction.update_hover(&self.scene, point);
                let press_changed = self.interaction.press(&self.scene, point);
                self.pressed_button = Some(button);
                self.synchronize_dock_model();
                self.flush_dock_commands(Instant::now());
                PointerUpdate {
                    redraw: hover_changed || press_changed,
                    runtime: std::mem::take(&mut self.pending_runtime),
                }
            }
            PointerInput::Release { point, button } => {
                self.last_pointer = Some(point);
                let hover_changed = self.interaction.update_hover(&self.scene, point);
                let had_press = self.interaction.pressed().is_some();
                let action = if self.pressed_button == Some(button) {
                    self.interaction
                        .release(&self.scene, point, button.action())
                } else {
                    self.interaction.cancel_press();
                    None
                };
                self.pressed_button = None;
                self.synchronize_dock_model();
                self.flush_dock_commands(Instant::now());
                let runtime = match action {
                    Some(action) => self.dispatch_scene_action(action),
                    None => std::mem::take(&mut self.pending_runtime),
                };
                PointerUpdate {
                    redraw: hover_changed || had_press || runtime.needs_redraw(),
                    runtime,
                }
            }
            PointerInput::Scroll { point, delta_y } => {
                self.last_pointer = Some(point);
                let hover_changed = self.interaction.update_hover(&self.scene, point);
                let action = PointerAction::from_vertical_delta(delta_y)
                    .and_then(|action| self.scene.action_at(point, action))
                    .map(|action| self.enrich_scene_action(action));
                self.synchronize_dock_model();
                self.flush_dock_commands(Instant::now());
                let runtime = match action {
                    Some(action) => self.dispatch_scene_action(action),
                    None => std::mem::take(&mut self.pending_runtime),
                };
                PointerUpdate {
                    redraw: hover_changed || runtime.needs_redraw(),
                    runtime,
                }
            }
        }
    }

    pub fn tick(&mut self) -> RuntimeUpdate {
        let update = self.runtime.tick();
        self.finish_runtime_turn(update)
    }

    /// Poll/recover transport and refresh providers in one service pass.
    pub fn service(&mut self) -> RuntimeUpdate {
        let update = self.runtime.service();
        self.finish_runtime_turn(update)
    }

    pub fn poll_transport(&mut self) -> RuntimeUpdate {
        let update = self.runtime.poll_transport();
        self.finish_runtime_turn(update)
    }
}

/// Reusable owned `ARgb32` CPU canvas for GPU-upload frontends.
///
/// wgpu bars keep one canvas alive, render each frame into it, and upload the
/// returned [`CpuFrame`] with its Cairo-preferred stride. Reallocation happens
/// only when the physical size changes.
#[derive(Debug, Default)]
pub struct CpuCanvas {
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

/// Device-pixel rectangle inside a CPU frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Borrowed view of one rendered CPU frame.
#[derive(Debug, Clone, Copy)]
pub struct CpuFrame<'a> {
    /// Premultiplied `ARgb32` bytes, `stride` bytes per row.
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Bytes per row, aligned as Cairo prefers; may exceed `width * 4`.
    pub stride: u32,
    /// Device pixels that changed since the previous frame from this canvas.
    ///
    /// `None` means the whole frame must be presented (first frame or a
    /// resize). `Some(rect)` may be empty when the scene was visually
    /// identical; presenters can then skip the upload entirely.
    pub damage: Option<PixelRect>,
}

impl CpuCanvas {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Render `bar` at the given physical size and return the frame bytes.
    pub fn render<'a>(
        &'a mut self,
        bar: &mut CairoBar,
        physical_width: u32,
        physical_height: u32,
        scale_factor: f64,
    ) -> Result<CpuFrame<'a>> {
        self.render_over(bar, physical_width, physical_height, scale_factor, None)
    }

    /// Render `bar` over a frosted backdrop built for these device pixels.
    ///
    /// `None` is exactly [`render`](Self::render), so a frontend keeps one
    /// call site whether or not the wallpaper was readable this frame.
    pub fn render_over<'a>(
        &'a mut self,
        bar: &mut CairoBar,
        physical_width: u32,
        physical_height: u32,
        scale_factor: f64,
        backdrop: Option<&ImageSurface>,
    ) -> Result<CpuFrame<'a>> {
        if physical_width == 0 || physical_height == 0 {
            anyhow::bail!("CPU canvas dimensions must be non-zero");
        }
        let reallocated = self.width != physical_width || self.height != physical_height;
        if reallocated {
            let width = i32::try_from(physical_width)
                .map_err(|_| anyhow::anyhow!("CPU canvas width does not fit Cairo"))?;
            let stride = cairo::Format::ARgb32.stride_for_width(physical_width)?;
            let stride = u32::try_from(stride)
                .map_err(|_| anyhow::anyhow!("Cairo returned a negative stride for {width}"))?;
            let size = usize::try_from(stride)
                .ok()
                .and_then(|stride| stride.checked_mul(physical_height as usize))
                .ok_or_else(|| anyhow::anyhow!("CPU canvas size overflow"))?;
            self.data.clear();
            self.data.resize(size, 0);
            self.width = physical_width;
            self.height = physical_height;
            self.stride = stride;
        }
        bar.render_into_bgra_over(
            &mut self.data,
            self.width,
            self.height,
            self.stride,
            scale_factor,
            backdrop,
        )?;
        let damage = if reallocated {
            None
        } else {
            Some(physical_damage(
                bar.last_damage(),
                scale_factor,
                self.width,
                self.height,
            ))
        };
        Ok(CpuFrame {
            data: &self.data,
            width: self.width,
            height: self.height,
            stride: self.stride,
            damage,
        })
    }
}

/// Union logical damage regions into one outward-rounded device-pixel rect
/// clamped to the frame.
fn physical_damage(damage: &Damage, scale_factor: f64, width: u32, height: u32) -> PixelRect {
    let mut union: Option<Rect> = None;
    for region in damage.regions() {
        union = Some(match union {
            Some(current) => current.union(*region),
            None => *region,
        });
    }
    let Some(union) = union else {
        return PixelRect::default();
    };

    let left = (f64::from(union.x) * scale_factor).floor().max(0.0) as u32;
    let top = (f64::from(union.y) * scale_factor).floor().max(0.0) as u32;
    let right = (f64::from(union.x + union.width) * scale_factor)
        .ceil()
        .max(0.0) as u32;
    let bottom = (f64::from(union.y + union.height) * scale_factor)
        .ceil()
        .max(0.0) as u32;
    let left = left.min(width);
    let top = top.min(height);
    PixelRect {
        x: left,
        y: top,
        width: right.min(width).saturating_sub(left),
        height: bottom.min(height).saturating_sub(top),
    }
}

fn empty_scene() -> Scene {
    let viewport = Size::new(0.0, 0.0);
    Scene {
        viewport,
        clip: Rect::from_size(viewport),
        nodes: Vec::new(),
        hits: Vec::new(),
    }
}

#[derive(Debug, Clone, Copy)]
struct TextStyle {
    size: f32,
    color: Rgba,
    align: TextAlign,
}

/// Pango-backed text metrics suitable for [`crate::presentation::LayoutEngine`].
#[derive(Debug, Clone)]
pub struct PangoTextMeasurer {
    context: pango::Context,
    font: FontDescription,
}

impl PangoTextMeasurer {
    /// Construct a headless measurer using a Pango-Cairo font map.
    #[must_use]
    pub fn new(font: FontDescription) -> Self {
        let font_map = pangocairo::FontMap::new();
        Self {
            context: font_map.create_context(),
            font,
        }
    }

    /// Construct a measurer using the same transform, resolution and font
    /// options as an existing Cairo context.
    #[must_use]
    pub fn from_cairo_context(context: &Context, font: FontDescription) -> Self {
        Self {
            context: pangocairo::functions::create_context(context),
            font,
        }
    }

    #[must_use]
    pub const fn font(&self) -> &FontDescription {
        &self.font
    }
}

impl TextMeasurer for PangoTextMeasurer {
    fn measure(&self, text: &str, size: f32) -> Size {
        let size = finite_non_negative(size);
        if text.is_empty() || size <= 0.0 {
            return Size::new(0.0, 0.0);
        }

        let layout = Layout::new(&self.context);
        layout.set_single_paragraph_mode(true);
        configure_layout(&layout, &self.font, text, size);
        // Report the same ink/logical union the renderer paints, so layout
        // reserves enough pill width for overflowing icon glyphs.
        let extents = layout_paint_extents(&layout);
        Size::new(
            extents.width().max(0) as f32,
            extents.height().max(0) as f32,
        )
    }
}

fn configure_layout(layout: &Layout, font: &FontDescription, text: &str, size: f32) {
    let mut description = font.clone();
    description.set_absolute_size(f64::from(size) * f64::from(pango::SCALE));
    layout.set_font_description(Some(&description));
    layout.set_text(text);
}

fn draw_polyline(
    context: &Context,
    points: &[crate::presentation::Point],
    color: Rgba,
    width: f32,
) -> Result<()> {
    let width = finite_non_negative(width);
    if points.len() < 2 || width <= 0.0 {
        return Ok(());
    }

    context.new_path();
    context.set_line_cap(LineCap::Round);
    context.set_line_join(LineJoin::Round);
    context.set_line_width(f64::from(width));

    let mut active_run = false;
    let mut drawable_segments = 0_usize;
    for point in points {
        if !point.x.is_finite() || !point.y.is_finite() {
            active_run = false;
            continue;
        }
        if active_run {
            context.line_to(f64::from(point.x), f64::from(point.y));
            drawable_segments += 1;
        } else {
            context.move_to(f64::from(point.x), f64::from(point.y));
            active_run = true;
        }
    }
    if drawable_segments == 0 {
        context.new_path();
        return Ok(());
    }

    set_source(context, color, 1.0);
    context.stroke()?;
    Ok(())
}

fn rounded_rect_path(context: &Context, bounds: Rect, radius: f32) {
    let x = f64::from(bounds.x);
    let y = f64::from(bounds.y);
    let width = f64::from(bounds.width);
    let height = f64::from(bounds.height);
    let radius = f64::from(radius)
        .max(0.0)
        .min(width * 0.5)
        .min(height * 0.5);

    context.new_path();
    if radius == 0.0 {
        context.rectangle(x, y, width, height);
        return;
    }

    let half_pi = std::f64::consts::FRAC_PI_2;
    context.move_to(x + radius, y);
    context.line_to(x + width - radius, y);
    context.arc(x + width - radius, y + radius, radius, -half_pi, 0.0);
    context.line_to(x + width, y + height - radius);
    context.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        half_pi,
    );
    context.line_to(x + radius, y + height);
    context.arc(
        x + radius,
        y + height - radius,
        radius,
        half_pi,
        std::f64::consts::PI,
    );
    context.line_to(x, y + radius);
    context.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::PI,
        3.0 * half_pi,
    );
    context.close_path();
}

fn text_origin(bounds: Rect, text_width: i32, text_height: i32, align: TextAlign) -> (f64, f64) {
    let text_width = f64::from(text_width.max(0));
    let text_height = f64::from(text_height.max(0));
    let x = match align {
        TextAlign::Start => f64::from(bounds.x),
        TextAlign::Center => f64::from(bounds.x) + (f64::from(bounds.width) - text_width) * 0.5,
        TextAlign::End => f64::from(bounds.right()) - text_width,
    };
    let y = f64::from(bounds.y) + (f64::from(bounds.height) - text_height) * 0.5;
    (x, y)
}

/// Paint `backdrop` across `bounds`, replacing whatever the target held.
///
/// The image is stretched onto `bounds` rather than blitted at its own size:
/// the frontend builds it in device pixels while the scene is laid out in
/// logical units, and mapping the two makes the transform between them the
/// caller's business instead of this function's.
fn paint_backdrop(context: &Context, backdrop: &ImageSurface, bounds: Rect) -> Result<()> {
    let (width, height) = (backdrop.width(), backdrop.height());
    if width <= 0 || height <= 0 {
        return Ok(());
    }
    with_saved(context, || {
        clip_to_rect(context, bounds)?;
        context.scale(
            f64::from(bounds.width) / f64::from(width),
            f64::from(bounds.height) / f64::from(height),
        );
        context.set_source_surface(backdrop, 0.0, 0.0)?;
        let source = context.source();
        source.set_extend(Extend::Pad);
        source.set_filter(Filter::Bilinear);
        context.set_operator(Operator::Source);
        context.paint()?;
        context.status()?;
        Ok(())
    })
}

fn clip_to_rect(context: &Context, bounds: Rect) -> Result<()> {
    context.rectangle(
        f64::from(bounds.x),
        f64::from(bounds.y),
        f64::from(bounds.width),
        f64::from(bounds.height),
    );
    context.clip();
    context.status()?;
    Ok(())
}

fn set_source(context: &Context, color: Rgba, alpha_multiplier: f64) {
    context.set_source_rgba(
        component(color.red),
        component(color.green),
        component(color.blue),
        component(color.alpha) * normalize_opacity(alpha_multiplier),
    );
}

fn component(value: f32) -> f64 {
    if value.is_finite() {
        f64::from(value.clamp(0.0, 1.0))
    } else {
        0.0
    }
}

fn normalize_opacity(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn valid_rect(rect: Rect) -> Option<Rect> {
    (rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && !rect.is_empty())
    .then_some(rect)
}

fn with_saved<T>(context: &Context, draw: impl FnOnce() -> Result<T>) -> Result<T> {
    context.save()?;
    let result = draw();
    let restored = context.restore();
    match result {
        Ok(value) => {
            restored?;
            Ok(value)
        }
        Err(error) => {
            let _ = restored;
            Err(error)
        }
    }
}

fn with_preserved_path<T>(context: &Context, draw: impl FnOnce() -> Result<T>) -> Result<T> {
    let path = context.copy_path()?;
    context.new_path();
    let result = draw();
    context.new_path();
    context.append_path(&path);
    let restored = context.status();
    match result {
        Ok(value) => {
            restored?;
            Ok(value)
        }
        Err(error) => {
            let _ = restored;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "transport-shared")]
    use crate::WindowToken;
    use crate::presentation::{NodeId, Point, PresentationVisibility, VisualState};
    use crate::{DirtyBits, ThemeMode, UserAction};
    use cairo::{Format, ImageSurface};
    #[cfg(feature = "transport-shared")]
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(feature = "transport-shared")]
    static NEXT_DOCK_TRANSPORT_PATH: AtomicU64 = AtomicU64::new(0);

    #[cfg(feature = "transport-shared")]
    fn transport_dock_bar() -> (CairoBar, shared_structures::SharedRingBuffer) {
        let sequence = NEXT_DOCK_TRANSPORT_PATH.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "/tmp/xbar-core-cairo-dock-{}-{sequence}",
            std::process::id()
        );
        let owner = shared_structures::SharedRingBufferOptions::new()
            .capacity(8)
            .adaptive_poll_spins(0)
            .create(&path)
            .expect("create isolated Dock transport");
        let transport = crate::SharedTransport::open(&path).unwrap();
        let mut runtime =
            crate::BarRuntime::with_transport(crate::ModelConfig::default(), Some(transport))
                .unwrap();

        let mut monitor_info = shared_structures::MonitorInfo {
            monitor_num: 2,
            monitor_x: 300,
            monitor_y: 400,
            monitor_width: 1600,
            monitor_height: 900,
            ..shared_structures::MonitorInfo::default()
        };
        monitor_info.set_ltsymbol("[]=");
        let mut window = shared_structures::MinimizedWindowInfo::new(
            41,
            2,
            shared_structures::MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
        );
        window.set_title("Terminal");
        window.set_app_id("foot");
        let mut message = shared_structures::SharedMessage {
            timestamp: 1,
            wm_session_id: 91,
            minimized_generation: 2,
            monitor_info,
            ..shared_structures::SharedMessage::default()
        };
        message.set_minimized_windows(&[window]);
        message.minimized_flags |= shared_structures::MINIMIZED_LIST_FLAG_OVERFLOW;
        assert!(owner.try_write_message(&message).unwrap());
        let update = runtime.poll_transport();
        assert!(!update.has_issues());

        let config = crate::presentation::PresentationConfig {
            visibility: PresentationVisibility {
                client_name: false,
                monitor: false,
                system: false,
                audio: false,
                brightness: false,
                battery: false,
                network: false,
                media: false,
                theme: false,
                screenshot: false,
                clock: false,
                shell_hub: false,
                ..PresentationVisibility::default()
            },
            ..crate::presentation::PresentationConfig::default()
        };
        (
            CairoBar::new(runtime, config, FontDescription::from_string("Sans")),
            owner,
        )
    }

    #[cfg(feature = "transport-shared")]
    fn drain_dock_commands(
        owner: &shared_structures::SharedRingBuffer,
    ) -> Vec<shared_structures::SharedCommand> {
        let mut commands = Vec::new();
        while let Some(command) = owner.try_receive_command().unwrap() {
            commands.push(command);
        }
        commands
    }

    #[test]
    fn image_surface_smoke_renders_all_primitives_and_honors_clip() {
        let width = 64_i32;
        let height = 32_i32;
        let mut surface = ImageSurface::create(Format::ARgb32, width, height).unwrap();
        let scene = Scene {
            viewport: Size::new(width as f32, height as f32),
            clip: Rect::new(0.0, 0.0, 48.0, height as f32),
            hits: Vec::new(),
            nodes: vec![
                SceneNode::Background {
                    id: NodeId::Background,
                    bounds: Rect::new(0.0, 0.0, width as f32, height as f32),
                    fill: Rgba::new(1.0, 0.0, 0.0, 1.0),
                },
                SceneNode::RoundedRect {
                    id: NodeId::Audio,
                    bounds: Rect::new(6.0, 6.0, 22.0, 18.0),
                    radius: 5.0,
                    fill: Rgba::new(0.0, 1.0, 0.0, 1.0),
                    stroke: Some(Stroke {
                        color: Rgba::new(0.0, 0.0, 1.0, 1.0),
                        width: 2.0,
                    }),
                    state: VisualState::default(),
                },
                SceneNode::Text {
                    id: NodeId::Clock,
                    bounds: Rect::new(30.0, 4.0, 16.0, 16.0),
                    text: "Hi".to_owned(),
                    size: 10.0,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                    align: TextAlign::Center,
                    state: VisualState::default(),
                },
                SceneNode::Polyline {
                    id: NodeId::Cpu,
                    bounds: Rect::new(4.0, 27.0, 40.0, 3.0),
                    points: vec![Point::new(4.0, 29.0), Point::new(44.0, 29.0)],
                    color: Rgba::new(0.0, 0.0, 1.0, 1.0),
                    width: 2.0,
                    state: VisualState::default(),
                },
            ],
        };

        {
            let context = Context::new(&surface).unwrap();
            let renderer = CairoRenderer::new(FontDescription::from_string("Sans"))
                .with_background_opacity(0.5);
            renderer.render(&context, &scene).unwrap();
            // Rendering a second frame must not compound the translucent
            // background to 75% opacity.
            renderer.render(&context, &scene).unwrap();
        }
        surface.flush();

        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = y * stride + x * 4;
            u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap())
        };

        let background = pixel(2, 2);
        assert!((127..=129).contains(&((background >> 24) as u8)));
        assert!((127..=129).contains(&((background >> 16) as u8)));

        let rounded_center = pixel(17, 15);
        assert_eq!((rounded_center >> 24) as u8, 255);
        assert!(((rounded_center >> 8) as u8) > ((rounded_center >> 16) as u8));

        let clipped_out = pixel(55, 2);
        assert_eq!((clipped_out >> 24) as u8, 0);

        let line = pixel(20, 29);
        assert!((line as u8) > ((line >> 16) as u8));
    }

    /// A scene image reaches the surface: decoded, premultiplied the way Cairo
    /// expects, scaled into its bounds, and cached so the second frame costs
    /// nothing.
    #[test]
    fn a_scene_image_is_decoded_scaled_and_cached() {
        let directory = std::env::temp_dir().join(format!(
            "xbar_scene_image_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let icon_path = directory.join("icon.png");
        // Half-transparent green: the alpha proves premultiplication happened.
        let source = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 255, 0, 128]));
        source.save(&icon_path).unwrap();

        let (width, height) = (16_i32, 16_i32);
        let scene = Scene {
            viewport: Size::new(width as f32, height as f32),
            clip: Rect::new(0.0, 0.0, width as f32, height as f32),
            hits: Vec::new(),
            nodes: vec![
                SceneNode::Background {
                    id: NodeId::Background,
                    bounds: Rect::new(0.0, 0.0, width as f32, height as f32),
                    fill: Rgba::new(0.0, 0.0, 0.0, 1.0),
                },
                SceneNode::Image {
                    id: NodeId::ClientIcon,
                    bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
                    source: ImageSource::from(&crate::app_icon::AppIcon::new(icon_path.clone())),
                    state: VisualState::default(),
                },
                SceneNode::Image {
                    id: NodeId::ClientIcon,
                    bounds: Rect::new(8.0, 8.0, 8.0, 8.0),
                    source: ImageSource::from(&crate::app_icon::AppIcon::new(
                        directory.join("missing.png"),
                    )),
                    state: VisualState::default(),
                },
            ],
        };

        let mut surface = ImageSurface::create(Format::ARgb32, width, height).unwrap();
        let renderer = CairoRenderer::new(FontDescription::from_string("Sans"));
        {
            let context = Context::new(&surface).unwrap();
            renderer.render(&context, &scene).unwrap();
            renderer.render(&context, &scene).unwrap();
        }
        surface.flush();

        assert_eq!(
            renderer.images.borrow().len(),
            2,
            "both the drawable file and the failure are remembered"
        );

        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let pixel = |x: usize, y: usize| {
            let offset = y * stride + x * 4;
            u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap())
        };

        // Inside the icon's bounds: green over black at 50% alpha, twice over.
        let inside = pixel(4, 4);
        assert!(
            ((inside >> 8) as u8) > 100,
            "expected green ink, got {inside:#010x}"
        );
        // An undecodable image leaves its bounds exactly as the background.
        assert_eq!(pixel(12, 12), pixel(15, 2));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The whole point of a backdrop: the bar's background must tint it rather
    /// than replace it, and repeated frames must not drift.
    #[test]
    fn a_backdrop_shows_through_the_background_and_does_not_compound() {
        let (width, height) = (16_i32, 8_i32);
        // A backdrop half the bar's size exercises the viewport mapping too.
        let backdrop = ImageSurface::create(Format::ARgb32, width / 2, height / 2).unwrap();
        {
            let context = Context::new(&backdrop).unwrap();
            context.set_source_rgb(0.0, 0.0, 1.0);
            context.paint().unwrap();
        }
        backdrop.flush();

        let scene = Scene {
            viewport: Size::new(width as f32, height as f32),
            clip: Rect::new(0.0, 0.0, width as f32, height as f32),
            hits: Vec::new(),
            nodes: vec![SceneNode::Background {
                id: NodeId::Background,
                bounds: Rect::new(0.0, 0.0, width as f32, height as f32),
                fill: Rgba::new(1.0, 0.0, 0.0, 1.0),
            }],
        };

        let mut surface = ImageSurface::create(Format::ARgb32, width, height).unwrap();
        {
            let context = Context::new(&surface).unwrap();
            let renderer = CairoRenderer::new(FontDescription::from_string("Sans"))
                .with_background_opacity(0.5);
            renderer
                .render_over(&context, &scene, Some(&backdrop))
                .unwrap();
            renderer
                .render_over(&context, &scene, Some(&backdrop))
                .unwrap();
        }
        surface.flush();

        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let offset = 2 * stride + 8;
        let pixel = u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap());
        let (alpha, red, blue) = (
            (pixel >> 24) as u8,
            ((pixel >> 16) & 0xff) as u8,
            (pixel & 0xff) as u8,
        );

        // Opaque, half the background's red, and the backdrop's blue survives
        // at the complementary half. A second frame changes neither.
        assert_eq!(alpha, 255, "a backdrop must leave the frame opaque");
        assert!((126..=129).contains(&red), "background red was {red}");
        assert!((126..=129).contains(&blue), "backdrop blue was {blue}");
    }

    #[test]
    fn renderer_preserves_the_callers_current_path() {
        let surface = ImageSurface::create(Format::ARgb32, 32, 16).unwrap();
        let context = Context::new(&surface).unwrap();
        context.move_to(2.0, 3.0);
        context.line_to(8.0, 9.0);
        let before: Vec<_> = context.copy_path().unwrap().iter().collect();
        let scene = Scene {
            viewport: Size::new(32.0, 16.0),
            clip: Rect::new(0.0, 0.0, 32.0, 16.0),
            hits: Vec::new(),
            nodes: vec![SceneNode::Background {
                id: NodeId::Background,
                bounds: Rect::new(0.0, 0.0, 32.0, 16.0),
                fill: Rgba::rgb8(0, 0, 0),
            }],
        };

        CairoRenderer::new(FontDescription::from_string("Sans"))
            .render(&context, &scene)
            .unwrap();

        let after: Vec<_> = context.copy_path().unwrap().iter().collect();
        assert_eq!(after, before);
    }

    #[test]
    fn pango_measurer_tracks_text_and_font_size() {
        let measurer = PangoTextMeasurer::new(FontDescription::from_string("Sans"));
        let short = measurer.measure("abc", 12.0);
        let long = measurer.measure("abcdefgh", 12.0);
        let large = measurer.measure("abc", 24.0);

        assert!(short.width > 0.0 && short.height > 0.0);
        assert!(long.width > short.width);
        assert!(large.width > short.width);
        assert!(large.height > short.height);
        assert_eq!(measurer.measure("", 12.0), Size::new(0.0, 0.0));
    }

    #[test]
    fn text_alignment_origins_cover_start_center_and_end() {
        let bounds = Rect::new(10.0, 5.0, 100.0, 40.0);
        assert_eq!(text_origin(bounds, 20, 20, TextAlign::Start), (10.0, 15.0));
        assert_eq!(text_origin(bounds, 20, 20, TextAlign::Center), (50.0, 15.0));
        assert_eq!(text_origin(bounds, 20, 20, TextAlign::End), (90.0, 15.0));
    }

    #[test]
    fn cairo_bar_routes_scene_hits_to_the_runtime_and_renders_the_new_theme() {
        let config = crate::presentation::PresentationConfig {
            visibility: PresentationVisibility {
                client_name: false,
                monitor: false,
                system: false,
                audio: false,
                brightness: false,
                battery: false,
                network: false,
                media: false,
                theme: true,
                screenshot: false,
                clock: false,
                shell_hub: false,
                ..PresentationVisibility::default()
            },
            labels: crate::presentation::PresentationLabels {
                theme_dark: "dark".to_owned(),
                theme_light: "light".to_owned(),
                ..crate::presentation::PresentationLabels::default()
            },
            ..crate::presentation::PresentationConfig::default()
        };

        let mut bar = CairoBar::new(
            BarRuntime::default(),
            config,
            FontDescription::from_string("Sans"),
        );
        assert_eq!(bar.scene().viewport, Size::new(0.0, 0.0));
        assert_eq!(bar.model().view().theme, ThemeMode::Dark);

        let width = 640_i32;
        let height = 38_i32;
        let mut surface = ImageSurface::create(Format::ARgb32, width, height).unwrap();
        {
            let context = Context::new(&surface).unwrap();
            let viewport = Size::new(width as f32, height as f32);
            bar.render(&context, viewport).unwrap();

            let dark_background = scene_background(bar.scene());
            let theme_bounds = bar
                .scene()
                .hits
                .iter()
                .find(|region| region.id == NodeId::Theme)
                .expect("theme hit region")
                .bounds;
            let point = Point::new(
                theme_bounds.x + theme_bounds.width * 0.5,
                theme_bounds.y + theme_bounds.height * 0.5,
            );
            assert_eq!(
                bar.scene().action_at(point, PointerAction::Primary),
                Some(UserAction::ToggleTheme)
            );

            assert!(bar.pointer_motion(point));
            bar.render(&context, viewport).unwrap();
            assert!(
                bar.scene()
                    .nodes_for(NodeId::Theme)
                    .any(|node| node.visual_state().hovered)
            );

            let update = bar.pointer_action(point, PointerAction::Primary);
            assert!(update.changes.contains(DirtyBits::THEME_CHANGED));
            assert!(update.needs_redraw());
            assert_eq!(bar.snapshot().theme, ThemeMode::Light);

            bar.render(&context, viewport).unwrap();
            assert_ne!(scene_background(bar.scene()), dark_background);
            assert!(bar.pointer_leave());
            assert!(!bar.pointer_leave());
        }

        surface.flush();
        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let pixel = u32::from_ne_bytes(data[stride + 4..stride + 8].try_into().unwrap());
        assert_eq!((pixel >> 24) as u8, 255);
        assert!((pixel >> 16) as u8 > 240);
    }

    #[test]
    fn cpu_canvas_renders_scaled_frames_and_validates_external_buffers() {
        let mut bar = CairoBar::new(
            BarRuntime::default(),
            crate::presentation::PresentationConfig::default(),
            FontDescription::from_string("Sans"),
        );

        let mut canvas = CpuCanvas::new();
        let first_stride = {
            let frame = canvas.render(&mut bar, 640, 76, 2.0).unwrap();
            assert_eq!((frame.width, frame.height), (640, 76));
            assert!(frame.stride >= 640 * 4);
            assert_eq!(frame.data.len(), frame.stride as usize * 76);
            assert!(
                frame.data.iter().any(|byte| *byte != 0),
                "rendered frame stayed fully transparent"
            );
            assert_eq!(frame.damage, None, "first frame must present fully");
            frame.stride
        };
        let second = canvas.render(&mut bar, 640, 76, 2.0).unwrap();
        assert_eq!(
            second.stride, first_stride,
            "unchanged size must not reallocate"
        );
        let damage = second.damage.expect("steady-state frames carry damage");
        assert!(
            damage.is_empty(),
            "identical scenes must produce empty damage, got {damage:?}"
        );

        let _ = bar.runtime_mut().dispatch(UserAction::ToggleTheme);
        let themed = canvas.render(&mut bar, 640, 76, 2.0).unwrap();
        let damage = themed.damage.expect("steady-state frames carry damage");
        assert!(!damage.is_empty(), "a theme change must damage the frame");
        assert!(damage.x + damage.width <= 640 && damage.y + damage.height <= 76);

        let mut tight = vec![0_u8; 64 * 38 * 4];
        bar.render_into_bgra(&mut tight, 64, 38, 64 * 4, 1.0)
            .unwrap();
        assert!(tight.iter().any(|byte| *byte != 0));

        let error = bar
            .render_into_bgra(&mut tight, 64, 38, 63 * 4, 1.0)
            .unwrap_err();
        assert!(error.to_string().contains("stride"));
        let error = bar
            .render_into_bgra(&mut tight[..16], 64, 38, 64 * 4, 1.0)
            .unwrap_err();
        assert!(error.to_string().contains("too small"));
        let error = bar
            .render_into_bgra(&mut tight, 64, 38, 64 * 4, f64::NAN)
            .unwrap_err();
        assert!(error.to_string().contains("scale factor"));
    }

    #[test]
    fn cairo_bar_pointer_state_requires_matching_button_and_node() {
        let mut bar = CairoBar::new(
            BarRuntime::default(),
            crate::presentation::PresentationConfig::default(),
            FontDescription::from_string("Sans"),
        );
        let surface = ImageSurface::create(Format::ARgb32, 640, 38).unwrap();
        let context = Context::new(&surface).unwrap();
        bar.render(&context, Size::new(640.0, 38.0)).unwrap();

        let bounds = bar
            .scene()
            .hits
            .iter()
            .find(|region| region.id == NodeId::Theme)
            .unwrap()
            .bounds;
        let point = Point::new(
            bounds.x + bounds.width * 0.5,
            bounds.y + bounds.height * 0.5,
        );

        let press = bar.handle_pointer(PointerInput::Press {
            point,
            button: PointerButton::Primary,
        });
        assert!(press.needs_redraw());
        let mismatch = bar.handle_pointer(PointerInput::Release {
            point,
            button: PointerButton::Secondary,
        });
        assert!(mismatch.runtime.is_empty());
        assert_eq!(bar.snapshot().theme, ThemeMode::Dark);

        let _ = bar.handle_pointer(PointerInput::Press {
            point,
            button: PointerButton::Primary,
        });
        let release = bar.handle_pointer(PointerInput::Release {
            point,
            button: PointerButton::Primary,
        });
        assert!(release.runtime.changes.contains(DirtyBits::THEME_CHANGED));
        assert_eq!(bar.snapshot().theme, ThemeMode::Light);

        let ignored = bar.handle_pointer(PointerInput::Scroll {
            point,
            delta_y: f64::NAN,
        });
        assert!(ignored.runtime.is_empty());
    }

    #[test]
    fn cairo_bar_rehits_a_stationary_pointer_after_layout_moves() {
        let config = crate::presentation::PresentationConfig {
            tag_labels: vec!["1".to_owned(); 9],
            visibility: PresentationVisibility {
                client_name: false,
                monitor: false,
                system: false,
                audio: false,
                brightness: false,
                battery: false,
                network: false,
                media: false,
                theme: false,
                screenshot: false,
                clock: false,
                shell_hub: false,
                ..PresentationVisibility::default()
            },
            ..crate::presentation::PresentationConfig::default()
        };
        let mut bar = CairoBar::new(
            BarRuntime::default(),
            config,
            FontDescription::from_string("Sans"),
        );
        let surface = ImageSurface::create(Format::ARgb32, 900, 38).unwrap();
        let context = Context::new(&surface).unwrap();
        let viewport = Size::new(900.0, 38.0);
        bar.render(&context, viewport).unwrap();

        let layout_bounds = bar
            .scene()
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::LayoutButton)
            .unwrap()
            .bounds;
        let stationary = Point::new(
            layout_bounds.x + layout_bounds.width * 0.5,
            layout_bounds.y + layout_bounds.height * 0.5,
        );
        assert!(bar.pointer_motion(stationary));
        bar.render(&context, viewport).unwrap();
        assert_eq!(
            bar.scene().hit_test(stationary).map(|hit| hit.id),
            Some(NodeId::LayoutButton)
        );

        bar.config_mut().tag_labels[0] = "a much wider first workspace".to_owned();
        bar.render(&context, viewport).unwrap();

        let current_target = bar.scene().hit_test(stationary).map(|hit| hit.id);
        assert_ne!(current_target, Some(NodeId::LayoutButton));
        assert!(
            bar.scene()
                .nodes_for(NodeId::LayoutButton)
                .all(|node| !node.visual_state().hovered)
        );
        if let Some(target) = current_target {
            assert!(
                bar.scene()
                    .nodes_for(target)
                    .all(|node| node.visual_state().hovered)
            );
        }
    }

    #[test]
    fn cairo_bar_commits_scene_only_after_successful_render() {
        let mut bar = CairoBar::new(
            BarRuntime::default(),
            crate::presentation::PresentationConfig::default(),
            FontDescription::from_string("Sans"),
        );
        let surface = ImageSurface::create(Format::ARgb32, 640, 38).unwrap();
        let context = Context::new(&surface).unwrap();
        let viewport = Size::new(640.0, 38.0);
        bar.render(&context, viewport).unwrap();
        let before = bar.scene().clone();

        bar.config_mut().labels.clock = "changed".to_owned();
        let broken_surface = ImageSurface::create(Format::ARgb32, 640, 38).unwrap();
        let broken = Context::new(&broken_surface).unwrap();
        assert!(broken.restore().is_err());
        assert!(bar.render(&broken, viewport).is_err());

        assert_eq!(bar.scene(), &before);
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn cairo_dock_reports_stable_physical_geometry_and_leases_hover_preview() {
        let (mut bar, owner) = transport_dock_bar();
        let mut frame = vec![0_u8; 1600 * 76 * 4];
        bar.render_into_bgra(&mut frame, 1600, 76, 1600 * 4, 2.0)
            .unwrap();

        let geometry_commands = drain_dock_commands(&owner);
        assert_eq!(geometry_commands.len(), 2, "shelf plus one item");
        assert!(geometry_commands.iter().all(|command| {
            command.get_command_type() == shared_structures::CommandType::SetMinimizedGeometry
                && command.get_wm_session_id() == 91
        }));
        let item_report = bar
            .dock_reporter
            .desired_geometry()
            .iter()
            .find(|report| report.window == Some(WindowToken(41)))
            .copied()
            .unwrap();
        assert_eq!(
            (item_report.geometry.width, item_report.geometry.height),
            (54, 36)
        );
        assert!(item_report.geometry.x >= 300 && item_report.geometry.y >= 400);
        let item_command = geometry_commands
            .iter()
            .find(|command| command.get_window_id() == 41)
            .unwrap();
        assert_eq!(
            item_command.anchor(),
            shared_structures::MinimizedWindowAnchor::new(
                item_report.geometry.x,
                item_report.geometry.y,
                item_report.geometry.width,
                item_report.geometry.height,
            )
        );

        let hit = bar
            .scene()
            .hits
            .iter()
            .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(41)))
            .unwrap();
        let point = Point::new(
            hit.bounds.x + hit.bounds.width * 0.5,
            hit.bounds.y + hit.bounds.height * 0.5,
        );
        let entered = bar.handle_pointer(PointerInput::Move(point));
        assert!(entered.redraw && !entered.runtime.has_issues());
        assert!(
            drain_dock_commands(&owner).is_empty(),
            "preview waits for the frame containing hover magnification"
        );
        bar.render_into_bgra(&mut frame, 1600, 76, 1600 * 4, 2.0)
            .unwrap();
        let commands = drain_dock_commands(&owner);
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].get_command_type(),
            shared_structures::CommandType::PreviewMinimized
        );
        assert_eq!(
            commands[0].get_flags(),
            shared_structures::PREVIEW_MINIMIZED_FLAG_VISIBLE
        );
        assert!(commands[0].anchor().width > item_command.anchor().width);
        assert!(commands[0].anchor().height > item_command.anchor().height);

        let moved = bar.handle_pointer(PointerInput::Move(Point::new(point.x + 1.0, point.y)));
        assert!(moved.redraw);
        assert!(drain_dock_commands(&owner).is_empty());
        bar.render_into_bgra(&mut frame, 1600, 76, 1600 * 4, 2.0)
            .unwrap();
        assert!(
            drain_dock_commands(&owner).is_empty(),
            "hover magnification must not churn stable target geometry"
        );

        let lease_start = bar.dock_reporter.preview_last_sent().unwrap();
        bar.flush_dock_commands(lease_start + crate::DOCK_PREVIEW_LEASE_INTERVAL);
        let renewed = drain_dock_commands(&owner);
        assert_eq!(renewed.len(), 1);
        assert_eq!(
            renewed[0].get_flags(),
            shared_structures::PREVIEW_MINIMIZED_FLAG_VISIBLE
                | shared_structures::PREVIEW_MINIMIZED_FLAG_RENEWAL
        );

        let left = bar.handle_pointer(PointerInput::Leave);
        assert!(left.redraw);
        let hidden = drain_dock_commands(&owner);
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].get_flags(), 0);

        let _ = bar.handle_pointer(PointerInput::Move(point));
        assert!(drain_dock_commands(&owner).is_empty());
        bar.render_into_bgra(&mut frame, 1600, 76, 1600 * 4, 2.0)
            .unwrap();
        let preview = drain_dock_commands(&owner);
        assert_eq!(preview.len(), 1);
        let restore_geometry = bar.dock_reporter.geometry_for(WindowToken(41));
        let restored = bar.pointer_action(point, PointerAction::Primary);
        assert!(!restored.has_issues());
        let commands = drain_dock_commands(&owner);
        assert_eq!(commands.len(), 2, "restore also tears down preview intent");
        assert_eq!(
            commands[0].get_command_type(),
            shared_structures::CommandType::RestoreMinimized
        );
        assert_eq!(
            commands[0].anchor(),
            shared_structures::MinimizedWindowAnchor::new(
                restore_geometry.x,
                restore_geometry.y,
                restore_geometry.width,
                restore_geometry.height,
            )
        );
        assert_eq!(
            commands[1].get_command_type(),
            shared_structures::CommandType::PreviewMinimized
        );
        assert_eq!(commands[1].get_flags(), 0);

        let mut narrow = vec![0_u8; 130 * 38 * 4];
        bar.render_into_bgra(&mut narrow, 130, 38, 130 * 4, 1.0)
            .unwrap();
        let withdrawn = drain_dock_commands(&owner);
        assert!(withdrawn.iter().any(|command| {
            command.get_command_type() == shared_structures::CommandType::SetMinimizedGeometry
                && command.get_window_id() == 41
                && command.anchor() == shared_structures::MinimizedWindowAnchor::default()
        }));

        bar.render_into_bgra(&mut frame, 1600, 76, 1600 * 4, 2.0)
            .unwrap();
        let restored_target = drain_dock_commands(&owner);
        assert!(restored_target.iter().any(|command| {
            command.get_command_type() == shared_structures::CommandType::SetMinimizedGeometry
                && command.get_window_id() == 41
                && command.anchor().width == 54
        }));
        owner.destroy().unwrap();
    }

    #[cfg(feature = "transport-shared")]
    #[test]
    fn cairo_dock_retries_queue_full_and_clears_state_on_disconnect() {
        let (mut bar, owner) = transport_dock_bar();
        let filler = crate::WmCommand::SetLayout {
            layout: crate::LayoutId(1),
            monitor: crate::MonitorId(2),
        };
        for _ in 0..owner.command_capacity() {
            assert_eq!(
                bar.runtime().transport().unwrap().execute(filler).unwrap(),
                crate::SendOutcome::Sent
            );
        }

        let mut frame = vec![0_u8; 800 * 38 * 4];
        bar.render_into_bgra(&mut frame, 800, 38, 800 * 4, 1.0)
            .unwrap();
        assert!(bar.dock_reporter.acknowledged_geometry().is_empty());
        assert_eq!(
            bar.pending_runtime
                .issues
                .iter()
                .filter(|issue| matches!(issue, crate::RuntimeIssue::QueueFull { .. }))
                .count(),
            1,
            "one failed burst must not emit an issue per item"
        );
        assert_eq!(drain_dock_commands(&owner).len(), owner.command_capacity());

        let retry = bar.tick();
        assert_eq!(
            retry
                .issues
                .iter()
                .filter(|issue| matches!(issue, crate::RuntimeIssue::QueueFull { .. }))
                .count(),
            1,
            "the original backpressure remains observable"
        );
        let retry_at = bar
            .next_dock_deadline(Instant::now())
            .expect("unacknowledged Dock targets remain scheduled");
        bar.flush_dock_commands(retry_at);
        let retried = drain_dock_commands(&owner);
        assert_eq!(retried.len(), 2);
        assert!(retried.iter().all(|command| {
            command.get_command_type() == shared_structures::CommandType::SetMinimizedGeometry
        }));
        assert_eq!(bar.dock_reporter.acknowledged_geometry().len(), 2);

        owner.destroy().unwrap();
        bar.config_mut().dock_item_size = 19.0;
        bar.render_into_bgra(&mut frame, 800, 38, 800 * 4, 1.0)
            .unwrap();
        assert!(!bar.runtime().view().wm_available);
        assert!(bar.dock_reporter.state_key().is_none());
        assert!(bar.dock_reporter.desired_preview().is_none());
        assert!(bar.dock_reporter.acknowledged_preview().is_none());
        assert!(bar.dock_reporter.desired_geometry().is_empty());
        assert!(bar.dock_reporter.acknowledged_geometry().is_empty());
        assert!(bar.pending_runtime.has_issues());
    }

    fn scene_background(scene: &Scene) -> Rgba {
        scene
            .nodes
            .iter()
            .find_map(|node| match node {
                SceneNode::Background { fill, .. } => Some(*fill),
                _ => None,
            })
            .expect("scene background")
    }
}
