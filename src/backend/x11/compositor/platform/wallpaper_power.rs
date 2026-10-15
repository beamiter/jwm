//! Read-only, transport-independent DPMS sampling for animated wallpaper.
//!
//! The compositor already owns an Xlib display for both GLX and EGL. Using
//! that connection avoids adding an XCB extension feature or another display.
//! The caller owns the low-frequency timer and must keep polling while paused.

use super::GraphicsPlatform;

impl GraphicsPlatform {
    /// Whether DPMS currently permits wallpaper animation. `None` means the
    /// extension is unavailable, the query failed, or the reply is unknown.
    /// Callers keep the static wallpaper visible, pause animation on `None`,
    /// and retry later so temporary errors cannot permanently prevent wakeup.
    /// Only call while animated wallpaper is enabled, at its polling deadline.
    pub(in crate::backend::x11::compositor) fn wallpaper_power_allows_animation(
        &self,
    ) -> Option<bool> {
        if self.closed || self.xlib_display.is_null() {
            return None;
        }
        // SAFETY: this live display is owned by GraphicsPlatform, and these
        // requests run on the same compositor thread as all its other Xlib
        // operations. All output pointers refer to initialized local values.
        unsafe {
            let mut event_base = 0;
            let mut error_base = 0;
            if x11::dpms::DPMSQueryExtension(self.xlib_display, &mut event_base, &mut error_base)
                == 0
                || x11::dpms::DPMSCapable(self.xlib_display) == 0
            {
                return None;
            }
            let mut power_level = 0;
            let mut enabled = 0;
            if x11::dpms::DPMSInfo(self.xlib_display, &mut power_level, &mut enabled) == 0 {
                return None;
            }
            decode_dpms_power(enabled != 0, power_level)
        }
    }
}

/// DPMS wire levels: On=0, Standby=1, Suspend=2, Off=3. When DPMS is
/// disabled the power level is undefined and must not be used to pause.
fn decode_dpms_power(enabled: bool, power_level: u16) -> Option<bool> {
    if !enabled {
        return Some(true);
    }
    match power_level {
        0 => Some(true),
        1..=3 => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode_dpms_power;

    #[test]
    fn enabled_dpms_respects_all_four_power_levels() {
        assert_eq!(decode_dpms_power(true, 0), Some(true));
        for level in 1..=3 {
            assert_eq!(decode_dpms_power(true, level), Some(false));
        }
    }

    #[test]
    fn disabled_dpms_ignores_undefined_level() {
        for level in [0, 1, 2, 3, 4, u16::MAX] {
            assert_eq!(decode_dpms_power(false, level), Some(true));
        }
    }

    #[test]
    fn unknown_enabled_levels_do_not_authorize_animation() {
        for level in [4, 255, u16::MAX] {
            assert_eq!(decode_dpms_power(true, level), None);
        }
    }

    #[test]
    fn fresh_on_sample_recovers_after_off_or_unknown() {
        let samples = [(true, 3), (true, u16::MAX), (true, 0), (false, 3)];
        let allowed: Vec<_> = samples
            .into_iter()
            .map(|(enabled, level)| decode_dpms_power(enabled, level) == Some(true))
            .collect();
        assert_eq!(allowed, [false, false, true, true]);
    }
}
