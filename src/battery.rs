use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Battery state read from `/sys/class/power_supply`.
///
/// Aggregates the first `type == Battery` entry it finds (capacity + status).
pub struct BatteryManager {
    capacity: Option<u8>,
    charging: bool,
    present: bool,
    last_update: Instant,
    update_interval: Duration,
}

impl BatteryManager {
    pub fn new() -> Self {
        let mut manager = Self {
            capacity: None,
            charging: false,
            present: false,
            last_update: Instant::now(),
            update_interval: Duration::from_secs(5),
        };
        manager.refresh();
        manager
    }

    /// Re-read battery state. Returns true if anything changed.
    pub fn refresh(&mut self) -> bool {
        let prev = (self.capacity, self.charging, self.present);
        let snapshot = read_battery();
        self.capacity = snapshot.capacity;
        self.charging = snapshot.charging;
        self.present = snapshot.present;
        self.last_update = Instant::now();
        prev != (self.capacity, self.charging, self.present)
    }

    /// Refresh only when the cached value is older than the update interval.
    pub fn update_if_needed(&mut self) -> bool {
        if self.last_update.elapsed() >= self.update_interval {
            self.refresh()
        } else {
            false
        }
    }

    pub fn capacity(&self) -> Option<u8> {
        self.capacity
    }

    pub fn is_charging(&self) -> bool {
        self.charging
    }

    pub fn is_present(&self) -> bool {
        self.present
    }
}

impl Default for BatteryManager {
    fn default() -> Self {
        Self::new()
    }
}

struct BatterySnapshot {
    capacity: Option<u8>,
    charging: bool,
    present: bool,
}

fn read_battery() -> BatterySnapshot {
    let base = Path::new("/sys/class/power_supply");
    let Ok(entries) = fs::read_dir(base) else {
        return BatterySnapshot {
            capacity: None,
            charging: false,
            present: false,
        };
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        let kind = fs::read_to_string(dir.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }
        let capacity = fs::read_to_string(dir.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
            .map(|c| c.min(100));
        let status = fs::read_to_string(dir.join("status")).unwrap_or_default();
        let charging = matches!(status.trim(), "Charging" | "Full");
        return BatterySnapshot {
            capacity,
            charging,
            present: true,
        };
    }

    BatterySnapshot {
        capacity: None,
        charging: false,
        present: false,
    }
}
