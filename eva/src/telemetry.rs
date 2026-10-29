//! Deliberately excludes audio, transcript, window title and credential fields.
pub fn fields(fields: &serde_json::Value) -> serde_json::Value {
    const FIELDS: &[&str] = &[
        "generation",
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
    ];
    let mut record = serde_json::Map::new();
    if let Some(fields) = fields.as_object() {
        for (name, value) in fields {
            if FIELDS.contains(&name.as_str()) {
                record.insert(name.clone(), value.clone());
            }
        }
    }
    serde_json::Value::Object(record)
}
pub fn event(stage: &str, labels: &serde_json::Value) {
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
