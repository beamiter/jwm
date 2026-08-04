# Adopting the 0.5 host-integration layer

Version 0.5 keeps every 0.4 surface. It absorbs the host-integration copies the
0.4 consumer audit left in place: epoll ownership, the EWMH dock property
protocol, X11/wheel pointer translation, Cairo-to-CPU-buffer rendering, and the
per-bar platform-effect switch. No API was removed; existing consumers can
adopt each helper independently.

## Owned epoll loop (`runtime-linux`)

`linux::Epoll` replaces the `epoll_create1`/`epoll_ctl`/`epoll_wait` copies in
the XCB and x11rb bars:

```rust,ignore
use std::os::fd::AsFd as _;
use xbar_core::linux::{AlignedTimer, Epoll};

let mut epoll = Epoll::new()?;
epoll.add(connection_fd, X_TOKEN)?;
epoll.add(timer.as_fd(), TIMER_TOKEN)?;

loop {
    for token in epoll.wait()? {
        // dispatch on token
    }
}
```

Registration is read-interest and token-identified; `wait` blocks, retries
`EINTR`, and yields only ready tokens. Descriptor ownership stays with the
caller, so an owned notifier replaced after reconnect just registers its new
descriptor and lets the closed one drop out of the interest set.

## EWMH dock protocol as data

`DockWindowSpec` describes the complete dock property protocol without a
connection dependency. Frontends intern the returned atom *names* and write
each value with their native `change_property` call:

```rust,ignore
use xbar_core::{BarPlacement, DockPropertyValue, DockWindowSpec};

let placement = BarPlacement { x: 0, y: 0, width, height: bar_height };
let spec = DockWindowSpec::top("xcb_bar", placement);
for property in spec.properties() {
    match property.value {
        DockPropertyValue::Atoms(names) => { /* intern + write ATOM[] */ }
        DockPropertyValue::Cardinals(values) => { /* write CARDINAL[] */ }
        DockPropertyValue::Utf8Text(text) => { /* write UTF8_STRING */ }
    }
}
```

`strut_properties` returns only the `_NET_WM_STRUT_PARTIAL`/`_NET_WM_STRUT`
writes to repeat after each geometry change, and it derives both arrays from
the shared `EwmhStrut` maths, so the four X11 bars can delete their local
`Atoms` structs and strut arithmetic.

## Pointer translation

`PointerAction::from_x11_button` maps the conventional X11 core buttons
(1/3/4/5) and `PointerAction::from_vertical_delta` maps signed wheel/trackpad
deltas. Both already existed in 0.4; 0.5 makes them the expected path — bars
should not keep local `pointer_action(button)` or delta-sign copies.

## Cairo CPU frames

`CairoBar::render_into_bgra` renders directly into a caller-owned
premultiplied `ARgb32` buffer, validating dimensions, stride, buffer length,
and scale before creating the temporary surface:

```rust,ignore
bar.render_into_bgra(pixels.frame_mut(), width, height, width * 4, scale)?;
```

`render::cairo::CpuCanvas` is the owned variant for GPU-upload frontends: it
keeps one buffer with Cairo's preferred stride, reallocates only on resize, and
returns a `CpuFrame { data, width, height, stride }` ready for a wgpu texture
upload or a softbuffer row copy. Together they remove every
`ImageSurface::create_for_data_unsafe`, stride-overflow, and context-scale copy
from the pixels, softbuffer, and wgpu bars.

## One host effect route (`xbar_linux_actions` 0.2)

`EffectRouter` owns the per-update policy every native bar repeated: log
issues, forward geometry effects, launch process effects, and log anything no
adapter handles.

```rust,ignore
use xbar_linux_actions::{EffectRouter, GeometryRequest};

let mut router = EffectRouter::default();
let needs_redraw = router.route(update, |request| match request {
    GeometryRequest::Apply(geometry) => window.apply_geometry(geometry),
    GeometryRequest::Clear => window.restore_default_placement(),
})?;
```

Only the geometry closure can fail the route; process-launch failures are
logged so a missing `flameshot` cannot wedge a frame. The returned flag is the
`needs_redraw` value bars previously computed by hand. Effect handling that is
not the standard Linux policy should keep using
`RuntimeUpdate::handle_platform_effects` directly.

## Adoption map

| Consumer family | 0.5 change |
|---|---|
| `xcb_bar`, `x11rb_bar` | `linux::Epoll`, `DockWindowSpec`, `from_x11_button`, `EffectRouter` |
| `xcb_wgpu_bar`, `x11rb_wgpu_bar` | the above plus `CpuCanvas` for GPU upload |
| `winit_*`, `tao_*` pixels/softbuffer | `render_into_bgra`, `from_vertical_delta`, `EffectRouter` |
| `winit_wgpu_bar`, `tao_wgpu_bar` | `CpuCanvas`, `from_vertical_delta`, `EffectRouter` |
| toolkit bars | unchanged API; rev bump only |
| Tauri bars | unchanged API; `xbar_tauri` already owns this layer |
