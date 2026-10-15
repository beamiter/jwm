//! Backend-neutral preparation of four-byte native presentation buffers.

use std::{fmt, num::NonZeroU32};

#[derive(Debug, PartialEq, Eq)]
pub enum SurfacePreparationError<E> {
    FrameTooLarge { width: u32, height: u32 },
    Resize(E),
}

impl<E: fmt::Display> fmt::Display for SurfacePreparationError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { width, height } => write!(
                f,
                "surface {width}x{height} exceeds the {}-byte frontend frame budget",
                crate::MAX_FRONTEND_FRAME_BYTES
            ),
            Self::Resize(error) => write!(f, "surface resize failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for SurfacePreparationError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::FrameTooLarge { .. } => None,
            Self::Resize(error) => Some(error),
        }
    }
}

/// Check the common frame budget before invoking a native allocator.
///
/// A zero extent is an allocation-free no-op; callers must also skip painting
/// that frame. Invoke this on redraw as well as resize events: failures are
/// never cached here, so the next frame can retry without another OS resize.
/// Native backends may impose tighter limits and cache successful dimensions.
pub fn prepare_surface_size<E>(
    width: u32,
    height: u32,
    resize: impl FnOnce(NonZeroU32, NonZeroU32) -> Result<(), E>,
) -> Result<(), SurfacePreparationError<E>> {
    let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
        return Ok(());
    };
    // The product of two u32 values fits u64, unlike a platform-sized usize
    // product or a subsequent unchecked multiplication by four.
    let pixels = u64::from(width) * u64::from(height);
    if pixels > (crate::MAX_FRONTEND_FRAME_BYTES / 4) as u64 {
        return Err(SurfacePreparationError::FrameTooLarge { width, height });
    }
    resize(w, h).map_err(SurfacePreparationError::Resize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_extent_never_enters_the_backend() {
        for (width, height) in [(0, 0), (0, u32::MAX), (u32::MAX, 0)] {
            prepare_surface_size::<&str>(width, height, |_, _| {
                panic!("zero-sized surfaces must not allocate")
            })
            .unwrap();
        }
    }

    #[test]
    fn frame_budget_is_checked_before_backend_allocation() {
        let max_pixels = (crate::MAX_FRONTEND_FRAME_BYTES / 4) as u32;
        for (width, height) in [(u32::MAX, u32::MAX), (65_535, 65_535), (max_pixels + 1, 1)] {
            assert_eq!(
                prepare_surface_size::<&str>(width, height, |_, _| panic!("oversized allocation")),
                Err(SurfacePreparationError::FrameTooLarge { width, height })
            );
        }
        for (width, height) in [(1, 1), (8192, 64), (32768, 64), (max_pixels, 1)] {
            let mut prepared = None;
            prepare_surface_size::<&str>(width, height, |w, h| {
                prepared = Some((w.get(), h.get()));
                Ok(())
            })
            .unwrap();
            assert_eq!(prepared, Some((width, height)));
        }
    }

    #[test]
    fn the_same_extent_retries_after_a_transient_backend_failure() {
        let mut calls = 0;
        let mut committed = (640, 40);
        let mut resize = |w: NonZeroU32, h: NonZeroU32| {
            calls += 1;
            if calls == 1 {
                Err("temporary allocation failure")
            } else {
                committed = (w.get(), h.get());
                Ok(())
            }
        };
        assert_eq!(
            prepare_surface_size(1920, 48, &mut resize),
            Err(SurfacePreparationError::Resize(
                "temporary allocation failure"
            ))
        );
        prepare_surface_size(1920, 48, &mut resize).unwrap();
        assert_eq!(calls, 2);
        assert_eq!(committed, (1920, 48));
    }
}
