use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_POWER_SUPPLY_ATTRIBUTE_BYTES: usize = 4 * 1024;
const MAX_POWER_SUPPLY_ENTRIES: usize = 64;

/// Battery state read from `/sys/class/power_supply`.
///
/// Aggregates all present `type == Battery` entries (capacity + status).
#[derive(Debug)]
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
        self.try_refresh().unwrap_or(false)
    }

    /// Re-read battery state and preserve the last good snapshot on failure.
    pub fn try_refresh(&mut self) -> io::Result<bool> {
        let prev = (self.capacity, self.charging, self.present);
        let snapshot = try_read_battery();
        // Failed probes are rate-limited exactly like successful ones.
        self.last_update = Instant::now();
        let snapshot = snapshot?;
        self.capacity = snapshot.capacity;
        self.charging = snapshot.charging;
        self.present = snapshot.present;
        Ok(prev != (self.capacity, self.charging, self.present))
    }

    /// Refresh only when the cached value is older than the update interval.
    pub fn update_if_needed(&mut self) -> bool {
        self.try_update_if_needed().unwrap_or(false)
    }

    /// Refresh stale state and return a sysfs error to an orchestrator.
    pub fn try_update_if_needed(&mut self) -> io::Result<bool> {
        if self.last_update.elapsed() >= self.update_interval {
            self.try_refresh()
        } else {
            Ok(false)
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

fn try_read_battery() -> io::Result<BatterySnapshot> {
    try_read_battery_from(Path::new("/sys/class/power_supply"))
}

#[cfg(test)]
fn read_battery_from(base: &Path) -> BatterySnapshot {
    try_read_battery_from(base).unwrap_or_default()
}

fn try_read_battery_from(base: &Path) -> io::Result<BatterySnapshot> {
    let entries = fs::read_dir(base)?;
    let mut directories: Vec<_> = entries
        .take(MAX_POWER_SUPPLY_ENTRIES + 1)
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<_>>()?;
    if directories.len() > MAX_POWER_SUPPLY_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "power-supply directory exceeds entry limit",
        ));
    }
    directories.sort();

    let mut capacity_total = 0_u32;
    let mut capacity_count = 0_u32;
    let mut charging = false;
    let mut present = false;

    for dir in directories {
        let kind = read_power_supply_attribute(&dir.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }

        let battery_present = read_power_supply_attribute(&dir.join("present"))
            .map_or(true, |value| value.trim() != "0");
        if !battery_present {
            continue;
        }

        present = true;
        let capacity = read_power_supply_attribute(&dir.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
            .map(|c| c.min(100));
        if let Some(capacity) = capacity {
            capacity_total += u32::from(capacity);
            capacity_count += 1;
        }

        let status = read_power_supply_attribute(&dir.join("status")).unwrap_or_default();
        charging |= matches!(status.trim(), "Charging" | "Full");
    }

    Ok(BatterySnapshot {
        capacity: (capacity_count != 0)
            .then(|| ((capacity_total + capacity_count / 2) / capacity_count) as u8),
        charging,
        present,
    })
}

fn read_power_supply_attribute(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::with_capacity(MAX_POWER_SUPPLY_ATTRIBUTE_BYTES + 1);
    let mut input = fs::File::open(path)?.take((MAX_POWER_SUPPLY_ATTRIBUTE_BYTES + 1) as u64);
    input.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_POWER_SUPPLY_ATTRIBUTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "power-supply attribute exceeds read limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        BatterySnapshot, MAX_POWER_SUPPLY_ATTRIBUTE_BYTES, MAX_POWER_SUPPLY_ENTRIES,
        read_battery_from, read_power_supply_attribute, try_read_battery_from,
    };

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
        assert_eq!(
            try_read_battery_from(&missing).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn oversized_attributes_are_rejected_after_a_bounded_read() {
        let directory = TestDirectory::new();
        let path = directory.path().join("oversized");
        fs::write(&path, vec![b'x'; MAX_POWER_SUPPLY_ATTRIBUTE_BYTES * 16])
            .expect("write oversized attribute");

        assert_eq!(
            read_power_supply_attribute(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn power_supply_directory_has_a_hard_entry_budget() {
        let directory = TestDirectory::new();
        for index in 0..MAX_POWER_SUPPLY_ENTRIES {
            fs::create_dir(directory.path().join(format!("supply-{index:03}")))
                .expect("create power supply entry");
        }

        assert_eq!(
            try_read_battery_from(directory.path()).unwrap(),
            BatterySnapshot::default()
        );

        fs::create_dir(directory.path().join("supply-over-budget"))
            .expect("create excess power supply entry");
        assert_eq!(
            try_read_battery_from(directory.path()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}
