use std::fs;
use std::time::{Duration, Instant};

/// Power source type detected from sysfs.
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum PowerSource {
    AC,
    Battery,
    Unknown,
}

/// Battery status read from sysfs.
#[derive(Clone, Debug)]
pub struct BatteryStatus {
    capacity: u32,
    source: PowerSource,
}

impl BatteryStatus {
    const CAPACITY_PATH: &'static str = "/sys/class/power_supply/BAT0/capacity";
    const STATUS_PATH: &'static str = "/sys/class/power_supply/BAT0/status";

    /// Read battery status from sysfs. Defaults to AC/100% if paths don't exist.
    pub fn read() -> Self {
        let capacity = fs::read_to_string(Self::CAPACITY_PATH)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(100);

        let source = fs::read_to_string(Self::STATUS_PATH)
            .ok()
            .map(|s| {
                let status = s.trim().to_lowercase();
                match status.as_str() {
                    "discharging" => PowerSource::Battery,
                    "charging" | "full" | "not charging" => PowerSource::AC,
                    _ => PowerSource::Unknown,
                }
            })
            .unwrap_or(PowerSource::AC);

        Self { capacity, source }
    }

    /// Current battery capacity as a percentage (0-100).
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Whether the system is running on battery power.
    pub fn on_battery(&self) -> bool {
        self.source == PowerSource::Battery
    }

    /// Whether the battery level is below the given threshold.
    pub fn is_low(&self, threshold: u32) -> bool {
        self.on_battery() && self.capacity < threshold
    }
}

/// Power profile controlling compositor quality settings.
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum PowerProfile {
    Performance,
    Balanced,
    PowerSaver,
    UltraLowPower,
}

/// Recommended compositor settings for the current power profile.
#[derive(Clone, Debug)]
pub struct PowerRecommendations {
    pub fps_limit: u32,
    pub blur_quality: u32,
    pub enable_shadows: bool,
    pub enable_animations: bool,
    pub enable_blur: bool,
    pub blur_strength: f32,
}

/// Configuration for power saving thresholds and FPS limits.
#[derive(Clone, Debug)]
pub struct PowerSavingConfig {
    pub low_battery_threshold: u32,
    pub critical_battery_threshold: u32,
    pub performance_fps: u32,
    pub balanced_fps: u32,
    pub power_saver_fps: u32,
    pub ultra_low_fps: u32,
}

impl Default for PowerSavingConfig {
    fn default() -> Self {
        Self {
            low_battery_threshold: 20,
            critical_battery_threshold: 10,
            performance_fps: 60,
            balanced_fps: 60,
            power_saver_fps: 30,
            ultra_low_fps: 15,
        }
    }
}

/// Manages power profile selection based on battery status.
pub struct PowerSavingManager {
    config: PowerSavingConfig,
    battery: BatteryStatus,
    current_profile: PowerProfile,
    last_update: Instant,
    update_interval: Duration,
    profile_start: Instant,
}

impl PowerSavingManager {
    /// Create a new power saving manager with the given configuration.
    pub fn new(config: PowerSavingConfig) -> Self {
        let battery = BatteryStatus::read();
        let now = Instant::now();
        let profile = Self::select_profile(&battery, &config);

        Self {
            config,
            battery,
            current_profile: profile,
            last_update: now,
            update_interval: Duration::from_secs(5),
            profile_start: now,
        }
    }

    /// Update battery status and power profile. Returns true if the profile changed.
    pub fn update(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_update) < self.update_interval {
            return false;
        }

        self.last_update = now;
        self.battery = BatteryStatus::read();

        let new_profile = Self::select_profile(&self.battery, &self.config);
        if new_profile != self.current_profile {
            self.current_profile = new_profile;
            self.profile_start = now;
            return true;
        }

        false
    }

    /// Get the current power profile.
    pub fn current_profile(&self) -> PowerProfile {
        self.current_profile
    }

    /// Get compositor recommendations for the current power profile.
    pub fn get_recommendations(&self) -> PowerRecommendations {
        match self.current_profile {
            PowerProfile::Performance => PowerRecommendations {
                fps_limit: self.config.performance_fps,
                blur_quality: 3,
                enable_shadows: true,
                enable_animations: true,
                enable_blur: true,
                blur_strength: 1.0,
            },
            PowerProfile::Balanced => PowerRecommendations {
                fps_limit: self.config.balanced_fps,
                blur_quality: 2,
                enable_shadows: true,
                enable_animations: true,
                enable_blur: true,
                blur_strength: 0.8,
            },
            PowerProfile::PowerSaver => PowerRecommendations {
                fps_limit: self.config.power_saver_fps,
                blur_quality: 1,
                enable_shadows: false,
                enable_animations: true,
                enable_blur: true,
                blur_strength: 0.5,
            },
            PowerProfile::UltraLowPower => PowerRecommendations {
                fps_limit: self.config.ultra_low_fps,
                blur_quality: 0,
                enable_shadows: false,
                enable_animations: false,
                enable_blur: false,
                blur_strength: 0.0,
            },
        }
    }

    /// Get a reference to the current battery status.
    pub fn battery_status(&self) -> &BatteryStatus {
        &self.battery
    }

    /// Force a specific power profile, bypassing automatic selection.
    pub fn force_profile(&mut self, profile: PowerProfile) {
        if self.current_profile != profile {
            self.current_profile = profile;
            self.profile_start = Instant::now();
        }
    }

    /// Select the appropriate power profile based on battery status.
    fn select_profile(battery: &BatteryStatus, config: &PowerSavingConfig) -> PowerProfile {
        if !battery.on_battery() {
            return PowerProfile::Performance;
        }

        let capacity = battery.capacity();
        if capacity >= 50 {
            PowerProfile::Balanced
        } else if capacity >= config.low_battery_threshold {
            PowerProfile::PowerSaver
        } else {
            PowerProfile::UltraLowPower
        }
    }
}
