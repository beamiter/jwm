//! Render the real Cairo/Pango scene for each edge at 1x and 2x without X11,
//! Wayland, a running WM, providers, or screenshot timing dependencies.
//!
//! cargo run -p xbar_core --features render-cairo --example render_four_edges -- /tmp/jwm-bar-previews
//! Append `named` for compact named workspaces or `crowded` for multi-selection
//! and hidden urgent workspaces on a short output.
//!
//! PNGs use portable text labels so geometry can be inspected even on CI hosts
//! without Nerd Fonts. JSON contains the identical scene and pointer targets.

#[cfg(feature = "render-cairo")]
mod preview {
    use cairo::{Context, Format, ImageSurface};
    use pango::FontDescription;
    use std::path::Path;
    use xbar_core::presentation::{
        InteractionState, LayoutEngine, NodeId, Point, PresentationConfig, Size,
    };
    use xbar_core::render::cairo::CairoRenderer;
    use xbar_core::{
        AudioState, BarEdge, BarModel, BatteryState, BrightnessState, LayoutId,
        MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE, MinimizedWindow, MonitorId, Percent, SystemState,
        TagId, TagState, ThemeMode, WindowToken,
    };

    pub fn run(directory: &Path, scenario: &str) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(directory)?;
        let mut snapshot = BarModel::default().snapshot();
        snapshot.wm_available = true;
        snapshot.wm_session_id = 1;
        snapshot.wm_sequence = Some(1);
        snapshot.tags = (0..9)
            .map(|index| TagState {
                selected: index == 2,
                occupied: matches!(index, 0 | 2 | 4 | 7),
                urgent: index == 7,
                ..TagState::default()
            })
            .collect();
        snapshot.active_tag = TagId::new(2);
        snapshot.layout = Some(LayoutId(0));
        snapshot.layout_symbol = "[]=".to_owned();
        snapshot.client_name = "JWM — four-edge bar refactor".to_owned();
        snapshot.time = "Sun 18 Oct 08:40".to_owned();
        snapshot.audio = AudioState::new(Percent::from_whole(42).ok(), false);
        snapshot.battery = BatteryState::present(Percent::from_whole(73).ok(), false);
        snapshot.brightness = BrightnessState::new(Percent::from_whole(65).ok());
        snapshot.system =
            SystemState::new(Percent::from_whole(18).ok(), Percent::from_whole(47).ok());
        snapshot.minimized_windows = ["Terminal", "Browser", "Editor"]
            .iter()
            .enumerate()
            .map(|(index, title)| MinimizedWindow {
                token: WindowToken(index as u64 + 1),
                monitor: MonitorId(0),
                title: (*title).to_owned(),
                app_id: title.to_lowercase(),
                flags: MINIMIZED_WINDOW_FLAG_PREVIEW_AVAILABLE,
            })
            .collect();
        let mut config = PresentationConfig {
            tag_labels: (1..=9).map(|number| number.to_string()).collect(),
            ..PresentationConfig::default()
        };
        config.labels.clock.clear();
        config.labels.cpu = "CPU".to_owned();
        config.labels.memory = "RAM".to_owned();
        config.labels.audio = "VOL".to_owned();
        config.labels.battery = "BAT".to_owned();
        config.labels.brightness = "☀".to_owned();
        config.labels.network_offline = "NET".to_owned();
        config.labels.media_paused = "▶".to_owned();
        config.labels.screenshot = "▣".to_owned();
        config.labels.theme_dark = "◐".to_owned();
        config.labels.theme_light = "◑".to_owned();
        config.labels.shell_routes[0] = "≡".to_owned();

        match scenario {
            "standard" => {}
            "named" => {
                snapshot.tags = (0..4)
                    .map(|index| TagState {
                        selected: index == 3,
                        occupied: true,
                        ..TagState::default()
                    })
                    .collect();
                snapshot.active_tag = TagId::new(3);
                snapshot.minimized_windows.clear();
                config.tag_labels = (1..=4).map(|index| format!("Workspace {index}")).collect();
            }
            "crowded" => {
                snapshot.tags = (0..32)
                    .map(|index| TagState {
                        selected: index < 4 || index == 31,
                        occupied: index < 8 || index == 31,
                        urgent: index < 8,
                        ..TagState::default()
                    })
                    .collect();
                snapshot.active_tag = TagId::new(31);
                snapshot.minimized_windows.clear();
                config.tag_labels = (1..=32).map(|index| format!("Workspace {index}")).collect();
            }
            _ => {
                return Err(format!(
                    "Unknown scenario {scenario:?}: use standard, named or crowded"
                )
                .into());
            }
        }

        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            for scale in [1_u32, 2] {
                for theme in [ThemeMode::Dark, ThemeMode::Light] {
                    snapshot.theme = theme;
                    config.edge = edge;
                    let extent = match scenario {
                        "named" => 460.0,
                        "crowded" => 300.0,
                        _ if edge.is_vertical() => 1100.0,
                        _ => 1600.0,
                    };
                    let viewport = if edge.is_vertical() {
                        Size::new(42.0, extent)
                    } else {
                        Size::new(extent, 42.0)
                    };
                    let width = viewport.width as u32 * scale;
                    let height = viewport.height as u32 * scale;
                    let mut surface =
                        ImageSurface::create(Format::ARgb32, width as i32, height as i32)?;
                    let context = Context::new(&surface)?;
                    context.scale(f64::from(scale), f64::from(scale));
                    let renderer = CairoRenderer::new(FontDescription::from_string("DejaVu Sans"));
                    let engine =
                        LayoutEngine::new(config.clone(), renderer.text_measurer(&context));
                    let initial =
                        engine.build(snapshot.view(), viewport, &InteractionState::default());
                    let mut interaction = InteractionState::default();
                    if let Some(hit) = initial
                        .hits
                        .iter()
                        .find(|hit| hit.id == NodeId::MinimizedWindow(WindowToken(2)))
                    {
                        interaction.update_hover(
                            &initial,
                            Point::new(
                                hit.bounds.x + hit.bounds.width * 0.5,
                                hit.bounds.y + hit.bounds.height * 0.5,
                            ),
                        );
                    }
                    let scene = engine.build(snapshot.view(), viewport, &interaction);
                    renderer.render(&context, &scene)?;
                    drop(context);
                    surface.flush();
                    let stride = surface.stride() as usize;
                    let data = surface.data()?;
                    let mut rgba = vec![0_u8; width as usize * height as usize * 4];
                    for y in 0..height as usize {
                        for x in 0..width as usize {
                            let pixel = &data[y * stride + x * 4..][..4];
                            let out = &mut rgba[(y * width as usize + x) * 4..][..4];
                            let argb = u32::from_ne_bytes(pixel.try_into()?);
                            let alpha = ((argb >> 24) & 255) as u8;
                            let unpremultiply = |component: u32| -> u8 {
                                if alpha == 0 {
                                    0
                                } else {
                                    ((component * 255 + u32::from(alpha) / 2) / u32::from(alpha))
                                        .min(255) as u8
                                }
                            };
                            out.copy_from_slice(&[
                                unpremultiply((argb >> 16) & 255),
                                unpremultiply((argb >> 8) & 255),
                                unpremultiply(argb & 255),
                                alpha,
                            ]);
                        }
                    }
                    let mode = match theme {
                        ThemeMode::Dark => "dark",
                        ThemeMode::Light => "light",
                    };
                    let name = format!("{}-{mode}-{scale}x", edge.as_str());
                    image::save_buffer(
                        directory.join(format!("{name}.png")),
                        &rgba,
                        width,
                        height,
                        image::ColorType::Rgba8,
                    )?;
                    std::fs::write(
                        directory.join(format!("{name}.json")),
                        serde_json::to_vec_pretty(&scene)?,
                    )?;
                }
            }
        }
        println!(
            "Rendered {scenario} across four edges, two themes and two scales to {}",
            directory.display()
        );
        Ok(())
    }
}

#[cfg(feature = "render-cairo")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/jwm-bar-previews"));
    let scenario = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "standard".to_owned());
    preview::run(&directory, &scenario)
}

#[cfg(not(feature = "render-cairo"))]
fn main() {
    eprintln!("Enable --features render-cairo to render the bar fixtures.");
}
