//! Cairo/Pango renderer for the backend-neutral presentation scene.
//!
//! Scene coordinates are logical units.  A frontend that needs HiDPI output
//! should configure the Cairo context transform before calling [`CairoRenderer::render`].

use anyhow::Result;
use cairo::{Context, LineCap, LineJoin, Operator};
use pango::prelude::FontMapExt as _;
use pango::{FontDescription, Layout};

use crate::presentation::{
    Damage, InteractionState, LayoutEngine, Point, PointerAction, Rect, Rgba, Scene, SceneNode,
    Size, Stroke, TextAlign, TextMeasurer,
};
use crate::{BarModel, BarRuntime, BarSnapshot, RuntimeUpdate};

/// Renders a [`Scene`] into an existing Cairo context.
#[derive(Debug, Clone)]
pub struct CairoRenderer {
    font: FontDescription,
    background_opacity: Option<f64>,
}

impl CairoRenderer {
    #[must_use]
    pub const fn new(font: FontDescription) -> Self {
        Self {
            font,
            background_opacity: None,
        }
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

        with_preserved_path(context, || {
            with_saved(context, || {
                clip_to_rect(context, scene_clip)?;
                let layout = pangocairo::functions::create_layout(context);
                layout.set_single_paragraph_mode(true);

                for node in &scene.nodes {
                    self.draw_node(context, &layout, node)?;
                }
                context.status()?;
                Ok(())
            })
        })
    }

    fn draw_node(&self, context: &Context, layout: &Layout, node: &SceneNode) -> Result<()> {
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
                    // translucent background from accumulating opacity.
                    context.set_operator(Operator::Source);
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
        let (text_width, text_height) = layout.pixel_size();
        let (x, y) = text_origin(bounds, text_width, text_height, style.align);
        set_source(context, style.color, 1.0);
        context.move_to(x, y);
        pangocairo::functions::show_layout(context, layout);
        context.status()?;
        Ok(())
    }
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
}

impl CairoBar {
    #[must_use]
    pub fn new(
        runtime: BarRuntime,
        config: crate::presentation::PresentationConfig,
        font: FontDescription,
    ) -> Self {
        Self {
            runtime,
            config,
            interaction: InteractionState::default(),
            last_pointer: None,
            pressed_button: None,
            scene: empty_scene(),
            renderer: CairoRenderer::new(font),
            last_damage: Damage::default(),
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
        let measurer = self.renderer.text_measurer(context);
        let layout = LayoutEngine::new(self.config.clone(), measurer);
        let mut next_interaction = self.interaction;
        let mut next_scene = layout.build(self.runtime.view(), viewport, &next_interaction);
        if let Some(point) = self.last_pointer
            && next_interaction.update_hover(&next_scene, point)
        {
            next_scene = layout.build(self.runtime.view(), viewport, &next_interaction);
        }
        self.renderer.render(context, &next_scene)?;
        self.last_damage = next_scene.damage_from(&self.scene);
        self.interaction = next_interaction;
        self.scene = next_scene;
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
        self.render(&context, viewport)?;
        drop(context);
        surface.flush();
        Ok(())
    }

    /// Update the semantic hover target. `true` means the frontend should
    /// schedule a render so node visual states can be rebuilt.
    pub fn pointer_motion(&mut self, point: Point) -> bool {
        self.last_pointer = Some(point);
        self.interaction.update_hover(&self.scene, point)
    }

    /// Clear all pointer state. `true` means the visible hover state changed.
    pub fn pointer_leave(&mut self) -> bool {
        self.last_pointer = None;
        self.pressed_button = None;
        self.interaction.clear_hover()
    }

    /// Hit-test the last rendered scene and dispatch its semantic action.
    pub fn pointer_action(&mut self, point: Point, input: PointerAction) -> RuntimeUpdate {
        self.last_pointer = Some(point);
        self.scene
            .action_at(point, input)
            .map_or_else(RuntimeUpdate::default, |action| {
                self.runtime.dispatch(action)
            })
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
                ..PointerUpdate::default()
            },
            PointerInput::Leave => PointerUpdate {
                redraw: self.pointer_leave(),
                ..PointerUpdate::default()
            },
            PointerInput::Press { point, button } => {
                self.last_pointer = Some(point);
                let hover_changed = self.interaction.update_hover(&self.scene, point);
                let press_changed = self.interaction.press(&self.scene, point);
                self.pressed_button = Some(button);
                PointerUpdate {
                    redraw: hover_changed || press_changed,
                    ..PointerUpdate::default()
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
                let runtime = action.map_or_else(RuntimeUpdate::default, |action| {
                    self.runtime.dispatch(action)
                });
                PointerUpdate {
                    redraw: hover_changed || had_press || runtime.needs_redraw(),
                    runtime,
                }
            }
            PointerInput::Scroll { point, delta_y } => {
                self.last_pointer = Some(point);
                let hover_changed = self.interaction.update_hover(&self.scene, point);
                let runtime = PointerAction::from_vertical_delta(delta_y)
                    .and_then(|action| self.scene.action_at(point, action))
                    .map_or_else(RuntimeUpdate::default, |action| {
                        self.runtime.dispatch(action)
                    });
                PointerUpdate {
                    redraw: hover_changed || runtime.needs_redraw(),
                    runtime,
                }
            }
        }
    }

    pub fn tick(&mut self) -> RuntimeUpdate {
        self.runtime.tick()
    }

    /// Poll/recover transport and refresh providers in one service pass.
    pub fn service(&mut self) -> RuntimeUpdate {
        self.runtime.service()
    }

    pub fn poll_transport(&mut self) -> RuntimeUpdate {
        self.runtime.poll_transport()
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
        bar.render_into_bgra(
            &mut self.data,
            self.width,
            self.height,
            self.stride,
            scale_factor,
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
        let (width, height) = layout.pixel_size();
        Size::new(width.max(0) as f32, height.max(0) as f32)
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
    use crate::presentation::{NodeId, Point, PresentationVisibility, VisualState};
    use crate::{DirtyBits, ThemeMode, UserAction};
    use cairo::{Format, ImageSurface};

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
