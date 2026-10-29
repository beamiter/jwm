//! Shared upright sidebar for the GPUI and GPUI-component examples.
//! Both consume the core projection rather than maintaining another list of
//! sidebar actions, status values, or workspace state precedence.

use gpui::prelude::*;
use gpui::{
    Context, MouseButton, Render, Rgba, ScrollDelta, ScrollEvent, SharedString, Window, div, px,
};
use xbar_core::presentation::{NodeId, Palette, PointerAction, PresentationConfig, VisualState};
use xbar_core::tag_contrast::TagPalette;
use xbar_core::{
    BarEdge, BarSnapshot, ControlScroll, ControlSpec, DockBridge, DockItemBinding,
    PresentationProjector, ScrollPhase, ThemeMode, UserAction,
};

pub trait ScrollOwner {
    fn control_scroll(&mut self) -> &mut ControlScroll;
    fn scroll_edge(&self) -> BarEdge;
}

pub fn hover_control<T: ScrollOwner>(this: &mut T, id: NodeId, hovered: bool) {
    let edge = this.scroll_edge();
    this.control_scroll()
        .hover(hovered.then_some(id), edge, std::time::Instant::now());
}

/// GPUI deltas are already logical pixels. Preserve their units and both axes.
pub fn scroll_action<T: ScrollOwner>(
    this: &mut T,
    id: NodeId,
    event: &ScrollEvent,
) -> Option<PointerAction> {
    let delta = match event.delta {
        ScrollDelta::Pixels(delta) => xbar_core::ScrollDelta::Pixels {
            x: f64::from(f32::from(delta.x)),
            y: f64::from(f32::from(delta.y)),
        },
        ScrollDelta::Lines(delta) => xbar_core::ScrollDelta::Lines {
            x: f64::from(delta.x),
            y: f64::from(delta.y),
        },
    };
    #[allow(unreachable_patterns)]
    let phase = match event.touch_phase {
        gpui::TouchPhase::Started => ScrollPhase::Started,
        gpui::TouchPhase::Moved => ScrollPhase::Moved,
        gpui::TouchPhase::Ended => ScrollPhase::Ended,
        _ => ScrollPhase::Cancelled,
    };
    let edge = this.scroll_edge();
    this.control_scroll()
        .step(id, edge, delta, phase, std::time::Instant::now())
}

struct ControlTooltip(SharedString);

impl Render for ControlTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(5.))
            .rounded(px(5.))
            .bg(gpui::rgb(0x202126))
            .text_color(gpui::rgb(0xffffff))
            .text_size(px(12.))
            .child(self.0.clone())
    }
}

fn color(value: xbar_core::presentation::Rgba) -> Rgba {
    Rgba {
        r: value.red,
        g: value.green,
        b: value.blue,
        a: value.alpha,
    }
}

fn cell<T: ScrollOwner + 'static>(
    spec: ControlSpec,
    tags: TagPalette,
    palette: Palette,
    dispatch: fn(&mut T, UserAction),
    cx: &mut Context<T>,
) -> impl IntoElement + use<T> {
    let is_tag = matches!(spec.id, NodeId::Tag(_));
    let state = VisualState {
        selected: spec.state.selected,
        urgent: spec.state.urgent,
        occupied: spec.state.occupied,
        hovered: false,
    };
    let tag = tags.colors(state);
    let foreground = color(if is_tag { tag.foreground } else { palette.text });
    let background = color(if is_tag { tag.fill } else { palette.occupied });
    let hovered = color(if is_tag {
        tags.hovered.fill
    } else {
        palette.hovered
    });
    let border = color(if is_tag {
        tag.stroke.map_or(tag.fill, |stroke| stroke.color)
    } else {
        palette.muted_text
    });
    let title = SharedString::from(spec.tooltip_text());
    let mut label = div().flex().flex_col().items_center().justify_center();
    for line in spec.sidebar_lines() {
        label = label.child(div().child(SharedString::from(line)));
    }
    let bindings = spec.bindings;
    let mut item = div()
        .id(SharedString::from(format!("sidebar-{:?}", spec.id)))
        .relative()
        .w_full()
        .min_h(px(36.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .py(px(4.))
        .px(px(2.))
        .rounded(px(6.))
        .border_1()
        .border_color(border)
        .bg(background)
        .text_color(foreground)
        .text_size(px(11.))
        .overflow_hidden()
        .child(label)
        .tooltip(move |_, cx| cx.new(|_| ControlTooltip(title.clone())).into())
        .hover(move |style| style.bg(hovered))
        .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
            let edge = this.scroll_edge();
            this.control_scroll().hover(
                hovered.then_some(spec.id),
                edge,
                std::time::Instant::now(),
            );
        }));
    if is_tag && spec.state.occupied {
        item = item.child(
            div()
                .absolute()
                .bottom(px(2.))
                .w(px(6.))
                .h(px(2.))
                .rounded(px(1.))
                .bg(foreground),
        );
    }
    if spec.state.enabled {
        if let Some(action) = bindings.primary {
            item = item
                .focusable()
                .tab_index(0)
                .focus(|style| style.border_color(gpui::rgb(0x66b3ff)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.control_scroll().reset();
                    dispatch(this, action);
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                    let action = match event.keystroke.key.as_str() {
                        "enter" | "space" => Some(action),
                        "up" => bindings.scroll_up,
                        "down" => bindings.scroll_down,
                        _ => None,
                    };
                    if let Some(action) = action {
                        dispatch(this, action);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }));
        }
        if let Some(action) = bindings.secondary {
            item = item.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.control_scroll().reset();
                    dispatch(this, action);
                    cx.notify();
                }),
            );
        }
        item = item.on_scroll(cx.listener(move |this, event: &ScrollEvent, _, cx| {
            if bindings.scroll_up.is_none() && bindings.scroll_down.is_none() {
                return;
            }
            cx.stop_propagation();
            let action = scroll_action(this, spec.id, event).and_then(|input| match input {
                PointerAction::ScrollUp => bindings.scroll_up,
                PointerAction::ScrollDown => bindings.scroll_down,
                _ => None,
            });
            if let Some(action) = action {
                dispatch(this, action);
                cx.stop_propagation();
                cx.notify();
            }
        }));
    }
    item
}

pub fn render<T: ScrollOwner + 'static, D: IntoElement>(
    snapshot: &BarSnapshot,
    config: &PresentationConfig,
    background: Rgba,
    dock: D,
    dock_bounds: Option<xbar_core::presentation::Rect>,
    dispatch: fn(&mut T, UserAction),
    cx: &mut Context<T>,
) -> impl IntoElement + use<T, D> {
    let projection = PresentationProjector::project(snapshot.view(), config);
    let tags = TagPalette::for_wallpaper(
        snapshot.theme,
        snapshot.wallpaper_luminance,
        config.tag_contrast,
    );
    let palette = Palette::for_theme(snapshot.theme);
    let mut leading = div()
        .id("sidebar-workspaces")
        .flex()
        .flex_col()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .gap(px(4.));
    for spec in projection
        .tags
        .into_iter()
        .chain([projection.layout_button])
        .chain(projection.layout_choices)
    {
        leading = leading.child(cell(spec, tags, palette, dispatch, cx));
    }
    let mut trailing = div()
        .id("sidebar-controls")
        .flex()
        .flex_col()
        .w_full()
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .gap(px(4.));
    for spec in projection.status.into_iter().rev() {
        trailing = trailing.child(cell(spec, tags, palette, dispatch, cx));
    }
    let mut controls = div()
        .absolute()
        .top(px(3.))
        .left(px(3.))
        .right(px(3.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(leading)
        .child(trailing);
    controls = if let Some(bounds) = dock_bounds {
        controls.h(px((bounds.y - 9.0).max(0.0)))
    } else {
        controls.bottom(px(3.))
    };
    div()
        .size_full()
        .relative()
        .overflow_hidden()
        .bg(background)
        .child(controls)
        .child(dock)
}

/// The fixed shelf and every hit/paint rectangle use the same logical
/// allocation that the bridge publishes to JWM. It never joins a scroller.
pub fn render_dock<T: 'static>(
    bridge: &DockBridge,
    theme: ThemeMode,
    on_hover: fn(&mut T, DockItemBinding, bool, &mut Context<T>),
    on_restore: fn(&mut T, DockItemBinding, &mut Context<T>),
    cx: &mut Context<T>,
) -> impl IntoElement + use<T> {
    let bounds = bridge.shelf_bounds().unwrap_or_default();
    let palette = Palette::for_theme(theme);
    let mut shelf = div()
        .id("minimized-dock")
        .absolute()
        .left(px(bounds.x))
        .top(px(bounds.y))
        .w(px(bounds.width))
        .h(px(bounds.height));
    for window in bridge.visible_windows() {
        let token = window.token;
        let Some((binding, slot, item)) = bridge
            .item_binding(token)
            .zip(bridge.slot_bounds(token))
            .zip(bridge.item_bounds(token))
            .map(|((binding, slot), item)| (binding, slot, item))
        else {
            continue;
        };
        let title = SharedString::from(if window.title.trim().is_empty() {
            window.app_id.clone()
        } else {
            window.title.clone()
        });
        let card = div()
            .absolute()
            .left(px(item.x - slot.x))
            .top(px(item.y - slot.y))
            .w(px(item.width))
            .h(px(item.height))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .border_1()
            .border_color(color(if window.urgent() {
                palette.urgent_stroke
            } else {
                palette.muted_text
            }))
            .bg(color(if window.urgent() {
                palette.urgent
            } else {
                palette.selected
            }))
            .text_color(color(palette.selected_text))
            .text_size(px(11.))
            .child(window.initial().to_string());
        shelf = shelf.child(
            div()
                .id(SharedString::from(format!(
                    "dock-slot-{}-{}-{}",
                    binding.wm_session_id,
                    binding.minimized_generation,
                    token.get()
                )))
                .absolute()
                .left(px(slot.x - bounds.x))
                .top(px(slot.y - bounds.y))
                .w(px(slot.width))
                .h(px(slot.height))
                .focusable()
                .tab_index(0)
                .focus(|style| style.bg(gpui::rgba(0x66b3ff40)))
                .tooltip(move |_, cx| cx.new(|_| ControlTooltip(title.clone())).into())
                .child(card)
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    on_hover(this, binding, *hovered, cx)
                }))
                .on_click(cx.listener(move |this, _, _, cx| on_restore(this, binding, cx)))
                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        on_restore(this, binding, cx);
                        cx.stop_propagation();
                    }
                })),
        );
    }
    if let Some(overflow) = bridge.overflow_bounds() {
        let mut item = div()
            .id("dock-overflow")
            .absolute()
            .left(px(overflow.x - bounds.x))
            .top(px(overflow.y - bounds.y))
            .w(px(overflow.width))
            .h(px(overflow.height))
            .flex()
            .items_center()
            .justify_center()
            .text_color(color(palette.text))
            .text_size(px(11.))
            .child(if bridge.overflow_count() > 0 {
                format!("+{}", bridge.overflow_count())
            } else {
                "…".into()
            });
        if let Some(binding) = bridge.overflow_binding() {
            item = item
                .focusable()
                .tab_index(0)
                .focus(|style| style.bg(gpui::rgba(0x66b3ff40)))
                .tooltip(|_, cx| {
                    cx.new(|_| ControlTooltip("Restore another minimized window".into()))
                        .into()
                })
                .on_click(cx.listener(move |this, _, _, cx| on_restore(this, binding, cx)))
                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        on_restore(this, binding, cx);
                        cx.stop_propagation();
                    }
                }));
        }
        shelf = shelf.child(item);
    }
    shelf
}
