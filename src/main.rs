use anyhow::{Result, anyhow};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::ffi::c_void;
use std::num::{NonZero, NonZeroU32};
use std::os::fd::AsFd as _;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle, XcbDisplayHandle, XcbWindowHandle,
};

use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::{Point, PointerAction, PresentationConfig};
use xbar_core::render::cairo::{CairoBar, CpuCanvas};
use xbar_core::{
    BarPlacement, BarRuntime, DockProperty, DockPropertyValue, DockWindowSpec, ModelConfig,
    MonitorGeometry, NotifierChange, RuntimeUpdate, TransportNotifierSlot, TransportRecoveryConfig,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};

const BAR_NAME: &str = "x11rb_wgpu_bar";
const X_TOKEN: u64 = 1;
const TIMER_TOKEN: u64 = 2;
const SHARED_TOKEN: u64 = 3;
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ============================================================================
// 1. RAW WINDOW HANDLE 用于 WGPU 识别 XCB
// ============================================================================
struct XcbTarget {
    conn: *mut c_void,
    screen: i32,
    window: u32,
    visual_id: u32,
}

// 解决 *mut c_void 的跨线程传递问题
unsafe impl Send for XcbTarget {}
unsafe impl Sync for XcbTarget {}

impl HasDisplayHandle for XcbTarget {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        let handle = XcbDisplayHandle::new(Some(NonNull::new(self.conn).unwrap()), self.screen);
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Xcb(handle)) })
    }
}

impl HasWindowHandle for XcbTarget {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        let mut handle = XcbWindowHandle::new(NonZeroU32::new(self.window).unwrap());
        handle.visual_id = NonZeroU32::new(self.visual_id);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xcb(handle)) })
    }
}

// ============================================================================
// 2. WGPU 封装
// ============================================================================
struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    cpu_tex: wgpu::Texture,
    cpu_tex_view: wgpu::TextureView,
    cpu_tex_format: wgpu::TextureFormat,
    upload_scratch: Vec<u8>,
    sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

const FULLSCREEN_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VSOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vid: u32) -> VSOut {
  var pos = array<vec2<f32>, 3>(
    vec2(-1.0, -1.0),
    vec2( 3.0, -1.0),
    vec2(-1.0,  3.0),
  )[vid];
  var out: VSOut;
  out.pos = vec4(pos, 0.0, 1.0);
  let uv = 0.5 * pos + vec2(0.5, 0.5);
  out.uv = vec2(uv.x, 1.0 - uv.y);
  return out;
}

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
  return textureSample(tex, samp, in.uv);
}
"#;

impl Gpu {
    async fn new(target: Arc<XcbTarget>, width: u32, height: u32) -> Result<Self> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(target)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await
            .expect("No adapter found");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let cpu_tex_format = if surface_format == wgpu::TextureFormat::Bgra8UnormSrgb {
            wgpu::TextureFormat::Bgra8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8UnormSrgb
        };

        let cpu_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cpu-upload-texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: cpu_tex_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let cpu_tex_view = cpu_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fullscreen-shader"),
            source: wgpu::ShaderSource::Wgsl(FULLSCREEN_WGSL.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tex-sampler-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pipeline-layout"),
            bind_group_layouts: &[Some(&bind_layout)], // 修复：包装在 Some 中
            immediate_size: 0,                         // 修复：添加 immediate_size
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fullscreen-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: NonZero::new(0),
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tex-sampler-bindgroup"),
            layout: &bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&cpu_tex_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            cpu_tex,
            cpu_tex_view,
            cpu_tex_format,
            upload_scratch: Vec::new(),
            sampler,
            pipeline,
            bind_group,
            width,
            height,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);

        self.cpu_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cpu-upload-texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.cpu_tex_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.cpu_tex_view = self
            .cpu_tex
            .create_view(&wgpu::TextureViewDescriptor::default());

        let bind_layout = self.pipeline.get_bind_group_layout(0);
        self.bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tex-sampler-bindgroup"),
            layout: &bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.cpu_tex_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
    }

    fn upload_and_present(&mut self, cpu_data: &[u8], stride: u32) -> Result<()> {
        let bpr = stride;
        let aligned_bpr = bpr.div_ceil(256) * 256;
        let height = self.height;
        let width = self.width;
        let source_row_bytes = bpr as usize;
        let upload_row_bytes = aligned_bpr as usize;
        let height_usize = height as usize;
        let pixel_bytes = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| anyhow!("upload row size overflow"))?;
        if pixel_bytes > source_row_bytes {
            return Err(anyhow!(
                "Cairo stride is smaller than the visible pixel row"
            ));
        }
        let source_len = source_row_bytes
            .checked_mul(height_usize)
            .ok_or_else(|| anyhow!("source frame size overflow"))?;
        if cpu_data.len() < source_len {
            return Err(anyhow!("Cairo frame is shorter than stride * height"));
        }

        let rgba_upload = self.cpu_tex_format == wgpu::TextureFormat::Rgba8UnormSrgb;
        let data_ref: &[u8] = if rgba_upload || aligned_bpr != bpr {
            let upload_len = upload_row_bytes
                .checked_mul(height_usize)
                .ok_or_else(|| anyhow!("upload frame size overflow"))?;
            self.upload_scratch.resize(upload_len, 0);
            for row in 0..height_usize {
                let source_start = row * source_row_bytes;
                let upload_start = row * upload_row_bytes;
                let source = &cpu_data[source_start..source_start + source_row_bytes];
                let upload =
                    &mut self.upload_scratch[upload_start..upload_start + upload_row_bytes];
                if rgba_upload {
                    for (source, upload) in source[..pixel_bytes]
                        .chunks_exact(4)
                        .zip(upload[..pixel_bytes].chunks_exact_mut(4))
                    {
                        upload.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                    }
                    upload[pixel_bytes..source_row_bytes]
                        .copy_from_slice(&source[pixel_bytes..source_row_bytes]);
                } else {
                    upload[..source_row_bytes].copy_from_slice(source);
                }
                upload[source_row_bytes..].fill(0);
            }
            &self.upload_scratch
        } else {
            cpu_data
        };

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.cpu_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data_ref,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(aligned_bpr),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                self.surface.configure(&self.device, &self.config);
                frame
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => anyhow::bail!("wgpu surface lost"),
            wgpu::CurrentSurfaceTexture::Validation => {
                anyhow::bail!("wgpu surface validation error")
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &self.bind_group, &[]);
            rp.draw(0..3, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }
}

// ============================================================================
// 3. X11 platform adapter and Cairo-to-wgpu presentation
// ============================================================================
fn intern_atom(conn: &XCBConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Write core-described dock properties with this connection. Atom names come
/// from `DockWindowSpec`; only interning and the property calls live here.
fn write_dock_properties(
    conn: &XCBConnection,
    win: Window,
    properties: &[DockProperty],
) -> Result<()> {
    let atom_type = intern_atom(conn, "ATOM")?;
    let cardinal_type = intern_atom(conn, "CARDINAL")?;
    for property in properties {
        let name = intern_atom(conn, property.name)?;
        match &property.value {
            DockPropertyValue::Atoms(values) => {
                let values = values
                    .iter()
                    .map(|value| intern_atom(conn, value))
                    .collect::<Result<Vec<Atom>>>()?;
                change_property_32(conn, win, name, atom_type, &values)?;
            }
            DockPropertyValue::Cardinals(values) => {
                change_property_32(conn, win, name, cardinal_type, values)?;
            }
            DockPropertyValue::Utf8Text(text) => {
                let utf8_string = intern_atom(conn, "UTF8_STRING")?;
                change_property_8(conn, win, name, utf8_string, text.as_bytes())?;
            }
        }
    }
    Ok(())
}

fn dock_spec(x: i32, y: i32, width: u32, bar_height: u16) -> DockWindowSpec {
    DockWindowSpec::top(
        BAR_NAME,
        BarPlacement {
            x,
            y,
            width,
            height: u32::from(bar_height),
        },
    )
}

fn change_property_32(
    conn: &XCBConnection,
    win: Window,
    property: Atom,
    property_type: Atom,
    data: &[u32],
) -> Result<()> {
    conn.change_property32(PropMode::REPLACE, win, property, property_type, data)?
        .check()?;
    Ok(())
}

fn change_property_8(
    conn: &XCBConnection,
    win: Window,
    property: Atom,
    property_type: Atom,
    data: &[u8],
) -> Result<()> {
    conn.change_property8(PropMode::REPLACE, win, property, property_type, data)?
        .check()?;
    Ok(())
}

struct WindowAdapter<'a> {
    conn: &'a XCBConnection,
    screen: &'a x11rb::protocol::xproto::Screen,
    win: Window,
    bar_height: Cell<u16>,
    effects: RefCell<EffectRouter>,
}

impl WindowAdapter<'_> {
    fn sync_bar_height(&self, bar: &mut CairoBar, height: u16) {
        // A window manager may enforce its configured dock height instead of
        // the size requested when the window was created. Keep both future
        // geometry requests and the presentation viewport fill in sync with
        // that final server-side height.
        self.bar_height.set(height);
        bar.config_mut().bar_height = f32::from(height);
    }

    fn apply_runtime_update(&self, update: RuntimeUpdate) -> Result<bool> {
        self.effects.borrow_mut().route(update, |request| {
            let geometry = match request {
                GeometryRequest::Apply(geometry) => geometry,
                GeometryRequest::Clear => MonitorGeometry {
                    x: 0,
                    y: 0,
                    width: u32::from(self.screen.width_in_pixels),
                    height: u32::from(self.screen.height_in_pixels),
                },
            };
            self.apply_geometry(geometry)
        })
    }

    fn apply_geometry(&self, geometry: MonitorGeometry) -> Result<()> {
        let width = geometry.width.max(1);
        let bar_height = self.bar_height.get();
        self.conn
            .configure_window(
                self.win,
                &ConfigureWindowAux::new()
                    .x(geometry.x)
                    .y(geometry.y)
                    .width(width)
                    .height(u32::from(bar_height)),
            )?
            .check()?;
        let spec = dock_spec(geometry.x, geometry.y, width, bar_height);
        write_dock_properties(self.conn, self.win, &spec.strut_properties())?;
        self.conn.flush()?;
        Ok(())
    }
}

fn redraw(
    gpu: &mut Gpu,
    canvas: &mut CpuCanvas,
    width: u16,
    height: u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let frame = canvas.render(bar, u32::from(width), u32::from(height), 1.0)?;
    let _ = bar.runtime_mut().take_changes();
    gpu.upload_and_present(frame.data, frame.stride)?;
    Ok(())
}

fn sync_notifier(
    slot: &mut TransportNotifierSlot,
    runtime: &BarRuntime,
    epoll: &Epoll,
) -> Result<()> {
    if let NotifierChange::Replaced { fd, .. } = slot.sync(runtime)? {
        epoll.add(fd, SHARED_TOKEN)?;
    }
    Ok(())
}

// ============================================================================
// 4. MAIN LOOP
// ============================================================================
fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    xbar_core::logging::init(BAR_NAME, &shared_path)?;

    let runtime = if shared_path.is_empty() {
        BarRuntime::new(ModelConfig::default())?
    } else {
        let recovery = TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)?;
        BarRuntime::with_managed_transport(ModelConfig::default(), recovery)?
    };

    let (conn, screen_num) = XCBConnection::connect(None)?;
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];

    let presentation = PresentationConfig::default();
    let bar_height = presentation
        .bar_height
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let font_name = env::var("XBAR_FONT").unwrap_or_else(|_| "monospace 11".to_owned());
    let font = FontDescription::from_string(&font_name);
    let mut bar = CairoBar::new(runtime, presentation, font);

    let win = conn.generate_id()?;
    let mut current_width = screen.width_in_pixels;
    let mut current_height = bar_height;

    conn.create_window(
        screen.root_depth,
        win,
        screen.root,
        0,
        0,
        current_width,
        current_height,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixmap(AtomEnum::NONE)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::POINTER_MOTION
                    | EventMask::ENTER_WINDOW
                    | EventMask::LEAVE_WINDOW,
            ),
    )?
    .check()?;

    let spec = dock_spec(0, 0, u32::from(current_width), current_height);
    write_dock_properties(&conn, win, &spec.properties())?;

    // 绑定 WGPU
    let target = Arc::new(XcbTarget {
        conn: conn.get_raw_xcb_connection(),
        screen: screen_num as i32,
        window: win,
        visual_id: screen.root_visual,
    });
    let mut gpu = pollster::block_on(Gpu::new(
        target,
        u32::from(current_width),
        u32::from(current_height),
    ))?;
    let mut canvas = CpuCanvas::new();

    conn.map_window(win)?.check()?;
    conn.flush()?;

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        effects: RefCell::new(EffectRouter::default()),
    };

    let mut initial_update = bar.tick();
    initial_update.merge(bar.poll_transport());
    window.apply_runtime_update(initial_update)?;

    redraw(
        &mut gpu,
        &mut canvas,
        current_width,
        current_height,
        &mut bar,
    )?;

    let timer = AlignedTimer::new(Duration::from_secs(1))?;
    let mut epoll = Epoll::new()?;
    epoll.add(window.conn.as_fd(), X_TOKEN)?;
    epoll.add(timer.as_fd(), TIMER_TOKEN)?;
    let mut notifier_slot = TransportNotifierSlot::new(true);
    sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;

    let mut ready_tokens = Vec::new();
    loop {
        ready_tokens.clear();
        ready_tokens.extend(epoll.wait()?);
        for token in &ready_tokens {
            match *token {
                X_TOKEN => {
                    while let Some(x_event) = conn.poll_for_event()? {
                        let should_redraw = match x_event {
                            Event::Expose(event) => event.count == 0,
                            Event::ConfigureNotify(event) if event.window == win => {
                                current_width = event.width;
                                current_height = event.height;
                                window.sync_bar_height(&mut bar, event.height);
                                gpu.resize(u32::from(current_width), u32::from(current_height));
                                true
                            }
                            Event::EnterNotify(event) => bar.pointer_motion(Point::new(
                                f32::from(event.event_x),
                                f32::from(event.event_y),
                            )),
                            Event::MotionNotify(event) => bar.pointer_motion(Point::new(
                                f32::from(event.event_x),
                                f32::from(event.event_y),
                            )),
                            Event::LeaveNotify(_) => bar.pointer_leave(),
                            Event::ButtonPress(event) => {
                                if let Some(input) = PointerAction::from_x11_button(event.detail) {
                                    let update = bar.pointer_action(
                                        Point::new(
                                            f32::from(event.event_x),
                                            f32::from(event.event_y),
                                        ),
                                        input,
                                    );
                                    window.apply_runtime_update(update)?
                                } else {
                                    false
                                }
                            }
                            _ => false,
                        };
                        if should_redraw {
                            redraw(
                                &mut gpu,
                                &mut canvas,
                                current_width,
                                current_height,
                                &mut bar,
                            )?;
                        }
                    }
                }
                TIMER_TOKEN => {
                    if timer.drain()? > 0 {
                        let mut update = bar.tick();
                        update.merge(bar.poll_transport());
                        let needs_redraw = window.apply_runtime_update(update)?;
                        sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
                        if needs_redraw {
                            redraw(
                                &mut gpu,
                                &mut canvas,
                                current_width,
                                current_height,
                                &mut bar,
                            )?;
                        }
                    }
                }
                SHARED_TOKEN => {
                    if let Some(notifier) = notifier_slot.notifier() {
                        notifier.drain()?;
                        let update = bar.poll_transport();
                        let needs_redraw = window.apply_runtime_update(update)?;
                        sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
                        if needs_redraw {
                            redraw(
                                &mut gpu,
                                &mut canvas,
                                current_width,
                                current_height,
                                &mut bar,
                            )?;
                        }
                    }
                }
                token => log::debug!("unexpected epoll token: {token}"),
            }
        }
    }
}
