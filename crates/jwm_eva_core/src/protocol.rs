use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const PROTOCOL_VERSION: u32 = 1;

/// Additive assistant contract, independent from the guarded request envelope.
/// Feature names version their semantics; later revisions may add features
/// without changing the meaning of these existing names.
pub const ASSISTANT_PROTOCOL_REVISION: u32 = 1;
pub const ASSISTANT_REQUIRED_FEATURES: &[&str] = &[
    "guarded_execution_v1",
    "wake_source_identity_v1",
    "wake_onset_monitor_history_v1",
    "voice_session_finish_v1",
    "verified_task_progress_v1",
];
pub const ASSISTANT_EXECUTION_FEATURES: &[&str] = &[
    "guarded_execution_v1",
    "voice_session_finish_v1",
    "verified_task_progress_v1",
];
pub const ASSISTANT_WAKE_FEATURES: &[&str] =
    &["wake_source_identity_v1", "wake_onset_monitor_history_v1"];

pub fn assistant_protocol() -> Value {
    serde_json::json!({"revision":ASSISTANT_PROTOCOL_REVISION,"features":ASSISTANT_REQUIRED_FEATURES})
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantProtocolError {
    Missing,
    Malformed,
    RevisionUnsupported { found: u32, required: u32 },
    MissingFeatures(Vec<String>),
}
impl std::fmt::Display for AssistantProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("assistant_protocol_missing"),
            Self::Malformed => f.write_str("assistant_protocol_malformed"),
            Self::RevisionUnsupported { .. } => {
                f.write_str("assistant_protocol_revision_unsupported")
            }
            Self::MissingFeatures(features) => write!(
                f,
                "assistant_protocol_missing_features:{}",
                features.join(",")
            ),
        }
    }
}
impl std::error::Error for AssistantProtocolError {}

/// Validate a complete native query response. Do not infer support from the
/// guarded envelope version, action names or the presence of runtime fields.
pub fn validate_assistant_protocol(document: &Value) -> Result<(), AssistantProtocolError> {
    validate_assistant_features(document, ASSISTANT_REQUIRED_FEATURES)
}

/// Unknown future features and later additive revisions are compatible; an
/// omitted, malformed or incomplete advertisement never grants support.
pub fn validate_assistant_features(
    document: &Value,
    required: &[&str],
) -> Result<(), AssistantProtocolError> {
    let contract = document
        .get("assistant_protocol")
        .ok_or(AssistantProtocolError::Missing)?;
    let object = contract
        .as_object()
        .ok_or(AssistantProtocolError::Malformed)?;
    let revision = object
        .get("revision")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(AssistantProtocolError::Malformed)?;
    if revision < ASSISTANT_PROTOCOL_REVISION {
        return Err(AssistantProtocolError::RevisionUnsupported {
            found: revision,
            required: ASSISTANT_PROTOCOL_REVISION,
        });
    }
    let features = object
        .get("features")
        .and_then(Value::as_array)
        .ok_or(AssistantProtocolError::Malformed)?;
    let mut names = std::collections::BTreeSet::new();
    for feature in features {
        let name = feature
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or(AssistantProtocolError::Malformed)?;
        if !names.insert(name) {
            return Err(AssistantProtocolError::Malformed);
        }
    }
    let missing: Vec<String> = required
        .iter()
        .filter(|name| !names.contains(**name))
        .map(|name| (*name).to_owned())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(AssistantProtocolError::MissingFeatures(missing))
    }
}

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
    fn assistant_protocol_does_not_infer_new_support_from_the_old_guarded_version() {
        let old =
            json!({"protocol_version":1,"guarded_execution":true,"actions":[],"wake_generation":0});
        assert_eq!(
            validate_assistant_protocol(&old),
            Err(AssistantProtocolError::Missing)
        );
        let mut current = old;
        current["assistant_protocol"] = assistant_protocol();
        assert!(validate_assistant_protocol(&current).is_ok());
        current["assistant_protocol"]["features"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert_eq!(
            validate_assistant_protocol(&current),
            Err(AssistantProtocolError::MissingFeatures(vec![
                "verified_task_progress_v1".into()
            ]))
        );
        assert!(validate_assistant_features(&current, ASSISTANT_WAKE_FEATURES).is_ok());
    }
    #[test]
    fn assistant_contract_rejects_malformed_or_old_advertisements() {
        for contract in [
            Value::Null,
            json!(true),
            json!({}),
            json!({"revision":"1","features":[]}),
            json!({"revision":1,"features":"all"}),
            json!({"revision":1,"features":["guarded_execution_v1",true]}),
            json!({"revision":1,"features":[""]}),
            json!({"revision":1,"features":["guarded_execution_v1","guarded_execution_v1"]}),
        ] {
            assert_eq!(
                validate_assistant_protocol(&json!({"assistant_protocol":contract})),
                Err(AssistantProtocolError::Malformed)
            );
        }
        assert!(matches!(
            validate_assistant_protocol(
                &json!({"assistant_protocol":{"revision":0,"features":ASSISTANT_REQUIRED_FEATURES}})
            ),
            Err(AssistantProtocolError::RevisionUnsupported { .. })
        ));
    }
    #[test]
    fn later_additive_revisions_and_unknown_features_keep_existing_semantics() {
        let mut contract = assistant_protocol();
        contract["revision"] = json!(ASSISTANT_PROTOCOL_REVISION + 1);
        contract["features"]
            .as_array_mut()
            .unwrap()
            .push(json!("future_feature_v1"));
        contract["future_metadata"] = json!({"supported":true});
        assert!(validate_assistant_protocol(&json!({"assistant_protocol":contract})).is_ok());
        let wake_only = json!({"assistant_protocol":{"revision":ASSISTANT_PROTOCOL_REVISION,"features":ASSISTANT_WAKE_FEATURES}});
        assert!(validate_assistant_features(&wake_only, ASSISTANT_WAKE_FEATURES).is_ok());
        assert!(matches!(
            validate_assistant_protocol(&wake_only),
            Err(AssistantProtocolError::MissingFeatures(_))
        ));
    }
    #[test]
    fn digest_canonicalizes_nested_keys() {
        assert_eq!(
            digest(&json!({"b":2,"a":{"z":0,"x":1}})),
            digest(&json!({"a":{"x":1,"z":0},"b":2}))
        );
        assert_ne!(digest(&json!({"a":1})), digest(&json!({"a":2})));
    }
}
