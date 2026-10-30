//! Raw backlight receipts shared by the worker and Eva. No percentage tolerance:
//! write and read-back must refer to the same device, range and raw target.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BacklightReading {
    pub device: String,
    pub raw: u32,
    pub max: u32,
}

impl BacklightReading {
    pub(crate) fn percent(&self) -> Option<u8> {
        if self.max == 0 || self.raw > self.max {
            return None;
        }
        Some(((u64::from(self.raw) * 100 + u64::from(self.max) / 2) / u64::from(self.max)) as u8)
    }

    pub(crate) fn target(&self, percent: u8) -> Option<u32> {
        self.percent()?;
        // Match nearest-step rounding, with the same positive minimum for
        // brightnessctl and sysfs. Use u64 for large firmware ranges.
        Some(((u64::from(percent.min(100)) * u64::from(self.max) + 50) / 100).max(1) as u32)
    }
}

/// Parse raw units, never the helper's rounded display-percentage column.
/// A pinned query must return exactly one backlight row.
pub(crate) fn parse_reading(output: &str) -> Option<BacklightReading> {
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let fields: Vec<_> = lines.next()?.split(',').collect();
    if lines.next().is_some() || fields.len() != 5 || fields[1] != "backlight" {
        return None;
    }
    let device = fields[0];
    if device.is_empty() || device.starts_with('-') || device.contains(['/', '*', '?', '[', ']']) {
        return None;
    }
    let reading = BacklightReading {
        device: device.into(),
        raw: fields[2].parse().ok()?,
        max: fields[4].parse().ok()?,
    };
    reading.percent()?;
    Some(reading)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrightnessReceipt {
    pub requested: u8,
    pub before: BacklightReading,
    pub target_raw: u32,
    pub write_ok: bool,
    pub after: Option<BacklightReading>,
}

impl BrightnessReceipt {
    pub(crate) fn verified(&self, requested: u8) -> Option<bool> {
        if !self.write_ok
            || self.requested != requested
            || self.before.target(requested) != Some(self.target_raw)
        {
            return Some(false);
        }
        self.after.as_ref().map(|after| {
            after.device == self.before.device
                && after.max == self.before.max
                && after.percent().is_some()
                && after.raw == self.target_raw
        })
    }

    pub(crate) fn percent(&self) -> Option<u8> {
        self.after.as_ref()?.percent()
    }
}

/// Keep read-back even on write failure for honest diagnostics; it must never
/// turn an unsuccessful write into success just because the old level matches.
pub(crate) fn set_with(
    before: BacklightReading,
    requested: u8,
    write: impl FnOnce(&str, u32) -> bool,
    read: impl FnOnce(&str) -> Option<BacklightReading>,
) -> Option<BrightnessReceipt> {
    let target_raw = before.target(requested)?;
    let write_ok = write(&before.device, target_raw);
    let after = read(&before.device);
    Some(BrightnessReceipt {
        requested,
        before,
        target_raw,
        write_ok,
        after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(raw: u32, max: u32) -> BacklightReading {
        BacklightReading {
            device: "panel".into(),
            raw,
            max,
        }
    }

    #[test]
    fn quantized_targets_cover_low_steps_boundaries_and_both_rounding_directions() {
        for (max, requested, raw, percent) in [
            (15, 50, 8, 53),
            (15, 49, 7, 47),
            (15, 1, 1, 7),
            (15, 0, 1, 7),
            (15, 99, 15, 100),
            (15, 100, 15, 100),
            (1, 50, 1, 100),
            (2, 25, 1, 50),
            (3, 50, 2, 67),
            (1000, 1, 10, 1),
            (1000, 0, 1, 0),
            (u32::MAX, 50, 2147483648, 50),
        ] {
            let receipt = set_with(
                reading(0, max),
                requested,
                |device, target| {
                    assert_eq!(device, "panel");
                    assert_eq!(target, raw);
                    true
                },
                |_| Some(reading(raw, max)),
            )
            .unwrap();
            assert_eq!(receipt.verified(requested), Some(true));
            assert_eq!(receipt.percent(), Some(percent));
        }
    }

    #[test]
    fn wrong_device_range_raw_request_and_failed_write_never_succeed() {
        let good = set_with(reading(0, 15), 50, |_, _| true, |_| Some(reading(8, 15))).unwrap();
        for after in [
            reading(7, 15),
            reading(8, 16),
            reading(16, 15),
            BacklightReading {
                device: "other-panel".into(),
                ..reading(8, 15)
            },
        ] {
            let mut bad = good.clone();
            bad.after = Some(after);
            assert_eq!(bad.verified(50), Some(false));
        }
        assert_eq!(good.verified(49), Some(false));
        let failed = set_with(reading(8, 15), 50, |_, _| false, |_| Some(reading(8, 15))).unwrap();
        assert_eq!(failed.verified(50), Some(false));
        assert_eq!(failed.percent(), Some(53));
        let missing = set_with(reading(8, 15), 50, |_, _| true, |_| None).unwrap();
        assert_eq!(missing.verified(50), None);
        assert!(set_with(reading(0, 0), 50, |_, _| panic!(), |_| panic!()).is_none());
    }

    #[test]
    fn same_display_percentage_is_not_proof_of_the_raw_target() {
        let receipt = set_with(
            reading(0, 1000),
            50,
            |_, _| true,
            |_| Some(reading(501, 1000)),
        )
        .unwrap();
        assert_eq!(receipt.percent(), Some(50));
        assert_eq!(receipt.verified(50), Some(false));
    }

    #[test]
    fn helper_display_rounding_cannot_change_verification() {
        for display in ["53%", "54%", "50%"] {
            let state = parse_reading(&format!("panel,backlight,8,{display},15\n")).unwrap();
            assert_eq!(state.percent(), Some(53));
        }
        for bad in [
            "panel,leds,8,53%,15",
            "panel,backlight,8,53%,0",
            "panel,backlight,16,100%,15",
            "*,backlight,8,53%,15",
            "a,backlight,8,53%,15\nb,backlight,8,53%,15",
        ] {
            assert!(parse_reading(bad).is_none());
        }
    }
}
