//! Audio system management with anyhow for error handling and a simplified, robust state model.

use alsa::mixer::{Mixer, Selem, SelemChannelId, SelemId};
use anyhow::{Context, Result, anyhow};
use log::{debug, error, info, warn};
use std::time::{Duration, Instant};

const MAX_REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_REFRESH_BACKOFF_SHIFT: u32 = 6;

/// Audio device information.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioDevice {
    pub name: String,
    pub index: usize,
    pub volume: i32,
    pub is_muted: bool,
    pub has_volume_control: bool,
    pub has_switch_control: bool,
    pub description: String,
    pub device_type: AudioDeviceType,
}

/// Types of audio devices.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioDeviceType {
    Master,
    Headphone,
    Speaker,
    Microphone,
    LineIn,
    Other(String),
}

impl AudioDeviceType {
    /// Determines the device type from its name.
    fn from_name(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "master" => Self::Master,
            "headphone" | "headphones" => Self::Headphone,
            "speaker" | "speakers" => Self::Speaker,
            "mic" | "microphone" | "capture" | "internal mic" => Self::Microphone,
            "line" | "line in" | "line-in" => Self::LineIn,
            _ => Self::Other(name.to_string()),
        }
    }

    /// Provides a human-readable description for the device type.
    fn description(&self) -> &str {
        match self {
            Self::Master => "主音量",
            Self::Headphone => "耳机",
            Self::Speaker => "扬声器",
            Self::Microphone => "麦克风",
            Self::LineIn => "线路输入",
            Self::Other(name) => name,
        }
    }
}

/// Audio system manager with a single source of truth for device state.
#[derive(Debug)]
pub struct AudioManager {
    mixer: Option<Mixer>,
    /// The single, authoritative list of audio devices.
    devices: Vec<AudioDevice>,
    last_update: Instant,
    last_refresh_attempt: Instant,
    update_interval: Duration,
    consecutive_refresh_failures: u32,
    last_error_time: Option<Instant>,
    error_count: usize,
    max_error_logs: usize,
}

impl AudioManager {
    /// Creates a new audio manager and performs an initial device scan.
    pub fn new() -> Self {
        let now = Instant::now();

        let mut manager = Self {
            mixer: None,
            devices: Vec::new(),
            last_update: now,
            last_refresh_attempt: now,
            update_interval: Duration::from_millis(500),
            consecutive_refresh_failures: 0,
            last_error_time: None,
            error_count: 0,
            max_error_logs: 10,
        };

        if let Err(e) = manager.refresh_devices() {
            // Using {:?} with anyhow prints the full error chain with context.
            error!("Failed to initialize audio devices: {:?}", e);
        }

        manager
    }

    /// Initializes the ALSA mixer.
    fn initialize_mixer() -> Result<Mixer> {
        let mixer = Mixer::new("default", false).context("Failed to initialize ALSA mixer")?;
        info!("Successfully initialized ALSA mixer");
        Ok(mixer)
    }

    /// Refreshes the list of audio devices from the system.
    pub fn refresh_devices(&mut self) -> Result<()> {
        let refresh_result = self.scan_devices();
        let now = Instant::now();
        self.last_refresh_attempt = now;

        match refresh_result {
            Ok(new_devices) => {
                self.devices = new_devices;
                self.last_update = now;
                self.consecutive_refresh_failures = 0;
                self.last_error_time = None;
                self.error_count = 0;

                debug!("Refreshed {} audio devices", self.devices.len());
                Ok(())
            }
            Err(error) => {
                self.consecutive_refresh_failures =
                    self.consecutive_refresh_failures.saturating_add(1);
                // A failed handle must not permanently prevent recovery. The next scheduled
                // refresh will create a fresh mixer while the last good device cache remains.
                self.mixer = None;
                Err(error)
            }
        }
    }

    fn scan_devices(&mut self) -> Result<Vec<AudioDevice>> {
        if self.mixer.is_none() {
            self.mixer = Some(Self::initialize_mixer()?);
        }

        let mixer = self
            .mixer
            .as_ref()
            .ok_or_else(|| anyhow!("Mixer not available during device refresh"))?;

        mixer
            .handle_events()
            .context("Failed to process ALSA mixer events")?;

        let mut new_devices = Vec::new();

        for selem in mixer.iter().filter_map(Selem::new) {
            let name = selem
                .get_id()
                .get_name()
                .context("Failed to get element name")?
                .to_string();

            let has_playback_volume = selem.has_playback_volume();
            let has_playback_switch = selem.has_playback_switch();

            if !has_playback_volume && !has_playback_switch {
                continue;
            }

            let device_type = AudioDeviceType::from_name(&name);
            let volume = Self::get_element_volume(&selem).unwrap_or(0);
            let is_muted = Self::get_element_mute_status(&selem).unwrap_or(false);

            push_indexed_device(
                &mut new_devices,
                AudioDevice {
                    name,
                    index: 0,
                    volume,
                    is_muted,
                    has_volume_control: has_playback_volume,
                    has_switch_control: has_playback_switch,
                    description: device_type.description().to_string(),
                    device_type,
                },
            );
        }

        Ok(new_devices)
    }

    /// Returns a slice of all available audio devices.
    pub fn get_devices(&self) -> &[AudioDevice] {
        &self.devices
    }

    /// Finds a device by its name.
    pub fn find_device(&self, name: &str) -> Option<&AudioDevice> {
        self.devices.iter().find(|dev| dev.name == name)
    }

    /// Gets a device by its index.
    pub fn get_device_by_index(&self, index: usize) -> Option<&AudioDevice> {
        self.devices.get(index)
    }

    /// Gets the master audio device, falling back to the first available device with volume control.
    pub fn get_master_device(&self) -> Option<&AudioDevice> {
        self.devices
            .iter()
            .find(|dev| matches!(dev.device_type, AudioDeviceType::Master))
            .or_else(|| self.devices.iter().find(|dev| dev.has_volume_control))
    }

    /// Sets the volume and mute state for a given device.
    pub fn set_volume(&mut self, device_name: &str, volume: i32, mute: bool) -> Result<()> {
        let volume = clamp_volume(volume);
        let mixer = self
            .mixer
            .as_ref()
            .ok_or_else(|| anyhow!("No mixer available to set volume"))?;

        let selem = mixer
            .find_selem(&SelemId::new(device_name, 0))
            .ok_or_else(|| anyhow!("Audio device '{}' not found", device_name))?;

        let has_volume = selem.has_playback_volume();
        let has_switch = selem.has_playback_switch();

        if has_volume {
            let (min, max) = selem.get_playback_volume_range();
            let alsa_volume = percentage_to_alsa_volume(volume, min, max);
            selem
                .set_playback_volume_all(alsa_volume)
                .with_context(|| format!("Failed to set volume for '{}'", device_name))?;
        }

        if has_switch {
            let switch_val = if mute { 0 } else { 1 };
            selem
                .set_playback_switch_all(switch_val)
                .with_context(|| format!("Failed to set mute for '{}'", device_name))?;
        }

        // Update the state in our single source of truth.
        update_cached_device(
            &mut self.devices,
            device_name,
            has_volume.then_some(volume),
            has_switch.then_some(mute),
        );

        info!(
            "Set '{}' volume to {}%, muted: {}",
            device_name, volume, mute
        );
        Ok(())
    }

    /// Toggles the mute state of a device.
    pub fn toggle_mute(&mut self, device_name: &str) -> Result<bool> {
        let current_state = self
            .find_device(device_name)
            .cloned() // Clone to avoid mutable/immutable borrow issues.
            .ok_or_else(|| anyhow!("Device '{}' not found for toggling mute", device_name))?;

        if !current_state.has_switch_control {
            return Err(anyhow!("Audio device '{}' has no mute switch", device_name));
        }

        let new_mute_state = !current_state.is_muted;
        self.set_volume(device_name, current_state.volume, new_mute_state)?;

        Ok(new_mute_state)
    }

    /// Adjusts the volume of a device by a given step.
    pub fn adjust_volume(&mut self, device_name: &str, step: i32) -> Result<i32> {
        let current_device = self
            .find_device(device_name)
            .cloned() // Clone to avoid mutable/immutable borrow issues.
            .ok_or_else(|| anyhow!("Device '{}' not found for adjusting volume", device_name))?;

        if !current_device.has_volume_control {
            return Err(anyhow!(
                "Audio device '{}' has no playback volume control",
                device_name
            ));
        }

        let new_volume = adjusted_volume(current_device.volume, step);
        self.set_volume(device_name, new_volume, current_device.is_muted)?;

        Ok(new_volume)
    }

    /// Refreshes devices if the update interval has passed.
    pub fn update_if_needed(&mut self) -> bool {
        match self.try_update_if_needed() {
            Ok(changed) => changed,
            Err(error) => {
                self.handle_error(error);
                false
            }
        }
    }

    /// Refresh stale devices and return an adapter error to an orchestrator.
    pub fn try_update_if_needed(&mut self) -> Result<bool> {
        if !refresh_is_due(
            self.last_refresh_attempt.elapsed(),
            self.update_interval,
            self.consecutive_refresh_failures,
        ) {
            return Ok(false);
        }

        self.refresh_devices().map(|_| true)
    }

    /// Gets the volume of an element as a percentage (0-100).
    fn get_element_volume(selem: &Selem<'_>) -> Result<i32> {
        if !selem.has_playback_volume() {
            return Ok(0);
        }

        let (min, max) = selem.get_playback_volume_range();
        if min == max {
            return Ok(0);
        }

        let channel = first_playback_channel(selem)?;
        let volume = selem
            .get_playback_volume(channel)
            .context("Failed to get playback volume")?;

        Ok(alsa_volume_to_percentage(volume, min, max))
    }

    /// Gets the mute status of an element.
    fn get_element_mute_status(selem: &Selem<'_>) -> Result<bool> {
        if !selem.has_playback_switch() {
            return Ok(false);
        }

        let channel = first_playback_channel(selem)?;
        let switch = selem
            .get_playback_switch(channel)
            .context("Failed to get playback switch state")?;

        Ok(switch == 0)
    }

    /// Handles audio system errors with rate limiting for logging.
    fn handle_error(&mut self, error: anyhow::Error) {
        let now = Instant::now();
        let should_log = if let Some(last_error_time) = self.last_error_time {
            if now.duration_since(last_error_time) < Duration::from_secs(5) {
                self.error_count += 1;
                self.error_count <= self.max_error_logs
            } else {
                self.error_count = 1;
                true
            }
        } else {
            self.error_count = 1;
            true
        };

        self.last_error_time = Some(now);

        if should_log {
            error!("Audio system error: {:?}", error);
        } else if self.error_count == self.max_error_logs + 1 {
            warn!("Audio error rate limit reached, suppressing further errors");
        }
    }

    /// Gets statistics about the current audio devices.
    pub fn get_stats(&self) -> AudioStats {
        AudioStats {
            total_devices: self.devices.len(),
            devices_with_volume: self.devices.iter().filter(|d| d.has_volume_control).count(),
            devices_with_switch: self.devices.iter().filter(|d| d.has_switch_control).count(),
            muted_devices: self.devices.iter().filter(|d| d.is_muted).count(),
            last_update: self.last_update,
        }
    }
}

fn push_indexed_device(devices: &mut Vec<AudioDevice>, mut device: AudioDevice) {
    device.index = devices.len();
    devices.push(device);
}

fn clamp_volume(volume: i32) -> i32 {
    volume.clamp(0, 100)
}

fn adjusted_volume(current: i32, step: i32) -> i32 {
    current.saturating_add(step).clamp(0, 100)
}

fn first_playback_channel(selem: &Selem<'_>) -> Result<SelemChannelId> {
    SelemChannelId::all()
        .iter()
        .copied()
        .find(|channel| selem.has_playback_channel(*channel))
        .ok_or_else(|| anyhow!("Mixer element has no playback channel"))
}

fn update_cached_device(
    devices: &mut [AudioDevice],
    device_name: &str,
    volume: Option<i32>,
    mute: Option<bool>,
) {
    if let Some(device) = devices.iter_mut().find(|device| device.name == device_name) {
        if let Some(volume) = volume {
            device.volume = clamp_volume(volume);
        }
        if let Some(mute) = mute {
            device.is_muted = mute;
        }
    }
}

fn percentage_to_alsa_volume(percentage: i32, min: i64, max: i64) -> i64 {
    if min >= max {
        return min;
    }

    let span = i128::from(max) - i128::from(min);
    let scaled = i128::from(min) + span * i128::from(clamp_volume(percentage)) / 100;
    // `scaled` is a point between two i64 endpoints, so this conversion cannot fail.
    i64::try_from(scaled).unwrap_or(min)
}

fn alsa_volume_to_percentage(volume: i64, min: i64, max: i64) -> i32 {
    if min >= max {
        return 0;
    }

    let volume = volume.clamp(min, max);
    let offset = i128::from(volume) - i128::from(min);
    let span = i128::from(max) - i128::from(min);
    (offset * 100 / span) as i32
}

fn refresh_retry_interval(base_interval: Duration, consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.min(MAX_REFRESH_BACKOFF_SHIFT);
    base_interval
        .saturating_mul(1_u32 << shift)
        .min(MAX_REFRESH_RETRY_INTERVAL)
}

fn refresh_is_due(
    elapsed_since_attempt: Duration,
    base_interval: Duration,
    consecutive_failures: u32,
) -> bool {
    elapsed_since_attempt >= refresh_retry_interval(base_interval, consecutive_failures)
}

impl Default for AudioManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the audio system.
#[derive(Debug, Clone)]
pub struct AudioStats {
    pub total_devices: usize,
    pub devices_with_volume: usize,
    pub devices_with_switch: usize,
    pub muted_devices: usize,
    pub last_update: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_device(name: &str, index: usize, volume: i32) -> AudioDevice {
        AudioDevice {
            name: name.to_string(),
            index,
            volume,
            is_muted: false,
            has_volume_control: true,
            has_switch_control: true,
            description: name.to_string(),
            device_type: AudioDeviceType::Other(name.to_string()),
        }
    }

    #[test]
    fn supported_devices_are_indexed_by_vector_position() {
        let mut devices = Vec::new();

        for (source_index, is_supported) in [true, false, true].into_iter().enumerate() {
            if is_supported {
                push_indexed_device(
                    &mut devices,
                    test_device(&format!("device-{source_index}"), source_index, 50),
                );
            }
        }

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].index, 0);
        assert_eq!(devices[1].index, 1);
    }

    #[test]
    fn cached_volume_is_clamped_to_percentage_range() {
        let mut devices = vec![test_device("Master", 0, 50)];

        update_cached_device(&mut devices, "Master", Some(150), Some(true));
        assert_eq!(devices[0].volume, 100);
        assert!(devices[0].is_muted);

        update_cached_device(&mut devices, "Master", Some(-20), Some(false));
        assert_eq!(devices[0].volume, 0);
        assert!(!devices[0].is_muted);

        update_cached_device(&mut devices, "Master", None, Some(true));
        assert_eq!(devices[0].volume, 0);
        assert!(devices[0].is_muted);
    }

    #[test]
    fn volume_adjustment_handles_the_full_action_range() {
        assert_eq!(adjusted_volume(50, i32::MAX), 100);
        assert_eq!(adjusted_volume(50, i32::MIN), 0);
        assert_eq!(adjusted_volume(90, 20), 100);
        assert_eq!(adjusted_volume(10, -20), 0);
    }

    #[test]
    fn alsa_volume_conversion_clamps_both_directions() {
        assert_eq!(percentage_to_alsa_volume(-1, 10, 110), 10);
        assert_eq!(percentage_to_alsa_volume(50, 10, 110), 60);
        assert_eq!(percentage_to_alsa_volume(101, 10, 110), 110);

        assert_eq!(alsa_volume_to_percentage(0, 10, 110), 0);
        assert_eq!(alsa_volume_to_percentage(60, 10, 110), 50);
        assert_eq!(alsa_volume_to_percentage(120, 10, 110), 100);
        assert_eq!(alsa_volume_to_percentage(10, 10, 10), 0);
    }

    #[test]
    fn refresh_failures_use_bounded_exponential_backoff() {
        let base = Duration::from_millis(500);

        assert_eq!(refresh_retry_interval(base, 0), base);
        assert_eq!(refresh_retry_interval(base, 1), Duration::from_secs(1));
        assert_eq!(refresh_retry_interval(base, 2), Duration::from_secs(2));
        assert_eq!(refresh_retry_interval(base, 5), Duration::from_secs(16));
        assert_eq!(refresh_retry_interval(base, 6), MAX_REFRESH_RETRY_INTERVAL);
        assert_eq!(
            refresh_retry_interval(base, u32::MAX),
            MAX_REFRESH_RETRY_INTERVAL
        );
        assert!(!refresh_is_due(Duration::from_millis(999), base, 1));
        assert!(refresh_is_due(Duration::from_secs(1), base, 1));
    }
}
