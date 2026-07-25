//! D-Bus providers for `xbar_core` hosts.
//!
//! This companion stays outside the core for the same reason as the process
//! adapter: bus names, object paths, and service quirks are host-platform
//! policy. Providers translate D-Bus state into existing `xbar_core` model
//! values, so adopting one changes a bar's data source without changing its
//! model, wire, or presentation surface.
//!
//! The first provider covers UPower batteries as an alternative to the
//! `provider-battery-sysfs` feature: laptops with vendor charge thresholds or
//! multiple batteries often report a more accurate aggregate through UPower's
//! display device than through raw sysfs.

use xbar_core::{BatteryState, Percent};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

const UPOWER_SERVICE: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_INTERFACE: &str = "org.freedesktop.UPower";
const DEVICE_INTERFACE: &str = "org.freedesktop.UPower.Device";

// org.freedesktop.UPower.Device.State values.
const STATE_CHARGING: u32 = 1;
const STATE_PENDING_CHARGE: u32 = 5;

// org.freedesktop.UPower.Device.Type battery value.
const TYPE_BATTERY: u32 = 2;

/// Polls the UPower display device and reduces it to a [`BatteryState`].
///
/// The display device is UPower's own aggregate over all power sources, so
/// multi-battery weighting stays with the daemon. A missing service, a
/// non-battery display device, or an absent battery all reduce to
/// [`BatteryState::absent`] rather than an error: on desktops this provider
/// simply reports no battery, exactly like the sysfs provider.
#[derive(Debug)]
pub struct UPowerBatteryProvider {
    connection: Connection,
    display_device: OwnedObjectPath,
}

impl UPowerBatteryProvider {
    /// Connect to the system bus and resolve UPower's display device.
    pub fn connect() -> zbus::Result<Self> {
        let connection = Connection::system()?;
        let upower = Proxy::new(&connection, UPOWER_SERVICE, UPOWER_PATH, UPOWER_INTERFACE)?;
        let display_device: OwnedObjectPath = upower.call("GetDisplayDevice", &())?;
        Ok(Self {
            connection,
            display_device,
        })
    }

    /// Read the current aggregate battery state.
    pub fn poll(&self) -> zbus::Result<BatteryState> {
        let device = Proxy::new(
            &self.connection,
            UPOWER_SERVICE,
            &self.display_device,
            DEVICE_INTERFACE,
        )?;

        let device_type: u32 = device.get_property("Type")?;
        let present: bool = device.get_property("IsPresent")?;
        if device_type != TYPE_BATTERY || !present {
            return Ok(BatteryState::absent());
        }

        let percentage: f64 = device.get_property("Percentage")?;
        let state: u32 = device.get_property("State")?;
        Ok(BatteryState {
            percent: percent_from_upower(percentage),
            charging: matches!(state, STATE_CHARGING | STATE_PENDING_CHARGE),
            present: true,
        })
    }
}

/// Clamp UPower's `Percentage` (documented 0..=100 but occasionally slightly
/// above on calibrating batteries) into a checked [`Percent`]. Non-finite
/// readings become unavailable instead of a healthy-looking value.
fn percent_from_upower(percentage: f64) -> Option<Percent> {
    if !percentage.is_finite() {
        return None;
    }
    Percent::new(percentage.clamp(0.0, 100.0)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upower_percentages_clamp_and_reject_non_finite_readings() {
        assert_eq!(percent_from_upower(55.4).map(Percent::rounded), Some(55));
        assert_eq!(percent_from_upower(101.2).map(Percent::rounded), Some(100));
        assert_eq!(percent_from_upower(-3.0).map(Percent::rounded), Some(0));
        assert_eq!(percent_from_upower(f64::NAN), None);
        assert_eq!(percent_from_upower(f64::INFINITY), None);
    }

    /// Integration check against the real system bus; skipped where UPower is
    /// not running so CI and headless machines stay green.
    #[test]
    fn display_device_poll_reduces_to_a_valid_state_when_upower_exists() {
        let Ok(provider) = UPowerBatteryProvider::connect() else {
            eprintln!("skipping: UPower/system bus unavailable");
            return;
        };
        let state = provider.poll().expect("connected UPower must answer");
        if let Some(percent) = state.percent {
            assert!(percent.rounded() <= 100);
        } else {
            assert!(!state.present || !state.charging);
        }
    }
}
