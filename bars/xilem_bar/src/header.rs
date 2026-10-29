//! Xilem/Masonry adapter for the shared desktop-header scene and hit map.
use super::*;
use masonry::core::render_text;
use masonry::kurbo::{Affine, BezPath, RoundedRect};
use masonry::parley::style::{FontFamily, FontFamilyName, StyleProperty};
use masonry::parley::{FontContext, LayoutContext};
use std::cell::RefCell;
use xbar_core::presentation::{InteractionState, LayoutEngine, SceneNode, TextAlign, TextMeasurer};
use xilem::view::canvas;

thread_local! {
    static METRICS: RefCell<(FontContext, LayoutContext<()>)> =
        RefCell::new((FontContext::new(), LayoutContext::new()));
    static IMAGES: RefCell<Vec<(u64, Option<masonry::peniko::ImageData>)>> = const { RefCell::new(Vec::new()) };
}

struct Metrics;
impl TextMeasurer for Metrics {
    fn measure(&self, text: &str, size: f32) -> xbar_core::presentation::Size {
        METRICS.with(|metrics| {
            let mut metrics = metrics.borrow_mut();
            let (fonts, context) = &mut *metrics;
            let mut builder = context.ranged_builder(fonts, text, 1.0, true);
            builder.push_default(StyleProperty::FontSize(size));
            builder.push_default(StyleProperty::FontFamily(FontFamily::Single(
                FontFamilyName::Generic(masonry::parley::GenericFamily::SansSerif),
            )));
            let mut layout = builder.build(text);
            layout.break_all_lines(None);
            xbar_core::presentation::Size::new(layout.width(), layout.height())
        })
    }
}

fn color(value: xbar_core::presentation::Rgba) -> Color {
    Color::from_rgba8(
        (value.red * 255.0).round() as u8,
        (value.green * 255.0).round() as u8,
        (value.blue * 255.0).round() as u8,
        (value.alpha * 255.0).round() as u8,
    )
}

fn image(source: &xbar_core::presentation::ImageSource) -> Option<masonry::peniko::ImageData> {
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        if let Some((_, image)) = images.iter().find(|(key, _)| *key == source.key) {
            return image.clone();
        }
        let decoded = (|| {
            if std::fs::metadata(&source.path).ok()?.len() > 8 * 1024 * 1024 {
                return None;
            }
            let mut reader = image::ImageReader::open(&source.path).ok()?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(2048);
            limits.max_image_height = Some(2048);
            limits.max_alloc = Some(32 * 1024 * 1024);
            reader.limits(limits);
            let pixels = reader.decode().ok()?.to_rgba8();
            Some(masonry::peniko::ImageData {
                width: pixels.width(),
                height: pixels.height(),
                format: masonry::peniko::ImageFormat::Rgba8,
                alpha_type: masonry::peniko::ImageAlphaType::Alpha,
                data: pixels.into_raw().into(),
            })
        })();
        if images.len() >= 64 {
            images.remove(0);
        }
        images.push((source.key, decoded.clone()));
        decoded
    })
}

pub fn view(state: &mut XilemBar) -> Box<xilem::AnyWidgetView<XilemBar>> {
    let bridge = state.geometry_bridge.snapshot();
    let scale = bridge.scale_factor.max(f64::EPSILON);
    let geometry = state
        .runtime
        .view()
        .geometry
        .or(bridge.geometry)
        .unwrap_or(MonitorGeometry {
            x: 0,
            y: 0,
            width: 1280,
            height: 42,
        });
    let viewport = xbar_core::presentation::Size::new(
        geometry.width as f32 / scale as f32,
        state.presentation.logical_thickness(scale) as f32,
    );
    let scene = LayoutEngine::new(state.presentation.clone(), Metrics).build(
        state.runtime.view(),
        viewport,
        &InteractionState::default(),
    );
    let hits = scene.hits.clone();
    let bounds = xbar_core::presentation::Rect::from_size(viewport);
    let alpha = xbar_core::background::bar_background_opacity(
        state.runtime.view().blur_status_bar,
        state.background_opacity,
        state.translucent,
    ) as f32;
    let drawing = canvas(move |_: &mut XilemBar, ctx, recording, _size| {
        let mut painter = Painter::new(recording);
        let outer = RoundedRect::new(
            0.0,
            0.0,
            f64::from(viewport.width),
            f64::from(viewport.height),
            f64::from(viewport.height) * 0.5,
        );
        painter.with_fill_clip(outer, |painter| {
            for node in &scene.nodes {
                let b = node.bounds();
                let rect = masonry::kurbo::Rect::new(
                    f64::from(b.x),
                    f64::from(b.y),
                    f64::from(b.right()),
                    f64::from(b.bottom()),
                );
                match node {
                    SceneNode::Background { fill, .. } => {
                        let mut fill = *fill;
                        fill.alpha *= alpha;
                        painter.fill(rect, color(fill)).draw();
                    }
                    SceneNode::RoundedRect {
                        radius,
                        fill,
                        stroke,
                        ..
                    } => {
                        let shape = rect.to_rounded_rect(f64::from(*radius));
                        painter.fill(shape, color(*fill)).draw();
                        if let Some(stroke) = stroke {
                            painter
                                .stroke(
                                    shape,
                                    &masonry::kurbo::Stroke::new(f64::from(stroke.width)),
                                    color(stroke.color),
                                )
                                .draw();
                        }
                    }
                    SceneNode::Text {
                        text,
                        size,
                        color: foreground,
                        align,
                        ..
                    } => {
                        let (fonts, layouts) = ctx.text_contexts();
                        let mut builder = layouts.ranged_builder(fonts, text, 1.0, true);
                        builder.push_default(StyleProperty::FontSize(*size));
                        builder.push_default(StyleProperty::FontFamily(FontFamily::Single(
                            FontFamilyName::Generic(masonry::parley::GenericFamily::SansSerif),
                        )));
                        let mut layout = builder.build(text);
                        layout.break_all_lines(None);
                        let alignment = match align {
                            TextAlign::Start => masonry::TextAlign::Start,
                            TextAlign::Center => masonry::TextAlign::Center,
                            TextAlign::End => masonry::TextAlign::End,
                        };
                        layout.align(Some(b.width), alignment, Default::default());
                        let transform = Affine::translate((
                            f64::from(b.x),
                            f64::from(b.y)
                                + (f64::from(b.height) - f64::from(layout.height())) * 0.5,
                        ));
                        painter.with_fill_clip(rect, |painter| {
                            render_text(
                                painter,
                                transform,
                                &layout,
                                &[color(*foreground).into()],
                                true,
                            )
                        });
                    }
                    SceneNode::Polyline {
                        points,
                        color: foreground,
                        width,
                        ..
                    } => {
                        let mut path = BezPath::new();
                        for (index, p) in points.iter().enumerate() {
                            if index == 0 {
                                path.move_to((f64::from(p.x), f64::from(p.y)));
                            } else {
                                path.line_to((f64::from(p.x), f64::from(p.y)));
                            }
                        }
                        painter
                            .stroke(
                                &path,
                                &masonry::kurbo::Stroke::new(f64::from(*width)),
                                color(*foreground),
                            )
                            .draw();
                    }
                    SceneNode::Image { source, .. } => {
                        if let Some(image) = image(source) {
                            let source_rect = masonry::kurbo::Rect::new(
                                0.0,
                                0.0,
                                f64::from(image.width),
                                f64::from(image.height),
                            );
                            let transform =
                                masonry::properties::ObjectFit::Contain.affine(rect, source_rect);
                            painter.with_fill_clip(rect, |painter| {
                                painter.draw_image(&image, transform)
                            });
                        }
                    }
                }
            }
        });
    })
    .alt_text(
        "JWM desktop header: launcher, active workspaces, focused application and system status",
    );
    let controls: Vec<_> = hits
        .into_iter()
        .map(|hit| {
            let control = button_any_pointer(label(""), move |state: &mut XilemBar, pointer| {
                let action = if matches!(pointer, Some(PointerButton::Secondary)) {
                    hit.secondary
                } else {
                    hit.primary
                };
                if let Some(action) = action {
                    state.dispatch(action);
                }
            })
            .prop(Background::Color(Color::TRANSPARENT))
            .prop(BorderColor::new(Color::TRANSPARENT))
            .prop(BorderWidth::all(Length::ZERO))
            .prop(Padding::ZERO)
            .dims(Dimensions::new(Dim::Stretch, Dim::Stretch));
            let control = hover_sensor(
                control,
                format!("{:?}", hit.id),
                move |state: &mut XilemBar, event| {
                    handle_control_scroll(state, hit.id, hit.scroll_up, hit.scroll_down, event);
                },
            )
            .capture_wheel(hit.scroll_up.is_some() || hit.scroll_down.is_some());
            zstack_item(
                sized_box(control).dims(Dimensions::new(
                    Dim::Fixed(Length::px(f64::from(hit.bounds.width))),
                    Dim::Fixed(Length::px(f64::from(hit.bounds.height))),
                )),
                dock_alignment(bounds, hit.bounds),
            )
        })
        .collect();
    sized_box(zstack((drawing, controls)))
        .dims(Dimensions::new(Dim::Stretch, Dim::Stretch))
        .boxed()
}
