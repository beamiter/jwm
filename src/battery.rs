use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Battery state read from `/sys/class/power_supply`.
///
/// Aggregates all present `type == Battery` entries (capacity + status).
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

    /// Set the minimum time between automatic refreshes.
    pub fn set_update_interval(&mut self, interval: Duration) {
        self.update_interval = interval;
    }
}

impl Default for BatteryManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BatterySnapshot {
    capacity: Option<u8>,
    charging: bool,
    present: bool,
}

fn read_battery() -> BatterySnapshot {
    read_battery_from(Path::new("/sys/class/power_supply"))
}

fn read_battery_from(base: &Path) -> BatterySnapshot {
    let Ok(entries) = fs::read_dir(base) else {
        return BatterySnapshot::default();
    };

    let mut directories: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    directories.sort();

    let mut capacity_total = 0_u32;
    let mut capacity_count = 0_u32;
    let mut charging = false;
    let mut present = false;

    for dir in directories {
        let kind = fs::read_to_string(dir.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }

        let battery_present =
            fs::read_to_string(dir.join("present")).map_or(true, |value| value.trim() != "0");
        if !battery_present {
            continue;
        }

        present = true;
        let capacity = fs::read_to_string(dir.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
            .map(|c| c.min(100));
        if let Some(capacity) = capacity {
            capacity_total += u32::from(capacity);
            capacity_count += 1;
        }

        let status = fs::read_to_string(dir.join("status")).unwrap_or_default();
        charging |= matches!(status.trim(), "Charging" | "Full");
    }

    BatterySnapshot {
        capacity: (capacity_count != 0)
            .then(|| ((capacity_total + capacity_count / 2) / capacity_count) as u8),
        charging,
        present,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{BatterySnapshot, read_battery_from};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "xbar-core-battery-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn add_supply(
            &self,
            name: &str,
            kind: &str,
            capacity: Option<&str>,
            status: &str,
            present: Option<&str>,
        ) {
            let path = self.0.join(name);
            fs::create_dir(&path).expect("create power supply");
            fs::write(path.join("type"), kind).expect("write type");
            if let Some(capacity) = capacity {
                fs::write(path.join("capacity"), capacity).expect("write capacity");
            }
            fs::write(path.join("status"), status).expect("write status");
            if let Some(present) = present {
                fs::write(path.join("present"), present).expect("write present");
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn aggregates_all_present_batteries_deterministically() {
        let directory = TestDirectory::new();
        directory.add_supply("BAT9", "Battery", Some("80"), "Discharging", Some("1"));
        directory.add_supply("BAT0", "Battery", Some("20"), "Charging", None);
        directory.add_supply("AC", "Mains", None, "Online", None);

        assert_eq!(
            read_battery_from(directory.path()),
            BatterySnapshot {
                capacity: Some(50),
                charging: true,
                present: true,
            }
        );
    }

    #[test]
    fn ignores_absent_batteries_and_invalid_capacity() {
        let directory = TestDirectory::new();
        directory.add_supply("BAT0", "Battery", Some("99"), "Full", Some("0"));
        directory.add_supply("BAT1", "Battery", Some("invalid"), "Discharging", Some("1"));

        assert_eq!(
            read_battery_from(directory.path()),
            BatterySnapshot {
                capacity: None,
                charging: false,
                present: true,
            }
        );
    }

    #[test]
    fn reports_no_battery_for_missing_directory() {
        let directory = TestDirectory::new();
        let missing = directory.path().join("missing");
        assert_eq!(read_battery_from(&missing), BatterySnapshot::default());
    }
}
