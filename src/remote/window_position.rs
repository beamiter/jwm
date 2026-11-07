//! Explicit local startup position; never an observed or automatically saved state.
use serde::{Deserialize, Serialize};
use std::{fmt, io, str::FromStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowPosition {
    pub x: i16,
    pub y: i16,
}

impl fmt::Display for WindowPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{}", self.x, self.y)
    }
}
impl FromStr for WindowPosition {
    type Err = io::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "window position must be X,Y using signed ASCII integers in -32768..=32767",
            )
        };
        let (x, y) = value.split_once(',').ok_or_else(invalid)?;
        for part in [x, y] {
            let digits = part
                .strip_prefix('-')
                .or_else(|| part.strip_prefix('+'))
                .unwrap_or(part);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
        }
        Ok(Self {
            x: x.parse().map_err(|_| invalid())?,
            y: y.parse().map_err(|_| invalid())?,
        })
    }
}

pub(crate) type Monitor = (i16, i16, u16, u16);

#[derive(Clone, Copy, Debug)]
pub(crate) struct PlacementTarget {
    pub bounds: Monitor,
    pub position_visible: bool,
}

/// All edges use i32, which covers any i16 origin plus u16 extent.
fn clipped(monitor: Monitor, root: (u16, u16)) -> Option<Monitor> {
    let left = i32::from(monitor.0).max(0);
    let top = i32::from(monitor.1).max(0);
    let right = (i32::from(monitor.0) + i32::from(monitor.2)).min(i32::from(root.0));
    let bottom = (i32::from(monitor.1) + i32::from(monitor.3)).min(i32::from(root.1));
    (right > left && bottom > top).then(|| {
        (
            left as i16,
            top as i16,
            (right - left) as u16,
            (bottom - top) as u16,
        )
    })
}
fn contains(bounds: Monitor, position: WindowPosition) -> bool {
    let (x, y) = (i32::from(position.x), i32::from(position.y));
    x >= i32::from(bounds.0)
        && y >= i32::from(bounds.1)
        && x < i32::from(bounds.0) + i32::from(bounds.2)
        && y < i32::from(bounds.1) + i32::from(bounds.3)
}

/// Unknown topology never certifies a saved position, even if it fits the root.
pub(crate) fn select_target(
    monitors: Option<&[Monitor]>,
    fallback: Monitor,
    root: (u16, u16),
    position: WindowPosition,
) -> io::Result<PlacementTarget> {
    if root.0 == 0 || root.1 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local screen has an empty dimension",
        ));
    }
    let visible: Vec<_> = monitors
        .unwrap_or_default()
        .iter()
        .filter_map(|m| clipped(*m, root))
        .collect();
    if let Some(bounds) = visible.iter().find(|bounds| contains(**bounds, position)) {
        return Ok(PlacementTarget {
            bounds: *bounds,
            position_visible: true,
        });
    }
    // If monitors are known, never center in a root-spanning gap merely
    // because the fallback topology query failed or changed underneath us.
    let fallback = clipped(fallback, root);
    let bounds = visible
        .iter()
        .copied()
        .find(|bounds| Some(*bounds) == fallback)
        .or_else(|| visible.first().copied())
        .or(fallback)
        .unwrap_or((0, 0, root.0, root.1));
    Ok(PlacementTarget {
        bounds,
        position_visible: false,
    })
}

/// Accept only a fully visible client rectangle. Otherwise put its origin on
/// the selected monitor, including when an explicit size exceeds that monitor.
pub(crate) fn place(
    target: PlacementTarget,
    size: (u16, u16),
    requested: WindowPosition,
) -> ((i16, i16), bool) {
    let (left, top, width, height) = target.bounds;
    let fits = target.position_visible
        && size.0 > 0
        && size.1 > 0
        && contains(target.bounds, requested)
        && i32::from(requested.x) + i32::from(size.0) <= i32::from(left) + i32::from(width)
        && i32::from(requested.y) + i32::from(size.1) <= i32::from(top) + i32::from(height);
    if fits {
        return ((requested.x, requested.y), true);
    }
    let center = |origin: i16, extent: u16, length: u16| {
        (i32::from(origin) + (i32::from(extent) - i32::from(length)).max(0) / 2)
            .min(i32::from(i16::MAX)) as i16
    };
    (
        (center(left, width, size.0), center(top, height, size.1)),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_signed_position_parser() {
        for (value, expected) in [
            ("0,0", (0, 0)),
            ("-32768,32767", (-32768, 32767)),
            ("+1,-2", (1, -2)),
        ] {
            let parsed: WindowPosition = value.parse().unwrap();
            assert_eq!((parsed.x, parsed.y), expected);
            assert_eq!(
                parsed.to_string().parse::<WindowPosition>().unwrap(),
                parsed
            );
        }
        for value in [
            "",
            "1",
            "1,",
            ",1",
            "1,2,3",
            " 1,2",
            "1,2 ",
            "١,2",
            "--1,2",
            "+,1",
            "32768,0",
            "0,-32769",
            "99999999999999,1",
        ] {
            assert!(value.parse::<WindowPosition>().is_err(), "{value}");
        }
    }
    #[test]
    fn target_monitor_gaps_edges_and_offscreen_fallback() {
        let monitors = [(0, 0, 1920, 1080), (2000, 0, 1920, 1080)];
        let root = (3920, 1080);
        let p = WindowPosition { x: 2100, y: 100 };
        let target = select_target(Some(&monitors), monitors[0], root, p).unwrap();
        assert_eq!(target.bounds, monitors[1]);
        assert_eq!(place(target, (800, 600), p), ((2100, 100), true));
        assert_eq!(place(target, (1820, 980), p), ((2100, 100), true));
        assert!(!place(target, (1821, 980), p).1);
        for p in [
            WindowPosition { x: 1950, y: 10 },
            WindowPosition { x: -1, y: 10 },
            WindowPosition { x: 3920, y: 0 },
        ] {
            let target = select_target(Some(&monitors), monitors[0], root, p).unwrap();
            assert!(!target.position_visible);
            assert_eq!(place(target, (800, 600), p), ((560, 240), false));
        }
        let stale_fallback = select_target(
            Some(&monitors),
            (0, 0, 3920, 1080),
            root,
            WindowPosition { x: 1950, y: 10 },
        )
        .unwrap();
        assert_eq!(stale_fallback.bounds, monitors[0]);
        let target = select_target(None, monitors[0], root, p).unwrap();
        assert!(!target.position_visible);
    }
    #[test]
    fn clipping_extremes_and_oversized_fallback_keep_visible_origin() {
        assert_eq!(
            clipped((-100, -50, 500, 300), (1000, 1000)),
            Some((0, 0, 400, 250))
        );
        assert_eq!(clipped((0, 0, 0, 100), (1000, 1000)), None);
        assert_eq!(clipped((2000, 0, 100, 100), (1000, 1000)), None);
        let p = WindowPosition { x: 32767, y: 32767 };
        let target = select_target(
            Some(&[(32767, 32767, 65535, 65535)]),
            (0, 0, 1, 1),
            (65535, 65535),
            p,
        )
        .unwrap();
        assert_eq!(place(target, (1, 1), p), ((32767, 32767), true));
        let target = select_target(
            Some(&[(0, 0, 800, 600)]),
            (0, 0, 800, 600),
            (800, 600),
            WindowPosition { x: 790, y: 590 },
        )
        .unwrap();
        assert_eq!(
            place(target, (8192, 2048), WindowPosition { x: 790, y: 590 }),
            ((0, 0), false)
        );
        assert!(select_target(None, (0, 0, 0, 0), (0, 0), p).is_err());
        let removed = select_target(Some(&[]), (2000, 0, 800, 600), (800, 600), p).unwrap();
        assert_eq!(removed.bounds, (0, 0, 800, 600));
        assert!(!removed.position_visible);
    }
}
