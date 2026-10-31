//! Deliberately excludes audio, transcript, window title and credential fields.
pub fn fields(fields: &serde_json::Value) -> serde_json::Value {
    const FIELDS: &[&str] = &[
        "generation",
        "wake_generation",
        "session_epoch",
        "turn_id",
        "response_id",
        "call_id",
        "plan_hash",
        "operation_id",
        "action",
        "model",
        "result",
        "phase",
        "price_version",
        "usage",
        "cost_microusd",
        "source_activation_ns",
        "played_samples",
        // Per-turn aggregate signal levels/counts, never sample arrays.
        "input_frames",
        "noise_frames",
        "above_threshold_frames",
        "echo_frames",
        "peak_level",
        "threshold_level",
        "max_voiced_frames",
        "rejected_source_frames",
        "source_discontinuities",
        // Standby upload gate counters: frame counts only, never audio.
        "uploaded_frames",
        "suppressed_frames",
        "gate_openings",
    ];
    let mut record = serde_json::Map::new();
    if let Some(fields) = fields.as_object() {
        for (name, value) in fields {
            let signal_scalar = match name.as_str() {
                "wake_generation"
                | "input_frames"
                | "noise_frames"
                | "above_threshold_frames"
                | "echo_frames"
                | "max_voiced_frames"
                | "rejected_source_frames"
                | "source_discontinuities"
                | "uploaded_frames"
                | "suppressed_frames"
                | "gate_openings" => value.is_u64(),
                "peak_level" | "threshold_level" => value
                    .as_f64()
                    .is_some_and(|level| level.is_finite() && level >= 0.0),
                _ => true,
            };
            if FIELDS.contains(&name.as_str()) && signal_scalar {
                record.insert(name.clone(), value.clone());
            }
        }
    }
    serde_json::Value::Object(record)
}
pub fn event(stage: &str, labels: &serde_json::Value) {
    crate::companion::milestone(stage, labels);
    let mut record = fields(labels).as_object().cloned().unwrap_or_default();
    record.insert("component".into(), serde_json::json!("jwm-eva"));
    record.insert(
        "version".into(),
        serde_json::json!(env!("CARGO_PKG_VERSION")),
    );
    record.insert(
        "source_version".into(),
        serde_json::json!(option_env!("JWM_EVA_SOURCE_REVISION").unwrap_or("worktree")),
    );
    record.insert("stage".into(), serde_json::json!(stage));
    record.insert(
        "monotonic_ns".into(),
        serde_json::json!(crate::context::monotonic_ns().ok()),
    );
    record.insert("utc_ms".into(), serde_json::json!(crate::store::wall_ms()));
    eprintln!("{}", serde_json::Value::Object(record));
}
pub fn mark(stage: &str, generation: u64, turn: u64) {
    event(
        stage,
        &serde_json::json!({"generation":generation,"turn_id":turn}),
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn signal_diagnostics_accept_only_aggregate_numbers() {
        let filtered = super::fields(&serde_json::json!({
            "input_frames":100,"noise_frames":50,"threshold_level":0.006,
            "rejected_source_frames":7,"source_discontinuities":2,
            "peak_level":"private speech","echo_frames":[123,456],
            "pcm":[1,2,3],"transcript":"private speech","reference":[4,5,6]
        }));
        assert_eq!(
            filtered,
            serde_json::json!({"input_frames":100,"noise_frames":50,"threshold_level":0.006,
                "rejected_source_frames":7,"source_discontinuities":2})
        );
        assert_eq!(
            super::fields(&serde_json::json!({
                "rejected_source_frames":-1,"source_discontinuities":"private speech"
            })),
            serde_json::json!({})
        );
    }

    #[test]
    fn wake_generation_diagnostics_reject_text_and_negative_values() {
        for value in [serde_json::json!("private"), serde_json::json!(-1)] {
            assert_eq!(
                super::fields(&serde_json::json!({"wake_generation":value})),
                serde_json::json!({})
            );
        }
        assert_eq!(
            super::fields(&serde_json::json!({"wake_generation":7})),
            serde_json::json!({"wake_generation":7})
        );
    }
}
