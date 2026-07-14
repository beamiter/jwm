#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}\n--- needle ---\n{old}")
    file.write_text(text.replace(old, new, 1))


def replace_count(path: str, old: str, new: str, expected: int) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != expected:
        raise SystemExit(f"{path}: expected {expected} matches, found {count}: {old!r}")
    file.write_text(text.replace(old, new))


# ---------------------------------------------------------------------------
# Refresh-rate units: OutputInfo stores millihertz; compositor policies use Hz.
# ---------------------------------------------------------------------------
replace_once(
    "src/backend/x11/wm/mod.rs",
    "pub const DEFAULT_OUTPUT_REFRESH_MHZ: u32 = 60_000;\n",
    """pub const DEFAULT_OUTPUT_REFRESH_MHZ: u32 = 60_000;

/// Convert the backend-facing millihertz representation to the nearest whole Hz.
///
/// X11 output enumeration preserves fractional rates such as 120.081 Hz as
/// 120_081 mHz. Compositor policy (blur tiers, frame pacing) intentionally uses
/// whole Hz and must never consume the raw millihertz value directly.
pub fn refresh_millihz_to_hz(refresh_millihz: u32) -> u32 {
    if refresh_millihz == 0 {
        return 0;
    }
    ((refresh_millihz as u64 + 500) / 1000)
        .clamp(1, u32::MAX as u64) as u32
}

/// Calculate a rounded whole-Hz refresh rate from a RandR mode.
pub fn mode_refresh_hz(dot_clock: u32, htotal: u16, vtotal: u16) -> u32 {
    if dot_clock == 0 || htotal == 0 || vtotal == 0 {
        return 60;
    }
    let denominator = htotal as u64 * vtotal as u64;
    let refresh_millihz = ((dot_clock as u64 * 1000 + denominator / 2) / denominator)
        .min(u32::MAX as u64) as u32;
    refresh_millihz_to_hz(refresh_millihz).max(1)
}
""",
)

replace_once(
    "src/backend/x11/wm/mod.rs",
    """    use super::{
        decode_text_property, parse_icon_data, parse_normal_hints, parse_strut, parse_wm_class,
    };
""",
    """    use super::{
        decode_text_property, mode_refresh_hz, parse_icon_data, parse_normal_hints, parse_strut,
        parse_wm_class, refresh_millihz_to_hz,
    };
""",
)

replace_once(
    "src/backend/x11/wm/mod.rs",
    """    #[test]
    fn parses_wm_class_to_lowercase_parts() {
""",
    """    #[test]
    fn refresh_units_are_rounded_for_compositor_policy() {
        assert_eq!(refresh_millihz_to_hz(0), 0);
        assert_eq!(refresh_millihz_to_hz(59_940), 60);
        assert_eq!(refresh_millihz_to_hz(120_081), 120);
        assert_eq!(mode_refresh_hz(497_500_000, 2720, 1525), 120);
        assert_eq!(mode_refresh_hz(0, 0, 0), 60);
    }

    #[test]
    fn parses_wm_class_to_lowercase_parts() {
""",
)

replace_once(
    "src/backend/xcb/backend.rs",
    """    parse_wm_class, parse_wm_hints, property_kind_from_atom, protocol_supported,
    restack_window_changes, stack_mode_from_index, stack_mode_to_index,
""",
    """    parse_wm_class, parse_wm_hints, property_kind_from_atom, protocol_supported,
    refresh_millihz_to_hz, restack_window_changes, stack_mode_from_index, stack_mode_to_index,
""",
)

replace_once(
    "src/backend/xcb/backend.rs",
    """        let primary_refresh_hz = output_ops
            .enumerate_outputs()
            .into_iter()
            .find_map(|o| (o.refresh_rate > 0).then_some(o.refresh_rate))
            .unwrap_or(60);
        log::info!(
            "xcb backend: primary monitor refresh rate: {}Hz",
            primary_refresh_hz
        );
""",
    """        let primary_refresh_millihz = output_ops
            .enumerate_outputs()
            .into_iter()
            .find_map(|o| (o.refresh_rate > 0).then_some(o.refresh_rate))
            .unwrap_or(DEFAULT_OUTPUT_REFRESH_MHZ);
        let primary_refresh_hz = refresh_millihz_to_hz(primary_refresh_millihz).max(1);
        log::info!(
            "xcb backend: primary monitor refresh rate: {:.3}Hz ({}Hz compositor policy)",
            primary_refresh_millihz as f64 / 1000.0,
            primary_refresh_hz
        );
""",
)

replace_once(
    "src/backend/x11rb/backend.rs",
    "use crate::backend::x11::wm::SUPPORTED_EWMH_FEATURES;\n",
    """use crate::backend::x11::wm::{
    DEFAULT_OUTPUT_REFRESH_MHZ, SUPPORTED_EWMH_FEATURES, refresh_millihz_to_hz,
};
""",
)

replace_once(
    "src/backend/x11rb/backend.rs",
    """        let primary_refresh_hz = outputs
            .iter()
            .find_map(|o| {
                if o.refresh_rate > 0 {
                    Some(o.refresh_rate)
                } else {
                    None
                }
            })
            .unwrap_or(60);
        log::info!(
            "backend: primary monitor refresh rate: {}Hz",
            primary_refresh_hz
        );
""",
    """        let primary_refresh_millihz = outputs
            .iter()
            .find_map(|o| (o.refresh_rate > 0).then_some(o.refresh_rate))
            .unwrap_or(DEFAULT_OUTPUT_REFRESH_MHZ);
        let primary_refresh_hz = refresh_millihz_to_hz(primary_refresh_millihz).max(1);
        log::info!(
            "x11rb backend: primary monitor refresh rate: {:.3}Hz ({}Hz compositor policy)",
            primary_refresh_millihz as f64 / 1000.0,
            primary_refresh_hz
        );
""",
)

# Share identical RandR mode-rate math between xcb and x11rb.
replace_once(
    "src/backend/xcb/compositor_protocol.rs",
    "use crate::backend::error::BackendError;\n",
    """use crate::backend::error::BackendError;
use crate::backend::x11::wm::mode_refresh_hz;
""",
)
replace_once(
    "src/backend/xcb/compositor_protocol.rs",
    """        fn calc_refresh_hz(dot_clock: u32, htotal: u16, vtotal: u16) -> u32 {
            if htotal == 0 || vtotal == 0 {
                return 60;
            }
            ((dot_clock as u64 * 1000) / (htotal as u64 * vtotal as u64) / 1000) as u32
        }

""",
    "",
)
replace_count(
    "src/backend/xcb/compositor_protocol.rs",
    "calc_refresh_hz(m.dot_clock, m.htotal, m.vtotal)",
    "mode_refresh_hz(m.dot_clock, m.htotal, m.vtotal)",
    2,
)

replace_once(
    "src/backend/x11rb/shared_x11_adapters.rs",
    """use crate::backend::x11::compositor_common::{
    X11BootstrapOps, X11CompositeRedirectOps, X11ConnectionOps, X11PresentOps, X11RandrOps,
    X11TextureSourceOps, X11WindowResourceOps,
};
""",
    """use crate::backend::x11::compositor_common::{
    X11BootstrapOps, X11CompositeRedirectOps, X11ConnectionOps, X11PresentOps, X11RandrOps,
    X11TextureSourceOps, X11WindowResourceOps,
};
use crate::backend::x11::wm::mode_refresh_hz;
""",
)
replace_once(
    "src/backend/x11rb/shared_x11_adapters.rs",
    """    fn calc_refresh_mhz(mode: &x11rb::protocol::randr::ModeInfo) -> u32 {
        if mode.htotal == 0 || mode.vtotal == 0 {
            return 60000;
        }
        let dot_clock = mode.dot_clock as u64;
        let htotal = mode.htotal as u64;
        let vtotal = mode.vtotal as u64;
        ((dot_clock * 1000) / (htotal * vtotal)) as u32
    }

""",
    "",
)
replace_count(
    "src/backend/x11rb/shared_x11_adapters.rs",
    ".map(calc_refresh_mhz)",
    ".map(|mode| mode_refresh_hz(mode.dot_clock, mode.htotal, mode.vtotal))",
    2,
)
replace_count(
    "src/backend/x11rb/shared_x11_adapters.rs",
    ".unwrap_or(60000)",
    ".unwrap_or(60)",
    2,
)
replace_count(
    "src/backend/x11rb/shared_x11_adapters.rs",
    "refresh / 1000",
    "refresh",
    2,
)

# A single-monitor setup has an authoritative rate from OutputOps. Use it when
# the lower-level monitor query falls back to 60 Hz.
replace_once(
    "src/backend/x11/compositor/init.rs",
    """        // P5B Phase 2: Build monitor refresh rates from RandR
        let monitor_refresh_rates = Self::build_monitor_refresh_rates(&conn, root);

        // P5B: Log detected monitor configuration
""",
    """        // P5B Phase 2: Build monitor refresh rates from RandR.
        let mut monitor_refresh_rates = Self::build_monitor_refresh_rates(&conn, root);
        if let [single_monitor] = monitor_rects.as_slice() {
            let monitor_id = single_monitor.0;
            let queried_hz = monitor_refresh_rates.get(&monitor_id).copied();
            if queried_hz != Some(primary_refresh_hz) {
                log::debug!(
                    "compositor: replacing single-monitor RandR refresh {:?}Hz with primary {}Hz",
                    queried_hz,
                    primary_refresh_hz
                );
                monitor_refresh_rates.insert(monitor_id, primary_refresh_hz);
            }
        }

        // P5B: Log detected monitor configuration
""",
)

# ---------------------------------------------------------------------------
# EGL partial redraw: choose a preservable config, explicitly preserve the back
# buffer, and use KHR/EXT swap-with-damage when exposed by the driver.
# ---------------------------------------------------------------------------
replace_once(
    "src/backend/x11/compositor/platform.rs",
    """type EglDestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;
type GlEglImageTargetTexture2dOes = unsafe extern "system" fn(u32, *const c_void);
""",
    """type EglDestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;
type EglSwapBuffersWithDamage =
    unsafe extern "C" fn(EglDisplay, EglSurface, *const EglInt, EglInt) -> EglBoolean;
type GlEglImageTargetTexture2dOes = unsafe extern "system" fn(u32, *const c_void);
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """const EGL_IMAGE_PRESERVED_KHR: EglInt = 0x30D2;
const EGL_CORE_NATIVE_ENGINE: EglInt = 0x305B;
""",
    """const EGL_IMAGE_PRESERVED_KHR: EglInt = 0x30D2;
const EGL_CORE_NATIVE_ENGINE: EglInt = 0x305B;
const EGL_SWAP_BEHAVIOR: EglInt = 0x3093;
const EGL_BUFFER_PRESERVED: EglInt = 0x3094;
const EGL_SWAP_BEHAVIOR_PRESERVED_BIT: EglInt = 0x0400;
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """    fn eglSwapBuffers(display: EglDisplay, surface: EglSurface) -> EglBoolean;
    fn eglSwapInterval(display: EglDisplay, interval: EglInt) -> EglBoolean;
""",
    """    fn eglSwapBuffers(display: EglDisplay, surface: EglSurface) -> EglBoolean;
    fn eglSwapInterval(display: EglDisplay, interval: EglInt) -> EglBoolean;
    fn eglSurfaceAttrib(
        display: EglDisplay,
        surface: EglSurface,
        attribute: EglInt,
        value: EglInt,
    ) -> EglBoolean;
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """    image_target_texture: GlEglImageTargetTexture2dOes,
    gles_library: *mut c_void,
    output_is_10bit: bool,
""",
    """    image_target_texture: GlEglImageTargetTexture2dOes,
    swap_buffers_with_damage: Option<EglSwapBuffersWithDamage>,
    buffer_preserved: bool,
    gles_library: *mut c_void,
    output_is_10bit: bool,
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """            let has_image_base =
                extensions.contains("EGL_KHR_image_base") || extensions.contains("EGL_KHR_image");
            if !has_image_base || !extensions.contains("EGL_KHR_image_pixmap") {
                return Err(format!(
                    "EGL image-pixmap import unavailable (extensions: {extensions})"
                ));
            }

            let (config, output_is_10bit) =
""",
    """            let has_image_base =
                extensions.contains("EGL_KHR_image_base") || extensions.contains("EGL_KHR_image");
            if !has_image_base || !extensions.contains("EGL_KHR_image_pixmap") {
                return Err(format!(
                    "EGL image-pixmap import unavailable (extensions: {extensions})"
                ));
            }
            let swap_buffers_with_damage: Option<EglSwapBuffersWithDamage> =
                if extensions.contains("EGL_KHR_swap_buffers_with_damage") {
                    let proc = egl_proc_any(&["eglSwapBuffersWithDamageKHR"]);
                    (!proc.is_null()).then(|| unsafe {
                        std::mem::transmute::<*const c_void, EglSwapBuffersWithDamage>(proc)
                    })
                } else if extensions.contains("EGL_EXT_swap_buffers_with_damage") {
                    let proc = egl_proc_any(&["eglSwapBuffersWithDamageEXT"]);
                    (!proc.is_null()).then(|| unsafe {
                        std::mem::transmute::<*const c_void, EglSwapBuffersWithDamage>(proc)
                    })
                } else {
                    None
                };

            let (config, output_is_10bit) =
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """            if unsafe { eglSwapInterval(display, 1) } == EGL_FALSE {
                log::warn!(
                    "compositor: eglSwapInterval(1) failed: {}",
                    egl_error("EGL")
                );
            }

            let create_image_ptr = egl_proc_any(&["eglCreateImageKHR"]);
""",
    """            let mut surface_type = 0;
            let supports_preserved_swap = unsafe {
                eglGetConfigAttrib(display, config, EGL_SURFACE_TYPE, &mut surface_type)
            } != EGL_FALSE
                && surface_type & EGL_SWAP_BEHAVIOR_PRESERVED_BIT != 0;
            let buffer_preserved = if supports_preserved_swap {
                let preserved = unsafe {
                    eglSurfaceAttrib(
                        display,
                        surface,
                        EGL_SWAP_BEHAVIOR,
                        EGL_BUFFER_PRESERVED,
                    )
                } != EGL_FALSE;
                if !preserved {
                    log::debug!(
                        "compositor: EGL preserved swap request failed: {}",
                        egl_error("eglSurfaceAttrib(EGL_SWAP_BEHAVIOR)")
                    );
                }
                preserved
            } else {
                false
            };
            log::info!(
                "compositor: EGL partial redraw preserved_back_buffer={} swap_with_damage={}",
                buffer_preserved,
                swap_buffers_with_damage.is_some()
            );

            if unsafe { eglSwapInterval(display, 1) } == EGL_FALSE {
                log::warn!(
                    "compositor: eglSwapInterval(1) failed: {}",
                    egl_error("EGL")
                );
            }

            let create_image_ptr = egl_proc_any(&["eglCreateImageKHR"]);
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """                image_target_texture: unsafe { std::mem::transmute(image_target_ptr) },
                gles_library,
                output_is_10bit,
""",
    """                image_target_texture: unsafe { std::mem::transmute(image_target_ptr) },
                swap_buffers_with_damage,
                buffer_preserved,
                gles_library,
                output_is_10bit,
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """    fn swap_buffers(&self) -> Result<(), String> {
        if unsafe { eglSwapBuffers(self.display, self.surface) } == EGL_FALSE {
            Err(egl_error("eglSwapBuffers"))
        } else {
            Ok(())
        }
    }
""",
    """    fn swap_buffers(&self, damage: Option<(i32, i32, i32, i32)>) -> Result<(), String> {
        if let (Some(swap_with_damage), Some((x, y, width, height))) =
            (self.swap_buffers_with_damage, damage)
        {
            if width > 0 && height > 0 {
                let rect = [x, y, width, height];
                if unsafe {
                    swap_with_damage(self.display, self.surface, rect.as_ptr(), 1)
                } != EGL_FALSE
                {
                    return Ok(());
                }
                // A driver may advertise the extension but reject a particular
                // surface. Fall back to the core swap for correctness.
                log::debug!(
                    "compositor: EGL swap-with-damage failed; using full swap: {}",
                    egl_error("eglSwapBuffersWithDamage")
                );
            }
        }
        if unsafe { eglSwapBuffers(self.display, self.surface) } == EGL_FALSE {
            Err(egl_error("eglSwapBuffers"))
        } else {
            Ok(())
        }
    }
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """    pub(super) fn is_gles(&self) -> bool {
        matches!(self.backend, PlatformBackend::Egl(_))
    }

    pub(super) fn make_current(&self) -> Result<(), String> {
""",
    """    pub(super) fn is_gles(&self) -> bool {
        matches!(self.backend, PlatformBackend::Egl(_))
    }

    /// Partial redraw is safe only when the post-swap back buffer is retained.
    pub(super) fn supports_partial_redraw(&self) -> bool {
        match &self.backend {
            // Preserve the established GLX behavior. EGL explicitly verifies it.
            PlatformBackend::Glx(_) => true,
            PlatformBackend::Egl(egl) => egl.buffer_preserved,
        }
    }

    pub(super) fn make_current(&self) -> Result<(), String> {
""",
)

replace_once(
    "src/backend/x11/compositor/platform.rs",
    """    pub(super) fn swap_buffers(&self) -> Result<(), String> {
        match &self.backend {
            PlatformBackend::Glx(glx) => glx.swap_buffers(self.xlib_display),
            PlatformBackend::Egl(egl) => egl.swap_buffers(),
        }
    }
""",
    """    pub(super) fn swap_buffers(
        &self,
        damage: Option<(i32, i32, i32, i32)>,
    ) -> Result<(), String> {
        match &self.backend {
            PlatformBackend::Glx(glx) => glx.swap_buffers(self.xlib_display),
            PlatformBackend::Egl(egl) => egl.swap_buffers(damage),
        }
    }
""",
)

# Prefer an EGLConfig that supports retained back buffers when multiple configs
# expose the same X visual.
replace_once(
    "src/backend/x11/compositor/platform.rs",
    """    configs.into_iter().take(count as usize).find(|&config| {
        let mut native_visual = 0;
        unsafe {
            eglGetConfigAttrib(display, config, EGL_NATIVE_VISUAL_ID, &mut native_visual)
                != EGL_FALSE
                && native_visual as u32 == visual_id
        }
    })
""",
    """    let mut fallback = None;
    for config in configs.into_iter().take(count as usize) {
        let mut native_visual = 0;
        let visual_matches = unsafe {
            eglGetConfigAttrib(display, config, EGL_NATIVE_VISUAL_ID, &mut native_visual)
        } != EGL_FALSE
            && native_visual as u32 == visual_id;
        if !visual_matches {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(config);
        }
        let mut surface_type = 0;
        let preserves_back_buffer = unsafe {
            eglGetConfigAttrib(display, config, EGL_SURFACE_TYPE, &mut surface_type)
        } != EGL_FALSE
            && surface_type & EGL_SWAP_BEHAVIOR_PRESERVED_BIT != 0;
        if preserves_back_buffer {
            return Some(config);
        }
    }
    fallback
""",
)

# Make the renderer honor the platform's preservation guarantee and pass the
# same bottom-left damage rectangle to EGL's swap-with-damage extension.
replace_once(
    "src/backend/x11/compositor/render.rs",
    """        let use_scissor = self.partial_damage_enabled && dirty_rect.is_some() && !force_render;
""",
    """        let use_scissor = self.partial_damage_enabled
            && self.graphics.supports_partial_redraw()
            && dirty_rect.is_some()
            && !force_render;
""",
)

replace_once(
    "src/backend/x11/compositor/render.rs",
    """        // Swap the selected platform surface. OML remains a GLX-only pacing
        // optimization; EGL/GLES uses eglSwapInterval(1) or X Present pacing.
        let swap_result = match self.vsync_method {
""",
    """        // Swap the selected platform surface. The scissor rectangle already
        // uses EGL's bottom-left coordinate convention, so it can be forwarded
        // directly to KHR/EXT_swap_buffers_with_damage.
        let swap_damage = use_scissor.then_some(damage_scissor);
        // OML remains a GLX-only pacing optimization; EGL/GLES uses
        // eglSwapInterval(1) or X Present pacing.
        let swap_result = match self.vsync_method {
""",
)
replace_count(
    "src/backend/x11/compositor/render.rs",
    "self.graphics.swap_buffers()",
    "self.graphics.swap_buffers(swap_damage)",
    2,
)

# Avoid creating Damage/Pixmap resources for stale or input-only windows, and
# keep expected BadMatch failures out of normal warning logs.
replace_once(
    "src/backend/x11/compositor/tfp.rs",
    """        log::info!(
            "compositor: add_window START 0x{:x} {}x{} at ({},{})",
""",
    """        log::debug!(
            "compositor: add_window START 0x{:x} {}x{} at ({},{})",
""",
)

replace_once(
    "src/backend/x11/compositor/tfp.rs",
    """        let damage_id = match self.conn.generate_xid() {
""",
    """        let visual = match self.conn.get_window_visual(x11_win) {
            Ok(visual) => visual,
            Err(error) => {
                log::debug!(
                    "compositor: skipping stale window 0x{x11_win:x}; attributes unavailable: {error}"
                );
                return;
            }
        };
        let depth = match self.conn.get_window_depth(x11_win) {
            Ok(depth) => depth,
            Err(error) => {
                log::debug!(
                    "compositor: skipping stale window 0x{x11_win:x}; geometry unavailable: {error}"
                );
                return;
            }
        };
        if depth == 0 {
            log::debug!("compositor: skipping input-only window 0x{x11_win:x}");
            return;
        }

        let damage_id = match self.conn.generate_xid() {
""",
)

replace_once(
    "src/backend/x11/compositor/tfp.rs",
    """        if let Err(error) = self.conn.name_window_pixmap(x11_win, pixmap) {
            log::warn!("compositor: name_window_pixmap failed for 0x{x11_win:x}: {error}");
            let _ = self.conn.destroy_window_damage(damage_id);
            return;
        }
        let _ = self.conn.flush_x11();

        let visual = self.conn.get_window_visual(x11_win).unwrap_or(0);
        let depth = self.conn.get_window_depth(x11_win).unwrap_or(24);
""",
    """        if let Err(error) = self.conn.name_window_pixmap(x11_win, pixmap) {
            if error.contains("Match") || error.contains("BadMatch") {
                log::debug!(
                    "compositor: window 0x{x11_win:x} is not redirectable yet; skipping pixmap: {error}"
                );
            } else {
                log::warn!("compositor: name_window_pixmap failed for 0x{x11_win:x}: {error}");
            }
            let _ = self.conn.destroy_window_damage(damage_id);
            return;
        }
        let _ = self.conn.flush_x11();

""",
)

print("applied X11 EGL optimization patch")
