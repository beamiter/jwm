//! Native GTK widgets positioned by the common horizontal desktop-header scene.
//! Shapes remain CSS widgets, text remains Pango labels, and icons remain
//! Pictures; no Cairo screenshot replaces the GTK frontend.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use gtk4::prelude::*;
use xbar_core::presentation::{
    InteractionState, LayoutEngine, NodeId, Point, PointerAction, Rgba, Scene, SceneNode, Size,
    TextAlign, TextMeasurer,
};
use xbar_core::{BarSnapshot, ControlScroll, ScrollDelta, ScrollPhase, UserAction};

use crate::{BarTheme, Dispatch};

struct Measure {
    probe: gtk4::Label,
    font: gtk4::pango::FontDescription,
}
impl TextMeasurer for Measure {
    fn measure(&self, text: &str, size: f32) -> Size {
        let mut font = self.font.clone();
        font.set_absolute_size(f64::from(size) * f64::from(gtk4::pango::SCALE));
        let layout = self.probe.create_pango_layout(Some(text));
        layout.set_single_paragraph_mode(true);
        layout.set_font_description(Some(&font));
        let (ink, logical) = layout.pixel_extents();
        let left = ink.x().min(logical.x());
        let top = ink.y().min(logical.y());
        Size::new(
            (ink.x() + ink.width()).max(logical.x() + logical.width()) as f32 - left as f32,
            (ink.y() + ink.height()).max(logical.y() + logical.height()) as f32 - top as f32,
        )
    }
}

struct State {
    root: gtk4::Fixed,
    engine: LayoutEngine<Measure>,
    snapshot: Option<BarSnapshot>,
    scene: Option<Scene>,
    interaction: InteractionState,
    buttons: Vec<(NodeId, gtk4::Button, Rc<Cell<Option<UserAction>>>)>,
    provider: gtk4::CssProvider,
    dispatch: Dispatch,
    scroll: ControlScroll,
    scroll_started: bool,
    font: gtk4::pango::FontDescription,
    opacity: Option<f64>,
    translucent: bool,
}

pub(crate) struct HeaderSurface(Rc<RefCell<State>>);
impl HeaderSurface {
    pub fn new(theme: &BarTheme, dispatch: Dispatch) -> Self {
        let root = gtk4::Fixed::new();
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.set_overflow(gtk4::Overflow::Hidden);
        root.set_widget_name("jwm-desktop-header");
        let provider = gtk4::CssProvider::new();
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
        let state = Rc::new(RefCell::new(State {
            root: root.clone(),
            engine: LayoutEngine::new(
                theme.presentation.clone(),
                Measure {
                    probe: gtk4::Label::new(None),
                    font: theme.pill_style.font.clone(),
                },
            ),
            snapshot: None,
            scene: None,
            interaction: InteractionState::default(),
            buttons: Vec::new(),
            provider,
            dispatch,
            scroll: ControlScroll::with_policy(theme.presentation.scroll),
            scroll_started: false,
            font: theme.pill_style.font.clone(),
            opacity: theme.background_opacity,
            translucent: theme.translucent,
        }));
        let motion = gtk4::EventControllerMotion::new();
        motion.connect_motion({
            let state = Rc::downgrade(&state);
            move |_, x, y| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                let mut state = state.borrow_mut();
                if let Some(scene) = state.scene.clone()
                    && state
                        .interaction
                        .update_hover(&scene, Point::new(x as f32, y as f32))
                {
                    state.render();
                }
            }
        });
        motion.connect_leave({
            let state = Rc::downgrade(&state);
            move |_| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                let mut state = state.borrow_mut();
                if state.interaction.clear_hover() {
                    state.render();
                }
            }
        });
        root.add_controller(motion);
        let click = gtk4::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        click.connect_pressed({
            let state = Rc::downgrade(&state);
            move |gesture, _, x, y| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                if !matches!(gesture.current_button(), 1 | 3) {
                    return;
                }
                let mut state = state.borrow_mut();
                if let Some(scene) = state.scene.clone() {
                    state
                        .interaction
                        .press(&scene, Point::new(x as f32, y as f32));
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                    state.render();
                }
            }
        });
        click.connect_released({
            let state = Rc::downgrade(&state);
            move |gesture, _, x, y| {
                let Some(state) = state.upgrade() else {
                    return;
                };
                let mut state = state.borrow_mut();
                let action = state.scene.clone().and_then(|scene| {
                    PointerAction::from_x11_button(gesture.current_button() as u8).and_then(
                        |input| {
                            state
                                .interaction
                                .release(&scene, Point::new(x as f32, y as f32), input)
                        },
                    )
                });
                let dispatch = state.dispatch.clone();
                state.render();
                drop(state);
                if let Some(action) = action {
                    dispatch(action, None);
                }
            }
        });
        root.add_controller(click);
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        scroll.connect_scroll_begin({
            let state = Rc::downgrade(&state);
            move |_| {
                if let Some(state) = state.upgrade() {
                    let mut state = state.borrow_mut();
                    state.scroll.reset();
                    state.scroll_started = true;
                }
            }
        });
        scroll.connect_scroll_end({
            let state = Rc::downgrade(&state);
            move |_| {
                if let Some(state) = state.upgrade() {
                    let mut state = state.borrow_mut();
                    state.scroll_started = false;
                    state.scroll.cancel(Instant::now());
                }
            }
        });
        scroll.connect_scroll({
            let state = Rc::downgrade(&state);
            move |controller, x, y| {
                let Some(state) = state.upgrade() else {
                    return gtk4::glib::Propagation::Proceed;
                };
                let mut state = state.borrow_mut();
                let Some(hit) = state
                    .interaction
                    .pointer()
                    .and_then(|point| state.scene.as_ref()?.hit_test(point))
                    .cloned()
                else {
                    return gtk4::glib::Propagation::Proceed;
                };
                let delta = match controller.unit() {
                    gtk4::gdk::ScrollUnit::Surface => ScrollDelta::Pixels { x: -x, y: -y },
                    _ => ScrollDelta::Lines { x: -x, y: -y },
                };
                let edge = state.engine.config().edge;
                let phase = if std::mem::take(&mut state.scroll_started) {
                    ScrollPhase::Started
                } else {
                    ScrollPhase::Moved
                };
                let action = state
                    .scroll
                    .step(hit.id, edge, delta, phase, Instant::now())
                    .and_then(|input| hit.action(input));
                let dispatch = state.dispatch.clone();
                drop(state);
                if let Some(action) = action {
                    dispatch(action, None);
                }
                gtk4::glib::Propagation::Stop
            }
        });
        root.add_controller(scroll);
        root.add_tick_callback({
            let state = Rc::downgrade(&state);
            move |_, _| {
                let Some(state) = state.upgrade() else {
                    return gtk4::glib::ControlFlow::Break;
                };
                let mut state = state.borrow_mut();
                let size = state.size();
                if state
                    .scene
                    .as_ref()
                    .is_none_or(|scene| scene.viewport != size)
                {
                    state.render();
                }
                gtk4::glib::ControlFlow::Continue
            }
        });
        Self(state)
    }
    pub fn widget(&self) -> gtk4::Fixed {
        self.0.borrow().root.clone()
    }
    pub fn sync(&self, snapshot: &BarSnapshot) {
        let mut state = self.0.borrow_mut();
        state.snapshot = Some(snapshot.clone());
        state.render();
    }
    pub fn focused_control(&self) -> Option<NodeId> {
        self.0.borrow().focused_control()
    }
    pub fn restore_focus(&self, id: NodeId) {
        if let Some((_, button, _)) = self
            .0
            .borrow()
            .buttons
            .iter()
            .find(|(node, _, _)| *node == id)
        {
            button.grab_focus();
        }
    }
}

impl State {
    fn size(&self) -> Size {
        Size::new(
            self.root.width().max(1) as f32,
            self.root.height().max(1) as f32,
        )
    }
    fn focused_control(&self) -> Option<NodeId> {
        self.buttons
            .iter()
            .find_map(|(id, button, _)| button.has_focus().then_some(*id))
    }
    fn render(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let mut scene = self
            .engine
            .build(snapshot.view(), self.size(), &self.interaction);
        let opacity = xbar_core::background::bar_background_opacity(
            snapshot.blur_status_bar,
            self.opacity,
            self.translucent,
        );
        scene.adapt_text_to_background(snapshot.theme, snapshot.wallpaper_luminance, opacity);
        if self.scene.as_ref() == Some(&scene) {
            return;
        }
        let focused = self.focused_control();
        // Preserve native input targets across hover/press and clock repaints.
        // Unparenting a held button would cancel its GTK event sequence even
        // when its semantic binding has not changed.
        let mut child = self.root.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if !widget.is::<gtk4::Button>() {
                self.root.remove(&widget);
            }
        }
        let mut retained = std::mem::take(&mut self.buttons);
        let mut css = String::from(
            "#jwm-desktop-header { padding:0; } #jwm-desktop-header button { min-width:0; min-height:0; padding:0; border:0; background:none; box-shadow:none; } #jwm-desktop-header button:focus-visible { outline:2px solid #80bfff; outline-offset:-2px; }\n",
        );
        for (index, node) in scene.nodes.iter().enumerate() {
            let bounds = node.bounds();
            let name = format!("jwm-header-node-{index}");
            let widget: gtk4::Widget = match node {
                SceneNode::Background { fill, .. } | SceneNode::RoundedRect { fill, .. } => {
                    let mut fill = *fill;
                    let (radius, border) = match node {
                        SceneNode::Background { .. } => {
                            fill.alpha = opacity as f32;
                            (
                                bounds.height.min(bounds.width) * 0.5,
                                String::from("1px solid rgba(255,255,255,0.65)"),
                            )
                        }
                        SceneNode::RoundedRect { radius, stroke, .. } => (
                            *radius,
                            stroke.map_or_else(
                                || "none".into(),
                                |stroke| {
                                    format!("{}px solid {}", stroke.width, color(stroke.color))
                                },
                            ),
                        ),
                        _ => unreachable!(),
                    };
                    css.push_str(&format!("#{name} {{ background:{}; border-radius:{radius}px; border:{border}; min-width:0; min-height:0; }}\n",color(fill)));
                    gtk4::Box::new(gtk4::Orientation::Horizontal, 0).upcast()
                }
                SceneNode::Text {
                    text,
                    size,
                    color: ink,
                    align,
                    ..
                } => {
                    let label = gtk4::Label::new(Some(text));
                    label.set_single_line_mode(true);
                    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    label.set_max_width_chars(1);
                    label.set_xalign(match align {
                        TextAlign::Start => 0.0,
                        TextAlign::Center => 0.5,
                        TextAlign::End => 1.0,
                    });
                    css.push_str(&format!(
                        "#{name} {{ color:{}; {} padding:0; min-width:0; min-height:0; }}\n",
                        color(*ink),
                        super::theme::font_rules(&self.font.to_string(), *size)
                    ));
                    label.upcast()
                }
                SceneNode::Image { source, .. } => {
                    let image = gtk4::Picture::for_filename(&source.path);
                    image.set_can_shrink(true);
                    image.set_content_fit(gtk4::ContentFit::Contain);
                    image.upcast()
                }
                SceneNode::Polyline {
                    points,
                    color: ink,
                    width,
                    ..
                } => {
                    let area = gtk4::DrawingArea::new();
                    let points = points.clone();
                    let ink = *ink;
                    let width = *width;
                    area.set_draw_func(move |_, context, _, _| {
                        context.set_source_rgba(
                            ink.red as f64,
                            ink.green as f64,
                            ink.blue as f64,
                            ink.alpha as f64,
                        );
                        context.set_line_width(width as f64);
                        for (index, point) in points.iter().enumerate() {
                            let (x, y) = ((point.x - bounds.x) as f64, (point.y - bounds.y) as f64);
                            if index == 0 {
                                context.move_to(x, y);
                            } else {
                                context.line_to(x, y);
                            }
                        }
                        let _ = context.stroke();
                    });
                    area.upcast()
                }
            };
            widget.set_widget_name(&name);
            widget.set_can_target(false);
            widget.set_size_request(
                bounds.width.ceil().max(1.0) as i32,
                bounds.height.ceil().max(1.0) as i32,
            );
            self.root.put(&widget, bounds.x as f64, bounds.y as f64);
        }
        for hit in &scene.hits {
            let (button, binding) =
                if let Some(index) = retained.iter().position(|(id, _, _)| *id == hit.id) {
                    let (_, button, binding) = retained.remove(index);
                    (button, binding)
                } else {
                    let button = gtk4::Button::new();
                    let binding = Rc::new(Cell::new(None));
                    let action = binding.clone();
                    let dispatch = self.dispatch.clone();
                    button.connect_clicked(move |_| {
                        if let Some(action) = action.get() {
                            dispatch(action, None);
                        }
                    });
                    (button, binding)
                };
            binding.set(hit.primary);
            button.set_can_focus(hit.primary.is_some());
            let label = scene
                .nodes_for(hit.id)
                .filter_map(|node| {
                    if let SceneNode::Text { text, .. } = node {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            button.set_tooltip_text(Some(&label));
            button.update_property(&[gtk4::accessible::Property::Label(&label)]);
            button.set_size_request(
                hit.bounds.width.ceil() as i32,
                hit.bounds.height.ceil() as i32,
            );
            if button.parent().is_some() {
                self.root
                    .move_(&button, hit.bounds.x as f64, hit.bounds.y as f64);
            } else {
                self.root
                    .put(&button, hit.bounds.x as f64, hit.bounds.y as f64);
            }
            self.buttons.push((hit.id, button, binding));
        }
        for (_, button, _) in retained {
            self.root.remove(&button);
        }
        self.provider.load_from_string(&css);
        self.scene = Some(scene);
        if let Some(id) = focused
            && let Some((_, button, _)) = self.buttons.iter().find(|(node, _, _)| *node == id)
        {
            button.grab_focus();
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_remove_provider_for_display(&display, &self.provider);
        }
    }
}
fn color(value: Rgba) -> String {
    format!(
        "rgba({},{},{},{})",
        (value.red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (value.green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (value.blue.clamp(0.0, 1.0) * 255.0).round() as u8,
        value.alpha.clamp(0.0, 1.0)
    )
}
