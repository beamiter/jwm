//! Offline metrics from Eva's own telemetry records (the user journal or a
//! file). Inputs are the fixed-category JSON events the daemon prints; the
//! report holds counts, latencies and costs only. No audio, transcript or
//! model text exists in those records, and nothing is sent anywhere.
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
struct Turn {
    committed: Option<u64>,
    validated: Option<u64>,
    verified: Option<u64>,
    verified_result: Option<String>,
    first_audio: Option<u64>,
    playback_end: Option<u64>,
    feedback_done: Option<u64>,
    responses: u64,
    rejections: u64,
    normalized: u64,
    response_ids: Vec<String>,
}
#[derive(Default)]
struct Session {
    activated: Option<u64>,
    ready: Option<u64>,
    cloud: Option<String>,
    failed: Option<String>,
}

fn percentile(sorted: &[u64], percent: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() as u64 * percent).div_ceil(100).max(1) as usize;
    sorted[rank.min(sorted.len()) - 1]
}
fn distribution(mut values: Vec<u64>) -> Value {
    values.sort_unstable();
    json!({"count":values.len(),"p50":percentile(&values,50),"p95":percentile(&values,95),
        "max":values.last().copied().unwrap_or(0)})
}
fn delta(later: Option<u64>, earlier: Option<u64>) -> Option<u64> {
    match (later, earlier) {
        (Some(later), Some(earlier)) if later >= earlier => Some(later - earlier),
        _ => None,
    }
}

pub fn analyze<I: IntoIterator<Item = String>>(lines: I) -> Value {
    let mut records = 0u64;
    let mut sessions: Vec<Session> = Vec::new();
    let mut session_of_generation: BTreeMap<u64, usize> = BTreeMap::new();
    let mut turns: BTreeMap<(usize, u64, u64), Turn> = BTreeMap::new();
    let mut response_phase: BTreeMap<String, String> = BTreeMap::new();
    let mut cost_by_phase: BTreeMap<String, u64> = BTreeMap::new();
    let mut tokens_by_phase: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut standby = (0u64, 0u64, 0u64, 0u64);
    let mut warm: BTreeMap<String, u64> = BTreeMap::new();
    let mut actions: BTreeMap<String, u64> = BTreeMap::new();
    for line in lines {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event["component"] != "jwm-eva" {
            continue;
        }
        records += 1;
        let stage = event["stage"].as_str().unwrap_or("");
        let at = event["utc_ms"].as_u64();
        let generation = event["generation"].as_u64();
        if stage == "activation_received"
            && let Some(generation) = generation
        {
            sessions.push(Session {
                activated: at,
                ..Default::default()
            });
            session_of_generation.insert(generation, sessions.len() - 1);
            continue;
        }
        let session_index = generation.and_then(|g| session_of_generation.get(&g).copied());
        if let Some(index) = session_index {
            let session = &mut sessions[index];
            match stage {
                "audio_and_cloud_ready" => {
                    session.ready = session.ready.or(at);
                }
                "cloud_ready" => session.cloud = event["result"].as_str().map(str::to_owned),
                "session_failed" => session.failed = event["result"].as_str().map(str::to_owned),
                _ => {}
            }
        }
        // Some stages (feedback_completed, execution_started) carry no
        // session_epoch; they belong to the newest epoch of that session's turn.
        let turn_key = match (session_index, event["turn_id"].as_u64()) {
            (Some(index), Some(turn)) => match event["session_epoch"].as_u64() {
                Some(epoch) => Some((index, epoch, turn)),
                None => turns
                    .keys()
                    .filter(|(i, _, t)| *i == index && *t == turn)
                    .max()
                    .copied(),
            },
            _ => None,
        };
        if let Some(key) = turn_key {
            let turn = turns.entry(key).or_default();
            match stage {
                "input_committed" => turn.committed = turn.committed.or(at),
                "plan_completed" => {
                    turn.responses += 1;
                    if let Some(id) = event["response_id"].as_str() {
                        turn.response_ids.push(id.to_owned());
                    }
                }
                "plan_rejected" | "discovery_rejected" | "tool_output_rejected" => {
                    turn.rejections += 1
                }
                "plan_normalized" => turn.normalized += 1,
                "validation_completed" => turn.validated = turn.validated.or(at),
                "verification_completed" => {
                    turn.verified = turn.verified.or(at);
                    turn.verified_result = event["result"].as_str().map(str::to_owned);
                }
                "first_feedback_playback" => turn.first_audio = turn.first_audio.or(at),
                "playback_ended" => turn.playback_end = at.or(turn.playback_end),
                "feedback_completed" => turn.feedback_done = turn.feedback_done.or(at),
                _ => {}
            }
        }
        match stage {
            "usage_settled" => {
                let phase = event["phase"].as_str().unwrap_or("unknown").to_owned();
                *cost_by_phase.entry(phase.clone()).or_default() +=
                    event["cost_microusd"].as_u64().unwrap_or(0);
                if let Some(tokens) = event["usage"]["tokens"].as_object() {
                    let entry = tokens_by_phase.entry(phase.clone()).or_default();
                    for (name, count) in tokens {
                        *entry.entry(name.clone()).or_default() += count.as_u64().unwrap_or(0);
                    }
                }
                if let Some(id) = event["response_id"].as_str() {
                    response_phase.insert(id.to_owned(), phase);
                }
            }
            "standby_gate_summary" => {
                standby.0 += event["uploaded_frames"].as_u64().unwrap_or(0);
                standby.1 += event["suppressed_frames"].as_u64().unwrap_or(0);
                standby.2 += event["gate_openings"].as_u64().unwrap_or(0);
                standby.3 += 1;
            }
            "action_verified" => {
                *actions
                    .entry(event["result"].as_str().unwrap_or("unknown").to_owned())
                    .or_default() += 1;
            }
            stage if stage.starts_with("warm_") => {
                *warm.entry(stage.to_owned()).or_default() += 1;
            }
            _ => {}
        }
    }
    let committed_turns: Vec<&Turn> = turns.values().filter(|t| t.committed.is_some()).collect();
    let mut responses = BTreeMap::from([("1", 0u64), ("2", 0), ("3+", 0), ("0", 0)]);
    let (mut discovery, mut repair) = (0u64, 0u64);
    let mut results: BTreeMap<String, u64> = BTreeMap::new();
    let (mut to_plan, mut to_verified, mut to_first_audio, mut playback, mut to_feedback_done) =
        (vec![], vec![], vec![], vec![], vec![]);
    for turn in &committed_turns {
        let bucket = match turn.responses {
            0 => "0",
            1 => "1",
            2 => "2",
            _ => "3+",
        };
        *responses.get_mut(bucket).unwrap() += 1;
        for id in &turn.response_ids {
            match response_phase.get(id).map(String::as_str) {
                Some("discovery") => discovery += 1,
                Some("schema_repair") => repair += 1,
                _ => {}
            }
        }
        if let Some(result) = &turn.verified_result {
            *results.entry(result.clone()).or_default() += 1;
        }
        to_plan.extend(delta(turn.validated, turn.committed));
        to_verified.extend(delta(turn.verified, turn.committed));
        to_first_audio.extend(delta(turn.first_audio, turn.committed));
        playback.extend(delta(turn.playback_end, turn.first_audio));
        to_feedback_done.extend(delta(turn.feedback_done, turn.committed));
    }
    let total_responses: u64 = committed_turns.iter().map(|t| t.responses).sum();
    let (mut cold, mut warm_ready) = (vec![], vec![]);
    let mut failures: BTreeMap<String, u64> = BTreeMap::new();
    for session in &sessions {
        if let Some(ready) = delta(session.ready, session.activated) {
            if session.cloud.as_deref() == Some("warm") {
                warm_ready.push(ready);
            } else {
                cold.push(ready);
            }
        }
        if let Some(code) = &session.failed {
            *failures.entry(code.clone()).or_default() += 1;
        }
    }
    let total_cost: u64 = cost_by_phase.values().sum();
    let usd = |micro: u64| (micro as f64) / 1_000_000.0;
    json!({
        "records": records,
        "sessions": {
            "count": sessions.len(),
            "activation_to_ready_ms": {"cold": distribution(cold), "warm": distribution(warm_ready)},
            "failed": failures,
        },
        "turns": {
            "count": committed_turns.len(),
            "responses_per_turn": responses,
            "mean_responses": if committed_turns.is_empty() {0.0} else {total_responses as f64 / committed_turns.len() as f64},
            "discovery_rounds": discovery,
            "repair_rounds": repair,
            "rejections": committed_turns.iter().map(|t| t.rejections).sum::<u64>(),
            "normalized": committed_turns.iter().map(|t| t.normalized).sum::<u64>(),
            "results": results,
        },
        "latency_ms": {
            "speech_end_to_plan": distribution(to_plan),
            "speech_end_to_verified": distribution(to_verified),
            "speech_end_to_first_audio": distribution(to_first_audio),
            "reply_playback": distribution(playback),
            "speech_end_to_feedback_completed": distribution(to_feedback_done),
        },
        "actions_verified": actions,
        "cost_usd": {
            "total": usd(total_cost),
            "by_phase": cost_by_phase.iter().map(|(phase, micro)| (phase.clone(), json!(usd(*micro)))).collect::<serde_json::Map<_, _>>(),
            "tokens_by_phase": tokens_by_phase,
        },
        "standby_gate": {
            "summaries": standby.3,
            "uploaded_ms": standby.0.saturating_mul(20),
            "suppressed_ms": standby.1.saturating_mul(20),
            "openings": standby.2,
        },
        "warm_session": warm,
    })
}

fn dist_text(value: &Value) -> String {
    if value["count"].as_u64().unwrap_or(0) == 0 {
        return "n/a".into();
    }
    format!(
        "{}/{}/{} (n={})",
        value["p50"], value["p95"], value["max"], value["count"]
    )
}
fn map_text(value: &Value) -> String {
    let items: Vec<String> = value
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, count)| format!("{key} {count}"))
        .collect();
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}
pub fn render(report: &Value, since: &str) -> String {
    let turns = &report["turns"];
    let latency = &report["latency_ms"];
    let cost = &report["cost_usd"];
    let standby = &report["standby_gate"];
    let sessions = &report["sessions"];
    let mut out = String::new();
    out.push_str(&format!(
        "Eva report (since {since}): {} telemetry records\n",
        report["records"]
    ));
    out.push_str(&format!(
        "Sessions {} · activation→ready ms P50/P95/max cold {} warm {} · failed: {}\n",
        sessions["count"],
        dist_text(&sessions["activation_to_ready_ms"]["cold"]),
        dist_text(&sessions["activation_to_ready_ms"]["warm"]),
        map_text(&sessions["failed"])
    ));
    out.push_str(&format!(
        "Turns {} · responses/turn {} (mean {:.2}) · discovery {} · repair {} · rejections {} · normalized {}\n",
        turns["count"],
        map_text(&turns["responses_per_turn"]),
        turns["mean_responses"].as_f64().unwrap_or(0.0),
        turns["discovery_rounds"],
        turns["repair_rounds"],
        turns["rejections"],
        turns["normalized"]
    ));
    out.push_str(&format!(
        "Latency ms P50/P95/max · speech end→plan {} · →verified {} · →first audio {} · reply playback {} · →feedback done {}\n",
        dist_text(&latency["speech_end_to_plan"]),
        dist_text(&latency["speech_end_to_verified"]),
        dist_text(&latency["speech_end_to_first_audio"]),
        dist_text(&latency["reply_playback"]),
        dist_text(&latency["speech_end_to_feedback_completed"])
    ));
    out.push_str(&format!(
        "Results: {} · actions verified: {}\n",
        map_text(&turns["results"]),
        map_text(&report["actions_verified"])
    ));
    out.push_str(&format!(
        "Cost USD {:.4} · by phase: {}\n",
        cost["total"].as_f64().unwrap_or(0.0),
        cost["by_phase"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(phase, usd)| format!("{phase} {:.4}", usd.as_f64().unwrap_or(0.0)))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "Standby gate: {} summaries · uploaded {} s · suppressed {} s · openings {} · warm session events: {}\n",
        standby["summaries"],
        standby["uploaded_ms"].as_u64().unwrap_or(0) / 1000,
        standby["suppressed_ms"].as_u64().unwrap_or(0) / 1000,
        standby["openings"],
        map_text(&report["warm_session"])
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(stage: &str, extra: Value) -> String {
        let mut value = json!({"component":"jwm-eva","version":"0.1.0","stage":stage});
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value.to_string()
    }
    #[test]
    fn report_groups_turns_by_session_and_attributes_phases_latency_and_cost() {
        let lines = vec![
            "Started JWM Eva voice assistant.".to_string(),
            record("activation_received", json!({"generation":1,"utc_ms":1000})),
            record("cloud_ready", json!({"generation":1,"result":"configured","utc_ms":2500})),
            record("audio_and_cloud_ready", json!({"generation":1,"turn_id":1,"utc_ms":2600})),
            record("input_committed", json!({"generation":1,"session_epoch":1,"turn_id":1,"utc_ms":5000})),
            record("plan_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"response_id":"r1","utc_ms":6700})),
            record("usage_settled", json!({"phase":"discovery","response_id":"r1","cost_microusd":2000,"usage":{"tokens":{"input_text":100,"output_text":10}}})),
            record("plan_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"response_id":"r2","utc_ms":8000})),
            record("usage_settled", json!({"phase":"plan","response_id":"r2","cost_microusd":3000,"usage":{"tokens":{"input_text":200,"output_text":20}}})),
            record("plan_rejected", json!({"generation":1,"session_epoch":1,"turn_id":1,"result":"plan_invalid_structure"})),
            record("plan_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"response_id":"r3","utc_ms":9800})),
            record("usage_settled", json!({"phase":"schema_repair","response_id":"r3","cost_microusd":1500,"usage":{"tokens":{"input_text":50}}})),
            record("validation_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"utc_ms":9900})),
            record("action_verified", json!({"generation":1,"action":"set_layout","result":"succeeded"})),
            record("verification_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"result":"succeeded","utc_ms":10000})),
            record("first_feedback_playback", json!({"generation":1,"session_epoch":1,"turn_id":1,"utc_ms":12600})),
            record("usage_settled", json!({"phase":"feedback","response_id":"r4","cost_microusd":8000,"usage":{"tokens":{"output_audio":300}}})),
            record("playback_ended", json!({"generation":1,"session_epoch":1,"turn_id":1,"utc_ms":29400})),
            record("feedback_completed", json!({"generation":1,"turn_id":1,"utc_ms":29600})),
            // A restarted daemon reuses generation 1: a new session, not the old one.
            record("activation_received", json!({"generation":1,"utc_ms":50000})),
            record("cloud_ready", json!({"generation":1,"result":"warm","utc_ms":50050})),
            record("audio_and_cloud_ready", json!({"generation":1,"turn_id":1,"utc_ms":50120})),
            record("input_committed", json!({"generation":1,"session_epoch":1,"turn_id":1,"utc_ms":52000})),
            record("plan_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"response_id":"r5","utc_ms":53800})),
            record("usage_settled", json!({"phase":"plan","response_id":"r5","cost_microusd":2500,"usage":{"tokens":{"input_text":120}}})),
            record("plan_normalized", json!({"generation":1,"session_epoch":1,"turn_id":1,"result":"misplaced_protocol_field"})),
            record("validation_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"utc_ms":53850})),
            record("verification_completed", json!({"generation":1,"session_epoch":1,"turn_id":1,"result":"noop","utc_ms":53860})),
            record("session_failed", json!({"generation":1,"result":"realtime_write_timeout"})),
            record("standby_gate_summary", json!({"uploaded_frames":100,"suppressed_frames":9000,"gate_openings":3})),
            record("warm_ready", json!({"result":"expiry_known"})),
            record("warm_taken", json!({"result":"handed_over"})),
            // Foreign component and malformed lines are ignored.
            json!({"component":"other","stage":"input_committed","generation":1,"session_epoch":1,"turn_id":2,"utc_ms":1}).to_string(),
            "{not json".to_string(),
        ];
        let report = analyze(lines);
        assert_eq!(report["records"], 31);
        assert_eq!(report["sessions"]["count"], 2);
        assert_eq!(
            report["sessions"]["activation_to_ready_ms"]["cold"]["p50"],
            1600
        );
        assert_eq!(
            report["sessions"]["activation_to_ready_ms"]["warm"]["p50"],
            120
        );
        assert_eq!(report["sessions"]["failed"]["realtime_write_timeout"], 1);
        assert_eq!(report["turns"]["count"], 2);
        assert_eq!(report["turns"]["responses_per_turn"]["3+"], 1);
        assert_eq!(report["turns"]["responses_per_turn"]["1"], 1);
        assert_eq!(report["turns"]["mean_responses"], 2.0);
        assert_eq!(report["turns"]["discovery_rounds"], 1);
        assert_eq!(report["turns"]["repair_rounds"], 1);
        assert_eq!(report["turns"]["rejections"], 1);
        assert_eq!(report["turns"]["normalized"], 1);
        assert_eq!(report["turns"]["results"]["succeeded"], 1);
        assert_eq!(report["turns"]["results"]["noop"], 1);
        let latency = &report["latency_ms"];
        assert_eq!(latency["speech_end_to_plan"]["count"], 2);
        assert_eq!(latency["speech_end_to_plan"]["max"], 4900);
        assert_eq!(latency["speech_end_to_verified"]["p50"], 1860);
        assert_eq!(latency["speech_end_to_first_audio"]["p50"], 7600);
        assert_eq!(latency["reply_playback"]["p50"], 16800);
        assert_eq!(latency["speech_end_to_feedback_completed"]["count"], 1);
        assert_eq!(report["actions_verified"]["succeeded"], 1);
        assert!((report["cost_usd"]["total"].as_f64().unwrap() - 0.017).abs() < 1e-9);
        assert_eq!(
            report["cost_usd"]["tokens_by_phase"]["plan"]["input_text"],
            320
        );
        assert_eq!(report["standby_gate"]["uploaded_ms"], 2000);
        assert_eq!(report["standby_gate"]["suppressed_ms"], 180_000);
        assert_eq!(report["warm_session"]["warm_taken"], 1);
        let text = render(&report, "today");
        assert!(text.contains("Turns 2"));
        assert!(text.contains("warm 120/120/120"));
        assert!(text.contains("Cost USD 0.0170"));
    }
    #[test]
    fn percentiles_use_nearest_rank_and_tolerate_empty_input() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 95), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 95), 4);
        let empty = analyze(Vec::<String>::new());
        assert_eq!(empty["turns"]["count"], 0);
        assert_eq!(empty["latency_ms"]["speech_end_to_plan"]["count"], 0);
        assert!(render(&empty, "today").contains("n/a"));
    }
}
