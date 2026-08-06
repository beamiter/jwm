//! wgpu presenter for Cairo-produced BGRA CPU frames.
//!
//! Owns the surface configuration, resize/recovery, upload texture, and the
//! fullscreen blit that the wgpu bars previously copied. The presenter never
//! renders scenes itself: it accepts premultiplied `ARgb32` bytes (little
//! endian BGRA) from `xbar_core::render::cairo` and puts them on screen.

use anyhow::Result;

/// Device-pixel sub-rectangle of a frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresentRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PresentRect {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Owns a wgpu surface and presents CPU frames onto it.
pub struct WgpuPresenter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    cpu_tex: wgpu::Texture,
    cpu_tex_format: wgpu::TextureFormat,
    upload_scratch: Vec<u8>,
    sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    /// Set while the surface refuses to hand out textures, so the warning is
    /// logged once per episode rather than once per dropped frame.
    surface_stale: bool,
    /// Set when the upload texture was reallocated and so retains nothing of
    /// the previous frame; the next present must upload all of it.
    texture_empty: bool,
}

// Fullscreen triangle sampling the upload texture; no vertex buffer needed.
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

/// Which composite alpha mode the surface should be configured with.
///
/// `PostMultiplied` is deliberately never chosen: the Cairo frames arriving
/// here are premultiplied, and presenting them post-multiplied would force an
/// un-premultiply in the upload swizzle for no gain. A caller that wants alpha
/// gets `PreMultiplied` or `Inherit` when the surface offers one; otherwise —
/// and always when alpha was not requested — the surface stays opaque.
fn select_alpha_mode(
    caps: &wgpu::SurfaceCapabilities,
    want_alpha: bool,
) -> wgpu::CompositeAlphaMode {
    let advertised = |mode| caps.alpha_modes.contains(&mode);
    if want_alpha {
        if advertised(wgpu::CompositeAlphaMode::PreMultiplied) {
            return wgpu::CompositeAlphaMode::PreMultiplied;
        }
        if advertised(wgpu::CompositeAlphaMode::Inherit) {
            return wgpu::CompositeAlphaMode::Inherit;
        }
    }
    if advertised(wgpu::CompositeAlphaMode::Opaque) {
        wgpu::CompositeAlphaMode::Opaque
    } else {
        caps.alpha_modes[0]
    }
}

const fn mode_is_transparent(mode: wgpu::CompositeAlphaMode) -> bool {
    matches!(
        mode,
        wgpu::CompositeAlphaMode::PreMultiplied | wgpu::CompositeAlphaMode::Inherit
    )
}

impl WgpuPresenter {
    /// Create a presenter over any wgpu surface target (an `Arc<Window>` for
    /// winit/tao, or a raw-window-handle wrapper for XCB/x11rb).
    ///
    /// `want_alpha` asks for a surface whose alpha bytes reach the compositor;
    /// whether the surface actually granted one is answered by
    /// [`WgpuPresenter::is_transparent`], and callers must fall back to opaque
    /// painting when it says no.
    pub async fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        want_alpha: bool,
    ) -> Result<Self> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(target)?;
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
                label: Some("xbar-present-device"),
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
            .find(|format| format.is_srgb())
            .unwrap_or(caps.formats[0]);

        let alpha_mode = select_alpha_mode(&caps, want_alpha);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Upload format: prefer BGRA, matching Cairo's little-endian ARgb32.
        let cpu_tex_format = if surface_format == wgpu::TextureFormat::Bgra8UnormSrgb {
            wgpu::TextureFormat::Bgra8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8UnormSrgb
        };

        let cpu_tex = create_upload_texture(&device, cpu_tex_format, width, height);
        let cpu_tex_view = cpu_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("xbar-present-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("xbar-present-shader"),
            source: wgpu::ShaderSource::Wgsl(FULLSCREEN_WGSL.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("xbar-present-bind-layout"),
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
            label: Some("xbar-present-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("xbar-present-pipeline"),
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
                    // A transparent surface must carry the frame's alpha bytes
                    // through intact: the frame is full-coverage premultiplied
                    // BGRA, so REPLACE hands it to the compositor as-is, while
                    // blending it over the clear would corrupt the coverage.
                    blend: Some(if mode_is_transparent(alpha_mode) {
                        wgpu::BlendState::REPLACE
                    } else {
                        wgpu::BlendState::ALPHA_BLENDING
                    }),
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

        let bind_group = create_bind_group(&device, &pipeline, &cpu_tex_view, &sampler);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            cpu_tex,
            cpu_tex_format,
            upload_scratch: Vec::new(),
            sampler,
            pipeline,
            bind_group,
            width,
            height,
            surface_stale: false,
            texture_empty: true,
        })
    }

    /// [`WgpuPresenter::new`] driven by a local pollster executor.
    pub fn new_blocking(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        want_alpha: bool,
    ) -> Result<Self> {
        pollster::block_on(Self::new(target, width, height, want_alpha))
    }

    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The composite alpha mode the surface was configured with.
    #[must_use]
    pub const fn alpha_mode(&self) -> wgpu::CompositeAlphaMode {
        self.config.alpha_mode
    }

    /// Whether presented alpha bytes reach the compositor.
    ///
    /// Only `PreMultiplied` and `Inherit` qualify; anything else means the
    /// caller must paint every pixel opaque or the result is undefined.
    #[must_use]
    pub const fn is_transparent(&self) -> bool {
        mode_is_transparent(self.config.alpha_mode)
    }

    /// Reconfigure the surface and reallocate the upload texture.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || (width == self.width && height == self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);

        self.cpu_tex = create_upload_texture(&self.device, self.cpu_tex_format, width, height);
        let view = self
            .cpu_tex
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.bind_group = create_bind_group(&self.device, &self.pipeline, &view, &self.sampler);
        self.texture_empty = true;
    }

    /// Upload a BGRA frame (or just its damaged part) and present it.
    ///
    /// `data`/`stride` must describe a full frame at the current size.
    /// `damage: None` uploads everything; an empty rect skips the upload but
    /// still presents, which re-blits the retained texture after an expose.
    /// Damage is ignored for the first frame after a resize, when there is no
    /// retained texture left to patch.
    pub fn present_bgra(
        &mut self,
        data: &[u8],
        stride: u32,
        damage: Option<PresentRect>,
    ) -> Result<()> {
        let tight_row = self
            .width
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("upload row size overflow"))?;
        if stride < tight_row {
            anyhow::bail!("stride {stride} is smaller than the visible pixel row");
        }
        let source_len = (stride as usize)
            .checked_mul(self.height as usize)
            .ok_or_else(|| anyhow::anyhow!("source frame size overflow"))?;
        if data.len() < source_len {
            anyhow::bail!("frame is shorter than stride * height");
        }

        let full = PresentRect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        };
        // A damage rect is only meaningful against a texture that still holds
        // the previous frame. A reallocated one holds nothing, so honouring
        // damage there would leave everything outside it blank.
        let region = match damage {
            Some(rect) if !self.texture_empty => clamp_rect(rect, self.width, self.height),
            _ => full,
        };
        if !region.is_empty() {
            self.upload(data, stride, region)?;
            self.texture_empty = false;
        }
        self.blit_and_present()
    }

    fn upload(&mut self, data: &[u8], stride: u32, region: PresentRect) -> Result<()> {
        let rgba_upload = self.cpu_tex_format == wgpu::TextureFormat::Rgba8UnormSrgb;
        let region_row_bytes = region.width as usize * 4;
        let aligned_bpr = (region.width * 4).div_ceil(256) * 256;
        let upload_row_bytes = aligned_bpr as usize;
        let source_row_bytes = stride as usize;

        let full_width_direct =
            !rgba_upload && region.x == 0 && region.width == self.width && aligned_bpr == stride;
        let data_ref: &[u8] = if full_width_direct {
            &data[region.y as usize * source_row_bytes..]
                [..region.height as usize * source_row_bytes]
        } else {
            let upload_len = upload_row_bytes
                .checked_mul(region.height as usize)
                .ok_or_else(|| anyhow::anyhow!("upload frame size overflow"))?;
            self.upload_scratch.resize(upload_len, 0);
            for row in 0..region.height as usize {
                let source_start =
                    (region.y as usize + row) * source_row_bytes + region.x as usize * 4;
                let source = &data[source_start..source_start + region_row_bytes];
                let upload_start = row * upload_row_bytes;
                let upload =
                    &mut self.upload_scratch[upload_start..upload_start + upload_row_bytes];

                if rgba_upload {
                    for (source, upload) in source
                        .chunks_exact(4)
                        .zip(upload[..region_row_bytes].chunks_exact_mut(4))
                    {
                        upload.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                    }
                } else {
                    upload[..region_row_bytes].copy_from_slice(source);
                }
                upload[region_row_bytes..].fill(0);
            }
            &self.upload_scratch
        };

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.cpu_tex,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: region.x,
                    y: region.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            data_ref,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(if full_width_direct {
                    stride
                } else {
                    aligned_bpr
                }),
                rows_per_image: Some(region.height),
            },
            wgpu::Extent3d {
                width: region.width,
                height: region.height,
                depth_or_array_layers: 1,
            },
        );
        Ok(())
    }

    fn blit_and_present(&mut self) -> Result<()> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                // Reconfiguring can only use the size we know about, and after
                // a window manager resize that size is stale until the caller
                // has drained the resize event and called `resize`. So a second
                // refusal is not a failure — it means the frame we were asked
                // to present belongs to a window geometry that no longer
                // exists. Drop it and let the resize that is already on its way
                // reconfigure the surface properly; killing the bar over a
                // transient mismatch would be the far worse outcome.
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(frame)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                    wgpu::CurrentSurfaceTexture::Validation => {
                        anyhow::bail!("surface texture acquisition failed validation")
                    }
                    status => {
                        if !self.surface_stale {
                            self.surface_stale = true;
                            log::warn!(
                                "surface unavailable at {}x{} ({status:?}); skipping frames until it is reconfigured",
                                self.width,
                                self.height
                            );
                        }
                        return Ok(());
                    }
                }
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                anyhow::bail!("surface texture acquisition failed validation")
            }
        };
        if self.surface_stale {
            self.surface_stale = false;
            log::info!("surface recovered at {}x{}", self.width, self.height);
        }
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xbar-present-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("xbar-present-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // A transparent surface starts from zero coverage so
                        // pixels the frame leaves untouched stay see-through;
                        // an opaque one keeps the historical black base.
                        load: wgpu::LoadOp::Clear(if self.is_transparent() {
                            wgpu::Color::TRANSPARENT
                        } else {
                            wgpu::Color::BLACK
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }
}

fn create_upload_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("xbar-present-upload-texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn create_bind_group(
    device: &wgpu::Device,
    pipeline: &wgpu::RenderPipeline,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("xbar-present-bind-group"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

fn clamp_rect(rect: PresentRect, width: u32, height: u32) -> PresentRect {
    let x = rect.x.min(width);
    let y = rect.y.min(height);
    PresentRect {
        x,
        y,
        width: rect.width.min(width - x),
        height: rect.height.min(height - y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects_clamp_into_the_frame_and_classify_emptiness() {
        assert!(PresentRect::default().is_empty());
        let clamped = clamp_rect(
            PresentRect {
                x: 100,
                y: 30,
                width: 2000,
                height: 50,
            },
            1920,
            40,
        );
        assert_eq!(
            clamped,
            PresentRect {
                x: 100,
                y: 30,
                width: 1820,
                height: 10,
            }
        );
        let outside = clamp_rect(
            PresentRect {
                x: 5000,
                y: 0,
                width: 10,
                height: 10,
            },
            1920,
            40,
        );
        assert!(outside.is_empty());
    }
}
