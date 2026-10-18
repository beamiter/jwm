//! Fixed physical thickness is not the same as a fixed logical width.
//! Exercise the production layout and Pango rasterizer at managed DPI.
#![cfg(feature = "render-cairo")]

use cairo::{Context, Format, ImageSurface};
use pango::FontDescription;
use xbar_core::presentation::{
    InteractionState, LayoutEngine, NodeId, PresentationConfig, Rect, Scene, SceneNode, Size,
};
use xbar_core::render::cairo::CairoRenderer;
use xbar_core::{BarEdge, BarModel, TagId, TagState, ThemeMode};

fn raster(renderer: &CairoRenderer, scene: &Scene, width: u32, height: u32, scale: f64) -> Vec<u8> {
    let mut surface = ImageSurface::create(Format::ARgb32, width as i32, height as i32).unwrap();
    let context = Context::new(&surface).unwrap();
    context.scale(scale, scale);
    renderer.render(&context, scene).unwrap();
    drop(context);
    surface.flush();
    assert_eq!(surface.stride() as u32, width * 4);
    surface.data().unwrap().to_vec()
}

fn contains_ink(a: &[u8], b: &[u8], width: u32, height: u32, rect: Rect, scale: f64) -> bool {
    let left = (f64::from(rect.x) * scale).floor().max(0.0) as u32;
    let top = (f64::from(rect.y) * scale).floor().max(0.0) as u32;
    let right = ((f64::from(rect.right()) * scale).ceil() as u32).min(width);
    let bottom = ((f64::from(rect.bottom()) * scale).ceil() as u32).min(height);
    (top..bottom).any(|y| {
        (left..right).any(|x| {
            let offset = ((y * width + x) * 4) as usize;
            a[offset..offset + 4] != b[offset..offset + 4]
        })
    })
}

fn exercise(edge: BarEdge, padding: f32) {
    let active = TagId::new(2).unwrap();
    let mut snapshot = BarModel::default().snapshot();
    snapshot.wm_available = true;
    snapshot.wm_session_id = 1;
    snapshot.wm_sequence = Some(1);
    snapshot.active_tag = Some(active);
    snapshot.tags = (0..9)
        .map(|index| TagState {
            selected: index == 2,
            occupied: index == 2,
            ..TagState::default()
        })
        .collect();
    snapshot.time = "12:34".into();
    for physical in [32_u32, 42, 48] {
        for scale in [1.0_f64, 1.25, 1.5, 2.0] {
            for theme in [ThemeMode::Dark, ThemeMode::Light] {
                snapshot.theme = theme;
                let mut config = PresentationConfig {
                    edge,
                    physical_thickness: Some(physical),
                    bar_height: (f64::from(physical) / scale) as f32,
                    vertical_padding: padding,
                    tag_labels: (1..=9).map(|n| n.to_string()).collect(),
                    ..PresentationConfig::default()
                };
                config.labels.clock.clear();
                let viewport = Size::new(
                    config.logical_thickness(scale) as f32,
                    (1440.0 / scale) as f32,
                );
                let measure_surface =
                    ImageSurface::create(Format::ARgb32, physical as i32, 1440).unwrap();
                let context = Context::new(&measure_surface).unwrap();
                context.scale(scale, scale);
                let renderer = CairoRenderer::new(FontDescription::from_string("DejaVu Sans"));
                let engine = LayoutEngine::new(config, renderer.text_measurer(&context));
                let scene = engine.build(snapshot.view(), viewport, &InteractionState::default());
                let painted = raster(&renderer, &scene, physical, 1440, scale);
                for (id, expected) in [
                    (NodeId::Tag(active), "3"),
                    (NodeId::Clock, "12"),
                    (NodeId::Clock, "34"),
                ] {
                    let label = scene.nodes.iter().find_map(|node| match node {
                        SceneNode::Text {
                            id: actual,
                            text,
                            bounds,
                            ..
                        } if *actual == id && text == expected => Some(*bounds),
                        _ => None,
                    });
                    assert!(
                        label.is_some(),
                        "managed sidebar lost {id:?} label {expected:?}: edge={edge:?}, physical={physical}, scale={scale}, padding={padding}"
                    );
                    let bounds = label.unwrap();
                    assert!(bounds.x >= 0.0 && bounds.right() <= viewport.width + 0.01);
                    assert!(bounds.y >= 0.0 && bounds.bottom() <= viewport.height + 0.01);
                    let hit = scene.hits.iter().find(|hit| hit.id == id).unwrap();
                    assert!(hit.primary.is_some());
                    assert!(
                        hit.bounds.x.abs() < 0.01
                            && (hit.bounds.width - viewport.width).abs() < 0.01
                    );
                    let mut without = scene.clone();
                    without.nodes.retain(|node| !matches!(node, SceneNode::Text { id: actual, text, .. } if *actual == id && text == expected));
                    let empty = raster(&renderer, &without, physical, 1440, scale);
                    assert!(
                        contains_ink(&painted, &empty, physical, 1440, bounds, scale),
                        "label has no rasterized pixels: {edge:?} {physical}px {scale}x {id:?} {expected}"
                    );
                }
                if let Some(directory) = std::env::var_os("XBAR_SIDEBAR_PREVIEWS") {
                    let directory = std::path::PathBuf::from(directory);
                    std::fs::create_dir_all(&directory).unwrap();
                    let rgba: Vec<u8> = painted
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .flat_map(|pixel| {
                            let argb = u32::from_ne_bytes(*pixel);
                            let alpha = (argb >> 24) & 255;
                            let straight = |component: u32| {
                                (component * 255 + alpha / 2)
                                    .checked_div(alpha)
                                    .unwrap_or(0)
                                    .min(255) as u8
                            };
                            [
                                straight((argb >> 16) & 255),
                                straight((argb >> 8) & 255),
                                straight(argb & 255),
                                alpha as u8,
                            ]
                        })
                        .collect();
                    let name = format!(
                        "{}-{physical}px-{}pct-{theme:?}-pad{padding}",
                        edge.as_str(),
                        (scale * 100.0).round() as u32
                    );
                    image::save_buffer(
                        directory.join(format!("{name}.png")),
                        &rgba,
                        physical,
                        1440,
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                    std::fs::write(
                        directory.join(format!("{name}.json")),
                        serde_json::to_vec_pretty(&scene).unwrap(),
                    )
                    .unwrap();
                }
            }
        }
    }
}

#[test]
fn managed_left_sidebar_retains_readable_labels_and_full_width_hits() {
    exercise(BarEdge::Left, 6.0);
}

#[test]
fn managed_right_sidebar_retains_readable_labels_and_full_width_hits() {
    exercise(BarEdge::Right, 6.0);
}

#[test]
fn oversized_sidebar_padding_cannot_consume_all_text_space() {
    exercise(BarEdge::Left, 24.0);
    exercise(BarEdge::Right, 24.0);
}
