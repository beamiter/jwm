//! Read-only diagnostics of the atomically persisted usage ledger. This path
//! never opens Store, acquires its writer lock, creates state, or contacts API.
use crate::config::Config;
use jwm_eva_core::usage::UsageMeter;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_LEDGER_BYTES: u64 = 4 * 1024 * 1024;
// Official production guide links, verified 2026-10-29:
// https://developers.openai.com/api/docs/guides/production-best-practices
const ACCOUNT_USAGE: &str = "https://platform.openai.com/settings/organization/usage";
const ACCOUNT_LIMITS: &str = "https://platform.openai.com/settings/organization/limits";

#[derive(Default, Serialize)]
struct Totals {
    settled_records: u64,
    settled_microusd: u64,
    pending_records: u64,
    pending_reserved_microusd: u64,
    unknown_records: u64,
    unknown_reserved_microusd: u64,
    reserved_microusd: u64,
    consumed_and_reserved_microusd: u64,
}
impl Totals {
    fn add_settled(&mut self, records: u64, cost: u64) {
        self.settled_records = self.settled_records.saturating_add(records);
        self.settled_microusd = self.settled_microusd.saturating_add(cost);
        self.consumed_and_reserved_microusd =
            self.consumed_and_reserved_microusd.saturating_add(cost);
    }
    fn add_reserved(&mut self, unknown: bool, cost: u64) {
        self.reserved_microusd = self.reserved_microusd.saturating_add(cost);
        if unknown {
            self.unknown_records = self.unknown_records.saturating_add(1);
            self.unknown_reserved_microusd = self.unknown_reserved_microusd.saturating_add(cost);
        } else {
            self.pending_records = self.pending_records.saturating_add(1);
            self.pending_reserved_microusd = self.pending_reserved_microusd.saturating_add(cost);
        }
        self.consumed_and_reserved_microusd =
            self.consumed_and_reserved_microusd.saturating_add(cost);
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("jwm/eva")
}
fn day(config: &Config) -> String {
    match config.budget.timezone.as_str() {
        "Asia/Shanghai" => chrono::Utc::now()
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
            .format("%Y-%m-%d")
            .to_string(),
        "UTC" => chrono::Utc::now().format("%Y-%m-%d").to_string(),
        _ => chrono::Local::now().format("%Y-%m-%d").to_string(),
    }
}

fn read_meter(dir: &Path) -> io::Result<Option<UsageMeter>> {
    let directory = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !directory.is_dir()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o077 != 0
    {
        return Err(io::Error::other("invalid local usage directory"));
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("usage.json"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_LEDGER_BYTES
    {
        return Err(io::Error::other("invalid local usage ledger"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_LEDGER_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err(io::Error::other("invalid local usage ledger"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| io::Error::other("invalid local usage ledger"))
}

fn summarize(meter: &UsageMeter, day: &str) -> Value {
    let mut total = Totals::default();
    let mut groups: BTreeMap<(String, String), Totals> = BTreeMap::new();
    if let Some(models) = meter.summaries.get(day) {
        for (model, versions) in models {
            for (version, summary) in versions {
                total.add_settled(summary.settled_records, summary.settled_microusd);
                groups
                    .entry((model.clone(), version.clone()))
                    .or_default()
                    .add_settled(summary.settled_records, summary.settled_microusd);
            }
        }
    }
    for record in meter.records.values().filter(|record| record.day == day) {
        let group = groups
            .entry((
                if record.model.is_empty() {
                    "unknown_model"
                } else {
                    &record.model
                }
                .into(),
                if record.price_version.is_empty() {
                    "unknown_price_version"
                } else {
                    &record.price_version
                }
                .into(),
            ))
            .or_default();
        if let Some(cost) = record.settled_microusd {
            total.add_settled(1, cost);
            group.add_settled(1, cost);
        } else {
            total.add_reserved(record.unknown, record.estimate_microusd);
            group.add_reserved(record.unknown, record.estimate_microusd);
        }
    }
    let groups = groups.into_iter().map(|((model, price_version), totals)| {
        json!({"model":model,"price_version":price_version,"totals":totals})
    }).collect::<Vec<_>>();
    json!({"totals":total,"by_model_price_version":groups})
}

fn snapshot_at(config: &Config, dir: &Path, day: &str) -> Value {
    let mut report = json!({
        "source":"local_persisted_snapshot",
        "day":day,
        "timezone":config.budget.timezone,
        "currency":"USD",
        "microusd_per_usd":1_000_000,
        "may_lag_running_session":true,
        "account_total":false,
        "account_usage_url":ACCOUNT_USAGE,
        "account_limits_url":ACCOUNT_LIMITS,
        "notice":"本地预留和未知费用不是已结算费用；账户费用与额度请查看官方入口。",
        "budget_enabled":config.budget.enabled,
        "daily_limit_microusd":(config.budget.daily_limit_usd * 1_000_000.0) as u64,
        "configured_model":config.openai.model,
        "configured_price_version":config.pricing.version,
    });
    match read_meter(dir) {
        Ok(meter) => {
            report["available"] = json!(true);
            report["ledger_present"] = json!(meter.is_some());
            let meter = meter.unwrap_or_default();
            let summary = summarize(&meter, day);
            report["totals"] = summary["totals"].clone();
            report["by_model_price_version"] = summary["by_model_price_version"].clone();
        }
        Err(_) => {
            report["available"] = json!(false);
            report["error"] = json!("local_usage_unavailable");
        }
    }
    report
}

/// Diagnostics remain available while the daemon owns Store's writer lock.
/// An atomic rename yields either complete persisted snapshot, with no claim
/// that an in-flight response or the account's other projects are included.
pub fn snapshot(config: &Config) -> Value {
    snapshot_at(config, &state_dir(), &day(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_eva_core::usage::{UsageRecord, UsageSummary};
    use std::os::unix::fs::{PermissionsExt, symlink};

    const DAY: &str = "2026-10-29";
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = [PathBuf::from("/dev/shm"), std::env::temp_dir()]
                .into_iter()
                .find_map(|base| {
                    let name = format!(
                        "eva-cost-{}-{}",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_nanos()
                    );
                    let path = base.join(name);
                    std::fs::create_dir(&path).ok().map(|()| path)
                })
                .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn write(&self, meter: &UsageMeter) {
            crate::store::atomic_json(&self.0.join("usage.json"), meter).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn record(day: &str, version: &str, settled: Option<u64>, unknown: bool) -> UsageRecord {
        serde_json::from_value(json!({"day":day,"model":"model-a","price_version":version,
            "phase":"plan","estimate_microusd":1000,"settled_microusd":settled,
            "unknown":unknown,"usage":null}))
        .unwrap()
    }
    #[test]
    fn cost_report_separates_settlement_reservations_unknowns_and_price_versions() {
        let mut meter = UsageMeter::default();
        meter
            .summaries
            .entry(DAY.into())
            .or_default()
            .entry("model-a".into())
            .or_default()
            .insert(
                "v1".into(),
                UsageSummary {
                    settled_records: 2,
                    settled_microusd: 300,
                    ..Default::default()
                },
            );
        meter.records.insert(
            "settled-private-id".into(),
            record(DAY, "v1", Some(70), false),
        );
        meter
            .records
            .insert("pending-private-id".into(), record(DAY, "v2", None, false));
        meter
            .records
            .insert("unknown-private-id".into(), record(DAY, "v2", None, true));
        meter.records.insert(
            "yesterday-private-id".into(),
            record("2026-10-28", "v1", Some(999), false),
        );
        let mut other_model = record(DAY, "v1", Some(100), false);
        other_model.model = "model-b".into();
        meter
            .records
            .insert("other-model-private-id".into(), other_model);
        let report = summarize(&meter, DAY);
        assert_eq!(report["totals"]["settled_microusd"], 470);
        assert_eq!(report["totals"]["settled_records"], 4);
        assert_eq!(report["totals"]["pending_reserved_microusd"], 1000);
        assert_eq!(report["totals"]["unknown_reserved_microusd"], 1000);
        assert_eq!(report["totals"]["unknown_records"], 1);
        assert_eq!(report["totals"]["reserved_microusd"], 2000);
        assert_eq!(
            report["totals"]["consumed_and_reserved_microusd"],
            meter.consumed_and_reserved(DAY)
        );
        assert_eq!(
            report["by_model_price_version"].as_array().unwrap().len(),
            3
        );
        assert_eq!(
            report["by_model_price_version"][0]["totals"]["settled_microusd"],
            370
        );
        assert!(!report.to_string().contains("private-id"));
    }
    #[test]
    fn cost_report_reads_with_daemon_writer_lock_and_never_changes_snapshot() {
        let fixture = Fixture::new();
        let mut writer = crate::store::Store::open_at(fixture.0.clone()).unwrap();
        writer
            .usage
            .records
            .insert("live".into(), record(DAY, "v1", None, false));
        writer.save().unwrap();
        let path = fixture.0.join("usage.json");
        let before = std::fs::read(&path).unwrap();
        assert!(crate::store::Store::open_at(fixture.0.clone()).is_err());
        let report = snapshot_at(&Config::default(), &fixture.0, DAY);
        assert_eq!(report["available"], true);
        assert_eq!(report["totals"]["pending_records"], 1);
        assert_eq!(report["totals"]["unknown_records"], 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!writer.usage.records["live"].unknown);
    }
    #[test]
    fn cost_report_absent_state_does_not_create_directories_or_infer_account_usage() {
        let fixture = Fixture::new();
        let absent = fixture.0.join("not-created");
        let report = snapshot_at(&Config::default(), &absent, DAY);
        assert_eq!(report["available"], true);
        assert_eq!(report["ledger_present"], false);
        assert_eq!(report["account_total"], false);
        assert_eq!(report["totals"]["settled_microusd"], 0);
        assert!(!absent.exists());
        assert!(
            report["account_usage_url"]
                .as_str()
                .unwrap()
                .starts_with("https://platform.openai.com/")
        );
    }
    #[test]
    fn cost_report_rejects_unsafe_and_invalid_files_without_leaking_their_contents() {
        let fixture = Fixture::new();
        let path = fixture.0.join("usage.json");
        fixture.write(&UsageMeter::default());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_meter(&fixture.0).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, "PRIVATE_MALFORMED_LEDGER").unwrap();
        let report = snapshot_at(&Config::default(), &fixture.0, DAY);
        assert_eq!(report["available"], false);
        assert!(report.get("totals").is_none());
        assert!(!report.to_string().contains("PRIVATE_MALFORMED_LEDGER"));
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_LEDGER_BYTES + 1)
            .unwrap();
        assert!(read_meter(&fixture.0).is_err());
        std::fs::remove_file(&path).unwrap();
        symlink("missing-target", &path).unwrap();
        assert!(read_meter(&fixture.0).is_err());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            unsafe {
                libc::mkfifo(
                    std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                        .unwrap()
                        .as_ptr(),
                    0o600,
                )
            },
            0
        );
        assert!(
            read_meter(&fixture.0).is_err(),
            "a FIFO must never block diagnostics"
        );
    }
}
