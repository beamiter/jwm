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
        "pitched_frames",
        "max_pitched_frames",
        "tail_frames",
        "rejected_source_frames",
        "source_discontinuities",
        // Rejected model output shape: item counts only, never content.
        "output_items",
        "function_calls",
        "submit_calls",
        "describe_calls",
        "other_calls",
        "incomplete_calls",
        "message_items",
        // Standby upload gate counters: frame counts only, never audio.
        "uploaded_frames",
        "suppressed_frames",
        "gate_openings",
        "silenced_frames",
        "voice_probability",
        "voice_backend",
        "voice_backend_requested",
        "voice_backend_degraded_reason",
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
                | "pitched_frames"
                | "max_pitched_frames"
                | "tail_frames"
                | "rejected_source_frames"
                | "source_discontinuities"
                | "uploaded_frames"
                | "suppressed_frames"
                | "gate_openings"
                | "silenced_frames"
                | "output_items"
                | "function_calls"
                | "submit_calls"
                | "describe_calls"
                | "other_calls"
                | "incomplete_calls"
                | "message_items" => value.is_u64(),
                "voice_probability" => value
                    .as_f64()
                    .is_some_and(|p| p.is_finite() && (0.0..=1.0).contains(&p)),
                "voice_backend" | "voice_backend_requested" => value
                    .as_str()
                    .is_some_and(|s| ["silero", "rnnoise", "unavailable"].contains(&s)),
                "voice_backend_degraded_reason" => {
                    value.is_null()
                        || value.as_str().is_some_and(|s| {
                            matches!(
                                s,
                                "silero_runtime_unavailable"
                                    | "silero_runtime_incompatible"
                                    | "silero_runtime_initialization_failed"
                                    | "silero_model_unavailable"
                                    | "silero_model_checksum_mismatch"
                                    | "silero_model_load_failed"
                                    | "silero_feature_unavailable"
                            )
                        })
                }
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
    fn backend_diagnostics_are_fixed_codes_never_paths_or_error_bodies() {
        let fields = super::fields(&serde_json::json!({
            "voice_backend":"silero", "voice_backend_requested":"silero",
            "voice_backend_degraded_reason":null, "model_path":"private/path"
        }));
        assert_eq!(
            fields,
            serde_json::json!({"voice_backend":"silero","voice_backend_requested":"silero","voice_backend_degraded_reason":null})
        );
        assert_eq!(
            super::fields(&serde_json::json!({
                "voice_backend":"private path", "voice_backend_degraded_reason":"silero_private_secret"
            })),
            serde_json::json!({})
        );
    }
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
