//! Phase 5 performance contract: versioned baselines, system labels, and
//! regression budgets.
//!
//! A baseline is a machine-readable snapshot of the contract's benchmark
//! scenarios, permanently attached to the identity of the system that
//! produced it. Comparison is only defined between two results carrying the
//! same complete label — the contract's rule that unlabeled results from
//! different systems must never be compared as if they were the same
//! benchmark is enforced here, not left to reviewer discipline.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

/// The seven contract scenarios, in roadmap order.
pub const SCENARIOS: &[&str] = &[
    "idle",
    "steady_frame",
    "damage_redraw",
    "input_latency",
    "allocation_steady",
    "multi_monitor",
    "direct_scanout",
];

/// Identity of the measured system. Two results are comparable only when
/// every field matches exactly and none is empty or `"unknown"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemLabel {
    pub cpu: String,
    pub gpu: String,
    pub driver: String,
    pub kernel: String,
    pub backend: String,
    pub renderer_api: String,
    pub resolution: String,
    /// Hash of the effective configuration; configuration changes create a
    /// different benchmark, not a comparable run.
    pub config_fingerprint: String,
}

impl SystemLabel {
    /// Label fields still unknown; a non-empty result makes the baseline
    /// unusable for comparison.
    #[must_use]
    pub fn incomplete_fields(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        for (name, value) in [
            ("cpu", &self.cpu),
            ("gpu", &self.gpu),
            ("driver", &self.driver),
            ("kernel", &self.kernel),
            ("backend", &self.backend),
            ("renderer_api", &self.renderer_api),
            ("resolution", &self.resolution),
            ("config_fingerprint", &self.config_fingerprint),
        ] {
            if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("unknown") {
                missing.push(name);
            }
        }
        missing
    }

    /// Fields that differ from `other`, by name.
    #[must_use]
    pub fn differing_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut differs = Vec::new();
        for (name, a, b) in [
            ("cpu", &self.cpu, &other.cpu),
            ("gpu", &self.gpu, &other.gpu),
            ("driver", &self.driver, &other.driver),
            ("kernel", &self.kernel, &other.kernel),
            ("backend", &self.backend, &other.backend),
            ("renderer_api", &self.renderer_api, &other.renderer_api),
            ("resolution", &self.resolution, &other.resolution),
            (
                "config_fingerprint",
                &self.config_fingerprint,
                &other.config_fingerprint,
            ),
        ] {
            if a != b {
                differs.push(name);
            }
        }
        differs
    }

    /// Short filesystem-safe identifier for baseline file names.
    #[must_use]
    pub fn slug(&self) -> String {
        let sanitize = |value: &str| {
            value
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .split('-')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("-")
        };
        let gpu = sanitize(&self.gpu);
        let gpu_short: String = gpu.chars().take(24).collect();
        format!(
            "{}-{}-{}",
            sanitize(&self.backend),
            sanitize(&self.renderer_api),
            gpu_short.trim_end_matches('-')
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScenarioStatus {
    Recorded,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub status: ScenarioStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metrics: BTreeMap<String, f64>,
}

impl ScenarioResult {
    #[must_use]
    pub fn recorded(metrics: BTreeMap<String, f64>) -> Self {
        Self {
            status: ScenarioStatus::Recorded,
            reason: None,
            metrics,
        }
    }

    #[must_use]
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self {
            status: ScenarioStatus::Skipped,
            reason: Some(reason.into()),
            metrics: BTreeMap::new(),
        }
    }
}

/// One labeled performance snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerfBaselineV1 {
    pub schema_version: u32,
    pub recorded_at: String,
    pub jwm_version: String,
    pub label: SystemLabel,
    pub scenarios: BTreeMap<String, ScenarioResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Candidate must not exceed baseline × ratio (frame times, latency, …).
    LowerIsBetter,
    /// Candidate must not fall below baseline × ratio (frame rate).
    HigherIsBetter,
    /// Candidate must equal the baseline (structural facts such as monitor
    /// count and refresh rate).
    Exact,
}

/// A regression budget for one metric of one scenario.
#[derive(Clone, Debug, Serialize)]
pub struct BudgetRule {
    pub scenario: &'static str,
    pub metric: &'static str,
    pub direction: Direction,
    /// Allowed candidate/baseline ratio (>= 1.0 for lower-is-better head
    /// room, <= 1.0 for higher-is-better floors, ignored for `Exact`).
    pub ratio: f64,
    /// Absolute guard rail applied to the candidate regardless of the
    /// baseline value: a cap for lower-is-better metrics, a floor for
    /// higher-is-better ones.
    pub absolute: Option<f64>,
}

/// The version-1 regression budgets. Ratios bound drift against the
/// baseline; absolute values are generous guard rails calibrated from the
/// first recorded baseline (see docs/performance.md).
#[must_use]
pub fn default_budgets() -> Vec<BudgetRule> {
    use Direction::{Exact, HigherIsBetter, LowerIsBetter};
    let rule = |scenario, metric, direction, ratio, absolute| BudgetRule {
        scenario,
        metric,
        direction,
        ratio,
        absolute,
    };
    vec![
        rule("idle", "cpu_percent_avg", LowerIsBetter, 1.50, Some(10.0)),
        rule("idle", "wakeups_per_s", LowerIsBetter, 1.50, Some(600.0)),
        rule("idle", "rss_mb", LowerIsBetter, 1.30, Some(2048.0)),
        rule(
            "steady_frame",
            "frame_time_avg_ms",
            LowerIsBetter,
            1.10,
            None,
        ),
        rule(
            "steady_frame",
            "frame_time_p50_ms",
            LowerIsBetter,
            1.10,
            None,
        ),
        rule(
            "steady_frame",
            "frame_time_p95_ms",
            LowerIsBetter,
            1.15,
            None,
        ),
        rule(
            "steady_frame",
            "frame_time_p99_ms",
            LowerIsBetter,
            1.25,
            None,
        ),
        rule("steady_frame", "fps_avg", HigherIsBetter, 0.90, None),
        rule(
            "damage_redraw",
            "dirty_fraction_avg_percent",
            LowerIsBetter,
            1.25,
            Some(100.0),
        ),
        rule(
            "damage_redraw",
            "dirty_regions_avg",
            LowerIsBetter,
            1.50,
            None,
        ),
        rule(
            "input_latency",
            "input_latency_p50_ms",
            LowerIsBetter,
            1.10,
            None,
        ),
        rule(
            "input_latency",
            "input_latency_p95_ms",
            LowerIsBetter,
            1.15,
            None,
        ),
        rule(
            "input_latency",
            "input_latency_p99_ms",
            LowerIsBetter,
            1.25,
            None,
        ),
        rule(
            "allocation_steady",
            "allocs_per_frame",
            LowerIsBetter,
            1.15,
            None,
        ),
        rule("multi_monitor", "monitor_count", Exact, 1.0, None),
        rule("multi_monitor", "refresh_hz", Exact, 1.0, None),
        rule(
            "direct_scanout",
            "scanout_toggles_per_minute",
            LowerIsBetter,
            2.00,
            Some(120.0),
        ),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictOutcome {
    Pass,
    Violation,
    NotComparable,
}

#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    pub scenario: String,
    pub metric: String,
    pub outcome: VerdictOutcome,
    pub baseline: Option<f64>,
    pub candidate: Option<f64>,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompareReport {
    pub passed: bool,
    pub verdicts: Vec<Verdict>,
}

/// Evaluate `candidate` against `baseline` under `budgets`.
///
/// # Errors
///
/// Refuses (rather than reports) when the results are not comparable at all:
/// mismatched schema versions, an incomplete label on either side, or labels
/// that identify different systems or configurations.
pub fn compare(
    baseline: &PerfBaselineV1,
    candidate: &PerfBaselineV1,
    budgets: &[BudgetRule],
) -> Result<CompareReport, String> {
    for (role, snapshot) in [("baseline", baseline), ("candidate", candidate)] {
        if snapshot.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "{role} uses schema version {}, this tool understands {}",
                snapshot.schema_version, SCHEMA_VERSION
            ));
        }
        let incomplete = snapshot.label.incomplete_fields();
        if !incomplete.is_empty() {
            return Err(format!(
                "{role} is not fully labeled (missing {}); unlabeled results \
                 must never be compared",
                incomplete.join(", ")
            ));
        }
    }
    let differing = baseline.label.differing_fields(&candidate.label);
    if !differing.is_empty() {
        return Err(format!(
            "labels identify different systems (differing: {}); results from \
             different systems are different benchmarks",
            differing.join(", ")
        ));
    }

    let mut verdicts = Vec::new();
    for rule in budgets {
        verdicts.push(evaluate(baseline, candidate, rule));
    }
    let passed = verdicts
        .iter()
        .all(|verdict| verdict.outcome != VerdictOutcome::Violation);
    Ok(CompareReport { passed, verdicts })
}

fn metric_of(
    snapshot: &PerfBaselineV1,
    scenario: &str,
    metric: &str,
) -> Result<Option<f64>, String> {
    match snapshot.scenarios.get(scenario) {
        None => Err(format!("scenario {scenario} absent")),
        Some(result) if result.status == ScenarioStatus::Skipped => Err(result
            .reason
            .clone()
            .unwrap_or_else(|| format!("scenario {scenario} skipped"))),
        Some(result) => Ok(result.metrics.get(metric).copied()),
    }
}

fn evaluate(baseline: &PerfBaselineV1, candidate: &PerfBaselineV1, rule: &BudgetRule) -> Verdict {
    let verdict = |outcome, base: Option<f64>, cand: Option<f64>, detail: String| Verdict {
        scenario: rule.scenario.to_string(),
        metric: rule.metric.to_string(),
        outcome,
        baseline: base,
        candidate: cand,
        detail,
    };

    let base = match metric_of(baseline, rule.scenario, rule.metric) {
        Ok(Some(value)) => value,
        Ok(None) => {
            return verdict(
                VerdictOutcome::NotComparable,
                None,
                None,
                "metric absent from baseline".into(),
            );
        }
        Err(reason) => {
            return verdict(
                VerdictOutcome::NotComparable,
                None,
                None,
                format!("baseline: {reason}"),
            );
        }
    };
    let cand = match metric_of(candidate, rule.scenario, rule.metric) {
        Ok(Some(value)) => value,
        Ok(None) => {
            return verdict(
                VerdictOutcome::NotComparable,
                Some(base),
                None,
                "metric absent from candidate".into(),
            );
        }
        Err(reason) => {
            return verdict(
                VerdictOutcome::NotComparable,
                Some(base),
                None,
                format!("candidate: {reason}"),
            );
        }
    };

    // Absolute guard rail first: it applies independent of the baseline.
    if let Some(limit) = rule.absolute {
        let breached = match rule.direction {
            Direction::LowerIsBetter => cand > limit,
            Direction::HigherIsBetter => cand < limit,
            Direction::Exact => false,
        };
        if breached {
            return verdict(
                VerdictOutcome::Violation,
                Some(base),
                Some(cand),
                format!("candidate {cand:.3} breaches the absolute limit {limit:.3}"),
            );
        }
    }

    let outcome = match rule.direction {
        Direction::Exact => {
            if (cand - base).abs() <= 1e-9 {
                (VerdictOutcome::Pass, format!("{cand:.3} matches exactly"))
            } else {
                (
                    VerdictOutcome::Violation,
                    format!("{cand:.3} differs from required exact {base:.3}"),
                )
            }
        }
        Direction::LowerIsBetter => {
            if base == 0.0 {
                // No meaningful ratio; the absolute rail above is the only
                // enforceable bound, so a zero baseline passes by ratio.
                (
                    VerdictOutcome::Pass,
                    format!("baseline 0; candidate {cand:.3} bounded only by the absolute limit"),
                )
            } else if cand <= base * rule.ratio {
                (
                    VerdictOutcome::Pass,
                    format!(
                        "{cand:.3} within {:.0}% of baseline {base:.3}",
                        (rule.ratio - 1.0) * 100.0
                    ),
                )
            } else {
                (
                    VerdictOutcome::Violation,
                    format!(
                        "{cand:.3} exceeds baseline {base:.3} by more than the {:.0}% budget",
                        (rule.ratio - 1.0) * 100.0
                    ),
                )
            }
        }
        Direction::HigherIsBetter => {
            if cand >= base * rule.ratio {
                (
                    VerdictOutcome::Pass,
                    format!(
                        "{cand:.3} keeps at least {:.0}% of baseline {base:.3}",
                        rule.ratio * 100.0
                    ),
                )
            } else {
                (
                    VerdictOutcome::Violation,
                    format!(
                        "{cand:.3} falls below {:.0}% of baseline {base:.3}",
                        rule.ratio * 100.0
                    ),
                )
            }
        }
    };
    verdict(outcome.0, Some(base), Some(cand), outcome.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label() -> SystemLabel {
        SystemLabel {
            cpu: "Test CPU".into(),
            gpu: "Test GPU".into(),
            driver: "555.0".into(),
            kernel: "6.17.0".into(),
            backend: "xcb".into(),
            renderer_api: "glx/opengl".into(),
            resolution: "2560x1440".into(),
            config_fingerprint: "abcd1234".into(),
        }
    }

    fn snapshot(scenarios: &[(&str, ScenarioResult)]) -> PerfBaselineV1 {
        PerfBaselineV1 {
            schema_version: SCHEMA_VERSION,
            recorded_at: "2026-07-25T00:00:00Z".into(),
            jwm_version: "0.2.0".into(),
            label: label(),
            scenarios: scenarios
                .iter()
                .map(|(name, result)| ((*name).to_string(), result.clone()))
                .collect(),
        }
    }

    fn recorded(metrics: &[(&str, f64)]) -> ScenarioResult {
        ScenarioResult::recorded(
            metrics
                .iter()
                .map(|(name, value)| ((*name).to_string(), *value))
                .collect(),
        )
    }

    #[test]
    fn baselines_round_trip_through_json() {
        let baseline = snapshot(&[
            ("idle", recorded(&[("cpu_percent_avg", 1.5)])),
            ("steady_frame", ScenarioResult::skipped("no compositor")),
        ]);
        let encoded = serde_json::to_string_pretty(&baseline).unwrap();
        let decoded: PerfBaselineV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, baseline);
    }

    #[test]
    fn unlabeled_results_are_refused() {
        let mut unlabeled = snapshot(&[]);
        unlabeled.label.gpu = "unknown".into();
        let complete = snapshot(&[]);
        let error = compare(&unlabeled, &complete, &default_budgets()).unwrap_err();
        assert!(error.contains("not fully labeled"));
        assert!(error.contains("gpu"));
        let error = compare(&complete, &unlabeled, &default_budgets()).unwrap_err();
        assert!(error.contains("candidate"));
    }

    #[test]
    fn results_from_different_systems_are_refused() {
        let baseline = snapshot(&[]);
        let mut other = snapshot(&[]);
        other.label.gpu = "Other GPU".into();
        other.label.config_fingerprint = "ffff0000".into();
        let error = compare(&baseline, &other, &default_budgets()).unwrap_err();
        assert!(error.contains("different systems"));
        assert!(error.contains("gpu"));
        assert!(error.contains("config_fingerprint"));
    }

    #[test]
    fn schema_version_mismatch_is_refused() {
        let baseline = snapshot(&[]);
        let mut future = snapshot(&[]);
        future.schema_version = SCHEMA_VERSION + 1;
        let error = compare(&baseline, &future, &default_budgets()).unwrap_err();
        assert!(error.contains("schema version"));
    }

    #[test]
    fn a_regression_beyond_budget_is_a_violation() {
        let baseline = snapshot(&[(
            "steady_frame",
            recorded(&[("frame_time_p95_ms", 2.0), ("fps_avg", 60.0)]),
        )]);
        let candidate = snapshot(&[(
            "steady_frame",
            recorded(&[("frame_time_p95_ms", 2.5), ("fps_avg", 60.0)]),
        )]);
        let report = compare(&baseline, &candidate, &default_budgets()).unwrap();
        assert!(!report.passed);
        let violation = report
            .verdicts
            .iter()
            .find(|verdict| verdict.metric == "frame_time_p95_ms")
            .unwrap();
        assert_eq!(violation.outcome, VerdictOutcome::Violation);
    }

    #[test]
    fn drift_within_budget_passes() {
        let baseline = snapshot(&[(
            "steady_frame",
            recorded(&[("frame_time_p95_ms", 2.0), ("fps_avg", 60.0)]),
        )]);
        let candidate = snapshot(&[(
            "steady_frame",
            recorded(&[("frame_time_p95_ms", 2.2), ("fps_avg", 58.0)]),
        )]);
        let report = compare(&baseline, &candidate, &default_budgets()).unwrap();
        assert!(report.passed);
    }

    #[test]
    fn higher_is_better_floors_frame_rate() {
        let baseline = snapshot(&[("steady_frame", recorded(&[("fps_avg", 60.0)]))]);
        let candidate = snapshot(&[("steady_frame", recorded(&[("fps_avg", 50.0)]))]);
        let report = compare(&baseline, &candidate, &default_budgets()).unwrap();
        assert!(!report.passed);
    }

    #[test]
    fn exact_metrics_must_match() {
        let baseline = snapshot(&[(
            "multi_monitor",
            recorded(&[("monitor_count", 2.0), ("refresh_hz", 144.0)]),
        )]);
        let candidate = snapshot(&[(
            "multi_monitor",
            recorded(&[("monitor_count", 1.0), ("refresh_hz", 144.0)]),
        )]);
        let report = compare(&baseline, &candidate, &default_budgets()).unwrap();
        assert!(!report.passed);
    }

    #[test]
    fn absolute_limits_bind_even_against_a_zero_baseline() {
        let baseline = snapshot(&[(
            "direct_scanout",
            recorded(&[("scanout_toggles_per_minute", 0.0)]),
        )]);
        let flapping = snapshot(&[(
            "direct_scanout",
            recorded(&[("scanout_toggles_per_minute", 500.0)]),
        )]);
        let report = compare(&baseline, &flapping, &default_budgets()).unwrap();
        assert!(!report.passed);

        let quiet = snapshot(&[(
            "direct_scanout",
            recorded(&[("scanout_toggles_per_minute", 4.0)]),
        )]);
        let report = compare(&baseline, &quiet, &default_budgets()).unwrap();
        assert!(report.passed);
    }

    #[test]
    fn skipped_scenarios_are_not_comparable_but_not_failures() {
        let baseline = snapshot(&[(
            "allocation_steady",
            ScenarioResult::skipped("allocation counter not compiled in"),
        )]);
        let candidate = snapshot(&[(
            "allocation_steady",
            ScenarioResult::skipped("allocation counter not compiled in"),
        )]);
        let report = compare(&baseline, &candidate, &default_budgets()).unwrap();
        assert!(report.passed);
        let alloc = report
            .verdicts
            .iter()
            .find(|verdict| verdict.scenario == "allocation_steady")
            .unwrap();
        assert_eq!(alloc.outcome, VerdictOutcome::NotComparable);
    }

    #[test]
    fn every_budget_rule_targets_a_contract_scenario() {
        for rule in default_budgets() {
            assert!(
                SCENARIOS.contains(&rule.scenario),
                "budget rule references unknown scenario {}",
                rule.scenario
            );
        }
    }

    #[test]
    fn slugs_are_filesystem_safe() {
        let slug = label().slug();
        assert_eq!(slug, "xcb-glx-opengl-test-gpu");
        assert!(
            slug.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }
}
