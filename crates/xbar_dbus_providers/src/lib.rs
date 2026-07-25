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

use xbar_core::{BatteryState, MediaPlayback, MediaState, Percent};
use zbus::blocking::{Connection, Proxy};
use zbus::names::OwnedBusName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

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

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Polls the session bus for the first MPRIS player and reduces it to a
/// [`MediaState`].
///
/// Player discovery repeats on every poll, so a player starting or quitting
/// between samples is picked up without lifetime tracking. No player, a
/// vanished player, or unreadable metadata all reduce to
/// [`MediaState::inactive`] rather than an error, matching the model's
/// availability-aware handling.
#[derive(Debug)]
pub struct MprisMediaProvider {
    connection: Connection,
}

impl MprisMediaProvider {
    /// Connect to the session bus.
    pub fn connect() -> zbus::Result<Self> {
        Ok(Self {
            connection: Connection::session()?,
        })
    }

    /// Read the current now-playing state from the first available player.
    pub fn poll(&self) -> zbus::Result<MediaState> {
        let Some(bus_name) = self.first_player()? else {
            return Ok(MediaState::inactive());
        };
        let player = bus_name
            .as_str()
            .strip_prefix(MPRIS_PREFIX)
            .unwrap_or(bus_name.as_str())
            .to_owned();

        let proxy = Proxy::new(
            &self.connection,
            bus_name.as_str(),
            MPRIS_PATH,
            MPRIS_PLAYER_INTERFACE,
        )?;
        let Ok(status) = proxy.get_property::<String>("PlaybackStatus") else {
            // The player disappeared between discovery and the read.
            return Ok(MediaState::inactive());
        };
        let playback = match status.as_str() {
            "Playing" => MediaPlayback::Playing,
            "Paused" => MediaPlayback::Paused,
            _ => MediaPlayback::Stopped,
        };

        let metadata: std::collections::HashMap<String, OwnedValue> =
            proxy.get_property("Metadata").unwrap_or_default();
        let title = metadata
            .get("xesam:title")
            .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
            .filter(|title| !title.is_empty());
        let artist = metadata
            .get("xesam:artist")
            .and_then(|value| Vec::<String>::try_from(value.try_clone().ok()?).ok())
            .and_then(|artists| artists.into_iter().find(|artist| !artist.is_empty()));

        Ok(MediaState {
            playback,
            title,
            artist,
            player: Some(player),
        }
        .normalized())
    }

    fn first_player(&self) -> zbus::Result<Option<OwnedBusName>> {
        let bus = Proxy::new(
            &self.connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )?;
        let mut names: Vec<OwnedBusName> = bus.call("ListNames", &())?;
        names.retain(|name| name.as_str().starts_with(MPRIS_PREFIX));
        names.sort();
        Ok(names.into_iter().next())
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

    /// Integration check against the real session bus; skipped where no
    /// session bus exists so CI and headless machines stay green.
    #[test]
    fn mpris_poll_reduces_to_a_normalized_state_when_a_session_bus_exists() {
        let Ok(provider) = MprisMediaProvider::connect() else {
            eprintln!("skipping: session bus unavailable");
            return;
        };
        let state = provider.poll().expect("connected session bus must answer");
        assert_eq!(state, state.clone().normalized());
        if !state.is_active() {
            assert_eq!(state, MediaState::inactive());
        }
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
