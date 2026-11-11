//! Bounded, root-relative capture composition. Never consult output pixels.
use crate::backend::wayland::state::JwmWaylandState;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface};
use smithay::wayland::compositor::{get_children, with_states};
use smithay::wayland::shell::xdg::SurfaceCachedState;

pub(super) struct CaptureTree {
    pub(super) surface: WlSurface,
    pub(super) origin: (i32, i32),
    pub(super) mapped: usize,
}
// Keep every input and intermediate well away from i32 clipping arithmetic
// limits. This deliberately rejects extreme off-canvas app coordinates rather
// than passing them into renderer rectangle subtraction/intersection.
const COORDINATE_LIMIT: i32 = 65_536;
fn bounded(point: (i32, i32)) -> bool {
    (-COORDINATE_LIMIT..=COORDINATE_LIMIT).contains(&point.0)
        && (-COORDINATE_LIMIT..=COORDINATE_LIMIT).contains(&point.1)
}
fn add(a: (i32, i32), b: (i32, i32)) -> Option<(i32, i32)> {
    if !bounded(a) || !bounded(b) {
        return None;
    }
    let result = (a.0.checked_add(b.0)?, a.1.checked_add(b.1)?);
    bounded(result).then_some(result)
}
fn geometry_origin(surface: &WlSurface) -> (i32, i32) {
    with_states(surface, |s| {
        s.cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map(|r| (r.loc.x, r.loc.y))
            .unwrap_or((0, 0))
    })
}
fn validate_tree(root: &WlSurface, origin: (i32, i32), budget: &mut usize) -> Option<usize> {
    let owner = root.client()?.id();
    let mut pending = vec![(root.clone(), origin, 0usize)];
    let mut mapped = 0;
    while let Some((surface, location, depth)) = pending.pop() {
        *budget = budget.checked_add(1)?;
        if *budget > 128 || depth > 16 || !surface.is_alive() || surface.client()?.id() != owner {
            return None;
        }
        let view = with_states(&surface, |s| {
            s.data_map
                .get::<RendererSurfaceStateUserData>()
                .and_then(|d| d.lock().unwrap().view())
        });
        let Some(view) = view else {
            continue;
        };
        if view.dst.w <= 0 || view.dst.h <= 0 || view.dst.w > 16384 || view.dst.h > 16384 {
            return None;
        }
        let location = add(location, (view.offset.x, view.offset.y))?;
        add(location, (view.dst.w, view.dst.h))?;
        mapped += 1;
        let children = get_children(&surface);
        if children.len() > 128 || pending.len() + children.len() > 128 {
            return None;
        }
        pending.extend(children.into_iter().map(|s| (s, location, depth + 1)));
    }
    Some(mapped)
}
/// Front-to-back trees, including the root last. Invalid associated trees fail
/// the entire frame. The caller renders into the unchanged root-sized target.
pub(super) fn snapshot(
    state: &JwmWaylandState,
    root: &WlSurface,
    root_origin: (i32, i32),
) -> Option<Vec<CaptureTree>> {
    let owner = root.client()?.id();
    // Positioner locations are relative to the root xdg window geometry,
    // whereas follow sessions may crop its intersection with the actual buffer.
    // Keep that distinction even when the intersection moves without resizing.
    let popup_canvas_offset = add(geometry_origin(root), root_origin)?;
    // Also bound traversal of unrelated popup entries, not only emitted ones.
    if state.popup_order.len() > 512 {
        return None;
    }
    let mut trees = Vec::new();
    let mut budget = 0;
    let mut popup_count = 0;
    let mut ordered = Vec::new();
    for id in state.popup_order.iter().rev() {
        if ordered.contains(id) {
            return None;
        }
        ordered.push(id.clone());
        let Some(popup) = state.popups.get(id) else {
            continue;
        };
        let surface = popup.wl_surface();
        if !surface.is_alive() {
            continue;
        }
        let mut current = popup.clone();
        let mut offset = (0, 0);
        let mut seen = Vec::new();
        let mut associated = false;
        for _ in 0..16 {
            let current_surface = current.wl_surface();
            if seen.contains(&current_surface.id()) {
                return None;
            }
            seen.push(current_surface.id());
            let Some(geo) = current.with_committed_state(|s| s.map(|s| s.geometry)) else {
                break;
            };
            offset = add(offset, (geo.loc.x, geo.loc.y))?;
            let Some(parent) = current.get_parent_surface() else {
                break;
            };
            if parent == *root {
                associated = true;
                break;
            }
            let Some(next) = state.popups.get(&parent.id()) else {
                break;
            };
            current = next.clone();
        }
        if !associated {
            if seen.len() >= 16 {
                return None;
            }
            continue;
        }
        popup_count += 1;
        if popup_count > 32 {
            return None;
        }
        // Every actual ancestor must be alive and from the selected app, not
        // merely share a WindowId or the capture client's connection.
        let mut ancestors_mapped = true;
        for id in seen {
            let ancestor = state.popups.get(&id)?.wl_surface();
            if !ancestor.is_alive() || ancestor.client()?.id() != owner {
                return None;
            }
            ancestors_mapped &= with_states(ancestor, |s| {
                s.data_map
                    .get::<RendererSurfaceStateUserData>()
                    .and_then(|d| d.lock().unwrap().view())
                    .is_some()
            });
        }
        if !ancestors_mapped {
            continue;
        }
        let local = geometry_origin(surface);
        let origin = add(
            add(offset, (local.0.checked_neg()?, local.1.checked_neg()?))?,
            popup_canvas_offset,
        )?;
        let mapped = validate_tree(surface, origin, &mut budget)?;
        if mapped != 0 {
            trees.push(CaptureTree {
                surface: surface.clone(),
                origin,
                mapped,
            });
        }
    }
    let mapped = validate_tree(root, root_origin, &mut budget)?;
    if mapped == 0 {
        return None;
    }
    trees.push(CaptureTree {
        surface: root.clone(),
        origin: root_origin,
        mapped,
    });
    Some(trees)
}

#[cfg(test)]
mod tests {
    use super::add;
    #[test]
    fn popup_window_coordinates_follow_the_frozen_crop_not_buffer_origin() {
        // A window geometry beginning at 10 is clipped by an actual view at 14.
        // Its popup at x=7 therefore appears at canvas x=3, not x=7.
        let canvas_offset = add((10, 20), (-14, -25)).unwrap();
        assert_eq!(add((7, 9), canvas_offset), Some((3, 4)));
        // Default fixed-popup capture retains the old window-relative placement.
        assert_eq!(add((10, 20), (-10, -20)), Some((0, 0)));
        assert_eq!(add((65_536, 0), (1, 0)), None);
    }
    #[test]
    fn root_relative_coordinates_are_checked_not_wrapped() {
        assert_eq!(add((-12, 8), (3, -20)), Some((-9, -12)));
        assert_eq!(add((i32::MAX, 0), (1, 0)), None);
        assert_eq!(add((0, i32::MIN), (0, -1)), None);
        assert_eq!(add((-65_536, 0), (0, 16_384)), Some((-65_536, 16_384)));
        assert_eq!(add((-65_536, 0), (-1, 0)), None);
        // Even cancellation cannot launder an extreme operand into a valid sum.
        assert_eq!(add((i32::MIN + 1, 0), (i32::MAX, 0)), None);
    }
}

/// Interactive capture deliberately supports only unscaled, uncropped roots
/// and one fully contained direct popup. Unsupported mappings fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InteractivePopup {
    pub(crate) surface: WlSurface,
    pub(crate) rect: (i32, i32, i32, i32),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImmutableLayout {
    pub(crate) root: WlSurface,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) popup: Option<InteractivePopup>,
}
fn popup_inside(rect: (i32, i32, i32, i32), width: u32, height: u32) -> bool {
    rect.0 >= 0
        && rect.1 >= 0
        && rect.2 > 0
        && rect.3 > 0
        && i64::from(rect.0) + i64::from(rect.2) <= i64::from(width)
        && i64::from(rect.1) + i64::from(rect.3) <= i64::from(height)
}
fn popup_point(rect: (i32, i32, i32, i32), x: u32, y: u32) -> Option<(f64, f64)> {
    let (px, py, w, h) = rect;
    let (x, y) = (i64::from(x), i64::from(y));
    (w > 0
        && h > 0
        && x >= i64::from(px)
        && y >= i64::from(py)
        && x < i64::from(px) + i64::from(w)
        && y < i64::from(py) + i64::from(h))
    .then_some(((x - i64::from(px)) as f64, (y - i64::from(py)) as f64))
}
impl ImmutableLayout {
    pub(crate) fn target_at(&self, x: u32, y: u32) -> Option<(WlSurface, f64, f64)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        if let Some(popup) = &self.popup {
            if let Some((local_x, local_y)) = popup_point(popup.rect, x, y) {
                return Some((popup.surface.clone(), local_x, local_y));
            }
        }
        Some((self.root.clone(), f64::from(x), f64::from(y)))
    }
}
fn interactive_mapping(surface: &WlSurface, width: i32, height: i32) -> bool {
    use smithay::backend::renderer::utils::with_renderer_surface_state;
    use smithay::reexports::wayland_server::protocol::wl_output::Transform;
    use smithay::wayland::{compositor::SurfaceAttributes, viewporter::ViewportCachedState};
    if width <= 0 || height <= 0 || !surface.is_alive() || !get_children(surface).is_empty() {
        return false;
    }
    let mapped = with_renderer_surface_state(surface, |s| {
        s.buffer_size()
            .is_some_and(|b| (b.w, b.h) == (width, height))
            && s.view().is_some_and(|v| {
                v.offset.x == 0
                    && v.offset.y == 0
                    && v.src.loc.x == 0.0
                    && v.src.loc.y == 0.0
                    && (v.src.size.w, v.src.size.h) == (f64::from(width), f64::from(height))
                    && (v.dst.w, v.dst.h) == (width, height)
            })
    })
    .unwrap_or(false);
    mapped
        && with_states(surface, |s| {
            let mut a = s.cached_state.get::<SurfaceAttributes>();
            let a = a.current();
            let mut v = s.cached_state.get::<ViewportCachedState>();
            let v = v.current();
            let mut g = s.cached_state.get::<SurfaceCachedState>();
            let g = g.current();
            a.buffer_scale == 1
                && a.buffer_transform == Transform::Normal
                && a.input_region.is_none()
                && a.buffer_delta.is_none_or(|d| d.x == 0 && d.y == 0)
                && v.src.is_none()
                && v.dst.is_none()
                && g.geometry
                    .is_none_or(|r| (r.loc.x, r.loc.y, r.size.w, r.size.h) == (0, 0, width, height))
        })
}
pub(crate) fn interactive_snapshot(
    state: &JwmWaylandState,
    root: &WlSurface,
    width: u32,
    height: u32,
) -> Option<ImmutableLayout> {
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || !interactive_mapping(root, width as i32, height as i32)
    {
        return None;
    }
    let trees = snapshot(state, root, (0, 0))?;
    if trees.is_empty() || trees.len() > 2 || trees.iter().any(|t| t.mapped != 1) {
        return None;
    }
    let popup = if trees.len() == 2 {
        let t = &trees[0];
        let popup = state.popups.get(&t.surface.id())?;
        if popup.get_parent_surface().as_ref() != Some(root)
            || t.surface.client()?.id() != root.client()?.id()
        {
            return None;
        }
        let geo = popup.with_committed_state(|s| s.map(|s| s.geometry))?;
        let rect = (geo.loc.x, geo.loc.y, geo.size.w, geo.size.h);
        if !popup_inside(rect, width, height)
            || t.origin != (rect.0, rect.1)
            || !interactive_mapping(&t.surface, rect.2, rect.3)
        {
            return None;
        }
        Some(InteractivePopup {
            surface: t.surface.clone(),
            rect,
        })
    } else {
        None
    };
    Some(ImmutableLayout {
        root: root.clone(),
        width,
        height,
        popup,
    })
}

#[cfg(test)]
mod interactive_tests {
    use super::{popup_inside, popup_point};
    #[test]
    fn interactive_popup_is_fully_contained_and_half_open() {
        assert!(popup_inside((4, 7, 12, 9), 16, 16));
        assert_eq!(popup_point((4, 7, 12, 9), 4, 7), Some((0.0, 0.0)));
        assert_eq!(popup_point((4, 7, 12, 9), 15, 15), Some((11.0, 8.0)));
        for (x, y) in [(3, 7), (4, 6), (16, 7), (4, 16)] {
            assert_eq!(popup_point((4, 7, 12, 9), x, y), None);
        }
        for r in [
            (-1, 0, 1, 1),
            (0, -1, 1, 1),
            (0, 0, 0, 1),
            (0, 0, 1, 0),
            (0, 0, 17, 1),
            (0, 0, 1, 17),
            (i32::MAX, 0, i32::MAX, 1),
        ] {
            assert!(!popup_inside(r, 16, 16));
        }
    }
}
