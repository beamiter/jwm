use anyhow::Result;
use log::warn;
use pango::FontDescription;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use winit::window::Window;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{WindowAttributes, WindowId},
};

use xbar_core::{
    AlignedWakeThread, BarRuntime, ModelConfig, RuntimeUpdate, TransportRecoveryConfig,
    TransportWakeSlot, WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, PointerAction, PresentationConfig},
    render::cairo::{CairoBar, CpuCanvas},
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ===== 新增：wgpu 封装 =====
#[allow(unused)]
struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    surface_format: wgpu::TextureFormat,
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

// 全屏三角形采样纹理的 WGSL 着色器
const FULLSCREEN_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VSOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vid: u32) -> VSOut {
  // 全屏三角形（避免顶点缓冲）
  var pos = array<vec2<f32>, 3>(
    vec2(-1.0, -1.0),
    vec2( 3.0, -1.0),
    vec2(-1.0,  3.0),
  )[vid];

  var out: VSOut;
  out.pos = vec4(pos, 0.0, 1.0);

  // 先做标准 NDC->UV 映射
  let uv = 0.5 * pos + vec2(0.5, 0.5);
  // 修正 Y 翻转：把屏幕的底部映射到纹理的底部（v=1）
  out.uv = vec2(uv.x, 1.0 - uv.y);

  return out;
}

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
  return textureSample(tex, samp, in.uv);
}
"#;

impl Gpu {
    async fn new(window: Arc<Window>, width: u32, height: u32) -> Result<Self> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await?; // 0.27 返回 Result<Adapter, RequestAdapterError>

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("wgpu-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: Default::default(),
                memory_hints: Default::default(),
                trace: Default::default(), // 替代 Trace::Disabled
            })
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

        // 上传纹理格式：优先 BGRA（匹配 Cairo 的 ARgb32 小端 BGRA）
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
            label: Some("nearest-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
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
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fullscreen-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
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
            surface_format,
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

        // 重新绑定（纹理视图变了）
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
        // 行对齐到 256 字节
        let bpr = stride;
        let height = self.height;
        let width = self.width;
        let aligned_bpr = bpr.div_ceil(256) * 256;

        let source_row_bytes = bpr as usize;
        let upload_row_bytes = aligned_bpr as usize;
        let height_usize = height as usize;
        let pixel_bytes = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("upload row size overflow"))?;
        if pixel_bytes > source_row_bytes {
            anyhow::bail!("Cairo stride is smaller than the visible pixel row");
        }
        let source_len = source_row_bytes
            .checked_mul(height_usize)
            .ok_or_else(|| anyhow::anyhow!("source frame size overflow"))?;
        if cpu_data.len() < source_len {
            anyhow::bail!("Cairo frame is shorter than stride * height");
        }

        let rgba_upload = self.cpu_tex_format == wgpu::TextureFormat::Rgba8UnormSrgb;
        let data_ref: &[u8] = if rgba_upload || aligned_bpr != bpr {
            let upload_len = upload_row_bytes
                .checked_mul(height_usize)
                .ok_or_else(|| anyhow::anyhow!("upload frame size overflow"))?;
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

        // 直接写入纹理（wgpu 0.27 的 TexelCopy* 类型）
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
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        // 绘制到 Surface
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                log::warn!("surface is outdated or lost; reconfiguring");
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(frame)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                    wgpu::CurrentSurfaceTexture::Timeout
                    | wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
                    status => anyhow::bail!(
                        "failed to acquire a surface texture after reconfiguration: {status:?}"
                    ),
                }
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                anyhow::bail!("surface texture acquisition failed validation")
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present-encoder"),
            });

        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present-pass"),
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

// ===== 原有：事件与应用逻辑 =====

#[derive(Debug)]
enum UserEvent {
    Tick,
    SharedUpdated(WakeAck),
}

struct App {
    window_id: Option<WindowId>,
    window: Option<Arc<Window>>,
    bar: CairoBar,

    // DPI/尺寸
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,

    // 最近一次鼠标逻辑坐标
    last_cursor_pos: Option<Point>,

    gpu: Option<Gpu>,
    canvas: CpuCanvas,
    proxy: EventLoopProxy<UserEvent>,
    transport_wake: TransportWakeSlot,
    effects: EffectRouter,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale: f64,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Self {
        Self {
            window_id: None,
            window: None,
            bar,
            scale_factor: scale,
            logical_size,
            default_logical_size: logical_size,
            last_physical_size: PhysicalSize::new(
                logical_size.width.round() as u32,
                logical_size.height.round() as u32,
            ),
            last_cursor_pos: None,
            gpu: None,
            canvas: CpuCanvas::new(),
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
        }
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        if self.window_id.is_none() || self.gpu.is_none() {
            return Ok(());
        }

        let width = self.last_physical_size.width;
        let height = self.last_physical_size.height;
        if width == 0 || height == 0 {
            return Ok(());
        }

        // Cairo builds the scene in logical coordinates; the CPU frame stays
        // in physical pixels.
        let frame = self
            .canvas
            .render(&mut self.bar, width, height, self.scale_factor)?;
        self.gpu
            .as_mut()
            .expect("GPU presence checked above")
            .upload_and_present(frame.data, frame.stride)?;
        Ok(())
    }

    #[inline]
    fn request_redraw(&self) {
        if let Some(win) = self.window.as_ref() {
            win.request_redraw();
        }
    }

    fn update_hover_and_redraw(&mut self, point: Point) {
        if self.bar.pointer_motion(point) {
            self.request_redraw();
        }
    }

    fn handle_pointer_action(&mut self, point: Point, action: PointerAction) {
        let update = self.bar.pointer_action(point, action);
        self.handle_runtime_update(update);
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) {
        let mut effects = std::mem::take(&mut self.effects);
        let needs_redraw = effects
            .route::<_, std::convert::Infallible>(update, |request| {
                match request {
                    GeometryRequest::Apply(geometry) => self.apply_monitor_geometry(geometry),
                    GeometryRequest::Clear => {
                        if let Some(window) = &self.window {
                            window.set_outer_position(LogicalPosition::new(0.0, 0.0));
                            let _ = window.request_inner_size(self.default_logical_size);
                        }
                    }
                }
                Ok(())
            })
            .expect("geometry closure is infallible");
        self.effects = effects;
        if needs_redraw {
            self.request_redraw();
        }
    }

    fn tick_and_poll(&mut self) {
        let mut update = self.bar.tick();
        update.merge(self.bar.poll_transport());
        self.handle_runtime_update(update);
        self.sync_transport_wake();
    }

    fn sync_transport_wake(&mut self) {
        let proxy = self.proxy.clone();
        if let Err(error) = self.transport_wake.sync(self.bar.runtime(), move |ack| {
            proxy.send_event(UserEvent::SharedUpdated(ack))
        }) {
            warn!("failed to synchronize shared transport wake: {error}");
        }
    }

    fn apply_monitor_geometry(&self, geometry: xbar_core::MonitorGeometry) {
        if let Some(window) = &self.window {
            let height = (f64::from(self.bar.config().bar_height) * self.scale_factor)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32;
            window.set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
            let _ = window.request_inner_size(PhysicalSize::new(geometry.width, height));
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window_id.is_none() {
            // 初始尺寸：主显示器宽度，bar 高度
            let primary = event_loop
                .primary_monitor()
                .or_else(|| event_loop.available_monitors().next());
            let scale = primary.as_ref().map(|m| m.scale_factor()).unwrap_or(1.0);
            self.scale_factor = scale;

            let screen_size: PhysicalSize<u32> = primary
                .as_ref()
                .map(|m| m.size())
                .unwrap_or(PhysicalSize::new(1920, 1080));
            let width_px = screen_size.width;
            let bar_height = f64::from(self.bar.config().bar_height);

            self.logical_size = LogicalSize::new((width_px as f64) / self.scale_factor, bar_height);
            self.default_logical_size = self.logical_size;

            let attrs = WindowAttributes::default()
                .with_title("winit_wgpu_bar")
                .with_inner_size(self.logical_size)
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(true)
                .with_transparent(false);

            // 创建 Window（owned）
            let window = event_loop
                .create_window(attrs)
                .expect("create_window failed");
            let win_id = window.id();
            let arc = Arc::new(window);

            // 初始化 wgpu
            let physical_size = arc.inner_size();
            self.last_physical_size = physical_size;

            self.window = Some(arc.clone());
            self.gpu = Some(
                pollster::block_on(Gpu::new(
                    arc.clone(),
                    physical_size.width,
                    physical_size.height,
                ))
                .expect("wgpu init failed"),
            );
            self.window_id = Some(win_id);

            let tick = self.bar.tick();
            self.handle_runtime_update(tick);
            let shared = self.bar.poll_transport();
            self.handle_runtime_update(shared);
            self.sync_transport_wake();
            self.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Tick => {
                self.tick_and_poll();
            }
            UserEvent::SharedUpdated(_ack) => {
                let update = self.bar.poll_transport();
                self.handle_runtime_update(update);
                self.sync_transport_wake();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if Some(window_id) != self.window_id {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                self.last_physical_size = new_size;
                self.logical_size = new_size.to_logical::<f64>(self.scale_factor);

                if let Some(gpu) = self.gpu.as_mut() {
                    let w = (self.logical_size.width * self.scale_factor).round() as u32;
                    let h = (self.logical_size.height * self.scale_factor).round() as u32;
                    gpu.resize(w, h);
                }

                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.logical_size = self.last_physical_size.to_logical::<f64>(self.scale_factor);

                if let Some(gpu) = self.gpu.as_mut() {
                    let w = (self.logical_size.width * self.scale_factor).round() as u32;
                    let h = (self.logical_size.height * self.scale_factor).round() as u32;
                    gpu.resize(w, h);
                }

                if let Some(geometry) = self.bar.runtime().view().geometry {
                    self.apply_monitor_geometry(geometry);
                }

                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let logical = position.to_logical::<f64>(self.scale_factor);
                let point = Point::new(logical.x as f32, logical.y as f32);
                self.last_cursor_pos = Some(point);
                self.update_hover_and_redraw(point);
            }
            WindowEvent::CursorLeft { .. } => {
                self.last_cursor_pos = None;
                if self.bar.pointer_leave() {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::{ElementState, MouseButton};
                if state == ElementState::Pressed
                    && let Some(point) = self.last_cursor_pos
                {
                    let action = match button {
                        MouseButton::Left => Some(PointerAction::Primary),
                        MouseButton::Right => Some(PointerAction::Secondary),
                        MouseButton::Middle
                        | MouseButton::Back
                        | MouseButton::Forward
                        | MouseButton::Other(_) => None,
                    };
                    if let Some(action) = action {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                if let Some(point) = self.last_cursor_pos {
                    let y = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y as f64,
                        MouseScrollDelta::PixelDelta(pos) => pos.y,
                    };

                    let action = if y > 0.0 {
                        Some(PointerAction::ScrollUp)
                    } else if y < 0.0 {
                        Some(PointerAction::ScrollDown)
                    } else {
                        None
                    };

                    if let Some(action) = action {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.redraw() {
                    warn!("redraw error (RedrawRequested): {}", e);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {}
}

fn main() -> Result<()> {
    // 参数
    let args: Vec<String> = env::args().collect();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

    // 日志
    if let Err(e) = initialize_logging("winit_wgpu_bar", &shared_path) {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

    let runtime = if shared_path.is_empty() {
        BarRuntime::new(ModelConfig::default())?
    } else {
        let recovery = TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)?;
        BarRuntime::with_managed_transport(ModelConfig::default(), recovery)?
    };
    let presentation = PresentationConfig {
        bar_height: 38.0,
        ..PresentationConfig::default()
    };
    let font_str = env::var("XBAR_FONT").unwrap_or_else(|_| "monospace 11".to_owned());
    let bar = CairoBar::new(
        runtime,
        presentation,
        FontDescription::from_string(&font_str),
    );

    // 事件循环与代理（winit 0.30.12）
    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let tick_proxy = proxy.clone();
    let _tick_forwarder = AlignedWakeThread::spawn(move || tick_proxy.send_event(UserEvent::Tick))?;

    // 初始逻辑尺寸，实际在 resumed 中根据显示器设置
    let logical_size = LogicalSize::new(800.0, 38.0);
    let mut app = App::new(bar, logical_size, 1.0, proxy);

    // 运行
    event_loop.run_app(&mut app)?;
    Ok(())
}
