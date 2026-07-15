use anyhow::{Context as _, Result};
use cairo::{Context as CairoContext, Format, ImageSurface};
use log::warn;
use pango::FontDescription;
use std::env;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tao::event_loop::EventLoopBuilder;
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowBuilder, WindowId},
};
use xbar_core::{
    BarEffect, BarRuntime, ModelConfig, RuntimeUpdate, SharedEventNotifier, SharedTransport,
    logging::init as initialize_logging,
    presentation::{Point, PointerAction, PresentationConfig, Size},
    render::cairo::CairoBar,
};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ===== 新增：wgpu 封装（保持不变，仅将 Window 改成 tao::window::Window） =====
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
    async fn new(window: Arc<Window>, width: u32, height: u32) -> Result<Self> {
        let instance = wgpu::Instance::default();

        // tao Window 提供 raw_window_handle/raw_display_handle，wgpu 可直接创建 Surface
        // 注意：不同 wgpu 版本的 create_surface 接口略有差异，请与项目当前 wgpu 版本保持一致
        let surface = instance.create_surface(window.clone())?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("wgpu-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: Default::default(),
                memory_hints: Default::default(),
                trace: Default::default(),
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

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            status => {
                log::warn!("get_current_texture returned {status:?}, reconfiguring surface");
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(frame)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                    status => anyhow::bail!("failed to acquire surface texture: {status:?}"),
                }
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
#[derive(Debug, Clone)]
enum UserEvent {
    Tick,
    SharedUpdated(Arc<AtomicBool>),
}

struct EventForwarder {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for EventForwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            warn!("event forwarding thread panicked: {payload:?}");
        }
    }
}

fn spawn_tick_thread(proxy: EventLoopProxy<UserEvent>) -> EventForwarder {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_else(|_| Duration::from_secs(0));
            let subns = u64::from(now.subsec_nanos());
            thread::sleep(Duration::from_nanos(
                1_000_000_000_u64.saturating_sub(subns).max(1),
            ));
            if worker_stop.load(Ordering::Acquire) || proxy.send_event(UserEvent::Tick).is_err() {
                break;
            }
        }
    });
    EventForwarder {
        stop,
        worker: Some(worker),
    }
}

fn spawn_shared_thread(
    proxy: EventLoopProxy<UserEvent>,
    notifier: Option<SharedEventNotifier>,
) -> Option<EventForwarder> {
    notifier.map(|notifier| {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        // The event-loop handler clears this only after it has drained the
        // transport, so at most one shared update can be queued at a time.
        let worker_pending = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            let mut descriptor = libc::pollfd {
                fd: notifier.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            while !worker_stop.load(Ordering::Acquire) {
                descriptor.revents = 0;
                let ready = unsafe { libc::poll(&mut descriptor, 1, 250) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    warn!("shared notifier poll failed: {error}");
                    break;
                }
                if ready == 0 {
                    continue;
                }
                if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    warn!("shared notifier fd became unusable: {}", descriptor.revents);
                    break;
                }
                if descriptor.revents & libc::POLLIN != 0 {
                    match notifier.drain() {
                        Ok(0) => {}
                        Ok(_) => {
                            if worker_pending
                                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok()
                            {
                                let event = UserEvent::SharedUpdated(Arc::clone(&worker_pending));
                                if proxy.send_event(event).is_err() {
                                    worker_pending.store(false, Ordering::Release);
                                    break;
                                }
                            }
                            while worker_pending.load(Ordering::Acquire)
                                && !worker_stop.load(Ordering::Acquire)
                            {
                                thread::sleep(Duration::from_millis(10));
                            }
                        }
                        Err(error) => {
                            warn!("shared notifier drain failed: {error}");
                            break;
                        }
                    }
                }
            }
        });
        EventForwarder {
            stop,
            worker: Some(worker),
        }
    })
}

struct App {
    window_id: Option<WindowId>,
    window: Option<Arc<Window>>,
    bar: CairoBar,
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,
    last_cursor_pos: Option<Point>,
    gpu: Option<Gpu>,
    cpu_frame: Vec<u8>,
    shared_path: String,
    last_transport_attempt: Instant,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        shared_path: String,
    ) -> Self {
        Self {
            window_id: None,
            window: None,
            bar,
            scale_factor,
            logical_size,
            default_logical_size: logical_size,
            last_physical_size: PhysicalSize::new(
                logical_size.width.round() as u32,
                logical_size.height.round() as u32,
            ),
            last_cursor_pos: None,
            gpu: None,
            cpu_frame: Vec::new(),
            shared_path,
            last_transport_attempt: Instant::now(),
        }
    }

    fn init_window_and_gpu(&mut self, event_loop: &EventLoop<UserEvent>) -> Result<()> {
        let primary = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        self.scale_factor = primary
            .as_ref()
            .map_or(1.0, |monitor| monitor.scale_factor());
        let screen_size = primary
            .as_ref()
            .map_or(PhysicalSize::new(1920, 1080), |monitor| monitor.size());
        self.logical_size = LogicalSize::new(
            f64::from(screen_size.width) / self.scale_factor,
            f64::from(self.bar.config().bar_height),
        );
        self.default_logical_size = self.logical_size;

        let window = Arc::new(
            WindowBuilder::new()
                .with_title("tao_wgpu_bar")
                .with_inner_size(self.logical_size)
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(true)
                .with_transparent(false)
                .build(event_loop)
                .context("failed to create tao window")?,
        );
        let size = window.inner_size();
        let safe_width = size.width.max(1);
        let safe_height = size.height.max(1);
        let gpu = pollster::block_on(Gpu::new(Arc::clone(&window), safe_width, safe_height))
            .context("failed to initialize wgpu")?;

        self.window_id = Some(window.id());
        self.window = Some(window);
        self.last_physical_size = size;
        self.gpu = Some(gpu);
        self.cpu_frame = vec![0; safe_width as usize * safe_height as usize * 4];

        let tick = self.bar.tick();
        self.handle_runtime_update(tick);
        let shared = self.bar.poll_transport();
        self.handle_runtime_update(shared);
        self.request_redraw();
        Ok(())
    }

    fn redraw(&mut self) -> Result<()> {
        if self.window_id.is_none() || self.gpu.is_none() {
            return Ok(());
        }
        let width = self.last_physical_size.width;
        let height = self.last_physical_size.height;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let width_i32 = i32::try_from(width).context("window width does not fit Cairo")?;
        let height_i32 = i32::try_from(height).context("window height does not fit Cairo")?;
        let stride = width_i32
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("Cairo stride overflow"))?;
        let required = usize::try_from(stride)?
            .checked_mul(usize::try_from(height_i32)?)
            .ok_or_else(|| anyhow::anyhow!("frame size overflow"))?;
        if self.cpu_frame.len() != required {
            self.cpu_frame.resize(required, 0);
        }

        {
            let surface = unsafe {
                ImageSurface::create_for_data_unsafe(
                    self.cpu_frame.as_mut_ptr(),
                    Format::ARgb32,
                    width_i32,
                    height_i32,
                    stride,
                )?
            };
            let context = CairoContext::new(&surface)?;
            context.scale(self.scale_factor, self.scale_factor);
            self.bar.render(
                &context,
                Size::new(
                    self.logical_size.width as f32,
                    self.logical_size.height as f32,
                ),
            )?;
            surface.flush();
        }

        self.gpu
            .as_mut()
            .expect("GPU presence checked above")
            .upload_and_present(&self.cpu_frame, stride as u32)?;
        Ok(())
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn handle_pointer_action(&mut self, point: Point, action: PointerAction) {
        let update = self.bar.pointer_action(point, action);
        self.handle_runtime_update(update);
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) {
        let RuntimeUpdate {
            changes,
            platform_effects,
            issues,
        } = update;
        for issue in issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        for effect in platform_effects {
            self.handle_platform_effect(effect);
        }
        if !changes.is_empty() {
            self.request_redraw();
        }
    }

    fn tick_and_poll(&mut self) {
        if !self.shared_path.is_empty()
            && self.bar.runtime().transport().is_none()
            && self.last_transport_attempt.elapsed() >= TRANSPORT_RETRY_INTERVAL
        {
            self.last_transport_attempt = Instant::now();
            match SharedTransport::open(&self.shared_path) {
                Ok(transport) => {
                    self.bar.runtime_mut().set_transport(Some(transport));
                    log::debug!("reconnected WM transport at {}", self.shared_path);
                }
                Err(error) => log::debug!("WM transport is still unavailable: {error}"),
            }
        }

        let mut update = self.bar.tick();
        update.merge(self.bar.poll_transport());
        self.handle_runtime_update(update);
    }

    fn handle_platform_effect(&mut self, effect: BarEffect) {
        match effect {
            BarEffect::ApplyMonitorGeometry(geometry) => self.apply_monitor_geometry(geometry),
            BarEffect::ClearMonitorGeometry => {
                if let Some(window) = &self.window {
                    window.set_outer_position(LogicalPosition::new(0.0, 0.0));
                    window.set_inner_size(self.default_logical_size);
                }
            }
            BarEffect::Screenshot => spawn_program("flameshot", &["gui"]),
            BarEffect::OpenAudioControl => spawn_program("pavucontrol", &[]),
            BarEffect::WindowManager(_)
            | BarEffect::ToggleMute
            | BarEffect::AdjustVolume(_)
            | BarEffect::AdjustBrightness(_)
            | BarEffect::RefreshBattery => {
                warn!("no frontend adapter handled platform effect: {effect:?}");
            }
        }
    }

    fn apply_monitor_geometry(&self, geometry: xbar_core::MonitorGeometry) {
        if let Some(window) = &self.window {
            let height = (f64::from(self.bar.config().bar_height) * self.scale_factor)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32;
            window.set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
            window.set_inner_size(PhysicalSize::new(geometry.width, height));
        }
    }

    fn on_user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::Tick => self.tick_and_poll(),
            UserEvent::SharedUpdated(pending) => {
                let update = self.bar.poll_transport();
                self.handle_runtime_update(update);
                pending.store(false, Ordering::Release);
            }
        }
    }

    fn on_window_event(&mut self, window_id: WindowId, event: WindowEvent) -> Option<ControlFlow> {
        if Some(window_id) != self.window_id {
            return None;
        }

        match event {
            WindowEvent::CloseRequested => return Some(ControlFlow::Exit),
            WindowEvent::Resized(size) => {
                self.last_physical_size = size;
                if size.width > 0 && size.height > 0 {
                    self.logical_size = size.to_logical(self.scale_factor);
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.resize(size.width, size.height);
                    }
                    self.cpu_frame
                        .resize(size.width as usize * size.height as usize * 4, 0);
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
            } => {
                self.scale_factor = scale_factor;
                self.last_physical_size = *new_inner_size;
                self.logical_size = self.last_physical_size.to_logical::<f64>(self.scale_factor);
                if self.last_physical_size.width > 0 && self.last_physical_size.height > 0 {
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.resize(
                            self.last_physical_size.width,
                            self.last_physical_size.height,
                        );
                    }
                    self.cpu_frame.resize(
                        self.last_physical_size.width as usize
                            * self.last_physical_size.height as usize
                            * 4,
                        0,
                    );
                }
                if let Some(geometry) = self.bar.runtime().view().geometry {
                    self.apply_monitor_geometry(geometry);
                }
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let position = position.to_logical::<f64>(self.scale_factor);
                let point = Point::new(position.x as f32, position.y as f32);
                self.last_cursor_pos = Some(point);
                if self.bar.pointer_motion(point) {
                    self.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.last_cursor_pos = None;
                if self.bar.pointer_leave() {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use tao::event::MouseScrollDelta;
                if let Some(point) = self.last_cursor_pos {
                    let vertical = match delta {
                        MouseScrollDelta::LineDelta(_, value) => f64::from(value),
                        MouseScrollDelta::PixelDelta(position) => position.y,
                        _ => 0.0,
                    };
                    let action = if vertical > 0.0 {
                        Some(PointerAction::ScrollUp)
                    } else if vertical < 0.0 {
                        Some(PointerAction::ScrollDown)
                    } else {
                        None
                    };
                    if let Some(action) = action {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use tao::event::{ElementState, MouseButton};
                if state == ElementState::Pressed
                    && let Some(point) = self.last_cursor_pos
                {
                    let action = match button {
                        MouseButton::Left => Some(PointerAction::Primary),
                        MouseButton::Right => Some(PointerAction::Secondary),
                        MouseButton::Middle | MouseButton::Other(_) => None,
                        _ => None,
                    };
                    if let Some(action) = action {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            _ => {}
        }
        None
    }
}

fn spawn_program(program: &str, args: &[&str]) {
    let program = program.to_owned();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    thread::spawn(move || {
        if let Err(error) = Command::new(&program).args(&args).status() {
            warn!("failed to run {program}: {error}");
        }
    });
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging("tao_wgpu_bar", &shared_path)?;

    let transport = if shared_path.is_empty() {
        None
    } else {
        Some(
            SharedTransport::open(&shared_path)
                .with_context(|| format!("failed to open shared transport {shared_path}"))?,
        )
    };
    let notifier = transport
        .as_ref()
        .map(|transport| transport.notifier(true))
        .transpose()
        .context("failed to start shared transport notifier")?;
    let runtime = BarRuntime::with_transport(ModelConfig::default(), transport)?;
    let presentation = PresentationConfig {
        bar_height: 38.0,
        ..PresentationConfig::default()
    };
    let font = env::var("XBAR_FONT").unwrap_or_else(|_| "monospace 11".to_owned());
    let bar = CairoBar::new(runtime, presentation, FontDescription::from_string(&font));

    let mut event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let _tick_forwarder = spawn_tick_thread(proxy.clone());
    let _shared_forwarder = spawn_shared_thread(proxy, notifier);

    let mut app = App::new(bar, LogicalSize::new(800.0, 38.0), 1.0, shared_path);
    app.init_window_and_gpu(&event_loop)?;

    let exit_code = event_loop.run_return(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(event) => app.on_user_event(event),
            Event::WindowEvent {
                window_id, event, ..
            } => {
                if let Some(next) = app.on_window_event(window_id, event) {
                    *control_flow = next;
                }
            }
            Event::RedrawRequested(window_id) if Some(window_id) == app.window_id => {
                if let Err(error) = app.redraw() {
                    warn!("redraw failed: {error}");
                }
            }
            _ => {}
        }
    });

    if exit_code == 0 {
        Ok(())
    } else {
        anyhow::bail!("tao event loop exited with status {exit_code}")
    }
}
