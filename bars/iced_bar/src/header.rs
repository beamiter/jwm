//! Iced canvas adapter for the common Tao desktop-header scene.
use super::Message;
use iced::advanced::text::Paragraph as _;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::{Color, Font, Point, Rectangle, Renderer, Size, Theme, mouse};
use xbar_core::presentation::{
    InteractionState, LayoutEngine, PointerAction, Scene, SceneNode, TextAlign, TextMeasurer,
};

#[derive(Default)]
pub struct State {
    interaction: InteractionState,
    pressed: Option<PointerAction>,
    pointer: Option<xbar_core::presentation::Point>,
}

pub struct Header {
    pub snapshot: xbar_core::BarSnapshot,
    pub config: xbar_core::presentation::PresentationConfig,
    pub background_alpha: f32,
}

struct Metrics;
impl TextMeasurer for Metrics {
    fn measure(&self, text: &str, size: f32) -> xbar_core::presentation::Size {
        let paragraph =
            iced::advanced::graphics::text::Paragraph::with_text(iced::advanced::Text {
                content: text,
                bounds: Size::INFINITE,
                size: iced::Pixels(size),
                line_height: iced::advanced::text::LineHeight::Relative(1.25),
                font: Font::DEFAULT,
                align_x: iced::advanced::text::Alignment::Left,
                align_y: iced::alignment::Vertical::Center,
                shaping: iced::advanced::text::Shaping::Advanced,
                wrapping: iced::advanced::text::Wrapping::None,
                ellipsis: iced::advanced::text::Ellipsis::None,
                hint_factor: None,
            });
        xbar_core::presentation::Size::new(paragraph.min_width(), paragraph.min_height())
    }
}

impl Header {
    fn scene(&self, bounds: Rectangle, state: &State) -> Scene {
        LayoutEngine::new(self.config.clone(), Metrics).build(
            self.snapshot.view(),
            xbar_core::presentation::Size::new(bounds.width, bounds.height),
            &state.interaction,
        )
    }
}

fn color(value: xbar_core::presentation::Rgba) -> Color {
    Color::from_rgba(value.red, value.green, value.blue, value.alpha)
}

impl canvas::Program<Message> for Header {
    type State = State;
    fn update(
        &self,
        state: &mut State,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<iced::widget::Action<Message>> {
        let scene = self.scene(bounds, state);
        // Iced samples the window cursor after collecting the native event
        // batch. A later CursorLeft can make it unavailable for an earlier
        // button event. Retain the position in event order so that a queued
        // click uses its own preceding motion, never a later window state.
        let position = match event {
            canvas::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                bounds.contains(*position).then(|| {
                    xbar_core::presentation::Point::new(
                        position.x - bounds.x,
                        position.y - bounds.y,
                    )
                })
            }
            canvas::Event::Mouse(mouse::Event::CursorLeft) => None,
            _ => match cursor {
                mouse::Cursor::Unavailable => state.pointer,
                _ => cursor
                    .position_in(bounds)
                    .map(|p| xbar_core::presentation::Point::new(p.x, p.y)),
            },
        };
        match event {
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                state.pointer = position;
                let changed = if let Some(p) = position {
                    state.interaction.update_hover(&scene, p)
                } else {
                    state.interaction.clear_hover()
                };
                changed.then(iced::widget::Action::request_redraw)
            }
            canvas::Event::Mouse(mouse::Event::CursorLeft) => {
                state.pointer = None;
                state.pressed = None;
                state
                    .interaction
                    .clear_hover()
                    .then(iced::widget::Action::request_redraw)
            }
            canvas::Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                state.pressed = match button {
                    mouse::Button::Left => Some(PointerAction::Primary),
                    mouse::Button::Right => Some(PointerAction::Secondary),
                    _ => None,
                };
                if let Some(p) = position {
                    state.interaction.press(&scene, p);
                }
                state
                    .pressed
                    .map(|_| iced::widget::Action::request_redraw().and_capture())
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(button)) => {
                let input = match button {
                    mouse::Button::Left => Some(PointerAction::Primary),
                    mouse::Button::Right => Some(PointerAction::Secondary),
                    _ => None,
                };
                let held = state.pressed.take();
                let action = position.and_then(|p| {
                    input
                        .filter(|input| Some(*input) == held)
                        .and_then(|input| state.interaction.release(&scene, p, input))
                });
                Some(iced::widget::Action::publish(Message::Activate(action)).and_capture())
            }
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let hit = position.and_then(|p| scene.hit_test(p))?;
                Some(
                    iced::widget::Action::publish(Message::ScrollControl {
                        id: hit.id,
                        delta: *delta,
                        up: hit.scroll_up,
                        down: hit.scroll_down,
                    })
                    .and_capture(),
                )
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &State,
        renderer: &Renderer,
        _: &Theme,
        bounds: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<Geometry> {
        let scene = self.scene(bounds, state);
        let mut frame = Frame::new(renderer, bounds.size());
        for node in scene.nodes {
            match node {
                SceneNode::Background {
                    bounds, mut fill, ..
                } => {
                    fill.alpha *= self.background_alpha;
                    let path = Path::rounded_rectangle(
                        Point::new(bounds.x, bounds.y),
                        Size::new(bounds.width, bounds.height),
                        (bounds.height * 0.5).into(),
                    );
                    frame.fill(&path, color(fill));
                }
                SceneNode::RoundedRect {
                    bounds,
                    radius,
                    fill,
                    stroke,
                    ..
                } => {
                    let path = Path::rounded_rectangle(
                        Point::new(bounds.x, bounds.y),
                        Size::new(bounds.width, bounds.height),
                        radius.into(),
                    );
                    frame.fill(&path, color(fill));
                    if let Some(stroke) = stroke {
                        frame.stroke(
                            &path,
                            Stroke::default()
                                .with_color(color(stroke.color))
                                .with_width(stroke.width),
                        );
                    }
                }
                SceneNode::Text {
                    bounds,
                    text,
                    size,
                    color: foreground,
                    align,
                    ..
                } => {
                    let (x, align_x) = match align {
                        TextAlign::Start => (bounds.x, iced::advanced::text::Alignment::Left),
                        TextAlign::Center => (
                            bounds.x + bounds.width * 0.5,
                            iced::advanced::text::Alignment::Center,
                        ),
                        TextAlign::End => (
                            bounds.x + bounds.width,
                            iced::advanced::text::Alignment::Right,
                        ),
                    };
                    frame.fill_text(Text {
                        content: text,
                        position: Point::new(x, bounds.y + bounds.height * 0.5),
                        max_width: bounds.width,
                        size: iced::Pixels(size),
                        color: color(foreground),
                        font: Font::DEFAULT,
                        align_x,
                        align_y: iced::alignment::Vertical::Center,
                        line_height: iced::advanced::text::LineHeight::Relative(1.25),
                        shaping: iced::advanced::text::Shaping::Advanced,
                        wrapping: iced::advanced::text::Wrapping::None,
                        ..Default::default()
                    });
                }
                SceneNode::Polyline {
                    points,
                    color: foreground,
                    width,
                    ..
                } => {
                    let path = Path::new(|builder| {
                        for (index, p) in points.iter().enumerate() {
                            if index == 0 {
                                builder.move_to(Point::new(p.x, p.y));
                            } else {
                                builder.line_to(Point::new(p.x, p.y));
                            }
                        }
                    });
                    frame.stroke(
                        &path,
                        Stroke::default()
                            .with_color(color(foreground))
                            .with_width(width),
                    );
                }
                SceneNode::Image { bounds, source, .. } => {
                    frame.draw_image(
                        Rectangle::new(
                            Point::new(bounds.x, bounds.y),
                            Size::new(bounds.width, bounds.height),
                        ),
                        &iced::widget::image::Handle::from_path(source.path),
                    );
                }
            }
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.position_in(bounds).is_some_and(|p| {
            self.scene(bounds, state)
                .hit_test(xbar_core::presentation::Point::new(p.x, p.y))
                .is_some()
        }) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::canvas::Program as _;
    use xbar_core::{BarModel, ModelConfig, TagState, UserAction};

    fn header() -> Header {
        let mut snapshot = BarModel::new(ModelConfig::default()).unwrap().snapshot();
        snapshot.wm_available = true;
        snapshot.tags = vec![TagState::default(); 9];
        for index in [0, 2, 7] {
            snapshot.tags[index].occupied = true;
        }
        snapshot.tags[4].selected = true;
        let mut config = xbar_core::presentation::PresentationConfig::default();
        config.apply_desktop_header_profile();
        Header {
            snapshot,
            config,
            background_alpha: 1.0,
        }
    }

    fn update(
        header: &Header,
        state: &mut State,
        event: mouse::Event,
        cursor: mouse::Cursor,
    ) -> Option<Message> {
        header
            .update(
                state,
                &canvas::Event::Mouse(event),
                Rectangle::new(Point::new(20.0, 30.0), Size::new(1280.0, 42.0)),
                cursor,
            )
            .and_then(|action| action.into_inner().0)
    }

    #[test]
    fn queued_click_uses_event_position_when_latest_window_cursor_has_left() {
        let header = header();
        let mut state = State::default();
        update(
            &header,
            &mut state,
            mouse::Event::CursorMoved {
                position: Point::new(128.0, 51.0),
            },
            mouse::Cursor::Unavailable,
        );
        update(
            &header,
            &mut state,
            mouse::Event::ButtonPressed(mouse::Button::Right),
            mouse::Cursor::Unavailable,
        );
        let action = update(
            &header,
            &mut state,
            mouse::Event::ButtonReleased(mouse::Button::Right),
            mouse::Cursor::Unavailable,
        );
        assert!(
            matches!(action, Some(Message::Activate(Some(UserAction::ToggleTag(tag)))) if tag.index() == 2)
        );
    }

    #[test]
    fn leaving_before_release_cancels_the_cached_click() {
        let header = header();
        let mut state = State::default();
        update(
            &header,
            &mut state,
            mouse::Event::CursorMoved {
                position: Point::new(128.0, 51.0),
            },
            mouse::Cursor::Unavailable,
        );
        update(
            &header,
            &mut state,
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Cursor::Unavailable,
        );
        update(
            &header,
            &mut state,
            mouse::Event::CursorLeft,
            mouse::Cursor::Unavailable,
        );
        let action = update(
            &header,
            &mut state,
            mouse::Event::ButtonReleased(mouse::Button::Left),
            mouse::Cursor::Unavailable,
        );
        assert!(matches!(action, Some(Message::Activate(None))));
    }
}
