//! System monitoring with caching and efficient updates

use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use sysinfo::System;

/// Rolling average calculator
#[derive(Debug, Clone)]
pub struct RollingAverage {
    values: VecDeque<f64>,
    capacity: usize,
    sum: f64,
}

impl RollingAverage {
    pub fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
            sum: 0.0,
        }
    }

    pub fn add(&mut self, value: f64) {
        if self.capacity == 0 {
            return;
        }

        if self.values.len() >= self.capacity
            && let Some(old_value) = self.values.pop_front()
        {
            self.sum -= old_value;
        }

        self.values.push_back(value);
        self.sum += value;
    }

    pub fn average(&self) -> f64 {
        if self.values.is_empty() {
            0.0
        } else {
            self.sum / self.values.len() as f64
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn values(&self) -> impl Iterator<Item = f64> + '_ {
        self.values.iter().copied()
    }
}

/// System information snapshot
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemSnapshot {
    pub cpu_usage: Vec<f32>,
    pub cpu_average: f32,
    pub memory_total: u64,
    pub memory_used: u64,
    pub memory_available: u64,
    pub memory_usage_percent: f32,
    pub uptime: u64,
    pub load_average: LoadAverage,
    #[serde(skip)]
    pub timestamp: Instant,
}

/// System load averages
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LoadAverage {
    pub one_minute: f64,
    pub five_minutes: f64,
    pub fifteen_minutes: f64,
}

/// CPU information
#[derive(Debug, Clone)]
pub struct CpuInfo {
    pub name: String,
    pub usage: f32,
    pub frequency: u64,
}

/// Memory information
#[derive(Debug, Clone)]
pub struct MemoryInfo {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub free: u64,
    pub usage_percent: f32,
}

/// System monitor with efficient caching
#[derive(Debug)]
pub struct SystemMonitor {
    system: System,
    last_update: Instant,
    update_interval: Duration,
    cpu_history: RollingAverage,
    memory_history: RollingAverage,
    last_snapshot: Option<SystemSnapshot>,
}

impl SystemMonitor {
    /// Create a new system monitor
    pub fn new(history_length: usize) -> Self {
        let system = System::new();

        let mut monitor = Self {
            system,
            last_update: Instant::now(),
            update_interval: Duration::from_millis(500),
            cpu_history: RollingAverage::new(history_length),
            memory_history: RollingAverage::new(history_length),
            last_snapshot: None,
        };
        monitor.refresh();
        monitor.last_update = Instant::now();
        monitor
    }

    /// Update system information if needed
    pub fn update_if_needed(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_update) >= self.update_interval {
            self.refresh();
            self.last_update = now;
            true
        } else {
            false
        }
    }

    /// Force refresh system information
    pub fn refresh(&mut self) {
        // Refresh only what we need to minimize overhead
        self.system.refresh_cpu_all();
        self.system.refresh_memory();

        // Create new snapshot
        let snapshot = self.create_snapshot();

        // Update history
        self.cpu_history.add(snapshot.cpu_average as f64);
        self.memory_history
            .add(snapshot.memory_usage_percent as f64);

        self.last_snapshot = Some(snapshot);
    }

    /// Create system snapshot
    fn create_snapshot(&self) -> SystemSnapshot {
        let cpu_usage: Vec<f32> = self
            .system
            .cpus()
            .iter()
            .map(|cpu| cpu.cpu_usage())
            .collect();

        let cpu_average = if cpu_usage.is_empty() {
            0.0
        } else {
            cpu_usage.iter().sum::<f32>() / cpu_usage.len() as f32
        };

        let memory_total = self.system.total_memory();
        let (memory_available, memory_used, memory_usage_percent) =
            normalized_memory(memory_total, self.system.available_memory());

        let load_average = self.get_load_average();

        SystemSnapshot {
            cpu_usage,
            cpu_average,
            memory_total,
            memory_used,
            memory_available,
            memory_usage_percent,
            uptime: sysinfo::System::uptime(),
            load_average,
            timestamp: Instant::now(),
        }
    }

    /// Get system load average (Unix-like systems)
    fn get_load_average(&self) -> LoadAverage {
        // On Linux, we can read from /proc/loadavg
        #[cfg(target_os = "linux")]
        {
            if let Ok(content) = std::fs::read_to_string("/proc/loadavg") {
                let parts: Vec<&str> = content.split_whitespace().collect();
                if parts.len() >= 3 {
                    return LoadAverage {
                        one_minute: parts[0].parse().unwrap_or(0.0),
                        five_minutes: parts[1].parse().unwrap_or(0.0),
                        fifteen_minutes: parts[2].parse().unwrap_or(0.0),
                    };
                }
            }
        }

        LoadAverage::default()
    }

    /// Get current system snapshot
    pub fn get_snapshot(&self) -> Option<&SystemSnapshot> {
        self.last_snapshot.as_ref()
    }

    /// Get CPU usage history
    pub fn get_cpu_history(&self) -> Vec<f64> {
        self.cpu_history.values().collect()
    }

    /// Get memory usage history
    pub fn get_memory_history(&self) -> Vec<f64> {
        self.memory_history.values().collect()
    }

    /// Get individual CPU information
    pub fn get_cpu_info(&self) -> Vec<CpuInfo> {
        self.system
            .cpus()
            .iter()
            .map(|cpu| CpuInfo {
                name: cpu.name().to_string(),
                usage: cpu.cpu_usage(),
                frequency: cpu.frequency(),
            })
            .collect()
    }

    /// Get memory information
    pub fn get_memory_info(&self) -> MemoryInfo {
        let total = self.system.total_memory();
        let (available, used, usage_percent) =
            normalized_memory(total, self.system.available_memory());
        let free = self.system.free_memory();

        MemoryInfo {
            total,
            used,
            available,
            free,
            usage_percent,
        }
    }

    /// Get CPU usage for chart display
    pub fn get_cpu_data_for_chart(&self) -> Vec<f64> {
        if let Some(snapshot) = &self.last_snapshot {
            snapshot
                .cpu_usage
                .iter()
                .map(|&usage| (usage / 100.0) as f64)
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Check if CPU usage is high
    pub fn is_cpu_usage_high(&self, threshold: f32) -> bool {
        if let Some(snapshot) = &self.last_snapshot {
            snapshot.cpu_average > threshold * 100.0
        } else {
            false
        }
    }

    /// Check if memory usage is high
    pub fn is_memory_usage_high(&self, threshold: f32) -> bool {
        if let Some(snapshot) = &self.last_snapshot {
            snapshot.memory_usage_percent > threshold * 100.0
        } else {
            false
        }
    }

    /// Get system uptime as formatted string
    pub fn get_uptime_string(&self) -> String {
        let uptime = if let Some(snapshot) = &self.last_snapshot {
            snapshot.uptime
        } else {
            sysinfo::System::uptime()
        };

        let days = uptime / 86400;
        let hours = (uptime % 86400) / 3600;
        let minutes = (uptime % 3600) / 60;

        if days > 0 {
            format!("{days}d {hours}h {minutes}m")
        } else if hours > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{minutes}m")
        }
    }

    /// Set update interval
    pub fn set_update_interval(&mut self, interval: Duration) {
        self.update_interval = interval;
    }

    /// Get average CPU usage over history
    pub fn get_average_cpu_usage(&self) -> f64 {
        self.cpu_history.average()
    }

    /// Get average memory usage over history
    pub fn get_average_memory_usage(&self) -> f64 {
        self.memory_history.average()
    }
}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self::new(6) // Default to 6 samples
    }
}

fn normalized_memory(total: u64, reported_available: u64) -> (u64, u64, f32) {
    // Kernel/cgroup counters are sampled separately and may briefly disagree
    // during a limit change. Keep the public snapshot internally consistent
    // instead of panicking in debug builds or wrapping in release builds.
    let available = reported_available.min(total);
    let used = total - available;
    let usage_percent = if total == 0 {
        0.0
    } else {
        used as f32 / total as f32 * 100.0
    };
    (available, used, usage_percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_capacity_rolling_average_does_not_retain_samples() {
        let mut average = RollingAverage::new(0);

        average.add(10.0);
        average.add(20.0);

        assert!(average.is_empty());
        assert_eq!(average.len(), 0);
        assert_eq!(average.average(), 0.0);
        assert_eq!(average.values().collect::<Vec<_>>(), Vec::<f64>::new());
    }

    #[test]
    fn rolling_average_retains_the_actual_bounded_history() {
        let mut average = RollingAverage::new(3);
        average.add(10.0);
        average.add(20.0);
        average.add(30.0);

        assert_eq!(average.values().collect::<Vec<_>>(), vec![10.0, 20.0, 30.0]);
        assert_eq!(average.average(), 20.0);

        average.add(40.0);

        assert_eq!(average.values().collect::<Vec<_>>(), vec![20.0, 30.0, 40.0]);
        assert_eq!(average.average(), 30.0);
    }

    #[test]
    fn monitor_starts_with_a_snapshot_and_exposes_actual_histories() {
        let mut monitor = SystemMonitor::new(3);

        // This checks the constructor invariant without assuming anything about
        // the host's CPU, memory, battery, or load values.
        assert!(monitor.get_snapshot().is_some());
        assert_eq!(monitor.get_cpu_history().len(), 1);
        assert_eq!(monitor.get_memory_history().len(), 1);

        monitor.cpu_history = RollingAverage::new(3);
        monitor.memory_history = RollingAverage::new(3);
        for value in [10.0, 20.0, 30.0] {
            monitor.cpu_history.add(value);
        }
        for value in [40.0, 50.0, 60.0] {
            monitor.memory_history.add(value);
        }

        assert_eq!(monitor.get_cpu_history(), vec![10.0, 20.0, 30.0]);
        assert_eq!(monitor.get_memory_history(), vec![40.0, 50.0, 60.0]);
    }

    #[test]
    fn memory_values_remain_consistent_when_available_exceeds_total() {
        let (available, used, percent) = normalized_memory(100, 40);
        assert_eq!((available, used), (40, 60));
        assert!((percent - 60.0).abs() < 0.001);
        assert_eq!(normalized_memory(100, 101), (100, 0, 0.0));
        assert_eq!(normalized_memory(0, u64::MAX), (0, 0, 0.0));
    }
}
