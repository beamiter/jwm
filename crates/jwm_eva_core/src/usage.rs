//! Local admission accounting. This is not an API billing limit.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rates {
    /// Micro-US-dollars per million tokens (one dollar = 1,000,000 microdollars).
    pub input_text: u64,
    pub input_audio: u64,
    pub cached_text: u64,
    pub cached_audio: u64,
    pub output_text: u64,
    pub output_audio: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_text: u64,
    pub input_audio: u64,
    pub cached_text: u64,
    pub cached_audio: u64,
    pub output_text: u64,
    pub output_audio: u64,
}
impl TokenUsage {
    pub fn from_realtime(value: &Value) -> Result<Self, String> {
        if !value.is_object() {
            return Err("usage_missing".into());
        }
        fn count(v: &Value, key: &str) -> Result<u64, String> {
            match v.get(key) {
                None => Ok(0),
                Some(v) => v.as_u64().ok_or_else(|| format!("invalid_usage:{key}")),
            }
        }
        let input = &value["input_token_details"];
        let output = &value["output_token_details"];
        if !input.is_object() || !output.is_object() {
            return Err("usage_breakdown_missing".into());
        }
        let cached = &input["cached_tokens_details"];
        let input_audio = count(input, "audio_tokens")?;
        let input_text = count(input, "text_tokens")?;
        let cached_audio = count(cached, "audio_tokens")?;
        let cached_text = count(cached, "text_tokens")?;
        if value["input_tokens"]
            .as_u64()
            .is_some_and(|n| n > input_audio.saturating_add(input_text))
            || value["output_tokens"].as_u64().is_some_and(|n| {
                n > count(output, "audio_tokens")
                    .unwrap_or(0)
                    .saturating_add(count(output, "text_tokens").unwrap_or(0))
            })
        {
            return Err("usage_breakdown_incomplete".into());
        }
        if cached_audio > input_audio || cached_text > input_text {
            return Err("cached_usage_exceeds_input".into());
        }
        Ok(Self {
            input_text: input_text - cached_text,
            input_audio: input_audio - cached_audio,
            cached_text,
            cached_audio,
            output_text: count(output, "text_tokens")?,
            output_audio: count(output, "audio_tokens")?,
        })
    }
    pub fn cost_microusd(&self, rates: Rates) -> u64 {
        let weighted = self.input_text as u128 * rates.input_text as u128
            + self.input_audio as u128 * rates.input_audio as u128
            + self.cached_text as u128 * rates.cached_text as u128
            + self.cached_audio as u128 * rates.cached_audio as u128
            + self.output_text as u128 * rates.output_text as u128
            + self.output_audio as u128 * rates.output_audio as u128;
        // Round up, so short responses do not evade budget admission.
        weighted.div_ceil(1_000_000).min(u64::MAX as u128) as u64
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default)]
    pub settled_ms: Option<u64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub duration_microusd_per_second: u64,
    #[serde(default)]
    pub price_version: String,
    #[serde(default)]
    pub rates: Rates,
    pub day: String,
    pub phase: String,
    pub estimate_microusd: u64,
    pub settled_microusd: Option<u64>,
    pub unknown: bool,
    pub usage: Option<TokenUsage>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageSummary {
    pub settled_records: u64,
    pub settled_microusd: u64,
    pub usage: TokenUsage,
    pub duration_seconds: f64,
}
type PriceSummaries = BTreeMap<String, UsageSummary>;
type ModelSummaries = BTreeMap<String, PriceSummaries>;
type DaySummaries = BTreeMap<String, ModelSummaries>;
fn deserialize_summaries<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<DaySummaries, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Format {
        Current(DaySummaries),
        Legacy(BTreeMap<String, PriceSummaries>),
    }
    Ok(match Format::deserialize(deserializer)? {
        Format::Current(summaries) => summaries,
        Format::Legacy(days) => days
            .into_iter()
            .map(|(day, versions)| (day, BTreeMap::from([("unknown_model".into(), versions)])))
            .collect(),
    })
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UsageMeter {
    pub records: BTreeMap<String, UsageRecord>,
    /// Exact settled totals by local billing day, model and price version.
    #[serde(default, deserialize_with = "deserialize_summaries")]
    pub summaries: DaySummaries,
    /// Bounded recent receipt index; older missing receipts are ignored too.
    #[serde(default)]
    pub archived_ids: VecDeque<String>,
}
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u64::MAX as u128) as u64
        })
}
impl UsageMeter {
    pub fn is_archived(&self, id: &str) -> bool {
        self.archived_ids.iter().any(|known| known == id)
    }
    pub fn consumed_and_reserved(&self, day: &str) -> u64 {
        let settled = self
            .summaries
            .get(day)
            .into_iter()
            .flat_map(|models| models.values())
            .flat_map(|versions| versions.values())
            .fold(0u64, |total, summary| {
                total.saturating_add(summary.settled_microusd)
            });
        self.records
            .values()
            .filter(|r| r.day == day)
            .fold(settled, |total, r| {
                total.saturating_add(r.settled_microusd.unwrap_or(r.estimate_microusd))
            })
    }
    pub fn reserve(
        &mut self,
        id: &str,
        day: &str,
        phase: &str,
        estimate_microusd: u64,
        limit_microusd: u64,
    ) -> Result<(), String> {
        self.reserve_at(id, day, phase, estimate_microusd, limit_microusd, wall_ms())
    }
    pub fn reserve_at(
        &mut self,
        id: &str,
        day: &str,
        phase: &str,
        estimate_microusd: u64,
        limit_microusd: u64,
        now_ms: u64,
    ) -> Result<(), String> {
        if self.records.contains_key(id) || self.is_archived(id) {
            return Err("duplicate_usage_reservation".into());
        }
        self.compact(now_ms, false)?;
        if self.records.len() >= 4096 {
            self.compact(now_ms, true)?;
        }
        if self.records.len() >= 4096 {
            return Err("usage_capacity_exceeded".into());
        }
        if self
            .consumed_and_reserved(day)
            .saturating_add(estimate_microusd)
            > limit_microusd
        {
            return Err("budget_exceeded".into());
        }
        self.records.insert(
            id.into(),
            UsageRecord {
                model: String::new(),
                created_ms: now_ms,
                settled_ms: None,
                duration_seconds: None,
                duration_microusd_per_second: 0,
                price_version: String::new(),
                rates: Rates::default(),
                day: day.into(),
                phase: phase.into(),
                estimate_microusd,
                settled_microusd: None,
                unknown: false,
                usage: None,
            },
        );
        Ok(())
    }
    /// Keep recent receipts while space permits. Under pressure, settled
    /// entries can be summarized immediately; unknown/in-flight entries stay.
    pub fn compact(&mut self, now_ms: u64, force: bool) -> Result<usize, String> {
        let candidates = self
            .records
            .iter()
            .filter(|(_, record)| {
                !record.unknown
                    && record.settled_microusd.is_some()
                    && (force
                        || now_ms.saturating_sub(record.settled_ms.unwrap_or(record.created_ms))
                            > 600_000)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut removed = 0;
        for id in candidates {
            let record = &self.records[&id];
            let model = if record.model.is_empty() {
                "unknown_model"
            } else {
                record.model.as_str()
            };
            let mut summary = self
                .summaries
                .get(&record.day)
                .and_then(|models| models.get(model))
                .and_then(|versions| versions.get(&record.price_version))
                .cloned()
                .unwrap_or_default();
            summary.settled_records = summary
                .settled_records
                .checked_add(1)
                .ok_or("usage_summary_overflow")?;
            summary.settled_microusd = summary
                .settled_microusd
                .checked_add(record.settled_microusd.unwrap())
                .ok_or("usage_summary_overflow")?;
            if let Some(usage) = record.usage.as_ref() {
                macro_rules! add {($($field:ident),*)=>{$(summary.usage.$field=summary.usage.$field.checked_add(usage.$field).ok_or("usage_summary_overflow")?;)*};}
                add!(
                    input_text,
                    input_audio,
                    cached_text,
                    cached_audio,
                    output_text,
                    output_audio
                );
            }
            summary.duration_seconds += record.duration_seconds.unwrap_or(0.0);
            if !summary.duration_seconds.is_finite() {
                return Err("usage_summary_overflow".into());
            }
            self.summaries
                .entry(record.day.clone())
                .or_default()
                .entry(model.into())
                .or_default()
                .insert(record.price_version.clone(), summary);
            self.records.remove(&id);
            self.archived_ids.push_back(id);
            while self.archived_ids.len() > 4096 {
                self.archived_ids.pop_front();
            }
            removed += 1;
        }
        Ok(removed)
    }
    /// Returns false for duplicate settlement; never charges the event twice.
    pub fn settle(&mut self, id: &str, usage: TokenUsage, rates: Rates) -> Result<bool, String> {
        let Some(record) = self.records.get_mut(id) else {
            return Ok(false);
        };
        if record.settled_microusd.is_some() {
            return Ok(false);
        }
        record.settled_microusd = Some(usage.cost_microusd(rates));
        record.usage = Some(usage);
        record.unknown = false;
        record.settled_ms = Some(wall_ms());
        Ok(true)
    }
    /// Duration-based input transcription is billed separately from responses.
    pub fn settle_duration(&mut self, id: &str, seconds: f64) -> Result<bool, String> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("invalid_duration_usage".into());
        }
        let Some(record) = self.records.get_mut(id) else {
            return Ok(false);
        };
        if record.settled_microusd.is_some() {
            return Ok(false);
        }
        let cost = (seconds * record.duration_microusd_per_second as f64).ceil();
        if !cost.is_finite() || cost > u64::MAX as f64 {
            return Err("duration_usage_overflow".into());
        }
        record.duration_seconds = Some(seconds);
        record.settled_microusd = Some(cost as u64);
        record.unknown = false;
        record.settled_ms = Some(wall_ms());
        Ok(true)
    }
    pub fn mark_unknown(&mut self, id: &str) -> Result<(), String> {
        let r = self
            .records
            .get_mut(id)
            .ok_or("unregistered_response_usage")?;
        if r.settled_microusd.is_none() {
            r.unknown = true;
        }
        Ok(())
    }
    /// Server response IDs may arrive after local admission under request IDs.
    pub fn associate_response(
        &mut self,
        request_id: &str,
        response_id: &str,
    ) -> Result<(), String> {
        if request_id == response_id {
            return Ok(());
        }
        if self.records.contains_key(response_id) || self.is_archived(response_id) {
            return Err("duplicate_response_id".into());
        }
        let record = self
            .records
            .remove(request_id)
            .ok_or("unknown_request_id")?;
        self.records.insert(response_id.into(), record);
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_unknown_requests_keep_reservation_and_late_receipts_deduplicate() {
        let mut m = UsageMeter::default();
        m.reserve("r1", "2026-10-29", "plan", 600, 1000).unwrap();
        m.mark_unknown("r1").unwrap();
        assert!(
            m.reserve("r2", "2026-10-29", "feedback", 500, 1000)
                .is_err()
        );
        let u = TokenUsage {
            output_text: 100,
            ..Default::default()
        };
        let r = Rates {
            output_text: 2_000_000,
            ..Default::default()
        };
        assert!(m.settle("r1", u.clone(), r).unwrap());
        assert!(!m.settle("r1", u, r).unwrap());
        assert_eq!(m.consumed_and_reserved("2026-10-29"), 200);
    }
    #[test]
    fn cached_audio_is_not_counted_twice() {
        let u=TokenUsage::from_realtime(&serde_json::json!({"input_token_details":{"audio_tokens":100,"text_tokens":20,"cached_tokens_details":{"audio_tokens":30,"text_tokens":5}},"output_token_details":{"audio_tokens":40,"text_tokens":10}})).unwrap();
        assert_eq!(u.input_audio, 70);
        assert_eq!(u.cached_audio, 30);
        assert_eq!(u.input_text, 15);
    }
    #[test]
    fn absent_or_incomplete_usage_is_unknown_not_free() {
        assert!(TokenUsage::from_realtime(&serde_json::json!({"input_tokens":100})).is_err());
        assert!(TokenUsage::from_realtime(&serde_json::json!({"input_tokens":100,"input_token_details":{"text_tokens":10},"output_token_details":{}})).is_err());
    }
    #[test]
    fn transcription_duration_uses_separate_item_record_and_deduplicates() {
        let mut meter = UsageMeter::default();
        meter
            .reserve("asr:item:0", "2026-10-29", "transcription", 1500, 1500)
            .unwrap();
        meter
            .records
            .get_mut("asr:item:0")
            .unwrap()
            .duration_microusd_per_second = 75;
        meter.mark_unknown("asr:item:0").unwrap();
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 1500);
        assert!(meter.settle_duration("asr:item:0", 10.5).unwrap());
        assert!(!meter.settle_duration("asr:item:0", 10.5).unwrap());
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 788);
        assert!(!meter.records["asr:item:0"].unknown);
    }
    #[test]
    fn thousands_of_settled_responses_compact_without_reducing_any_budget() {
        let mut meter = UsageMeter::default();
        let usage = TokenUsage {
            output_text: 1,
            ..Default::default()
        };
        let rates = Rates {
            output_text: 1_000_000,
            ..Default::default()
        };
        for number in 0..5000 {
            let id = format!("response-{number}");
            meter
                .reserve_at(&id, "2026-10-29", "plan", 2, 10_000, number)
                .unwrap();
            meter.records.get_mut(&id).unwrap().price_version =
                if number % 2 == 0 { "v1" } else { "v2" }.into();
            meter.settle(&id, usage.clone(), rates).unwrap();
        }
        assert!(meter.records.len() < 4096);
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 5000);
        assert!(!meter.settle("response-0", usage, rates).unwrap());
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 5000);
        meter.compact(u64::MAX, false).unwrap();
        assert!(meter.records.is_empty());
        for version in ["v1", "v2"] {
            let summary = &meter.summaries["2026-10-29"]["unknown_model"][version];
            assert_eq!(summary.settled_records, 2500);
            assert_eq!(summary.settled_microusd, 2500);
            assert_eq!(summary.usage.output_text, 2500);
        }
        let restored: UsageMeter =
            serde_json::from_slice(&serde_json::to_vec(&meter).unwrap()).unwrap();
        assert_eq!(restored.consumed_and_reserved("2026-10-29"), 5000);
        assert!(
            meter
                .reserve_at(
                    "over-budget",
                    "2026-10-29",
                    "feedback",
                    5001,
                    10_000,
                    u64::MAX
                )
                .is_err()
        );
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 5000);
    }
    #[test]
    fn capacity_pressure_never_discards_inflight_or_unknown_usage() {
        let mut meter = UsageMeter::default();
        for number in 0..4096 {
            let id = format!("pending-{number}");
            meter
                .reserve_at(&id, "2026-10-29", "plan", 1, u64::MAX, 0)
                .unwrap();
            if number % 2 == 0 {
                meter.mark_unknown(&id).unwrap();
            }
        }
        assert_eq!(meter.compact(u64::MAX, true).unwrap(), 0);
        assert_eq!(meter.records.len(), 4096);
        assert!(meter.records["pending-0"].unknown);
        assert!(
            meter
                .reserve_at("new", "2026-10-29", "plan", 1, u64::MAX, u64::MAX)
                .is_err()
        );
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 4096);
        meter
            .settle(
                "pending-0",
                TokenUsage {
                    output_text: 1,
                    ..Default::default()
                },
                Rates {
                    output_text: 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
        meter
            .reserve_at("new", "2026-10-29", "plan", 1, u64::MAX, u64::MAX)
            .unwrap();
        assert!(meter.records.contains_key("pending-1"));
        assert!(meter.records["pending-2"].unknown);
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 4097);
    }
    #[test]
    fn duration_totals_survive_summary_and_late_receipts_do_not_bill_again() {
        let mut meter = UsageMeter::default();
        meter
            .reserve_at("asr", "2026-10-29", "transcription", 1500, 2000, 1)
            .unwrap();
        let record = meter.records.get_mut("asr").unwrap();
        record.price_version = "transcribe-v1".into();
        record.duration_microusd_per_second = 75;
        meter.settle_duration("asr", 10.0).unwrap();
        meter.compact(u64::MAX, true).unwrap();
        let summary = &meter.summaries["2026-10-29"]["unknown_model"]["transcribe-v1"];
        assert_eq!(summary.duration_seconds, 10.0);
        assert_eq!(summary.settled_microusd, 750);
        assert!(!meter.settle_duration("asr", 10.0).unwrap());
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 750);
        assert_eq!(meter.consumed_and_reserved("2026-10-30"), 0);
    }
    #[test]
    fn models_are_separate_and_legacy_summaries_keep_their_cost() {
        let mut meter = UsageMeter::default();
        for (model, price) in [("model-a", 1_000_000), ("model-b", 2_000_000)] {
            meter
                .reserve_at(model, "2026-10-29", "plan", 5, 100, 0)
                .unwrap();
            let record = meter.records.get_mut(model).unwrap();
            record.model = model.into();
            record.price_version = "shared-version".into();
            meter
                .settle(
                    model,
                    TokenUsage {
                        output_text: 1,
                        ..Default::default()
                    },
                    Rates {
                        output_text: price,
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        meter.compact(u64::MAX, true).unwrap();
        assert_eq!(
            meter.summaries["2026-10-29"]["model-a"]["shared-version"].settled_microusd,
            1
        );
        assert_eq!(
            meter.summaries["2026-10-29"]["model-b"]["shared-version"].settled_microusd,
            2
        );
        assert_eq!(meter.consumed_and_reserved("2026-10-29"), 3);
        let legacy:UsageMeter=serde_json::from_value(serde_json::json!({"records":{},"summaries":{"2026-10-29":{"old-version":{"settled_records":2,"settled_microusd":17,"usage":TokenUsage::default(),"duration_seconds":0.0}}}})).unwrap();
        assert_eq!(legacy.consumed_and_reserved("2026-10-29"), 17);
        assert_eq!(
            legacy.summaries["2026-10-29"]["unknown_model"]["old-version"].settled_records,
            2
        );
    }
}
