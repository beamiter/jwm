//! Durable operation evidence contains only related state. Full query replies
//! live in the active call, never in the ordinary operation ledger.
use jwm_eva_core::protocol::{Action, digest};
use serde_json::{Value, json};

fn fields(value: &Value, names: &[&str]) -> Value {
    let mut output = serde_json::Map::new();
    for &name in names {
        if let Some(value) = value.get(name) {
            let kept = match value {
                Value::Bool(_) | Value::Number(_) | Value::Null => value.clone(),
                Value::String(text) => json!(
                    text.chars()
                        .take(if name.ends_with("path") { 4096 } else { 256 })
                        .collect::<String>()
                ),
                _ => continue,
            };
            output.insert(name.into(), kept);
        }
    }
    Value::Object(output)
}
fn nonempty(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}
fn native_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}
fn context_window<'a>(context: &'a Value, token: &Value) -> Option<&'a Value> {
    context["windows"]
        .as_array()?
        .iter()
        .find(|window| window["target_token"] == *token)
}
fn context_monitor<'a>(context: &'a Value, token: &Value) -> Option<&'a Value> {
    context["monitors"]
        .as_array()?
        .iter()
        .find(|monitor| monitor["monitor_token"] == *token)
}
fn window(value: &Value, token: &Value) -> Value {
    let mut result = fields(
        value,
        &[
            "target_token",
            "window_token",
            "monitor_token",
            "monitor_num",
            "tag_mask",
            "tags",
            "minimized",
            "maximized",
            "maximized_horz",
            "maximized_vert",
            "floating",
            "sticky",
            "pip",
            "is_minimized",
            "is_maximized",
            "is_floating",
            "is_sticky",
            "is_pip",
            "is_fullscreen",
            "is_focused",
            "client_fact",
            "x",
            "y",
            "w",
            "h",
            "stack_index",
            "tab_index",
        ],
    );
    if !token.is_null() {
        result["target_token"] = token.clone();
    }
    if let Some(id) = native_id(value.get("id").unwrap_or(&value["native_id"])) {
        result["native_id"] = json!(id);
    }
    if let Some(number) = value["monitor"].as_i64() {
        result["monitor_num"] = json!(number);
    }
    result
}
fn snapshot_window<'a>(snapshot: &'a Value, id: &str) -> Option<&'a Value> {
    if let Some(windows) = snapshot.as_array() {
        windows
            .iter()
            .find(|window| native_id(&window["id"]).as_deref() == Some(id))
    } else if native_id(&snapshot["id"]).as_deref() == Some(id) {
        Some(snapshot)
    } else {
        None
    }
}
fn workspace(value: &Value) -> Value {
    fields(
        value,
        &[
            "monitor_token",
            "monitor",
            "tag_mask",
            "tag_index",
            "tag_number",
            "layout",
            "m_fact",
            "n_master",
            "gap",
            "show_bar",
            "prev_layout",
            "num_clients",
            "focused",
        ],
    )
}
fn config_snapshot(snapshot: &Value, action: &Action) -> Value {
    let keys = if action.name == "set_config" {
        vec![action.arguments["key"].clone()]
    } else {
        action.arguments["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|change| change["key"].clone())
            .take(8)
            .collect()
    };
    let mut result = serde_json::Map::new();
    for key in keys
        .into_iter()
        .filter_map(|key| key.as_str().map(str::to_owned))
    {
        if let Some(value) = snapshot
            .get(&key)
            .or_else(|| snapshot.get(key.rsplit('.').next().unwrap_or(&key)))
        {
            if value.is_boolean() || value.is_number() {
                result.insert(key, value.clone());
            } else if let Some(text) = value.as_str() {
                result.insert(key, json!(text.chars().take(4096).collect::<String>()));
            }
        }
    }
    Value::Object(result)
}
fn domain_snapshot(snapshot: &Value, action: &Action) -> Value {
    match action.name.as_str() {
        "set_config" | "set_config_batch" => config_snapshot(snapshot, action),
        "clipboard_copy" | "clear_clipboard" => {
            let mut result = fields(snapshot, &["enabled", "count", "capacity"]);
            if let Some(index) = action.arguments["index"].as_u64() {
                result["index"] = json!(index);
                if let Some(entry) = snapshot["entries"]
                    .as_array()
                    .and_then(|entries| entries.get(index as usize))
                {
                    result["entry_metadata"] = fields(
                        entry,
                        &["index", "kind", "chars", "bytes", "width", "height"],
                    );
                }
            }
            result
        }
        name if name.starts_with("media_") || name == "open_media" => {
            let mut result = fields(
                snapshot,
                &[
                    "player",
                    "status",
                    "position_us",
                    "length_us",
                    "volume",
                    "shuffle",
                    "loop_status",
                    "track_id",
                ],
            );
            if snapshot.get("title").is_some() || snapshot.get("artist").is_some() {
                result["track_digest"] = json!(digest(
                    &json!({"title":snapshot["title"],"artist":snapshot["artist"],"album":snapshot["album"]})
                ));
            }
            result
        }
        "set_audio_device" => {
            let direction = action.arguments["direction"].as_str().unwrap_or("output");
            let id = &action.arguments["id"];
            snapshot[direction]
                .as_array()
                .and_then(|devices| devices.iter().find(|device| device["id"] == *id))
                .map(|device| fields(device, &["id", "default", "available", "muted", "volume"]))
                .unwrap_or_else(|| fields(snapshot, &["id", "default", "confirmed"]))
        }
        "set_mic_mute" => fields(snapshot, &["muted", "value", "confirmed", "pending"]),
        "set_power_profile" => fields(snapshot, &["active", "profile", "confirmed", "pending"]),
        name if name.contains("recording") || name.contains("screenshot") => {
            let mut result = fields(
                snapshot,
                &[
                    "active",
                    "finalizing",
                    "finalized",
                    "exporting",
                    "path",
                    "output_path",
                    "segment_path",
                    "format",
                    "was_active",
                ],
            );
            if let Some(region) = snapshot.get("region") {
                result["region"] = fields(region, &["x", "y", "width", "height", "w", "h"]);
            }
            result
        }
        "toggle_dnd" => fields(snapshot, &["enabled"]),
        "togglebar" => fields(snapshot, &["show_bar", "visible", "owns_output"]),
        "toggle_night_light" => fields(snapshot, &["enabled", "temperature", "temperature_kelvin"]),
        "toggle_idle_inhibit" => fields(snapshot, &["inhibited", "enabled"]),
        "toggle_peek" => fields(snapshot, &["active"]),
        "toggle_magnifier" => fields(snapshot, &["enabled", "zoom", "radius"]),
        "toggle_overview" | "cycle_overview" => {
            fields(snapshot, &["overview", "overview_index", "index"])
        }
        "toggle_expose" => fields(snapshot, &["expose"]),
        "toggle_annotation" => fields(snapshot, &["annotation"]),
        "togglecompositor" => fields(snapshot, &["compositor_active"]),
        "togglepartialdamage" => fields(snapshot, &["partial_damage"]),
        _ => Value::Object(Default::default()),
    }
}
fn lifecycle(value: &Value) -> Value {
    let mut result = fields(
        value,
        &[
            "available",
            "reason",
            "old_process_exited",
            "process_continued",
            "new_wm_instance_id",
        ],
    );
    for key in ["identity", "new_process_identity"] {
        if value.get(key).is_some() {
            result[key] = fields(&value[key], &["pid", "start_ticks", "wm_instance_id"]);
        }
    }
    result
}

/// `context` is either the pre-dispatch desktop context or an earlier minimal
/// ledger receipt when reconciling. Old full snapshots are safely discarded.
pub fn for_ledger(result: &Value, action: &Action, context: &Value) -> Value {
    let mut output = fields(result, &["operation_id", "status", "dispatched"]);
    output["projection_version"] = json!(1);
    if let Some(error) = result["error"].as_str() {
        let code = error.split(':').next().unwrap_or("");
        output["error"] = json!(if !code.is_empty()
            && code.len() <= 64
            && code.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        {
            code
        } else {
            "operation_failed"
        });
        output["error_digest"] = json!(digest(&json!(error)));
    }
    let source = &result["verified"];
    if matches!(action.name.as_str(), "query_state" | "query_window") {
        output["verified"] = json!({"query":if action.name=="query_window"{json!("get_window")}else{action.arguments["query"].clone()},"data_not_persisted":true});
        return output;
    }
    let mut verified = fields(
        source,
        &[
            "window_token",
            "target_token",
            "monitor_token",
            "focus_token",
            "tag_mask",
            "visible_tag_mask",
            "layout",
            "minimized",
            "maximized",
            "floating",
            "sticky",
            "pip",
            "enabled",
            "active",
            "muted",
            "factor",
            "client_factor",
            "column_width_factor",
            "queued",
            "accepted",
            "finalized",
            "was_active",
            "closed",
            "cleared",
            "applied",
            "atomic",
            "pid",
            "application_id",
            "path",
            "output_path",
            "completion_event",
            "verification_query",
            "completion",
        ],
    );
    if source.get("lifecycle").is_some() {
        verified["lifecycle"] = lifecycle(&source["lifecycle"]);
    }
    if let Some(reply) = source.get("reply") {
        let reply = fields(
            reply,
            &[
                "path",
                "output_path",
                "active",
                "queued",
                "accepted",
                "applied",
                "atomic",
                "closed",
                "cleared",
                "was_active",
            ],
        );
        if nonempty(&reply) {
            verified["reply"] = reply;
        }
    }
    let token = &action.arguments["window_token"];
    let before_window = context_window(context, token)
        .or_else(|| context["verified"].get("before_window"))
        .or_else(|| source.get("before_window"));
    let native =
        before_window.and_then(|value| native_id(value.get("id").unwrap_or(&value["native_id"])));
    if let Some(value) = before_window {
        verified["before_window"] = window(value, token);
    }
    for phase in ["before", "after"] {
        let projection = domain_snapshot(&source[phase], action);
        if nonempty(&projection) {
            verified[phase] = projection;
        }
        if let Some(id) = native.as_deref()
            && let Some(value) = snapshot_window(&source[phase], id)
        {
            verified[format!("{phase}_window")] = window(value, token);
        } else if let Some(value) = source.get(format!("{phase}_window")) {
            verified[format!("{phase}_window")] = window(value, token);
        }
    }
    let monitor_token = &action.arguments["monitor_token"];
    let before_monitor = context_monitor(context, monitor_token)
        .or_else(|| context["verified"].get("before_monitor"))
        .or_else(|| source.get("before_monitor"));
    if let Some(monitor) = before_monitor {
        verified["before_monitor"] = fields(
            monitor,
            &[
                "monitor_token",
                "num",
                "visible_tag_mask",
                "layout",
                "locked",
            ],
        );
    }
    if let Some(mask) = action.arguments["tag_mask"].as_u64() {
        if let Some(value) = context["workspaces"].as_array().and_then(|workspaces| {
            workspaces.iter().find(|workspace| {
                workspace["monitor_token"] == *monitor_token
                    && workspace["tag_mask"].as_u64() == Some(mask)
            })
        }) {
            verified["before_workspace"] = workspace(value);
        }
        for phase in ["before", "after"] {
            let related = source[phase]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|value| {
                    value["tag_mask"]
                        .as_u64()
                        .is_some_and(|tag| tag & mask != 0)
                        && before_monitor.is_some_and(|monitor| value["monitor"] == monitor["num"])
                })
                .map(workspace)
                .take(32)
                .collect::<Vec<_>>();
            if !related.is_empty() {
                verified[format!("{phase}_workspaces")] = json!(related);
            } else if let Some(value) = source.get(format!("{phase}_workspace")) {
                verified[format!("{phase}_workspace")] = workspace(value);
            }
        }
    }
    for field in ["read_back", "completion_payload"] {
        let projection = domain_snapshot(&source[field], action);
        if nonempty(&projection) {
            verified[field] = projection;
        }
    }
    output["verified"] = verified;
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_target_window_fields_persist_from_full_snapshots() {
        let action = Action {
            name: "togglefloating".into(),
            arguments: json!({"window_token":"w-token","enabled":true}),
        };
        let context = json!({"windows":[{"target_token":"w-token","id":"9007199254740993","title":"private target title","monitor_token":"m-token","tag_mask":1,"floating":false}]});
        let result = json!({"status":"succeeded","verified":{"before":[{"id":9007199254740993u64,"name":"private target title","is_floating":false},{"id":2,"name":"unrelated title"}],"after":[{"id":9007199254740993u64,"name":"private target title","is_floating":true},{"id":2,"name":"unrelated title"}]}});
        let projected = for_ledger(&result, &action, &context);
        assert_eq!(projected["verified"]["after_window"]["is_floating"], true);
        assert_eq!(
            projected["verified"]["after_window"]["native_id"],
            "9007199254740993"
        );
        let text = projected.to_string();
        assert!(!text.contains("private target title"));
        assert!(!text.contains("unrelated title"));
    }
    #[test]
    fn clipboard_and_query_data_are_not_persisted() {
        let result = json!({"status":"succeeded","verified":{"before":{"count":1,"entries":[{"index":0,"kind":"text","preview":"secret clipboard"}]},"after":{"count":1,"entries":[{"preview":"secret clipboard"}]}}});
        let action = Action {
            name: "clipboard_copy".into(),
            arguments: json!({"index":0}),
        };
        let projected = for_ledger(&result, &action, &Value::Null);
        assert_eq!(projected["verified"]["before"]["count"], 1);
        assert!(!projected.to_string().contains("secret clipboard"));
        let action = Action {
            name: "query_state".into(),
            arguments: json!({"query":"get_clipboard"}),
        };
        let projected = for_ledger(&result, &action, &Value::Null);
        assert_eq!(projected["verified"]["data_not_persisted"], true);
        assert!(!projected.to_string().contains("secret clipboard"));
    }
    #[test]
    fn configuration_and_lifecycle_keep_only_required_evidence() {
        let action = Action {
            name: "set_config".into(),
            arguments: json!({"key":"appearance.border_px","value":4}),
        };
        let result = json!({"status":"succeeded","verified":{"before":{"border_px":2,"unrelated":"secret config"},"after":{"border_px":4,"unrelated":"secret config"}}});
        let projected = for_ledger(&result, &action, &Value::Null);
        assert_eq!(projected["verified"]["after"]["appearance.border_px"], 4);
        assert!(!projected.to_string().contains("secret config"));
        let action = Action {
            name: "restart".into(),
            arguments: json!({}),
        };
        let result = json!({"status":"succeeded","verified":{"lifecycle":{"available":true,"identity":{"pid":12,"start_ticks":123,"wm_instance_id":"old"},"new_wm_instance_id":"new","process_continued":true},"before":{"windows":[{"name":"private title"}]}}});
        let projected = for_ledger(&result, &action, &Value::Null);
        assert_eq!(
            projected["verified"]["lifecycle"]["identity"]["start_ticks"],
            123
        );
        assert!(!projected.to_string().contains("private title"));
    }
}
