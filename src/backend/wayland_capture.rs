//! Bounded screenshots of the actual windowed Wayland compositor frame.
use crate::backend::compositor_common::{
    capture,
    screenshot::{self, ScreenshotQueue, ScreenshotRequest},
};
use smithay::backend::{
    allocator::Fourcc,
    renderer::{
        ExportMem, TextureMapping,
        gles::{GlesRenderer, GlesTarget},
    },
};
use smithay::utils::Rectangle;

pub(crate) fn drain(
    renderer: &mut GlesRenderer,
    framebuffer: &GlesTarget<'_>,
    requests: &mut ScreenshotQueue,
    width: u32,
    height: u32,
) {
    for request in requests.take_all() {
        let (path, permit, region) = match request {
            ScreenshotRequest::Full { path, permit } => (path, permit, None),
            ScreenshotRequest::Region {
                path,
                permit,
                x,
                y,
                width: w,
                height: h,
            } => {
                let Some(region) = capture::clip_region(width, height, x, y, w, h) else {
                    continue;
                };
                (path, permit, Some(region))
            }
        };
        if path.is_cancelled() {
            continue;
        }
        let result = (|| -> Result<(Vec<u8>, u32, u32), Box<dyn std::error::Error>> {
            capture::rgba_capture_len(width, height)?;
            let mapping = renderer.copy_framebuffer(
                framebuffer,
                Rectangle::from_size((width as i32, height as i32).into()),
                Fourcc::Abgr8888,
            )?;
            let mut pixels =
                capture::copy_rgba_capture(renderer.map_texture(&mapping)?, width, height)?;
            if mapping.flipped() {
                capture::flip_rgba_vertical(&mut pixels, width, height);
            }
            if let Some(region) = region {
                let mut cropped = capture::allocate_rgba_capture(region.width, region.height)?;
                let row = region.width as usize * 4;
                for y in 0..region.height as usize {
                    let source = ((region.y as usize + y) * width as usize + region.x as usize) * 4;
                    cropped[y * row..(y + 1) * row].copy_from_slice(&pixels[source..source + row]);
                }
                Ok((cropped, region.width, region.height))
            } else {
                Ok((pixels, width, height))
            }
        })();
        match result {
            Ok((pixels, width, height)) => {
                if let Err(error) =
                    screenshot::spawn_screenshot_worker("jwm-nested-shot", permit, move || {
                        if let Err(error) =
                            screenshot::save_screenshot_owned(path, pixels, width, height)
                        {
                            log::error!("nested Wayland screenshot PNG failed: {error}");
                        }
                    })
                {
                    log::error!("nested Wayland screenshot worker failed: {error}");
                }
            }
            Err(error) => log::error!("nested Wayland compositor readback failed: {error:#}"),
        }
    }
}
