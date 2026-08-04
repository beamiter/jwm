//! sysfs-backed primary-interface network provider.
//!
//! Reads `/sys/class/net` directly: no netlink socket, daemon, or extra
//! dependency. The primary interface is the alphabetically first non-loopback
//! interface whose `operstate` is `up`, which keeps the choice deterministic
//! across polls. Rates need two samples of the same interface; the first
//! sample after startup or an interface change reports unavailable rates
//! rather than a misleading zero.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::model::NetworkState;

#[derive(Debug, Clone)]
struct Sample {
    interface: String,
    rx_bytes: u64,
    tx_bytes: u64,
    at: Instant,
}

/// Polls interface counters and reduces them to a [`NetworkState`].
#[derive(Debug)]
pub struct NetworkMonitor {
    root: PathBuf,
    previous: Option<Sample>,
}

impl Default for NetworkMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkMonitor {
    #[must_use]
    pub fn new() -> Self {
        Self::with_root("/sys/class/net")
    }

    /// Use an alternate sysfs root; tests point this at a fixture tree.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            previous: None,
        }
    }

    /// Sample counters now and derive rates from the previous sample.
    pub fn poll(&mut self) -> io::Result<NetworkState> {
        self.poll_at(Instant::now())
    }

    /// Deterministic sampling entry point for tests and schedulers.
    pub fn poll_at(&mut self, now: Instant) -> io::Result<NetworkState> {
        let Some(interface) = self.primary_interface()? else {
            self.previous = None;
            return Ok(NetworkState::disconnected());
        };

        let statistics = self.root.join(&interface).join("statistics");
        let rx_bytes = read_counter(&statistics.join("rx_bytes"))?;
        let tx_bytes = read_counter(&statistics.join("tx_bytes"))?;

        let (rx_rate, tx_rate) = match &self.previous {
            Some(previous) if previous.interface == interface && now > previous.at => {
                let elapsed = now.duration_since(previous.at).as_secs_f64();
                // Counter resets (interface bounce, driver reload) surface as
                // unavailable for one sample instead of a huge wrapped rate.
                (
                    rate(rx_bytes.checked_sub(previous.rx_bytes), elapsed),
                    rate(tx_bytes.checked_sub(previous.tx_bytes), elapsed),
                )
            }
            _ => (None, None),
        };

        self.previous = Some(Sample {
            interface: interface.clone(),
            rx_bytes,
            tx_bytes,
            at: now,
        });
        Ok(NetworkState::connected(interface, rx_rate, tx_rate))
    }

    fn primary_interface(&self) -> io::Result<Option<String>> {
        let mut interfaces: Vec<String> = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name == "lo" {
                continue;
            }
            let operstate = self.root.join(&name).join("operstate");
            let Ok(state) = std::fs::read_to_string(operstate) else {
                continue;
            };
            if state.trim() == "up" {
                interfaces.push(name);
            }
        }
        interfaces.sort();
        Ok(interfaces.into_iter().next())
    }
}

fn read_counter(path: &Path) -> io::Result<u64> {
    let text = std::fs::read_to_string(path)?;
    text.trim()
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn rate(delta: Option<u64>, elapsed_seconds: f64) -> Option<u64> {
    let delta = delta?;
    if elapsed_seconds <= 0.0 || !elapsed_seconds.is_finite() {
        return None;
    }
    Some((delta as f64 / elapsed_seconds).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("xbar-network-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn interface(&self, name: &str, operstate: &str, rx: u64, tx: u64) {
            let statistics = self.root.join(name).join("statistics");
            std::fs::create_dir_all(&statistics).unwrap();
            std::fs::write(self.root.join(name).join("operstate"), operstate).unwrap();
            std::fs::write(statistics.join("rx_bytes"), format!("{rx}\n")).unwrap();
            std::fs::write(statistics.join("tx_bytes"), format!("{tx}\n")).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn rates_need_two_samples_and_survive_counter_resets() {
        let fixture = Fixture::new("rates");
        fixture.interface("lo", "up", 1, 1);
        fixture.interface("wlan0", "up", 1_000, 500);
        let mut monitor = NetworkMonitor::with_root(&fixture.root);

        let start = Instant::now();
        let first = monitor.poll_at(start).unwrap();
        assert_eq!(first.interface.as_deref(), Some("wlan0"));
        assert!(first.connected);
        assert_eq!(first.rx_bytes_per_second, None, "one sample has no rate");

        fixture.interface("wlan0", "up", 3_000, 500);
        let second = monitor.poll_at(start + Duration::from_secs(2)).unwrap();
        assert_eq!(second.rx_bytes_per_second, Some(1_000));
        assert_eq!(second.tx_bytes_per_second, Some(0));

        // A counter reset must not produce a wrapped rate.
        fixture.interface("wlan0", "up", 100, 600);
        let reset = monitor.poll_at(start + Duration::from_secs(3)).unwrap();
        assert_eq!(reset.rx_bytes_per_second, None);
        assert_eq!(reset.tx_bytes_per_second, Some(100));
    }

    #[test]
    fn primary_interface_is_deterministic_and_skips_loopback_and_down() {
        let fixture = Fixture::new("primary");
        fixture.interface("lo", "up", 0, 0);
        fixture.interface("wlan0", "up", 0, 0);
        fixture.interface("eth0", "down", 0, 0);
        let mut monitor = NetworkMonitor::with_root(&fixture.root);
        assert_eq!(
            monitor.poll().unwrap().interface.as_deref(),
            Some("wlan0"),
            "down interfaces and loopback are skipped"
        );

        fixture.interface("eth0", "up", 0, 0);
        assert_eq!(
            monitor.poll().unwrap().interface.as_deref(),
            Some("eth0"),
            "alphabetical order keeps the choice stable"
        );
    }

    #[test]
    fn missing_root_reports_disconnected_not_an_error() {
        let mut monitor = NetworkMonitor::with_root("/definitely/missing/xbar-net");
        assert_eq!(monitor.poll().unwrap(), NetworkState::disconnected());
    }
}
