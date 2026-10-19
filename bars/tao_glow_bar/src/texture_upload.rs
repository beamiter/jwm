//! Retained GL texture uploads for the reusable Cairo canvas.
//!
//! The texture is allocated only on a size change. Ordinary frames update
//! the damaged rectangle directly from the original, possibly padded rows.

use anyhow::{Result, ensure};
use glow::HasContext;
use xbar_core::render::cairo::{CpuFrame, PixelRect};

#[derive(Debug, Default)]
pub struct TextureUpload {
    size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UploadPlan {
    allocate: bool,
    rect: PixelRect,
    offset: usize,
    end: usize,
    row_length: i32,
}

impl TextureUpload {
    pub fn upload(
        &mut self,
        gl: &glow::Context,
        texture: glow::NativeTexture,
        frame: &CpuFrame<'_>,
    ) -> Result<()> {
        let plan = upload_plan(self.size, frame)?;
        if plan.rect.is_empty() {
            return Ok(());
        }
        if plan.allocate {
            let limit = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
            ensure!(
                limit > 0 && frame.width <= limit as u32 && frame.height <= limit as u32,
                "GL texture {}x{} exceeds device limit {limit}",
                frame.width,
                frame.height
            );
        }
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            if plan.allocate {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    frame.width as i32,
                    frame.height as i32,
                    0,
                    glow::BGRA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                let error = gl.get_error();
                if error != glow::NO_ERROR {
                    gl.bind_texture(glow::TEXTURE_2D, None);
                    self.size = None;
                    anyhow::bail!("GL texture allocation failed: 0x{error:04x}");
                }
            }
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, plan.row_length);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                plan.rect.x as i32,
                plan.rect.y as i32,
                plan.rect.width as i32,
                plan.rect.height as i32,
                glow::BGRA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&frame.data[plan.offset..plan.end])),
            );
            // This presenter owns its context. Leave the usual tightly
            // packed state for any subsequent full-frame upload/readback.
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.bind_texture(glow::TEXTURE_2D, None);
            let error = gl.get_error();
            if error != glow::NO_ERROR {
                // Cairo has already advanced its previous-scene baseline.
                // A failed region upload must therefore force a complete
                // replacement on retry, even when the next damage is empty.
                self.size = None;
                anyhow::bail!("GL frame upload failed: 0x{error:04x}");
            }
        }
        self.size = Some((frame.width, frame.height));
        Ok(())
    }
}

fn upload_plan(size: Option<(u32, u32)>, frame: &CpuFrame<'_>) -> Result<UploadPlan> {
    ensure!(
        frame.width > 0
            && frame.height > 0
            && frame.width <= i32::MAX as u32
            && frame.height <= i32::MAX as u32,
        "invalid GL frame dimensions"
    );
    let row_bytes = frame
        .width
        .checked_mul(4)
        .ok_or_else(|| anyhow::anyhow!("GL frame row size overflow"))?;
    ensure!(
        frame.stride >= row_bytes && frame.stride.is_multiple_of(4),
        "invalid BGRA frame stride"
    );
    let storage = u64::from(frame.stride) * u64::from(frame.height);
    ensure!(
        storage <= xbar_core::MAX_FRONTEND_FRAME_BYTES as u64 && storage <= frame.data.len() as u64,
        "GL frame storage exceeds its buffer or allocation budget"
    );
    let allocate = size != Some((frame.width, frame.height));
    let full = PixelRect {
        x: 0,
        y: 0,
        width: frame.width,
        height: frame.height,
    };
    let rect = if allocate {
        full
    } else {
        frame.damage.unwrap_or(full)
    };
    let x = rect.x.min(frame.width);
    let y = rect.y.min(frame.height);
    let rect = PixelRect {
        x,
        y,
        width: rect.width.min(frame.width - x),
        height: rect.height.min(frame.height - y),
    };
    let offset = y as usize * frame.stride as usize + x as usize * 4;
    let end = if rect.is_empty() {
        offset
    } else {
        offset + (rect.height as usize - 1) * frame.stride as usize + rect.width as usize * 4
    };
    Ok(UploadPlan {
        allocate,
        rect,
        offset,
        end,
        row_length: (frame.stride / 4) as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_and_resized_texture_ignore_partial_damage() {
        let frame = CpuFrame {
            data: &[0; 128],
            width: 7,
            height: 4,
            stride: 32,
            damage: Some(PixelRect::default()),
        };
        for size in [None, Some((8, 4)), Some((7, 3))] {
            let plan = upload_plan(size, &frame).unwrap();
            assert!(plan.allocate);
            assert_eq!(
                plan.rect,
                PixelRect {
                    x: 0,
                    y: 0,
                    width: 7,
                    height: 4
                }
            );
            assert_eq!((plan.offset, plan.end, plan.row_length), (0, 124, 8));
        }
        assert!(upload_plan(Some((7, 4)), &frame).unwrap().rect.is_empty());
    }

    #[test]
    fn partial_upload_keeps_padding_and_clamps_at_the_last_pixel() {
        let frame = CpuFrame {
            data: &[0; 128],
            width: 7,
            height: 4,
            stride: 32,
            damage: Some(PixelRect {
                x: 3,
                y: 1,
                width: u32::MAX,
                height: u32::MAX,
            }),
        };
        let plan = upload_plan(Some((7, 4)), &frame).unwrap();
        assert!(!plan.allocate);
        assert_eq!(
            plan.rect,
            PixelRect {
                x: 3,
                y: 1,
                width: 4,
                height: 3
            }
        );
        assert_eq!((plan.offset, plan.end, plan.row_length), (44, 124, 8));
    }

    #[test]
    fn malformed_storage_is_rejected_before_gl_sees_it() {
        for (width, height, stride, len) in [
            (0, 4, 32, 128),
            (7, 0, 32, 128),
            (7, 4, 24, 128),
            (7, 4, 31, 128),
            (7, 4, 32, 127),
            (u32::MAX, u32::MAX, u32::MAX, 128),
        ] {
            let bytes = vec![0; len];
            let frame = CpuFrame {
                data: &bytes,
                width,
                height,
                stride,
                damage: None,
            };
            assert!(upload_plan(None, &frame).is_err());
        }
    }

    // Run on an isolated test display, for example:
    // xvfb-run -a cargo test --manifest-path bars/tao_glow_bar/Cargo.toml \
    //   retained_texture_matches_full_frames -- --ignored
    #[test]
    #[ignore = "requires an X11 display with EGL/OpenGL support"]
    fn retained_texture_matches_full_frames() {
        use glutin::config::{ConfigSurfaceTypes, ConfigTemplateBuilder};
        use glutin::context::{ContextApi, ContextAttributesBuilder};
        use glutin::display::{Display, DisplayApiPreference};
        use glutin::prelude::*;
        use glutin::surface::{PbufferSurface, SurfaceAttributesBuilder};
        use raw_window_handle::{RawDisplayHandle, XlibDisplayHandle};
        use std::{ffi::CString, num::NonZeroU32};
        use xbar_core::presentation::PresentationConfig;
        use xbar_core::render::cairo::{CairoBar, CpuCanvas};
        use xbar_core::{BarEdge, BarEvent, BarRuntime, ClockState, UserAction};

        let display = unsafe {
            Display::new(
                RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0)),
                DisplayApiPreference::Egl,
            )
        }
        .unwrap();
        let template = ConfigTemplateBuilder::new()
            .with_surface_type(ConfigSurfaceTypes::PBUFFER)
            .with_alpha_size(8)
            .build();
        let config = unsafe { display.find_configs(template) }
            .unwrap()
            .next()
            .unwrap();
        let attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(None))
            .build(None);
        let context = unsafe { display.create_context(&config, &attributes) }.unwrap();
        let surface_attributes = SurfaceAttributesBuilder::<PbufferSurface>::new()
            .build(NonZeroU32::new(1).unwrap(), NonZeroU32::new(1).unwrap());
        let surface =
            unsafe { display.create_pbuffer_surface(&config, &surface_attributes) }.unwrap();
        let _context = context.make_current(&surface).unwrap();
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                display.get_proc_address(&CString::new(name).unwrap()) as *const _
            })
        };
        let texture = crate::create_texture(&gl);
        let mut upload = TextureUpload::default();

        let readback = |frame: &CpuFrame<'_>| {
            let mut actual = vec![0; (frame.width * frame.height * 4) as usize];
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.get_tex_image(
                    glow::TEXTURE_2D,
                    0,
                    glow::BGRA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut actual)),
                );
                gl.bind_texture(glow::TEXTURE_2D, None);
                assert_eq!(gl.get_error(), glow::NO_ERROR);
                assert_eq!(gl.get_parameter_i32(glow::UNPACK_ROW_LENGTH), 0);
            }
            let row_bytes = frame.width as usize * 4;
            for y in 0..frame.height as usize {
                assert_eq!(
                    &actual[y * row_bytes..(y + 1) * row_bytes],
                    &frame.data[y * frame.stride as usize..y * frame.stride as usize + row_bytes],
                    "retained texture diverged from the complete Cairo frame on row {y}"
                );
            }
        };

        // Nonzero offsets, padded source rows, the last pixel and a texture
        // resize all go through the same production uploader as the bar.
        let mut bytes = vec![0; 32 * 4];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut frame = CpuFrame {
            data: &bytes,
            width: 7,
            height: 4,
            stride: 32,
            damage: None,
        };
        upload.upload(&gl, texture, &frame).unwrap();
        readback(&frame);
        frame.damage = Some(PixelRect::default());
        upload.upload(&gl, texture, &frame).unwrap();
        readback(&frame);
        for y in 1..4 {
            for x in 3 * 4..7 * 4 {
                bytes[y * 32 + x] = 211;
            }
        }
        let frame = CpuFrame {
            data: &bytes,
            width: 7,
            height: 4,
            stride: 32,
            damage: Some(PixelRect {
                x: 3,
                y: 1,
                width: 4,
                height: 3,
            }),
        };
        upload.upload(&gl, texture, &frame).unwrap();
        readback(&frame);

        // Simulate retained storage becoming invalid between frames. The
        // failed subupload must not commit a false acknowledgement, and the
        // next frame must recover without requiring another size event.
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                1,
                1,
                0,
                glow::BGRA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        assert!(upload.upload(&gl, texture, &frame).is_err());
        assert_eq!(upload.size, None);
        let retry = CpuFrame {
            damage: Some(PixelRect::default()),
            ..frame
        };
        upload.upload(&gl, texture, &retry).unwrap();
        readback(&retry);

        let mut partial_frames = 0;
        for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
            let mut bar = CairoBar::new(
                BarRuntime::default(),
                PresentationConfig {
                    edge,
                    ..PresentationConfig::default()
                },
                pango::FontDescription::from_string("DejaVu Sans 13"),
            );
            let mut canvas = CpuCanvas::new();
            for scale in [1.0, 1.5, 2.0] {
                let (width, height) = if edge.is_vertical() {
                    (64, 960)
                } else {
                    (960, 64)
                };
                for minute in ["12:34", "12:35", "12:36", "12:36"] {
                    let update = bar.runtime_mut().apply_event(BarEvent::Clock(ClockState {
                        minute: minute.to_owned(),
                        second: format!("{minute}:00"),
                    }));
                    assert!(update.issues.is_empty());
                    let frame = canvas.render(&mut bar, width, height, scale).unwrap();
                    if let Some(damage) = frame.damage {
                        if !damage.is_empty() && damage.width * damage.height < width * height {
                            partial_frames += 1;
                        }
                    }
                    upload.upload(&gl, texture, &frame).unwrap();
                    readback(&frame);
                }
                let update = bar.runtime_mut().dispatch(UserAction::ToggleTheme);
                assert!(update.issues.is_empty());
                let frame = canvas.render(&mut bar, width, height, scale).unwrap();
                upload.upload(&gl, texture, &frame).unwrap();
                readback(&frame);
            }
        }
        assert!(
            partial_frames >= 12,
            "clock updates must exercise partial uploads"
        );
        unsafe {
            gl.delete_texture(texture);
        }
    }
}
