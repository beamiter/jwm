//! Read-only readiness interpretation. Failed probes contribute fixed, useful
//! guidance; transport bodies, window titles and credentials are never printed.
use crate::{audio::AudioStatus, config::Config};
use jwm_eva_core::protocol::{
    ASSISTANT_EXECUTION_FEATURES, ASSISTANT_WAKE_FEATURES, validate_assistant_features,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ready,
    Warning,
    Blocked,
    Error,
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub state: State,
    pub message: &'static str,
    pub recommendation: Option<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub ready: bool,
    pub protocol_ready: bool,
    pub wake_protocol_ready: bool,
    pub checks: Vec<Check>,
}
pub struct Inputs<'a> {
    pub credential_available: bool,
    pub capabilities: Option<&'a Value>,
    pub schema: Option<&'a Value>,
    pub context: Option<&'a Value>,
    pub assistant: Option<&'a Value>,
    pub audio: Option<&'a AudioStatus>,
}
pub fn evaluate(config: &Config, inputs: Inputs<'_>) -> Report {
    let mut checks = Vec::new();
    let mut add = |id, state, message, recommendation| {
        checks.push(Check {
            id,
            state,
            message,
            recommendation,
        })
    };
    if ["gpt-realtime-2.1-mini", "gpt-realtime-mini"].contains(&config.openai.model.as_str())
        && config.interaction.half_duplex
        && config.interaction.feedback == "brief"
        && config.interaction.follow_up_ms == 5000
        && config.interaction.max_utterance_ms == 20000
        && config.interaction.max_session_ms == 300000
        && config.execution.max_actions_per_turn == 8
        && config.execution.max_planning_rounds == 3
    {
        add(
            "legacy_profile",
            State::Warning,
            "当前仍使用旧版 mini 与半双工交互组合。",
            Some(
                "如需启用新版体验，先运行 jwm-eva config migrate --check；自定义设置不会自动覆盖。",
            ),
        );
    }
    add(
        "credential",
        if inputs.credential_available {
            State::Ready
        } else {
            State::Error
        },
        if inputs.credential_available {
            "已取得配置指定的凭据；未验证云端授权。"
        } else {
            "无法取得配置指定的 API 凭据。"
        },
        (!inputs.credential_available).then_some(
            "检查 Secret Service、凭据文件权限或服务中的 OPENAI_API_KEY；不要把密钥写入 TOML。",
        ),
    );
    let transport = inputs.capabilities.is_some();
    add(
        "ipc",
        if transport {
            State::Ready
        } else {
            State::Error
        },
        if transport {
            "已连接 JWM。"
        } else {
            "无法连接当前 JWM 会话。"
        },
        (!transport).then_some("在 JWM 图形会话内运行诊断，并检查 JWM_SOCKET 和会话是否仍在运行。"),
    );
    let documents = [inputs.schema, inputs.context, inputs.assistant];
    let matching_instance =
        inputs
            .context
            .zip(inputs.assistant)
            .is_some_and(|(context, status)| {
                context["wm_instance_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .zip(
                        status["wm_instance_id"]
                            .as_str()
                            .filter(|id| !id.is_empty()),
                    )
                    .is_some_and(|(context, status)| context == status)
            });
    let commands_ready = inputs.capabilities.is_some_and(|cap| {
        [
            (
                "commands",
                &[
                    "guarded_execute",
                    "assistant_control",
                    "set_assistant_status",
                ][..],
            ),
            (
                "queries",
                &[
                    "get_execution_context",
                    "get_execution_status",
                    "get_command_schema",
                    "get_assistant_status",
                ][..],
            ),
        ]
        .iter()
        .all(|(kind, names)| {
            names.iter().all(|name| {
                cap[*kind]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item == *name))
            })
        })
    });
    let protocol_ready = transport
        && commands_ready
        && matching_instance
        && inputs.context.is_some_and(|v| v["protocol_version"] == 1)
        && inputs.schema.is_some_and(|v| {
            v["protocol_version"] == 1
                && v["guarded_execution"] == true
                && v["context_tokens"] == true
        })
        && documents.iter().all(|doc| {
            doc.is_some_and(|doc| {
                validate_assistant_features(doc, ASSISTANT_EXECUTION_FEATURES).is_ok()
            })
        });
    add(
        "assistant_protocol",
        if protocol_ready {
            State::Ready
        } else {
            State::Error
        },
        if protocol_ready {
            "当前 JWM 支持受保护执行、语音收尾和已核验进度。"
        } else {
            "当前 JWM 的助手协议缺失、过旧或来自不同实例。"
        },
        (!protocol_ready).then_some(
            "安装同次构建的 JWM 后重新登录，再重启 jwm-eva.service；仅替换 Eva 二进制不够。",
        ),
    );
    let wake_protocol_ready = protocol_ready
        && inputs.capabilities.is_some_and(|cap| {
            cap["commands"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item == "set_assistant_wake_status"))
        })
        && documents.iter().all(|doc| {
            doc.is_some_and(|doc| validate_assistant_features(doc, ASSISTANT_WAKE_FEATURES).is_ok())
        })
        && inputs
            .context
            .is_some_and(|v| v["selected_monitor_history"].is_array())
        && inputs.assistant.is_some_and(|v| {
            v["wake_generation"].is_u64()
                && v["assistant_generation"].is_u64()
                && v["standby_allowed"].is_boolean()
        });
    add(
        "wake_protocol",
        if wake_protocol_ready {
            State::Ready
        } else if config.wake_word.enabled {
            State::Error
        } else {
            State::Warning
        },
        if wake_protocol_ready {
            "支持来源隔离与原始起声显示器交接。"
        } else {
            "当前 JWM 不具备完整唤醒交接协议。"
        },
        (!wake_protocol_ready).then_some("使用同次构建的 JWM；修复前不要开启全时监听。"),
    );
    let presentation = inputs
        .assistant
        .is_some_and(|v| v["standby_allowed"] == true);
    let blocked_reason = inputs
        .assistant
        .and_then(|v| v["standby_blocked_reason"].as_str());
    let (message, action) = match blocked_reason {
        Some("session_locked") => (
            "桌面当前已锁定。",
            "解锁桌面后再激活语音；解锁不会自动打开监听。",
        ),
        Some("session_exiting") => ("JWM 会话正在退出。", "登录新的 JWM 会话后重试。"),
        _ => (
            "麦克风提示浮层当前不可显示。",
            "启用 JWM 合成器并连接可见显示器；可见麦克风提示是激活条件。",
        ),
    };
    add(
        "presentation",
        if presentation {
            State::Ready
        } else {
            State::Blocked
        },
        if presentation {
            "可显示麦克风状态浮层。"
        } else {
            message
        },
        (!presentation).then_some(action),
    );
    for (id, available, message, action) in [
        (
            "audio_input",
            inputs.audio.is_some_and(|v| v.input_available),
            "找不到可用麦克风。",
            "检查输入设备与 audio.input_device；不要选择扬声器 monitor。",
        ),
        (
            "audio_output",
            inputs.audio.is_some_and(|v| v.output_available),
            "找不到可用播放设备。",
            "检查当前用户的 PulseAudio/PipeWire 服务和输出设备。",
        ),
    ] {
        add(
            id,
            if available {
                State::Ready
            } else {
                State::Error
            },
            if available {
                "音频设备可用；未打开录音或播放流。"
            } else {
                message
            },
            (!available).then_some(action),
        );
    }
    Report {
        ready: checks
            .iter()
            .all(|check| !matches!(check.state, State::Error | State::Blocked)),
        protocol_ready,
        wake_protocol_ready,
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn documents() -> Value {
        json!({"protocol_version":1,"guarded_execution":true,"context_tokens":true,
            "commands":["guarded_execute","assistant_control","set_assistant_status","set_assistant_wake_status"],
            "queries":["get_execution_context","get_execution_status","get_command_schema","get_assistant_status"],
            "wm_instance_id":"wm","assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),
            "standby_allowed":true,"standby_blocked_reason":null,"wake_generation":3,"assistant_generation":1,
            "selected_monitor_history":[]})
    }
    fn report(config: &Config, doc: &Value) -> Report {
        let audio = AudioStatus {
            backend: "test",
            input_available: true,
            output_available: true,
        };
        evaluate(
            config,
            Inputs {
                credential_available: true,
                capabilities: Some(doc),
                schema: Some(doc),
                context: Some(doc),
                assistant: Some(doc),
                audio: Some(&audio),
            },
        )
    }
    #[test]
    fn protocol_support_and_runtime_presentation_block_are_independent() {
        let mut doc = documents();
        assert!(report(&Config::default(), &doc).ready);
        doc["standby_allowed"] = json!(false);
        doc["standby_blocked_reason"] = json!("session_locked");
        let result = report(&Config::default(), &doc);
        assert!(result.protocol_ready);
        assert!(result.wake_protocol_ready);
        assert!(!result.ready);
        assert!(
            result
                .checks
                .iter()
                .any(|check| check.id == "presentation" && check.message.contains("锁定"))
        );
    }
    #[test]
    fn old_server_fields_alone_do_not_claim_new_features_are_ready() {
        let mut doc = documents();
        doc.as_object_mut().unwrap().remove("assistant_protocol");
        let result = report(&Config::default(), &doc);
        assert!(!result.protocol_ready);
        assert!(!result.wake_protocol_ready);
        assert!(
            result
                .checks
                .iter()
                .any(|check| check.id == "assistant_protocol"
                    && check.recommendation.unwrap().contains("重新登录"))
        );
    }
    #[test]
    fn missing_wake_features_warn_without_blocking_supported_hotkey_dialogue() {
        let mut doc = documents();
        doc["assistant_protocol"]["features"] = json!(ASSISTANT_EXECUTION_FEATURES);
        let result = report(&Config::default(), &doc);
        assert!(result.protocol_ready);
        assert!(!result.wake_protocol_ready);
        assert!(result.ready);
        let mut config = Config::default();
        config.wake_word.enabled = true;
        assert!(!report(&config, &doc).ready);
    }
    #[test]
    fn empty_or_missing_native_identity_cannot_compare_as_a_matching_instance() {
        let mut context = documents();
        let mut status = documents();
        context["wm_instance_id"] = json!("");
        status.as_object_mut().unwrap().remove("wm_instance_id");
        let audio = AudioStatus {
            backend: "test",
            input_available: true,
            output_available: true,
        };
        let result = evaluate(
            &Config::default(),
            Inputs {
                credential_available: true,
                capabilities: Some(&context),
                schema: Some(&context),
                context: Some(&context),
                assistant: Some(&status),
                audio: Some(&audio),
            },
        );
        assert!(!result.protocol_ready);
        assert!(!result.wake_protocol_ready);
        assert!(!result.ready);
    }
    #[test]
    fn unrelated_instance_or_untrusted_probe_text_is_not_reported_as_success() {
        let doc = documents();
        let mut status = doc.clone();
        status["wm_instance_id"] = json!("another-wm");
        status["text"] = json!("private credential text");
        let result = evaluate(
            &Config::default(),
            Inputs {
                credential_available: false,
                capabilities: Some(&doc),
                schema: Some(&doc),
                context: Some(&doc),
                assistant: Some(&status),
                audio: None,
            },
        );
        assert!(!result.protocol_ready);
        assert!(!result.ready);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("private credential")
        );
    }
}
