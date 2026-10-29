//! Local operation metadata and receipts, separate from ordinary media state.
use super::cli::{Open, file_uri};
use serde_json::{Value, json};
pub const ENV: &str = "JWM_EVA_MEDIA_REQUEST";
pub const RECEIPT_CAPACITY: usize = 64;

pub fn parse(text: &str) -> Result<Value, String> {
    if text.len() > 32768 {
        return Err("invalid Eva media request".into());
    }
    let input: Value = serde_json::from_str(text).map_err(|_| "invalid Eva media request")?;
    let mut output = serde_json::Map::new();
    for field in ["operation_id", "wm_instance_id", "resource_uri"] {
        let max = if field == "resource_uri" { 16384 } else { 256 };
        let value = input[field]
            .as_str()
            .filter(|text| {
                !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
            })
            .ok_or("invalid Eva media request")?;
        output.insert(field.into(), json!(value));
    }
    let sequence = input["sequence"]
        .as_u64()
        .filter(|sequence| *sequence > 0)
        .ok_or("invalid Eva media sequence")?;
    let enqueue = input["enqueue"].as_bool().ok_or("invalid Eva media mode")?;
    if input["target_player"] != "jwm" {
        return Err("invalid Eva player target".into());
    }
    let owner = match &input["target_bus_owner"] {
        Value::Null => None,
        Value::String(value)
            if value.len() <= 256 && zbus::names::UniqueName::try_from(value.as_str()).is_ok() =>
        {
            Some(value)
        }
        _ => return Err("invalid Eva owner".into()),
    };
    let pid = match &input["target_pid"] {
        Value::Null => None,
        value => Some(
            value
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .ok_or("invalid Eva PID")?,
        ),
    };
    if owner.is_some() != pid.is_some() {
        return Err("incomplete Eva player identity".into());
    }
    output.extend([
        (String::from("sequence"), json!(sequence)),
        ("enqueue".into(), json!(enqueue)),
        ("target_player".into(), json!("jwm")),
        ("target_bus_owner".into(), json!(owner)),
        ("target_pid".into(), json!(pid)),
    ]);
    Ok(Value::Object(output))
}
pub fn check_open(request: &Value, open: &Open) -> Result<(), String> {
    if open.files.len() != 1
        || request["enqueue"] != open.enqueue
        || request["resource_uri"] != file_uri(&open.files[0])
    {
        return Err("Eva media request does not match normalized resource".into());
    }
    Ok(())
}
pub fn check_owner(request: &Value, owner: &str, pid: u32) -> Result<(), String> {
    if request["target_bus_owner"]
        .as_str()
        .is_some_and(|expected| expected != owner)
        || request["target_pid"]
            .as_u64()
            .is_some_and(|expected| expected != u64::from(pid))
    {
        return Err("target_gone".into());
    }
    Ok(())
}
pub fn same_operation(left: &Value, right: &Value) -> bool {
    ["operation_id", "wm_instance_id", "sequence"]
        .iter()
        .all(|key| left[key] == right[key])
}
pub fn receipt(
    request: &Value,
    request_id: u64,
    entry_id: u64,
    verified: bool,
    error: Option<&str>,
) -> Value {
    let mut output = request.clone();
    output["player_request_id"] = json!(request_id);
    output["entry_id"] = json!(entry_id);
    output["mpv_command_ack"] = json!(request_id > 0);
    output["verified"] = json!(verified);
    if let Some(error) = error {
        output["error"] = json!(error);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_request_is_strict_and_receipts_cannot_change_operation_identity() {
        let request=parse(r#"{"operation_id":"a","wm_instance_id":"wm","sequence":1,"resource_uri":"file:///one","enqueue":true,"target_player":"jwm","target_bus_owner":null,"target_pid":null}"#).unwrap();
        let proof = receipt(&request, 9, 7, true, None);
        assert!(same_operation(&request, &proof));
        assert!(check_owner(&request, ":1.9", 10).is_ok());
        let mut bound = request.clone();
        bound["target_bus_owner"] = json!(":1.8");
        bound["target_pid"] = json!(11);
        assert!(check_owner(&bound, ":1.9", 11).is_err());
        assert!(check_owner(&bound, ":1.8", 10).is_err());
        let mut old = proof.clone();
        old["sequence"] = json!(2);
        assert!(!same_operation(&request, &old));
    }
}
