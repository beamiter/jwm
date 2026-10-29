use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantPermit {
    pub assistant_session_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedContext {
    pub session_unlocked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_tag_mask: Option<u32>,
}

impl Default for ExpectedContext {
    fn default() -> Self {
        Self {
            session_unlocked: true,
            monitor_token: None,
            focus_token: None,
            visible_tag_mask: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardedRequest {
    pub protocol_version: u32,
    pub wm_instance_id: String,
    pub assistant_permit: AssistantPermit,
    pub operation_id: String,
    pub request_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_token: Option<String>,
    pub expected_context: ExpectedContext,
    pub action: Action,
}

/// Hash of a canonical JSON tree (sorted object keys, never map insertion order).
pub fn digest(value: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort_unstable();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), canonical(&map[key])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
            _ => value.clone(),
        }
    }
    let bytes = serde_json::to_vec(&canonical(value)).expect("JSON value is serializable");
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl GuardedRequest {
    /// Digest omits only the digest itself; includes permit, operation and all guards.
    pub fn calculated_digest(&self) -> String {
        let mut value = serde_json::to_value(self).expect("request is serializable");
        value.as_object_mut().unwrap().remove("request_digest");
        digest(&value)
    }
    pub fn seal(&mut self) {
        self.request_digest = self.calculated_digest();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn digest_canonicalizes_nested_keys() {
        assert_eq!(
            digest(&json!({"b":2,"a":{"z":0,"x":1}})),
            digest(&json!({"a":{"x":1,"z":0},"b":2}))
        );
        assert_ne!(digest(&json!({"a":1})), digest(&json!({"a":2})));
    }
}
