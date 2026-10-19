//! Exercise numeric columns with the production Pango measurer and Cairo
//! renderer. Character counts cannot detect proportional-font or DPI drift.
#![cfg(feature = "render-cairo")]

use cairo::{Context, Format, ImageSurface};
use pango::FontDescription;
use xbar_core::presentation::{
    InteractionState, LayoutEngine, NodeId, PresentationConfig, Rect, Scene, SceneNode, Size,
    TextAlign, TextMeasurer,
};
use xbar_core::render::cairo::CairoRenderer;
use xbar_core::{
    AudioState, BarEdge, BarModel, BarSnapshot, BatteryState, BrightnessState, IconSet,
    NetworkState, Percent, SystemState, TagId, TagState,
};

const FONTS: [&str; 2] = ["DejaVu Sans", "DejaVu Sans Mono"];
const READINGS: [Option<f64>; 7] = [
    Some(0.0),
    Some(9.0),
    Some(10.0),
    Some(99.0),
    Some(100.0),
    None,
    Some(99.99),
];

fn percent(value: Option<f64>) -> Option<Percent> {
    value.map(|value| Percent::new(value).unwrap())
}

fn value_text(value: Option<Percent>) -> String {
    value.map_or_else(|| "--".into(), |value| format!("{}%", value.rounded()))
}

fn fixture() -> (PresentationConfig, BarSnapshot) {
    let mut config = PresentationConfig {
        tag_labels: vec!["1".into(), "2".into(), "3".into()],
        ..PresentationConfig::default()
    };
    // Keep real neighbouring controls, but leave enough room for CPU and MEM
    // to remain visible in the narrow cases rather than testing hidden items.
    config.visibility.minimized_windows = false;
    config.visibility.shell_hub = false;
    config.visibility.screenshot = false;
    config.visibility.theme = false;
    config.labels.cpu = "CPU".into();
    config.labels.memory = "MEM".into();
    config.labels.audio = "A".into();
    config.labels.muted = "X".into();
    config.labels.brightness = "B".into();
    config.labels.battery = "P".into();
    config.labels.charging = "+".into();
    config.labels.network = "N".into();
    config.labels.network_offline = "-".into();
    config.labels.clock.clear();
    let mut snapshot = BarModel::default().snapshot();
    snapshot.wm_available = true;
    snapshot.wm_session_id = 1;
    snapshot.wm_sequence = Some(1);
    snapshot.active_tag = Some(TagId::new(1).unwrap());
    snapshot.tags = (0..3)
        .map(|index| TagState {
            selected: index == 1,
            occupied: index == 1,
            ..TagState::default()
        })
        .collect();
    snapshot.time = "12:34".into();
    snapshot.client_name = "Stable CPU and memory readings".into();
    snapshot.system = SystemState::new(percent(Some(42.0)), percent(Some(57.0)));
    snapshot.audio = AudioState::new(percent(Some(50.0)), false);
    snapshot.brightness = BrightnessState::new(percent(Some(70.0)));
    snapshot.battery = BatteryState::present(percent(Some(80.0)), false);
    snapshot.network = NetworkState::connected("eth0", Some(1_229), Some(142_029));
    (config, snapshot)
}

fn set_value(snapshot: &mut BarSnapshot, id: NodeId, value: Option<Percent>) {
    match id {
        NodeId::Cpu => snapshot.system.cpu_percent = value,
        NodeId::Memory => snapshot.system.memory_percent = value,
        NodeId::Brightness => snapshot.brightness.percent = value,
        NodeId::Audio => snapshot.audio.volume_percent = value,
        NodeId::Battery => snapshot.battery.percent = value,
        _ => panic!("not a percentage control: {id:?}"),
    }
}

fn assert_stable(before: &Scene, after: &Scene, changed: NodeId, case: &str) {
    assert_eq!(after.hits, before.hits, "hit map moved: {case}");
    assert_eq!(
        after.bounds_for(changed),
        before.bounds_for(changed),
        "metric bounds moved: {case}"
    );
    let neighbours = |scene: &Scene| {
        scene
            .nodes
            .iter()
            .filter(|node| node.id() != changed)
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        neighbours(after),
        neighbours(before),
        "neighbouring primitives changed: {case}"
    );
}

fn value_slot(scene: &Scene, id: NodeId) -> Option<(Rect, &str, f32)> {
    scene.nodes_for(id).find_map(|node| match node {
        SceneNode::Text {
            bounds,
            text,
            size,
            align: TextAlign::End,
            ..
        } => Some((*bounds, text.as_str(), *size)),
        _ => None,
    })
}

fn assert_full_value(
    scene: &Scene,
    reference: &Scene,
    id: NodeId,
    expected: Option<Percent>,
    measurer: &impl TextMeasurer,
    case: &str,
) -> Rect {
    let (bounds, text, size) =
        value_slot(scene, id).unwrap_or_else(|| panic!("full value slot missing: {case}"));
    let reference = value_slot(reference, id).expect("reference must have a full value slot");
    assert_eq!(text, value_text(expected), "percentage was clipped: {case}");
    assert_eq!(
        bounds, reference.0,
        "right-aligned value slot moved: {case}"
    );
    assert_eq!(
        bounds.right(),
        reference.0.right(),
        "right anchor moved: {case}"
    );
    assert!(
        measurer.measure(text, size).width <= bounds.width + 0.01,
        "Pango ink does not fit the reserved value slot: {case}"
    );
    bounds
}

fn assert_sidebar_value(
    scene: &Scene,
    reference: &Scene,
    id: NodeId,
    expected: Option<Percent>,
    measurer: &impl TextMeasurer,
    case: &str,
) -> Rect {
    let find = |scene: &Scene| {
        scene.nodes_for(id).find_map(|node| match node {
            SceneNode::Text {
                bounds, text, size, ..
            } if text.ends_with('%') || text == "--" => Some((*bounds, text.clone(), *size)),
            _ => None,
        })
    };
    let (bounds, text, size) =
        find(scene).unwrap_or_else(|| panic!("sidebar percentage missing: {case}"));
    let before = find(reference).expect("reference sidebar value must exist");
    assert_eq!(
        text,
        value_text(expected),
        "sidebar percentage clipped: {case}"
    );
    assert_eq!(bounds, before.0, "sidebar value slot moved: {case}");
    assert_eq!(size, before.2, "sidebar value font size changed: {case}");
    assert!(
        measurer.measure(&text, size).width <= bounds.width + 0.01,
        "sidebar percentage does not fit its slot: {case}"
    );
    bounds
}

fn pixels(surface: &ImageSurface) -> Vec<u8> {
    let mut data = Vec::new();
    surface
        .with_data(|bytes| data.extend_from_slice(bytes))
        .unwrap();
    data
}

fn assert_value_paints(
    renderer: &CairoRenderer,
    context: &Context,
    surface: &ImageSurface,
    scene: &Scene,
    value: (NodeId, Rect),
    scale: f64,
    case: &str,
) {
    let (id, bounds) = value;
    renderer.render(context, scene).unwrap();
    let painted = pixels(surface);
    let mut without = scene.clone();
    without.nodes.retain(|node| {
        !matches!(node, SceneNode::Text { id: actual, bounds: actual_bounds, .. } if *actual == id && *actual_bounds == bounds)
    });
    renderer.render(context, &without).unwrap();
    let empty = pixels(surface);
    let left = (f64::from(bounds.x) * scale).floor().max(0.0) as usize;
    let top = (f64::from(bounds.y) * scale).floor().max(0.0) as usize;
    let right = ((f64::from(bounds.right()) * scale).ceil() as usize).min(surface.width() as usize);
    let bottom =
        ((f64::from(bounds.bottom()) * scale).ceil() as usize).min(surface.height() as usize);
    let stride = surface.stride() as usize;
    assert!(
        (top..bottom).any(|y| (left..right).any(|x| {
            let offset = y * stride + x * 4;
            painted[offset..offset + 4] != empty[offset..offset + 4]
        })),
        "value produced no visible Cairo pixels: {case}"
    );
}

#[test]
fn cpu_memory_readings_keep_neighbours_and_hits_fixed_on_every_edge() {
    let mut previews = Vec::new();
    for font in FONTS {
        // A fractional device scale and a larger proportional font catch
        // hinted glyph widths that character-count layout cannot predict.
        for (font_size, scale) in [(13.0, 1.0), (17.0, 1.5)] {
            for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
                for extent in [600.0, 1800.0] {
                    let (mut config, original) = fixture();
                    config.edge = edge;
                    config.font_size = font_size;
                    let viewport = if edge.is_vertical() {
                        Size::new(42.0, extent)
                    } else {
                        Size::new(extent, 42.0)
                    };
                    let surface = ImageSurface::create(
                        Format::ARgb32,
                        (f64::from(viewport.width) * scale) as i32,
                        (f64::from(viewport.height) * scale) as i32,
                    )
                    .unwrap();
                    let context = Context::new(&surface).unwrap();
                    context.scale(scale, scale);
                    let renderer = CairoRenderer::new(FontDescription::from_string(font));
                    let measurer = renderer.text_measurer(&context);
                    let engine = LayoutEngine::new(config, measurer.clone());
                    for id in [NodeId::Cpu, NodeId::Memory] {
                        let mut reference = None;
                        for (index, reading) in READINGS.into_iter().enumerate() {
                            let mut snapshot = original.clone();
                            let value = percent(reading);
                            set_value(&mut snapshot, id, value);
                            let scene = engine.build(
                                snapshot.view(),
                                viewport,
                                &InteractionState::default(),
                            );
                            let case = format!(
                                "{font} {font_size}px {scale}x {edge:?} extent={extent} {id:?}={reading:?}"
                            );
                            assert!(scene.bounds_for(id).is_some(), "metric hidden: {case}");
                            let before = reference.get_or_insert_with(|| scene.clone());
                            assert_stable(before, &scene, id, &case);
                            if !edge.is_vertical()
                                && (extent == 1800.0 || value_slot(&scene, id).is_some())
                            {
                                let bounds =
                                    assert_full_value(&scene, before, id, value, &measurer, &case);
                                assert_value_paints(
                                    &renderer,
                                    &context,
                                    &surface,
                                    &scene,
                                    (id, bounds),
                                    scale,
                                    &case,
                                );
                            } else if edge.is_vertical() {
                                let bounds = assert_sidebar_value(
                                    &scene, before, id, value, &measurer, &case,
                                );
                                assert_value_paints(
                                    &renderer,
                                    &context,
                                    &surface,
                                    &scene,
                                    (id, bounds),
                                    scale,
                                    &case,
                                );
                            } else {
                                renderer.render(&context, &scene).unwrap();
                            }
                            if font == "DejaVu Sans Mono"
                                && font_size == 13.0
                                && edge == BarEdge::Top
                                && extent == 1800.0
                                && id == NodeId::Cpu
                                && index < 6
                                && std::env::var_os("XBAR_METRIC_PREVIEW").is_some()
                            {
                                snapshot.system.memory_percent = value;
                                previews.push(engine.build(
                                    snapshot.view(),
                                    viewport,
                                    &InteractionState::default(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(path) = std::env::var_os("XBAR_METRIC_PREVIEW") {
        save_preview(&previews, std::path::Path::new(&path));
    }
}

#[test]
fn managed_two_x_sidebars_keep_full_percentages_at_a_fixed_font_size() {
    for font in FONTS {
        for edge in [BarEdge::Left, BarEdge::Right] {
            let (mut config, original) = fixture();
            config.edge = edge;
            config.physical_thickness = Some(42);
            config.bar_height = 21.0;
            let viewport = Size::new(config.logical_thickness(2.0) as f32, 720.0);
            let surface = ImageSurface::create(Format::ARgb32, 42, 1440).unwrap();
            let context = Context::new(&surface).unwrap();
            context.scale(2.0, 2.0);
            let renderer = CairoRenderer::new(FontDescription::from_string(font));
            let measurer = renderer.text_measurer(&context);
            let engine = LayoutEngine::new(config, measurer.clone());
            for id in [NodeId::Cpu, NodeId::Memory] {
                let mut reference = None;
                for reading in [Some(0.0), Some(100.0), None] {
                    let mut snapshot = original.clone();
                    let value = percent(reading);
                    set_value(&mut snapshot, id, value);
                    let scene =
                        engine.build(snapshot.view(), viewport, &InteractionState::default());
                    let case = format!("{font} managed 42px / 2x {edge:?} {id:?}={reading:?}");
                    assert!(scene.bounds_for(id).is_some(), "metric hidden: {case}");
                    let before = reference.get_or_insert_with(|| scene.clone());
                    assert_stable(before, &scene, id, &case);
                    let bounds = assert_sidebar_value(&scene, before, id, value, &measurer, &case);
                    assert!(bounds.x >= 0.0 && bounds.right() <= viewport.width + 0.01);
                    assert_value_paints(
                        &renderer,
                        &context,
                        &surface,
                        &scene,
                        (id, bounds),
                        2.0,
                        &case,
                    );
                }
            }
        }
    }
}

#[test]
fn audio_battery_and_brightness_keep_slots_across_icon_and_value_changes() {
    for font in FONTS {
        for edge in [BarEdge::Top, BarEdge::Bottom] {
            for use_icon_set in [false, true] {
                let (mut config, original) = fixture();
                config.edge = edge;
                config.labels.audio = "i".into();
                config.labels.muted = "WWW".into();
                config.labels.battery = "i".into();
                config.labels.charging = "".into();
                config.labels.brightness = "BRIGHT".into();
                if use_icon_set {
                    config.icon_set = Some(IconSet {
                        volume_unavailable: "UNKNOWN".into(),
                        volume_muted: "MUTE".into(),
                        volume_low: "".into(),
                        volume_medium: "i".into(),
                        volume_high: "WWW".into(),
                        battery: "i".into(),
                        battery_charging: "".into(),
                        unavailable: "UNAVAILABLE".into(),
                        ..IconSet::default()
                    });
                }
                let viewport = Size::new(1800.0, 42.0);
                let surface = ImageSurface::create(Format::ARgb32, 1800, 42).unwrap();
                let context = Context::new(&surface).unwrap();
                let renderer = CairoRenderer::new(FontDescription::from_string(font));
                let measurer = renderer.text_measurer(&context);
                let engine = LayoutEngine::new(config, measurer.clone());
                for id in [NodeId::Audio, NodeId::Battery, NodeId::Brightness] {
                    let mut states = Vec::new();
                    if id == NodeId::Audio {
                        for (value, muted) in [
                            (None, false),
                            (Some(0.0), false),
                            (Some(20.0), false),
                            (Some(50.0), false),
                            (Some(100.0), false),
                            (Some(100.0), true),
                            (None, true),
                        ] {
                            let mut snapshot = original.clone();
                            snapshot.audio = AudioState::new(percent(value), muted);
                            states.push((snapshot, percent(value)));
                        }
                    } else if id == NodeId::Battery {
                        // Battery severity intentionally changes allocation priority.
                        // These states isolate icon changes from that policy.
                        for battery in [
                            BatteryState::absent(),
                            BatteryState::present(None, false),
                            BatteryState::present(percent(Some(80.0)), false),
                            BatteryState::present(percent(Some(100.0)), true),
                        ] {
                            let mut snapshot = original.clone();
                            snapshot.battery = battery;
                            states.push((snapshot, battery.percent));
                        }
                    } else {
                        for reading in READINGS {
                            let mut snapshot = original.clone();
                            let value = percent(reading);
                            set_value(&mut snapshot, id, value);
                            states.push((snapshot, value));
                        }
                    }
                    let mut reference = None;
                    for (index, (snapshot, value)) in states.into_iter().enumerate() {
                        let scene =
                            engine.build(snapshot.view(), viewport, &InteractionState::default());
                        let case =
                            format!("{font} {edge:?} icon_set={use_icon_set} {id:?} state={index}");
                        let before = reference.get_or_insert_with(|| scene.clone());
                        assert_stable(before, &scene, id, &case);
                        let bounds = assert_full_value(&scene, before, id, value, &measurer, &case);
                        assert_value_paints(
                            &renderer,
                            &context,
                            &surface,
                            &scene,
                            (id, bounds),
                            1.0,
                            &case,
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn empty_metric_icons_retain_complete_aligned_values() {
    for font in FONTS {
        let (mut config, original) = fixture();
        config.labels.cpu.clear();
        config.labels.memory.clear();
        config.labels.audio.clear();
        config.labels.muted.clear();
        config.labels.brightness.clear();
        config.labels.battery.clear();
        config.labels.charging.clear();
        let viewport = Size::new(1800.0, 42.0);
        let surface = ImageSurface::create(Format::ARgb32, 1800, 42).unwrap();
        let context = Context::new(&surface).unwrap();
        let renderer = CairoRenderer::new(FontDescription::from_string(font));
        let measurer = renderer.text_measurer(&context);
        let engine = LayoutEngine::new(config, measurer.clone());
        for id in [
            NodeId::Cpu,
            NodeId::Memory,
            NodeId::Audio,
            NodeId::Battery,
            NodeId::Brightness,
        ] {
            let mut reference = None;
            for reading in [Some(0.0), Some(100.0), None] {
                let mut snapshot = original.clone();
                let value = percent(reading);
                set_value(&mut snapshot, id, value);
                let scene = engine.build(snapshot.view(), viewport, &InteractionState::default());
                let case = format!("{font} empty icons {id:?}={reading:?}");
                let before = reference.get_or_insert_with(|| scene.clone());
                assert_stable(before, &scene, id, &case);
                let bounds = assert_full_value(&scene, before, id, value, &measurer, &case);
                assert_value_paints(
                    &renderer,
                    &context,
                    &surface,
                    &scene,
                    (id, bounds),
                    1.0,
                    &case,
                );
            }
        }
    }
}

fn save_preview(scenes: &[Scene], path: &std::path::Path) {
    assert_eq!(scenes.len(), 6);
    let left = scenes[0].bounds_for(NodeId::Cpu).unwrap().x.floor() - 10.0;
    let width = (1800.0 - left).ceil() as i32;
    let height = scenes.len() as i32 * 52;
    let surface = ImageSurface::create(Format::ARgb32, width, height).unwrap();
    let context = Context::new(&surface).unwrap();
    context.set_source_rgb(0.11, 0.11, 0.12);
    context.paint().unwrap();
    let renderer = CairoRenderer::new(FontDescription::from_string("DejaVu Sans Mono"));
    for (index, scene) in scenes.iter().enumerate() {
        context.save().unwrap();
        context.translate(-f64::from(left), index as f64 * 52.0);
        renderer.render(&context, scene).unwrap();
        context.restore().unwrap();
    }
    let rgba = pixels(&surface)
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| {
            let argb = u32::from_ne_bytes(*pixel);
            let alpha = (argb >> 24) & 255;
            let straight = |shift: u32| {
                ((((argb >> shift) & 255) * 255 + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255)) as u8
            };
            [straight(16), straight(8), straight(0), alpha as u8]
        })
        .collect::<Vec<_>>();
    image::save_buffer(
        path,
        &rgba,
        width as u32,
        height as u32,
        image::ColorType::Rgba8,
    )
    .unwrap();
}
