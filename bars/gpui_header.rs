//! Native GPUI rendering of the same desktop-header scene used by Tao.
//! The two GPUI frontends share layout and semantic hit regions, while GPUI
//! still shapes text, builds native elements and renders the window.
use gpui::prelude::*;
use gpui::{Context, MouseButton, SharedString, TextRun, Window, div, px};
use xbar_core::presentation::{
    InteractionState, LayoutEngine, Rect, Rgba, SceneNode, Size, TextAlign, TextMeasurer,
};
use xbar_core::{BarSnapshot, UserAction};

fn color(value: Rgba) -> gpui::Rgba {
    gpui::Rgba {
        r: value.red,
        g: value.green,
        b: value.blue,
        a: value.alpha,
    }
}

struct Metrics<'a>(&'a Window, SharedString);
impl TextMeasurer for Metrics<'_> {
    fn measure(&self, text: &str, size: f32) -> Size {
        let text = text.replace('\n', " ");
        let run = TextRun {
            len: text.len(),
            font: gpui::font(self.1.clone()),
            ..Default::default()
        };
        let line = self
            .0
            .text_system()
            .shape_line(text.into(), px(size), &[run], None);
        Size::new(
            line.width.into(),
            f32::from(line.ascent + line.descent).max(size),
        )
    }
}

fn positioned(bounds: Rect) -> gpui::Div {
    div()
        .absolute()
        .left(px(bounds.x))
        .top(px(bounds.y))
        .w(px(bounds.width))
        .h(px(bounds.height))
        .overflow_hidden()
}

pub fn render<T: super::gpui_sidebar::ScrollOwner + 'static>(
    snapshot: &BarSnapshot,
    config: &xbar_core::presentation::PresentationConfig,
    background_alpha: f32,
    dispatch: fn(&mut T, UserAction),
    window: &mut Window,
    cx: &mut Context<T>,
) -> gpui::AnyElement {
    let bounds = window.viewport_size();
    let viewport = Size::new(bounds.width.into(), bounds.height.into());
    static FAMILY: std::sync::OnceLock<SharedString> = std::sync::OnceLock::new();
    let family = FAMILY
        .get_or_init(|| {
            let names = window.text_system().all_font_names();
            ["DejaVu Sans", "Noto Sans", "Liberation Sans", "Arial"]
                .into_iter()
                .find_map(|name| {
                    names
                        .iter()
                        .find(|candidate| candidate.eq_ignore_ascii_case(name))
                        .cloned()
                })
                .unwrap_or_else(|| "JetBrainsMono Nerd Font".into())
                .into()
        })
        .clone();
    let scene = LayoutEngine::new(config.clone(), Metrics(window, family.clone())).build(
        snapshot.view(),
        viewport,
        &InteractionState::default(),
    );
    let image_nodes = scene
        .nodes
        .iter()
        .filter_map(|node| match node {
            SceneNode::Image { bounds, source, .. } => Some((*bounds, source.path.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let paint_family = family.clone();
    let mut root = div()
        .flex()
        .relative()
        .w(px(viewport.width))
        .h(px(viewport.height))
        .rounded(px(viewport.height * 0.5))
        .overflow_hidden()
        .child(
            gpui::canvas(
                |_, _, _| (),
                move |canvas_bounds, _, window, cx| {
                    let rect = |bounds: Rect| {
                        gpui::Bounds::new(
                            gpui::point(
                                canvas_bounds.left() + px(bounds.x),
                                canvas_bounds.top() + px(bounds.y),
                            ),
                            gpui::size(px(bounds.width), px(bounds.height)),
                        )
                    };
                    for node in scene.nodes {
                        match node {
                            SceneNode::Background {
                                bounds, mut fill, ..
                            } => {
                                fill.alpha *= background_alpha;
                                window.paint_quad(gpui::quad(
                                    rect(bounds),
                                    px(viewport.height * 0.5),
                                    color(fill),
                                    px(0.),
                                    gpui::transparent_black(),
                                    Default::default(),
                                ));
                            }
                            SceneNode::RoundedRect {
                                bounds,
                                radius,
                                fill,
                                stroke,
                                ..
                            } => {
                                window.paint_quad(gpui::quad(
                                    rect(bounds),
                                    px(radius),
                                    color(fill),
                                    px(stroke.map_or(0., |stroke| stroke.width)),
                                    stroke.map_or_else(gpui::transparent_black, |stroke| {
                                        color(stroke.color).into()
                                    }),
                                    Default::default(),
                                ));
                            }
                            SceneNode::Text {
                                bounds,
                                text,
                                size,
                                color: foreground,
                                align,
                                ..
                            } => {
                                let text = text.replace('\n', " ");
                                let run = TextRun {
                                    len: text.len(),
                                    font: gpui::font(paint_family.clone()),
                                    color: color(foreground).into(),
                                    ..Default::default()
                                };
                                let line = window.text_system().shape_line(
                                    text.into(),
                                    px(size),
                                    &[run],
                                    None,
                                );
                                let height = line.ascent + line.descent;
                                let bounds = rect(bounds);
                                let origin = gpui::point(
                                    bounds.left(),
                                    bounds.top() + (bounds.size.height - height) * 0.5,
                                );
                                let align = match align {
                                    TextAlign::Start => gpui::TextAlign::Left,
                                    TextAlign::Center => gpui::TextAlign::Center,
                                    TextAlign::End => gpui::TextAlign::Right,
                                };
                                window.with_content_mask(
                                    Some(gpui::ContentMask { bounds }),
                                    |window| {
                                        let _ = line.paint(
                                            origin,
                                            height,
                                            align,
                                            Some(bounds.size.width),
                                            window,
                                            cx,
                                        );
                                    },
                                );
                            }
                            SceneNode::Polyline {
                                points,
                                color: foreground,
                                width,
                                ..
                            } => {
                                let mut path = gpui::PathBuilder::stroke(px(width));
                                for (index, point) in points.iter().enumerate() {
                                    let point = gpui::point(
                                        canvas_bounds.left() + px(point.x),
                                        canvas_bounds.top() + px(point.y),
                                    );
                                    if index == 0 {
                                        path.move_to(point);
                                    } else {
                                        path.line_to(point);
                                    }
                                }
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, color(foreground));
                                }
                            }
                            SceneNode::Image { .. } => {}
                        }
                    }
                },
            )
            .w(px(viewport.width))
            .h(px(viewport.height)),
        );
    for (bounds, path) in image_nodes {
        root = root.child(
            positioned(bounds).child(
                gpui::img(path)
                    .size_full()
                    .object_fit(gpui::ObjectFit::Contain),
            ),
        );
    }
    for hit in scene.hits {
        let mut target = positioned(hit.bounds)
            .id(SharedString::from(format!("desktop-header-{:?}", hit.id)))
            .hover(|style| style.bg(gpui::rgba(0x7799bb12)))
            .on_hover(cx.listener(move |this, hovered: &bool, _, _| {
                super::gpui_sidebar::hover_control(this, hit.id, *hovered);
            }));
        if let Some(action) = hit.primary {
            target = target
                .focusable()
                .tab_index(0)
                .focus(|style| style.bg(gpui::rgba(0x6699cc28)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    dispatch(this, action);
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        dispatch(this, action);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }));
        }
        if let Some(action) = hit.secondary {
            target = target.on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    dispatch(this, action);
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        }
        if hit.scroll_up.is_some() || hit.scroll_down.is_some() {
            target =
                target.on_scroll(cx.listener(move |this, event: &gpui::ScrollEvent, _, cx| {
                    let action = super::gpui_sidebar::scroll_action(this, hit.id, event)
                        .and_then(|input| hit.action(input));
                    if let Some(action) = action {
                        dispatch(this, action);
                        cx.notify();
                    }
                    cx.stop_propagation();
                }));
        }
        root = root.child(target);
    }
    root.into_any_element()
}
