//! XDamage regional readback for a private, native-size RGB baseline.
//!
//! The consumer always receives its own complete snapshot. Intermediate
//! mailbox replacements therefore cannot lose damage from skipped captures.

use super::*;
use x11rb::protocol::xproto::Rectangle;

const MAX_RECTS: usize = 16;
const MAX_PARTS_REPLY_RECTS: usize = 256;
const MAX_BASELINE_BYTES: u64 = 48 * 1024 * 1024;
const FULL_REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Scope {
    pub(super) drawable: Window,
    pub(super) damage: damage::Damage,
    pub(super) origin: (i16, i16),
    pub(super) dimensions: (u16, u16),
    pub(super) geometry_epoch: u64,
}

impl Scope {
    pub(super) fn bytes(self) -> u64 {
        u64::from(self.dimensions.0) * u64::from(self.dimensions.1) * 3
    }
}

struct Baseline {
    scope: Scope,
    full_at: Instant,
    image: RgbImage,
}

pub(super) struct Regions {
    parts: xfixes::Region,
    baseline: Option<Baseline>,
    reported_active: bool,
}

impl Regions {
    pub(super) fn open(conn: &RustConnection) -> RemoteResult<Self> {
        let parts = conn.generate_id()?;
        xfixes::create_region(conn, parts, &[])?.check()?;
        Ok(Self {
            parts,
            baseline: None,
            reported_active: false,
        })
    }

    pub(super) fn parts_for(&self, scope: Scope, now: Instant) -> Option<xfixes::Region> {
        self.baseline
            .as_ref()
            .filter(|base| {
                base.scope == scope && now.saturating_duration_since(base.full_at) < FULL_REFRESH
            })
            .map(|_| self.parts)
    }

    pub(super) fn invalidate(&mut self) {
        self.baseline = None;
    }

    pub(super) fn remember(
        &mut self,
        scope: Scope,
        image: &RgbImage,
        now: Instant,
    ) -> RemoteResult<()> {
        if scope.bytes() > MAX_BASELINE_BYTES
            || image.dimensions() != (scope.dimensions.0.into(), scope.dimensions.1.into())
        {
            self.invalidate();
            return Ok(());
        }
        if let Some(base) = self.baseline.as_mut().filter(|base| base.scope == scope) {
            base.image.as_mut().copy_from_slice(image.as_raw());
            base.full_at = now;
        } else {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(image.as_raw().len())?;
            bytes.extend_from_slice(image.as_raw());
            let image = RgbImage::from_raw(image.width(), image.height(), bytes)
                .ok_or_else(|| invalid_data("regional capture baseline has invalid dimensions"))?;
            self.baseline = Some(Baseline {
                scope,
                full_at: now,
                image,
            });
        }
        Ok(())
    }

    pub(super) fn rectangles(
        &self,
        conn: &RustConnection,
        scope: Scope,
    ) -> RemoteResult<Option<Vec<Rectangle>>> {
        let rectangles = xfixes::fetch_region(conn, self.parts)?.reply()?.rectangles;
        Ok(clipped_rectangles(scope, &rectangles))
    }

    pub(super) fn patch(&mut self, scope: Scope, rect: Rectangle, patch: &RgbImage) {
        let baseline = self.baseline.as_mut().expect("regional baseline selected");
        debug_assert_eq!(baseline.scope, scope);
        let local_x = (i32::from(rect.x) - i32::from(scope.origin.0)) as usize;
        let local_y = (i32::from(rect.y) - i32::from(scope.origin.1)) as usize;
        let row_bytes = usize::from(rect.width) * 3;
        let stride = usize::from(scope.dimensions.0) * 3;
        let target = baseline.image.as_mut();
        for (row, pixels) in patch.as_raw().chunks_exact(row_bytes).enumerate() {
            let start = (local_y + row) * stride + local_x * 3;
            target[start..start + row_bytes].copy_from_slice(pixels);
        }
    }

    pub(super) fn snapshot(&mut self, pool: &mut CaptureImageRecycler) -> RemoteResult<RgbImage> {
        let baseline = self.baseline.as_ref().expect("regional baseline selected");
        let (width, height) = baseline.scope.dimensions;
        let mut bytes = pool.take(width, height)?;
        bytes.copy_from_slice(baseline.image.as_raw());
        if !self.reported_active {
            self.reported_active = true;
            eprintln!("jwm-remote: X11 XDamage regional readback active");
        }
        RgbImage::from_raw(width.into(), height.into(), bytes)
            .ok_or_else(|| invalid_data("regional capture snapshot has invalid dimensions").into())
    }

    pub(super) fn release(self, conn: &RustConnection) {
        if let Ok(request) = xfixes::destroy_region(conn, self.parts) {
            request.ignore_error();
        }
    }
}

fn clipped_rectangles(scope: Scope, rectangles: &[Rectangle]) -> Option<Vec<Rectangle>> {
    if rectangles.len() > MAX_PARTS_REPLY_RECTS {
        return None;
    }
    let (left, top) = (i32::from(scope.origin.0), i32::from(scope.origin.1));
    let (right, bottom) = (
        left + i32::from(scope.dimensions.0),
        top + i32::from(scope.dimensions.1),
    );
    let mut clipped: Vec<Rectangle> = Vec::new();
    for rect in rectangles {
        let x = i32::from(rect.x);
        let y = i32::from(rect.y);
        let x0 = x.max(left);
        let y0 = y.max(top);
        let x1 = (x + i32::from(rect.width)).min(right);
        let y1 = (y + i32::from(rect.height)).min(bottom);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        if let Some(last) = clipped.last_mut()
            && i32::from(last.x) + i32::from(last.width) == x0
            && i32::from(last.y) == y0
            && i32::from(last.height) == y1 - y0
        {
            last.width = u16::try_from(x1 - i32::from(last.x)).ok()?;
        } else {
            if clipped.len() == MAX_RECTS {
                return None;
            }
            clipped.push(Rectangle {
                x: i16::try_from(x0).ok()?,
                y: i16::try_from(y0).ok()?,
                width: u16::try_from(x1 - x0).ok()?,
                height: u16::try_from(y1 - y0).ok()?,
            });
        }
    }
    let covered: u64 = clipped
        .iter()
        .map(|rect| u64::from(rect.width) * u64::from(rect.height))
        .sum();
    (covered * 12 <= scope.bytes()).then_some(clipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_and_merging_never_read_outside_the_granted_area() {
        let scope = Scope {
            drawable: 2,
            damage: 4,
            origin: (10, 20),
            dimensions: (200, 120),
            geometry_epoch: 3,
        };
        let damage = [
            Rectangle {
                x: 0,
                y: 18,
                width: 20,
                height: 10,
            },
            Rectangle {
                x: 20,
                y: 18,
                width: 10,
                height: 10,
            },
            Rectangle {
                x: 220,
                y: 10,
                width: 20,
                height: 20,
            },
        ];
        let clipped = clipped_rectangles(scope, &damage).unwrap();
        assert_eq!(clipped.len(), 1);
        let rect = clipped[0];
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (10, 20, 20, 8));
        let all = Rectangle {
            x: 10,
            y: 20,
            width: 200,
            height: 120,
        };
        assert!(clipped_rectangles(scope, &[all]).is_none());
    }

    #[cfg(any(
        feature = "backend-x11rb",
        feature = "backend-xcb",
        feature = "wayland-backends"
    ))]
    #[test]
    fn owned_x11_overlay_accumulates_partial_updates_without_overwriting_older_frames() {
        let x11 = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
        let (conn, screen) = open_display(Some(x11.name())).unwrap();
        let root = conn.setup().roots[screen].root;
        let owner = conn.generate_id().unwrap();
        conn.create_window(
            0,
            owner,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        let selection = compositor_selection(&conn, screen).unwrap();
        conn.set_selection_owner(owner, selection, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        let area = CaptureArea::Region(CaptureRegion {
            x: 11,
            y: 13,
            width: 96,
            height: 64,
        });
        let mut capture = X11Capture::connect(
            Some(x11.name()),
            CaptureSource::Overlay,
            0,
            area,
            ScalingPolicy::Cpu,
        )
        .unwrap();
        assert!(capture.enable_cursor_plane());
        capture.enable_region_readback();
        capture.enable_capture_metrics();
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, capture.drawable, &CreateGCAux::new())
            .unwrap()
            .check()
            .unwrap();
        let drawable = capture.drawable;
        let paint = |color, rectangle| {
            conn.change_gc(
                gc,
                &x11rb::protocol::xproto::ChangeGCAux::new().foreground(color),
            )
            .unwrap()
            .check()
            .unwrap();
            conn.poly_fill_rectangle(drawable, gc, &[rectangle])
                .unwrap()
                .check()
                .unwrap();
        };
        let background = [0x12, 0x34, 0x56];
        let first_color = [0x98, 0x76, 0x54];
        let second_color = [0x44, 0x99, 0x66];
        let updates = [
            (
                background,
                Rectangle {
                    x: 11,
                    y: 13,
                    width: 96,
                    height: 64,
                },
                [background, background],
            ),
            (
                first_color,
                Rectangle {
                    x: 5,
                    y: 11,
                    width: 16,
                    height: 8,
                },
                [first_color, background],
            ),
            (
                second_color,
                Rectangle {
                    x: 41,
                    y: 40,
                    width: 8,
                    height: 8,
                },
                [first_color, second_color],
            ),
        ];
        let recycler = capture.image_recycler();
        let captures = updates.into_iter().map(|([r, g, b], rect, expected)| {
            paint(u32::from_be_bytes([0, r, g, b]), rect);
            let CaptureOutcome::Frame(frame) = capture.frame().unwrap() else {
                panic!("paint must produce a complete frame");
            };
            assert_eq!(frame.image.get_pixel(0, 0).0, expected[0]);
            assert_eq!(frame.image.get_pixel(30, 27).0, expected[1]);
            frame
        });
        let [wire_base, wire_latest] =
            crate::remote::host::send_replaced_x11_capture(recycler, captures);
        let check_jpeg_pixel = |image: &RgbImage, x, y, expected| {
            for (actual, expected) in image.get_pixel(x, y).0.into_iter().zip(expected) {
                assert!(
                    actual.abs_diff(expected) <= 8,
                    "pixel ({x}, {y}) expected channel {expected}, got {actual}"
                );
            }
        };
        assert_eq!(wire_base.dimensions(), (96, 64));
        assert_eq!(wire_latest.dimensions(), wire_base.dimensions());
        check_jpeg_pixel(&wire_base, 3, 2, background);
        check_jpeg_pixel(&wire_base, 34, 30, background);
        check_jpeg_pixel(&wire_latest, 3, 2, first_color);
        check_jpeg_pixel(&wire_latest, 34, 30, second_color);
        check_jpeg_pixel(&wire_latest, 60, 52, background);
        let metrics = capture.take_capture_metrics().unwrap().regional_readback;
        assert_eq!(metrics.partial_frames, 2);
        assert_eq!(metrics.full_frames, 1);
        assert_eq!(metrics.pixels, 96 * 64 + 10 * 6 + 8 * 8);
    }
}
