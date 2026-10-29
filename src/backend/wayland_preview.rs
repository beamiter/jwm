//! Windowed Wayland Dock previews use retained real client surface trees and
//! the same inward placement as the hardware compositor.
use crate::backend::{
    api::CompositorRect, common_define::WindowId, wayland::state::JwmWaylandState,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::{
    backend::renderer::{
        element::{
            AsRenderElements,
            surface::WaylandSurfaceRenderElement,
            utils::{Relocate, RelocateRenderElement, RescaleRenderElement},
        },
        gles::GlesRenderer,
    },
    desktop::space::SurfaceTree,
    utils::{Logical, Physical, Point},
};

pub(crate) type PreviewElement<R> =
    RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>;
#[derive(Clone, Copy)]
pub(crate) struct Preview {
    pub window: WindowId,
    pub anchor: CompositorRect,
    pub deadline: std::time::Instant,
}

pub(crate) fn render(
    renderer: &mut GlesRenderer,
    state: &JwmWaylandState,
    preview: Preview,
    width: i32,
    height: i32,
    edge: xbar_core::BarEdge,
) -> Option<(Vec<PreviewElement<GlesRenderer>>, WlSurface)> {
    let surface = state.surface_for_window(preview.window)?;
    let bounds = smithay::desktop::utils::bbox_from_surface_tree(
        &surface,
        Point::<i32, Logical>::from((0, 0)),
    );
    let target = crate::backend::compositor_common::genie::preview_rect_at_edge(
        preview.anchor,
        bounds.size.w as f32,
        bounds.size.h as f32,
        CompositorRect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        },
        1.0,
        edge,
    )?;
    let fit = f64::from(target.width) / f64::from(bounds.size.w.max(1));
    let offset = Point::<i32, Physical>::from((
        (f64::from(target.x) - f64::from(bounds.loc.x) * fit).round() as i32,
        (f64::from(target.y) - f64::from(bounds.loc.y) * fit).round() as i32,
    ));
    let tree = SurfaceTree::from_surface(&surface);
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        AsRenderElements::<GlesRenderer>::render_elements(
            &tree,
            renderer,
            (0, 0).into(),
            1.0.into(),
            1.0,
        );
    Some((
        elements
            .into_iter()
            .map(|element| {
                RelocateRenderElement::from_element(
                    RescaleRenderElement::from_element(element, (0, 0).into(), fit),
                    offset,
                    Relocate::Relative,
                )
            })
            .collect(),
        surface,
    ))
}
