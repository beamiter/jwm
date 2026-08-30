//! sysfs-backed primary-interface network provider.
//!
//! Reads `/sys/class/net` directly and consults the bounded IPv4 routing table
//! in `/proc/net/route`: no netlink socket, daemon, or extra dependency. A
//! non-loopback interface carrying the lowest-metric default route is preferred,
//! including virtual devices whose drivers report an `unknown` operational state.
//! When the route table is unavailable or has no eligible default, genuinely
//! `up` interfaces precede `unknown` ones and names keep the fallback stable.
//! Rates need two samples of the same interface; the first sample after startup
//! or an interface change reports unavailable rates rather than a misleading zero.

use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::model::NetworkState;

const DEFAULT_ROUTE_PATH: &str = "/proc/net/route";
const MAX_ROUTE_TABLE_BYTES: u64 = 256 * 1024;
const ROUTE_FLAG_UP: u32 = 0x0001;
const ROUTE_FLAG_REJECT: u32 = 0x0200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DefaultRoute<'a> {
    interface: &'a str,
    metric: u32,
}

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
    route_path: Option<PathBuf>,
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
        Self::with_paths("/sys/class/net", DEFAULT_ROUTE_PATH)
    }

    /// Use an alternate sysfs root; tests point this at a fixture tree. The
    /// host route table is deliberately not consulted for an alternate tree,
    /// keeping fixtures and network-namespace snapshots hermetic.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            route_path: None,
            previous: None,
        }
    }

    fn with_paths(root: impl Into<PathBuf>, route_path: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            route_path: Some(route_path.into()),
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
        let mut up_interfaces: Vec<String> = Vec::new();
        let mut unknown_interfaces: Vec<String> = Vec::new();
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
            match state.trim() {
                "up" => up_interfaces.push(name),
                // Virtual interfaces such as WireGuard and TUN commonly have
                // no carrier provider and remain IF_OPER_UNKNOWN while fully
                // usable. A default route is stronger evidence than that
                // missing driver signal.
                "unknown" => unknown_interfaces.push(name),
                _ => {}
            }
        }
        up_interfaces.sort();
        unknown_interfaces.sort();
        let fallback_interface = up_interfaces
            .first()
            .or_else(|| unknown_interfaces.first())
            .cloned();
        let mut interfaces = up_interfaces;
        interfaces.append(&mut unknown_interfaces);
        interfaces.sort();
        if interfaces.is_empty() {
            return Ok(None);
        }

        let default_interface = self.route_path.as_deref().and_then(|route_path| {
            read_route_table(route_path)
                .ok()
                .flatten()
                .and_then(|table| preferred_default_route(&table, &interfaces).map(str::to_owned))
        });
        Ok(default_interface.or(fallback_interface))
    }
}

/// Read no more than one byte beyond the accepted route-table size. An
/// oversized table is treated like an unavailable one so a status poll never
/// allocates according to unbounded kernel or fixture output.
fn read_route_table(path: &Path) -> io::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(4096);
    file.take(MAX_ROUTE_TABLE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ROUTE_TABLE_BYTES {
        return Ok(None);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Parse usable IPv4 default routes from Linux's `/proc/net/route` format.
/// Destination, gateway, flags, and mask are hexadecimal; metric is decimal.
fn parse_default_routes(route_table: &str) -> impl Iterator<Item = DefaultRoute<'_>> {
    route_table.lines().filter_map(parse_default_route)
}

fn parse_default_route(line: &str) -> Option<DefaultRoute<'_>> {
    let mut fields = line.split_ascii_whitespace();
    let interface = fields.next()?;
    let destination = u32::from_str_radix(fields.next()?, 16).ok()?;
    let _gateway = u32::from_str_radix(fields.next()?, 16).ok()?;
    let flags = u32::from_str_radix(fields.next()?, 16).ok()?;
    fields.next()?; // RefCnt
    fields.next()?; // Use
    let metric = fields.next()?.parse::<u32>().ok()?;
    let mask = u32::from_str_radix(fields.next()?, 16).ok()?;

    (destination == 0 && mask == 0 && flags & ROUTE_FLAG_UP != 0 && flags & ROUTE_FLAG_REJECT == 0)
        .then_some(DefaultRoute { interface, metric })
}

fn preferred_default_route<'a>(
    route_table: &'a str,
    eligible_interfaces: &[String],
) -> Option<&'a str> {
    parse_default_routes(route_table)
        .filter(|route| {
            eligible_interfaces
                .binary_search_by(|interface| interface.as_str().cmp(route.interface))
                .is_ok()
        })
        .min_by(|left, right| {
            left.metric
                .cmp(&right.metric)
                .then_with(|| left.interface.cmp(right.interface))
        })
        .map(|route| route.interface)
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
        route_path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("xbar-network-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let route_path = root.join("proc-net-route");
            Self { root, route_path }
        }

        fn interface(&self, name: &str, operstate: &str, rx: u64, tx: u64) {
            let statistics = self.root.join(name).join("statistics");
            std::fs::create_dir_all(&statistics).unwrap();
            std::fs::write(self.root.join(name).join("operstate"), operstate).unwrap();
            std::fs::write(statistics.join("rx_bytes"), format!("{rx}\n")).unwrap();
            std::fs::write(statistics.join("tx_bytes"), format!("{tx}\n")).unwrap();
        }

        fn route_table(&self, contents: &str) {
            std::fs::write(&self.route_path, contents).unwrap();
        }

        fn monitor(&self) -> NetworkMonitor {
            NetworkMonitor::with_paths(&self.root, &self.route_path)
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
        let mut monitor = fixture.monitor();

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
        let mut monitor = fixture.monitor();
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

    #[test]
    fn route_parser_skips_headers_malformed_rows_and_unusable_defaults() {
        let routes: Vec<_> = parse_default_routes(
            "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\n\
             missing-fields 00000000\n\
             bad-destination nope 0102A8C0 0003 0 0 1 00000000 0 0 0\n\
             bad-gateway 00000000 nope 0003 0 0 2 00000000 0 0 0\n\
             bad-flags 00000000 0102A8C0 nope 0 0 3 00000000 0 0 0\n\
             bad-metric 00000000 0102A8C0 0003 0 0 nope 00000000 0 0 0\n\
             network 0000A8C0 00000000 0001 0 0 4 00FFFFFF 0 0 0\n\
             down0 00000000 0102A8C0 0002 0 0 5 00000000 0 0 0\n\
             reject0 00000000 0102A8C0 0201 0 0 6 00000000 0 0 0\n\
             ppp0 00000000 00000000 0001 0 0 7 00000000 0 0 0\n\
             eth0 00000000 0102A8C0 0003 0 0 8 00000000 0 0 0\n",
        )
        .collect();

        assert_eq!(
            routes,
            [
                DefaultRoute {
                    interface: "ppp0",
                    metric: 7,
                },
                DefaultRoute {
                    interface: "eth0",
                    metric: 8,
                },
            ]
        );
    }

    #[test]
    fn lowest_metric_eligible_default_route_wins_with_a_stable_tie_break() {
        let table = "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\n\
                     wlan0 00000000 0102A8C0 0003 0 0 600 00000000 0 0 0\n\
                     docker0 00000000 00000000 0001 0 0 0 00000000 0 0 0\n\
                     wlan1 00000000 0102A8C0 0003 0 0 100 00000000 0 0 0\n\
                     eth0 00000000 0102A8C0 0003 0 0 100 00000000 0 0 0\n";
        let eligible = ["docker0", "eth0", "wlan0", "wlan1"].map(str::to_owned);
        assert_eq!(preferred_default_route(table, &eligible), Some("docker0"));

        let eligible = ["eth0", "wlan0", "wlan1"].map(str::to_owned);
        assert_eq!(
            preferred_default_route(table, &eligible),
            Some("eth0"),
            "equal metrics use interface name as a deterministic tie-break"
        );

        let eligible = ["wlan0"].map(str::to_owned);
        assert_eq!(preferred_default_route(table, &eligible), Some("wlan0"));
    }

    #[test]
    fn default_route_beats_alphabetically_first_virtual_interface() {
        let fixture = Fixture::new("default-route");
        fixture.interface("docker0", "up", 0, 0);
        fixture.interface("wlan0", "up", 0, 0);
        fixture.route_table(
            "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\n\
             wlan0 00000000 0102A8C0 0003 0 0 600 00000000 0 0 0\n",
        );

        assert_eq!(
            fixture.monitor().poll().unwrap().interface.as_deref(),
            Some("wlan0")
        );
    }

    #[test]
    fn default_route_accepts_unknown_virtual_interfaces_without_weakening_fallback() {
        let fixture = Fixture::new("unknown-default-route");
        fixture.interface("eth0", "up", 0, 0);
        fixture.interface("wg0", "unknown", 0, 0);

        assert_eq!(
            fixture.monitor().poll().unwrap().interface.as_deref(),
            Some("eth0"),
            "without route evidence, a genuinely up interface wins"
        );

        fixture.route_table(
            "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\n\
             wg0 00000000 00000000 0001 0 0 50 00000000 0 0 0\n",
        );
        assert_eq!(
            fixture.monitor().poll().unwrap().interface.as_deref(),
            Some("wg0"),
            "a default route makes an unknown-state virtual interface authoritative"
        );

        fixture.interface("eth0", "down", 0, 0);
        std::fs::remove_file(&fixture.route_path).unwrap();
        assert_eq!(
            fixture.monitor().poll().unwrap().interface.as_deref(),
            Some("wg0"),
            "an unknown virtual interface is still better than disconnected"
        );
    }

    #[test]
    fn unavailable_or_oversized_route_table_keeps_alphabetical_fallback() {
        let fixture = Fixture::new("route-fallback");
        fixture.interface("docker0", "up", 0, 0);
        fixture.interface("wlan0", "up", 0, 0);

        assert_eq!(
            fixture.monitor().poll().unwrap().interface.as_deref(),
            Some("docker0"),
            "a missing route table uses the deterministic sysfs fallback"
        );

        std::fs::write(
            &fixture.route_path,
            vec![b'x'; MAX_ROUTE_TABLE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(read_route_table(&fixture.route_path).unwrap(), None);
        assert_eq!(
            fixture.monitor().poll().unwrap().interface.as_deref(),
            Some("docker0")
        );
    }
}
