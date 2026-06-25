//! Backend-independent process resource sampler.
//!
//! Both compositor backends render a debug HUD that includes memory and CPU
//! usage. The sampling logic is pure `/proc` parsing — it doesn't need GL or
//! any compositor state — so it lives here once.
//!
//! Sampling is throttled (>= 250 ms between reads) to keep per-frame overhead
//! negligible. Errors keep the previous values rather than panicking.

use std::fs;
use std::time::{Duration, Instant};

const MIN_SAMPLE_INTERVAL_MS: u64 = 250;

pub struct SysStatsSampler {
    last_sample_at: Option<Instant>,
    rss_mib: f32,
    cpu_pct: f32,
    last_proc_jiffies: u64,
    last_total_jiffies: u64,
    initialized: bool,
}

impl SysStatsSampler {
    pub fn new() -> Self {
        Self {
            last_sample_at: None,
            rss_mib: 0.0,
            cpu_pct: 0.0,
            last_proc_jiffies: 0,
            last_total_jiffies: 0,
            initialized: false,
        }
    }

    /// Sample if the throttle window has elapsed. Cheap (~one timestamp
    /// compare) when throttled.
    pub fn maybe_sample(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_sample_at {
            if now.duration_since(last) < Duration::from_millis(MIN_SAMPLE_INTERVAL_MS) {
                return;
            }
        }
        self.last_sample_at = Some(now);

        if let Ok(status) = fs::read_to_string("/proc/self/status") {
            if let Some(kib) = parse_vmrss_kib(&status) {
                self.rss_mib = kib as f32 / 1024.0;
            }
        }

        let proc_j = fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| parse_self_stat_jiffies(&s));
        let total_j = fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|s| parse_proc_stat_cpu_total(&s));

        if let (Some(pj), Some(tj)) = (proc_j, total_j) {
            if self.initialized {
                let dp = pj.saturating_sub(self.last_proc_jiffies);
                let dt = tj.saturating_sub(self.last_total_jiffies);
                if dt > 0 {
                    let ncpu = num_cpus_online().max(1) as f32;
                    self.cpu_pct = 100.0 * (dp as f32 / dt as f32) * ncpu;
                }
            }
            self.last_proc_jiffies = pj;
            self.last_total_jiffies = tj;
            self.initialized = true;
        }
    }

    pub fn rss_mib(&self) -> f32 {
        self.rss_mib
    }

    pub fn cpu_pct(&self) -> f32 {
        self.cpu_pct
    }
}

impl Default for SysStatsSampler {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn parse_vmrss_kib(status: &str) -> Option<u64> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let mut parts = rest.split_whitespace();
            if let Some(num) = parts.next() {
                if let Ok(v) = num.parse::<u64>() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Parse `utime + stime` (fields 14 and 15, 1-indexed) from `/proc/self/stat`.
/// Field 2 is `comm` wrapped in parentheses and may contain spaces, so we
/// split on the *last* `)` first.
pub(crate) fn parse_self_stat_jiffies(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    let after = &stat[close + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After the `)`, the next field is index 3 (state). utime = field 14,
    // stime = field 15. Offsets into `fields`: 14 - 3 = 11, 15 - 3 = 12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Sum the first jiffy-bucket line of `/proc/stat` (the aggregate `cpu` line).
pub(crate) fn parse_proc_stat_cpu_total(stat: &str) -> Option<u64> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let mut total: u64 = 0;
    for tok in line.split_whitespace().skip(1) {
        if let Ok(v) = tok.parse::<u64>() {
            total = total.saturating_add(v);
        }
    }
    if total == 0 { None } else { Some(total) }
}

fn num_cpus_online() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as usize } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmrss_extracted_from_status() {
        let sample = "\
Name:\tjwm
State:\tR (running)
VmPeak:\t  204800 kB
VmRSS:\t  184320 kB
VmData:\t  100000 kB
";
        assert_eq!(parse_vmrss_kib(sample), Some(184320));
    }

    #[test]
    fn vmrss_missing_returns_none() {
        assert_eq!(parse_vmrss_kib("Name:\tjwm\nState:\tR\n"), None);
    }

    #[test]
    fn self_stat_jiffies_skips_comm_with_spaces() {
        // Synthetic /proc/self/stat where comm contains a space and a ')'.
        // utime=100, stime=50 ⇒ total=150.
        let stat = "1234 (weird ) name) R 1 1234 1234 0 -1 4194304 100 0 0 0 100 50 0 0 20 0 1 0 12345 1024 256 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_self_stat_jiffies(stat), Some(150));
    }

    #[test]
    fn proc_stat_cpu_total_sums_buckets() {
        let sample = "\
cpu  100 20 30 5000 10 0 5 0 0 0
cpu0 50 10 15 2500 5 0 2 0 0 0
intr 99999
";
        // 100+20+30+5000+10+0+5 = 5165
        assert_eq!(parse_proc_stat_cpu_total(sample), Some(5165));
    }

    #[test]
    fn proc_stat_cpu_total_missing_returns_none() {
        assert_eq!(parse_proc_stat_cpu_total("intr 0\nctxt 0\n"), None);
    }

    #[test]
    fn sampler_throttles_repeat_calls() {
        let mut s = SysStatsSampler::new();
        s.maybe_sample();
        let first = s.last_sample_at;
        s.maybe_sample();
        // Second call within the throttle window must not move the timestamp.
        assert_eq!(s.last_sample_at, first);
    }
}
