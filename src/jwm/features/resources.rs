//! What the machine is doing: CPU load, memory in use, network throughput.
//!
//! Deliberately not `backend::sys_stats`. That sampler reads `/proc/self/*`
//! because the debug HUD is about the compositor — showing its 3% while a
//! build pins sixteen cores would be a lie in a panel that says "CPU". These
//! figures are the machine's, read from the same files `top` and `free` read.
//!
//! Two rules run through the whole module:
//!
//! *A VPN's bytes are counted once.* Throughput sums only the interfaces that
//! carry traffic off the machine — wired, wireless, mobile broadband. A tunnel
//! or a bridge would count the same bytes a second time, so the plumbing is
//! left out rather than added up.
//!
//! *Units are binary throughout.* The kernel writes `kB` in `/proc/meminfo`
//! and means KiB; if the network row then reported decimal MB/s, the panel
//! would be the one place in the shell that mixes two unit systems and never
//! says which.
//!
//! Everything here is a pure function over the text `/proc` produced, except
//! three `read_to_string` calls in [`ResourceSampler`]. The awkward parts —
//! a counter that went backwards over a suspend, a container with no network
//! interface, the first sample having no rate to report yet — are settled in
//! unit tests rather than by watching a panel and hoping.

use std::time::{Duration, Instant};

/// How often `/proc` is re-read. Fast enough that a download shows up while
/// you are looking at it, slow enough to be three small reads a minute rather
/// than three a frame.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A gap longer than this re-seeds the sampler instead of being divided
/// through. After a suspend or a VT switch the counters are minutes apart, and
/// a correct ten-minute average presented as the current rate is worse than
/// admitting there is nothing to report yet.
pub const MAX_DELTA_AGE: Duration = Duration::from_secs(30);

/// What a rate shows before there are two samples to subtract.
pub const UNKNOWN: &str = "\u{2014}";

/// Jiffy buckets from `/proc/stat`'s aggregate line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub total: u64,
    /// Idle *and* iowait: a machine blocked on a disk is not busy, whatever
    /// the scheduler is doing about it.
    pub idle: u64,
}

/// Memory as `free` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryUsage {
    pub total_kib: u64,
    pub used_kib: u64,
}

impl MemoryUsage {
    #[must_use]
    pub fn percent(&self) -> u8 {
        if self.total_kib == 0 {
            return 0;
        }
        let percent = self.used_kib.saturating_mul(100) / self.total_kib;
        percent.min(100) as u8
    }
}

/// Byte counters summed over the interfaces worth counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Bytes per second in each direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Throughput {
    pub rx_bytes_per_sec: u64,
    pub tx_bytes_per_sec: u64,
}

/// Everything the control center needs, with each part absent rather than
/// zeroed when the machine could not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceState {
    /// Whether `/proc/stat` answered at all. Without it there is no row —
    /// an empty row is a worse answer than no row.
    pub cpu_present: bool,
    /// Machine-wide busy percentage, once two samples exist.
    pub cpu_percent: Option<u8>,
    /// A level rather than a rate, so it is real from the first sample.
    pub memory: Option<MemoryUsage>,
    /// Whether any interface worth counting exists.
    pub net_present: bool,
    /// Once two samples exist.
    pub throughput: Option<Throughput>,
}

// -------------------------------------------------------------------------
// Parsing
// -------------------------------------------------------------------------

/// The aggregate `cpu` line of `/proc/stat`.
///
/// Only the aggregate: `cpu0` is one core, and a panel that says "CPU" while
/// reporting the first core of thirty-two says nothing useful.
#[must_use]
pub fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|line| {
        line.strip_prefix("cpu")
            .is_some_and(|rest| rest.starts_with(' '))
    })?;
    let buckets: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal …; anything shorter than
    // idle+iowait cannot answer the question.
    if buckets.len() < 5 {
        return None;
    }
    Some(CpuTimes {
        // The first eight buckets only. The kernel adds a guest tick to
        // `user` (or `nice`) *and* to `guest`, so columns nine and ten are a
        // subset of the first two; summing all ten inflates both the total
        // and the busy share on any machine running virtual machines. `top`
        // subtracts guest for the same reason.
        total: buckets.iter().take(8).sum(),
        idle: buckets[3] + buckets[4],
    })
}

/// Busy percentage between two readings, or `None` when there is nothing to
/// divide: one sample, two identical samples, or counters that went backwards
/// across a suspend.
#[must_use]
pub fn cpu_busy_percent(previous: CpuTimes, current: CpuTimes) -> Option<u8> {
    if current.total <= previous.total || current.idle < previous.idle {
        return None;
    }
    let total = current.total - previous.total;
    let idle = (current.idle - previous.idle).min(total);
    let busy = total - idle;
    // Rounded, then clamped: a machine at 99.7% must read 100, and nothing
    // must ever read 101.
    Some((((busy * 200 + total) / (total * 2)).min(100)) as u8)
}

/// `MemTotal` and what is actually unavailable, from `/proc/meminfo`.
///
/// Used is `MemTotal - MemAvailable`, not `MemTotal - MemFree`: a machine with
/// twenty gigabytes of page cache is not nearly full, and reporting it that
/// way would send people hunting for a leak that is a feature.
#[must_use]
pub fn parse_meminfo(meminfo: &str) -> Option<MemoryUsage> {
    let field = |name: &str| -> Option<u64> {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    };
    let total_kib = field("MemTotal").filter(|total| *total > 0)?;
    let available = field("MemAvailable").or_else(|| {
        // Kernels before 3.14, and some containers, do not offer it. The
        // classic approximation is closer than MemFree alone.
        Some(field("MemFree")? + field("Buffers").unwrap_or(0) + field("Cached").unwrap_or(0))
    })?;
    Some(MemoryUsage {
        total_kib,
        used_kib: total_kib.saturating_sub(available.min(total_kib)),
    })
}

/// Whether an interface's bytes leave the machine.
///
/// An allowlist of the kernel's naming prefixes rather than a denylist of
/// virtual ones. Both are incomplete; this one fails by hiding the row on a
/// machine with a hand-renamed link, where a denylist would fail by counting
/// a bridge's traffic twice and reporting double the real rate. A row that is
/// missing is easier to understand than one that is wrong.
///
/// The rule matches [`crate::jwm::features::connectivity`]'s — wired and
/// wireless count, virtual plumbing never does — but the implementation
/// cannot be shared: that module judges nmcli's connection types, and
/// `/proc/net/dev` gives nothing but a name.
#[must_use]
pub fn counts_toward_throughput(interface: &str) -> bool {
    const CARRIES_TRAFFIC: [&str; 6] = ["en", "eth", "wl", "ww", "ppp", "usb"];
    let name = interface.trim();
    // A VLAN device is named after the link beneath it — `enp2s0.100` — and
    // the kernel counts the same frames on both. Summing the pair would
    // report exactly twice the real rate, which is the failure the allowlist
    // exists to avoid.
    if name.contains('.') {
        return false;
    }
    CARRIES_TRAFFIC
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Summed byte counters from `/proc/net/dev`, or `None` when no interface
/// worth counting is listed — a container with only `lo`, or no file at all.
#[must_use]
pub fn parse_net_dev(dev: &str) -> Option<NetCounters> {
    let mut counters = NetCounters::default();
    let mut counted = false;
    for line in dev.lines() {
        // Split at the first colon rather than on whitespace: a counter long
        // enough to touch the name (`wlp3s0:2621464347 …`) leaves no space
        // there, and the two header lines have no colon in that position at
        // all, which is what skips them without matching their text.
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if !counts_toward_throughput(name) {
            continue;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .map(|field| field.parse().unwrap_or(0))
            .collect();
        // receive: bytes packets errs drop fifo frame compressed multicast,
        // then transmit bytes. Anything shorter is not a counter line.
        let (Some(rx), Some(tx)) = (fields.first(), fields.get(8)) else {
            continue;
        };
        counters.rx_bytes += rx;
        counters.tx_bytes += tx;
        counted = true;
    }
    counted.then_some(counters)
}

/// Whether a gap between samples can be divided through at all.
#[must_use]
pub fn delta_is_usable(elapsed: Duration) -> bool {
    !elapsed.is_zero() && elapsed <= MAX_DELTA_AGE
}

/// Bytes per second between two readings.
///
/// `None` when the gap is unusable or a counter shrank. Counters shrink when
/// an interface disappears or the kernel resets them; the honest answer is
/// that this interval has no rate, not a wrapped one.
#[must_use]
pub fn throughput(
    previous: NetCounters,
    current: NetCounters,
    elapsed: Duration,
) -> Option<Throughput> {
    if !delta_is_usable(elapsed) {
        return None;
    }
    if current.rx_bytes < previous.rx_bytes || current.tx_bytes < previous.tx_bytes {
        return None;
    }
    let seconds = elapsed.as_secs_f64();
    let rate = |delta: u64| (delta as f64 / seconds).round() as u64;
    Some(Throughput {
        rx_bytes_per_sec: rate(current.rx_bytes - previous.rx_bytes),
        tx_bytes_per_sec: rate(current.tx_bytes - previous.tx_bytes),
    })
}

// -------------------------------------------------------------------------
// Rendering
// -------------------------------------------------------------------------

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;

/// A transfer rate in binary units: `0 B/s`, `340 KiB/s`, `1.2 MiB/s`.
#[must_use]
pub fn format_rate(bytes_per_sec: u64) -> String {
    match bytes_per_sec {
        rate if rate < KIB => format!("{rate} B/s"),
        rate if rate < MIB => format!("{} KiB/s", rate / KIB),
        rate if rate < GIB => format!("{:.1} MiB/s", rate as f64 / MIB as f64),
        rate => format!("{:.1} GiB/s", rate as f64 / GIB as f64),
    }
}

/// An amount of memory, given in the KiB `/proc/meminfo` uses.
#[must_use]
pub fn format_size(kib: u64) -> String {
    if kib < MIB {
        // Below a gibibyte, MiB: a 512 MiB container should not read `0.5 GiB`.
        format!("{} MiB", kib / KIB)
    } else {
        format!("{:.1} GiB", kib as f64 / MIB as f64)
    }
}

/// One row: glyph, label, and the value right-aligned to the panel's column.
///
/// The gap never closes below two spaces, however long the value gets. A
/// format width alone does not pad when the content outgrows it, and
/// `Network I/O` touching `1023 KiB/s` is a row nobody can read — which is
/// exactly what a busy link produces.
fn row(glyph: &str, label: &str, detail: &str) -> String {
    const COLUMN: usize = 36;
    const MIN_GAP: usize = 2;
    let head = format!("{glyph}  {label}");
    let used = head.chars().count() + detail.chars().count();
    let gap = COLUMN.saturating_sub(used).max(MIN_GAP);
    format!("{head}{}{detail}", " ".repeat(gap))
}

/// The CPU row. `\u{f2db}` is fa-microchip.
#[must_use]
pub fn cpu_row(percent: Option<u8>) -> String {
    let detail = percent.map_or_else(|| UNKNOWN.to_string(), |percent| format!("{percent}%"));
    row("\u{f2db}", "CPU", &detail)
}

/// The memory row. `\u{f1c0}` is fa-database.
#[must_use]
pub fn memory_row(usage: MemoryUsage) -> String {
    let detail = format!(
        "{}%  {} / {}",
        usage.percent(),
        format_size(usage.used_kib),
        format_size(usage.total_kib)
    );
    row("\u{f1c0}", "Memory", &detail)
}

/// The throughput row. `\u{f0ec}` is fa-exchange, with fa-arrow-down and
/// fa-arrow-up for the two directions.
#[must_use]
pub fn throughput_row(rates: Option<Throughput>) -> String {
    let detail = rates.map_or_else(
        || UNKNOWN.to_string(),
        |rates| {
            format!(
                "\u{f063} {}  \u{f062} {}",
                format_rate(rates.rx_bytes_per_sec),
                format_rate(rates.tx_bytes_per_sec)
            )
        },
    );
    row("\u{f0ec}", "Network I/O", &detail)
}

// -------------------------------------------------------------------------
// Sampling
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    cpu: Option<CpuTimes>,
    net: Option<NetCounters>,
}

/// Reads `/proc` on its own interval and keeps the last sample to subtract
/// from.
#[derive(Debug, Default)]
pub struct ResourceSampler {
    previous: Option<Sample>,
    state: ResourceState,
}

impl ResourceSampler {
    /// Re-read if the interval is up. Returns whether [`Self::state`] changed,
    /// so a caller never redraws a panel that would look identical.
    ///
    /// The throttle's timestamp and the rate's divisor are deliberately the
    /// same field: kept apart they drift, and a rate divided by the interval
    /// we *meant* to wait rather than the one we did silently under-reports
    /// after every stall.
    pub fn maybe_sample(&mut self) -> bool {
        let now = Instant::now();
        if self
            .previous
            .is_some_and(|previous| now.duration_since(previous.at) < POLL_INTERVAL)
        {
            return false;
        }

        let cpu = read("/proc/stat").as_deref().and_then(parse_cpu_times);
        let net = read("/proc/net/dev").as_deref().and_then(parse_net_dev);
        let memory = read("/proc/meminfo").as_deref().and_then(parse_meminfo);
        let sample = Sample { at: now, cpu, net };

        let elapsed = self
            .previous
            .map_or(Duration::ZERO, |previous| now.duration_since(previous.at));
        let usable = delta_is_usable(elapsed);
        let state = ResourceState {
            cpu_present: cpu.is_some(),
            cpu_percent: match (self.previous.and_then(|previous| previous.cpu), cpu) {
                (Some(previous), Some(current)) if usable => cpu_busy_percent(previous, current),
                _ => None,
            },
            memory,
            net_present: net.is_some(),
            throughput: match (self.previous.and_then(|previous| previous.net), net) {
                (Some(previous), Some(current)) => throughput(previous, current, elapsed),
                _ => None,
            },
        };

        self.previous = Some(sample);
        let changed = state != self.state;
        self.state = state;
        changed
    }

    #[must_use]
    pub fn state(&self) -> ResourceState {
        self.state
    }
}

/// A `/proc` file that is missing or unreadable is an unanswered question,
/// not an error: a hardened container can hide any of these.
fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

impl crate::jwm::Jwm {
    /// Re-read the machine's resources and keep an open control center
    /// current. Called from the frame tick; the interval gate is inside.
    pub(crate) fn poll_resources(&mut self) {
        use crate::jwm::features::ControlKind;

        if !crate::config::CONFIG.load().behavior().resource_rows {
            if self.features.resources == ResourceState::default() {
                return;
            }
            // Switched off: forget what was sampled, because a stale reading
            // answered over IPC while the feature is off is a number nobody
            // is keeping true. The sampler goes with it, so switching back on
            // reads `/proc` at once instead of sitting out the rest of the
            // interval and leaving the rows missing.
            self.features.resources = ResourceState::default();
            self.features.resource_sampler = ResourceSampler::default();
            // A panel already on screen would otherwise keep three rows that
            // no longer update — frozen numbers are worse than none. The
            // rebuild marks itself dirty; the tick pushes it.
            if self.features.system_ui.is_control_center() {
                self.refresh_open_control_center();
            }
            return;
        }
        if !self.features.resource_sampler.maybe_sample() {
            return;
        }
        let state = self.features.resource_sampler.state();
        self.features.resources = state;

        // Only the three labels are retyped. Rebuilding the panel would
        // re-run `wpctl`, `brightnessctl` and `powerprofilesctl` — three
        // processes every two seconds for as long as the panel is open.
        if !self.features.system_ui.is_control_center() {
            return;
        }
        self.features
            .system_ui
            .update_control_label(ControlKind::Cpu, cpu_row(state.cpu_percent));
        if let Some(memory) = state.memory {
            self.features
                .system_ui
                .update_control_label(ControlKind::Memory, memory_row(memory));
        }
        self.features.system_ui.update_control_label(
            ControlKind::NetworkThroughput,
            throughput_row(state.throughput),
        );
        // Load-bearing: without this the row only changes when the user
        // happens to press a key. The push itself happens once at the end of
        // the tick, so several polls in one frame cost one redraw.
        self.mark_system_ui_dirty();
    }

    /// The machine's resources, for `get_resources`. Unanswered parts are
    /// null rather than zero.
    pub(crate) fn resources_json(&self) -> serde_json::Value {
        let state = self.features.resources;
        serde_json::json!({
            "cpu_present": state.cpu_present,
            "cpu_percent": state.cpu_percent,
            "memory": state.memory.map(|memory| serde_json::json!({
                "total_kib": memory.total_kib,
                "used_kib": memory.used_kib,
                "percent": memory.percent(),
            })),
            "net_present": state.net_present,
            "throughput": state.throughput.map(|rates| serde_json::json!({
                "rx_bytes_per_sec": rates.rx_bytes_per_sec,
                "tx_bytes_per_sec": rates.tx_bytes_per_sec,
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "\
cpu  5597383 6617 1459653 192827873 398755 0 72605 0 0 0
cpu0 213988 207 50163 9668881 17652 0 1414 0 0 0
cpu1 133157 9 18610 9888667 2862 0 483 0 0 0
intr 1234567 0 0
ctxt 987654321
";

    const MEMINFO: &str = "\
MemTotal:       32558940 kB
MemFree:         2370904 kB
MemAvailable:   27271184 kB
Buffers:          476456 kB
Cached:         24409240 kB
SwapTotal:             0 kB
";

    // The real shape of this machine's file: two header lines, loopback, a
    // wired link with nothing on it, a wireless link whose counter is long
    // enough to touch the colon, a VPN tunnel, and a docker bridge.
    const NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 12318358  130009    0    0    0     0          0         0 12318358  130009    0    0    0     0       0          0
enp2s0:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0
wlp3s0:2621464347 3965190    0    0    0     0          0         0 5724771402 3442753    0    0    0     0       0          0
  tun0:     168       3    0    0    0     0          0         0     3414      45    0    0    0     0       0          0
docker0:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0
";

    #[test]
    fn the_cpu_figure_is_the_machine_and_not_one_core() {
        let times = parse_cpu_times(STAT).expect("times");
        assert_eq!(
            times.total,
            5_597_383 + 6_617 + 1_459_653 + 192_827_873 + 398_755 + 72_605
        );
        // Idle includes iowait: blocked on a disk is not busy.
        assert_eq!(times.idle, 192_827_873 + 398_755);
    }

    #[test]
    fn a_stat_without_an_aggregate_line_answers_nothing() {
        assert_eq!(parse_cpu_times("cpu0 1 2 3 4 5 6 7\nintr 9\n"), None);
        assert_eq!(parse_cpu_times(""), None);
        assert_eq!(parse_cpu_times("cpu  1 2\n"), None, "too few buckets");
    }

    #[test]
    fn guest_time_is_not_counted_twice() {
        // The kernel adds a guest tick to `user` as well as to `guest`, so a
        // KVM host that spent 100 jiffies in a guest and 800 idle is 11% busy,
        // not 20% — summing all ten columns would count those ticks twice.
        let idle_only = parse_cpu_times("cpu  0 0 0 0 0 0 0 0 0 0\n").expect("times");
        let with_guest = parse_cpu_times("cpu  100 0 0 800 0 0 0 0 100 0\n").expect("times");
        assert_eq!(with_guest.total, 900);
        assert_eq!(cpu_busy_percent(idle_only, with_guest), Some(11));
    }

    #[test]
    fn cpu_load_needs_two_samples_that_moved_forwards() {
        let first = CpuTimes {
            total: 1_000,
            idle: 800,
        };
        // Half of the interval's jiffies were busy.
        let second = CpuTimes {
            total: 1_200,
            idle: 900,
        };
        assert_eq!(cpu_busy_percent(first, second), Some(50));
        // Nothing moved: no rate, rather than a division by zero.
        assert_eq!(cpu_busy_percent(first, first), None);
        // Backwards across a suspend or a counter reset.
        assert_eq!(cpu_busy_percent(second, first), None);
        assert_eq!(
            cpu_busy_percent(
                first,
                CpuTimes {
                    total: 1_200,
                    idle: 700
                }
            ),
            None,
            "idle cannot shrink while total grows"
        );
    }

    #[test]
    fn cpu_load_never_reads_above_a_hundred() {
        let busy = cpu_busy_percent(
            CpuTimes { total: 0, idle: 0 },
            CpuTimes {
                total: 999,
                idle: 0,
            },
        );
        assert_eq!(busy, Some(100));
        let nearly = cpu_busy_percent(
            CpuTimes { total: 0, idle: 0 },
            CpuTimes {
                total: 1000,
                idle: 3,
            },
        );
        assert_eq!(nearly, Some(100), "99.7% rounds to 100, not 101");
    }

    #[test]
    fn memory_used_is_what_is_unavailable_not_what_is_unfree() {
        let usage = parse_meminfo(MEMINFO).expect("usage");
        assert_eq!(usage.total_kib, 32_558_940);
        // 24 GiB of that is page cache; a machine with a warm cache is not
        // nearly full.
        assert_eq!(usage.used_kib, 32_558_940 - 27_271_184);
        assert_eq!(usage.percent(), 16);
    }

    #[test]
    fn a_kernel_without_mem_available_falls_back_rather_than_giving_up() {
        let old = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 200 kB\nCached: 300 kB\n";
        let usage = parse_meminfo(old).expect("usage");
        assert_eq!(usage.used_kib, 1000 - 600);
        assert_eq!(parse_meminfo("MemFree: 100 kB\n"), None, "no total");
        assert_eq!(parse_meminfo("MemTotal: 0 kB\nMemFree: 0 kB\n"), None);
    }

    #[test]
    fn only_the_interfaces_that_leave_the_machine_are_counted() {
        for carries in ["enp2s0", "eth0", "wlp3s0", "wlan0", "wwan0", "ppp0", "usb0"] {
            assert!(counts_toward_throughput(carries), "{carries}");
        }
        for plumbing in [
            "lo",
            "docker0",
            "br-1a2b3c",
            "br0",
            "veth9f2",
            "virbr0",
            "vnet0",
            "vmnet1",
            "tun0",
            "tap0",
            "wg0",
            "tailscale0",
            "bond0",
            "dummy0",
            // VLAN children are named after the link beneath them, and the
            // kernel counts the same frames on both.
            "enp2s0.100",
            "eth0.4094",
        ] {
            assert!(!counts_toward_throughput(plumbing), "{plumbing}");
        }
    }

    #[test]
    fn a_vlan_is_not_counted_on_top_of_the_link_beneath_it() {
        let tagged = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 10 1 0 0 0 0 0 0 10 1 0 0 0 0 0 0
enp2s0: 1000000 100 0 0 0 0 0 0 500000 50 0 0 0 0 0 0
enp2s0.100: 1000000 100 0 0 0 0 0 0 500000 50 0 0 0 0 0 0
";
        let counters = parse_net_dev(tagged).expect("counters");
        assert_eq!(counters.rx_bytes, 1_000_000, "the VLAN doubled the rate");
        assert_eq!(counters.tx_bytes, 500_000);
    }

    #[test]
    fn throughput_sums_the_real_links_and_ignores_the_plumbing() {
        let counters = parse_net_dev(NET_DEV).expect("counters");
        // Wireless plus the idle wired link; loopback, the tunnel and the
        // docker bridge are left out. Counting tun0 would report the VPN's
        // bytes a second time.
        assert_eq!(counters.rx_bytes, 2_621_464_347);
        assert_eq!(counters.tx_bytes, 5_724_771_402);
    }

    #[test]
    fn a_container_with_only_loopback_has_no_row_to_show() {
        let only_lo = "Inter-|   Receive\n face |bytes\n    lo: 1 2 3 4 5 6 7 8 9 10\n";
        assert_eq!(parse_net_dev(only_lo), None);
        assert_eq!(parse_net_dev(""), None);
        // A truncated counter line is skipped rather than half-read.
        assert_eq!(parse_net_dev("eth0: 1 2 3\n"), None);
    }

    #[test]
    fn a_rate_needs_two_samples_and_a_usable_gap() {
        let first = NetCounters {
            rx_bytes: 1_000_000,
            tx_bytes: 500_000,
        };
        let second = NetCounters {
            rx_bytes: 1_000_000 + 2 * MIB,
            tx_bytes: 500_000 + 2 * KIB,
        };
        let rates = throughput(first, second, Duration::from_secs(2)).expect("rates");
        assert_eq!(rates.rx_bytes_per_sec, MIB);
        assert_eq!(rates.tx_bytes_per_sec, KIB);

        assert_eq!(throughput(first, second, Duration::ZERO), None);
        assert_eq!(
            throughput(first, second, MAX_DELTA_AGE + Duration::from_secs(1)),
            None,
            "a gap across a suspend is not this second's rate"
        );
        assert_eq!(
            throughput(second, first, Duration::from_secs(2)),
            None,
            "a counter that shrank was reset, not negative"
        );
    }

    #[test]
    fn rates_and_sizes_are_binary_and_readable() {
        assert_eq!(format_rate(0), "0 B/s", "zero is an answer");
        assert_eq!(format_rate(999), "999 B/s");
        assert_eq!(format_rate(KIB), "1 KiB/s");
        assert_eq!(format_rate(340 * KIB), "340 KiB/s");
        assert_eq!(format_rate(MIB + MIB / 5), "1.2 MiB/s");
        assert_eq!(format_rate(3 * GIB / 2), "1.5 GiB/s");

        assert_eq!(format_size(512 * KIB), "512 MiB", "not 0.5 GiB");
        assert_eq!(format_size(6 * MIB + MIB / 5), "6.2 GiB");
    }

    #[test]
    fn a_pending_rate_is_an_em_dash_and_a_known_one_is_not() {
        assert!(cpu_row(None).contains(UNKNOWN));
        assert!(cpu_row(Some(37)).contains("37%"));
        assert!(throughput_row(None).contains(UNKNOWN));
        let row = throughput_row(Some(Throughput {
            rx_bytes_per_sec: MIB,
            tx_bytes_per_sec: 0,
        }));
        assert!(
            row.contains("1 MiB/s") || row.contains("1.0 MiB/s"),
            "{row}"
        );
        assert!(row.contains("0 B/s"));
        // Memory is a level, so it never has a pending form.
        let row = memory_row(parse_meminfo(MEMINFO).expect("usage"));
        assert!(row.contains("GiB") && row.contains("16%"), "{row}");
    }

    #[test]
    fn a_wide_value_never_touches_its_label() {
        // A busy link produces the longest detail this row can have; the
        // format width alone would let it run into "Network I/O".
        let busy = throughput_row(Some(Throughput {
            rx_bytes_per_sec: 1023 * KIB,
            tx_bytes_per_sec: 1023 * KIB,
        }));
        assert!(busy.contains("I/O  "), "label and value collided: {busy:?}");
        // A short one is still right-aligned to the same column as the others.
        let quiet = cpu_row(Some(4));
        assert_eq!(quiet.chars().count(), 36);
        assert_eq!(
            memory_row(MemoryUsage {
                total_kib: 32 * MIB,
                used_kib: 8 * MIB,
            })
            .chars()
            .count(),
            36
        );
    }

    #[test]
    fn every_glyph_stays_in_the_widely_available_range() {
        // Same rule as the connectivity rows: FA5-era f6xx codepoints render
        // as hollow boxes in common Nerd Font builds.
        let rows = [
            cpu_row(None),
            cpu_row(Some(50)),
            memory_row(MemoryUsage {
                total_kib: MIB,
                used_kib: KIB,
            }),
            throughput_row(None),
            throughput_row(Some(Throughput::default())),
        ];
        for row in rows {
            for ch in row
                .chars()
                .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
            {
                assert!(
                    (ch as u32) < 0xf600,
                    "{ch:?} is outside the FontAwesome-4 range in {row:?}"
                );
            }
        }
    }

    #[test]
    fn the_sampler_reports_levels_immediately_and_rates_only_later() {
        let mut sampler = ResourceSampler::default();
        assert!(sampler.maybe_sample(), "the first sample is a change");
        let first = sampler.state();
        // On any Linux machine these files exist; memory is a level.
        if first.cpu_present {
            assert_eq!(first.cpu_percent, None, "no rate from one sample");
        }
        if first.net_present {
            assert_eq!(first.throughput, None, "no rate from one sample");
        }
        assert!(first.memory.is_some_and(|memory| memory.total_kib > 0));
        // The interval gate holds the second read off.
        assert!(!sampler.maybe_sample());
    }
}
