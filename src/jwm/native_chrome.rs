//! Native X11 chrome that is visible when the compositor is off.
//!
//! The GPU overlay owns rounded rings, frost and the wallpaper. Without it
//! JWM still has an X11 border pixel and the root window, so those two follow
//! the same theme and picture the compositor would have drawn.

use crate::backend::api::Backend;
use crate::backend::common_define::SchemeType;
use crate::backend::compositor_common::ui_theme;
use crate::backend::error::BackendError;
use crate::config::CONFIG;

/// Push the live `appearance.ui_theme` inks and compositor border colours
/// into the X11 colormap schemes used for native `BorderPixel`.
pub(crate) fn apply_native_color_schemes(backend: &mut dyn Backend) -> Result<(), BackendError> {
    let (focused, unfocused, urgent) = {
        let cfg = CONFIG.load();
        let behavior = cfg.behavior();
        (
            behavior.border_color_focused,
            behavior.border_color_unfocused,
            behavior.attention_color,
        )
    };
    let (norm, sel, urgent_scheme) =
        ui_theme::native_color_schemes(ui_theme::palette(), focused, unfocused, urgent);
    let alloc = backend.color_allocator();
    let _ = alloc.free_all_theme_pixels();
    alloc.set_scheme(SchemeType::Norm, norm);
    alloc.set_scheme(SchemeType::Sel, sel);
    alloc.set_scheme(SchemeType::Urgent, urgent_scheme);
    alloc.allocate_schemes_pixels()
}
