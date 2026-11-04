//! Public errors carry stable codes and short user text, never transport bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    Transient,
    RateLimited,
    Permanent,
}

/// Decisions use local error categories; remote messages are never rendered.
/// Unknown/device failures are bounded by the retry controller rather than
/// spinning forever or assuming an unavailable microphone will repair itself.
pub fn recovery(error: &std::io::Error) -> Recovery {
    let s = error.to_string();
    if ["rate_limited", "rate_limit_exceeded", "rate_limit_error"]
        .iter()
        .any(|code| s.contains(code))
    {
        return Recovery::RateLimited;
    }
    if [
        "auth_failed",
        "credential",
        "api_key",
        "Secret Service",
        "OPENAI_API_KEY",
        "model_access_denied",
        "model_not_found",
        "model_not_available",
        "quota_exceeded",
        "insufficient_quota",
        "budget_exceeded",
        "budget_limit",
        "configuration",
        "invalid Eva configuration",
        "invalid_proxy_url",
        "proxy_host_missing",
        "only HTTP CONNECT",
        "invalid_realtime_url",
        "invalid_header_param",
        "invalid_input_device",
        "realtime_tls_native_roots_unavailable",
        "realtime_tls_configuration_failed",
        "pipewire_client_runtime_requires",
        "realtime_request_rejected",
        "version_incompatible",
    ]
    .iter()
    .any(|code| s.contains(code))
        || matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::InvalidInput
                | std::io::ErrorKind::NotFound
        )
    {
        Recovery::Permanent
    } else {
        Recovery::Transient
    }
}

/// Recognize only the local validator's fixed code/format. In particular,
/// never search arbitrary provider prose or an unknown argument's value for
/// words that might resemble a public error category.
fn local_plan_validation_error(message: &str) -> bool {
    const CODES: &[&str] = &[
        "action_schema_not_loaded",
        "missing_required_argument",
        "unknown_argument",
        "expected_object",
        "expected_string",
        "expected_integer",
        "expected_number",
        "expected_null",
        "expected_boolean",
        "expected_array",
        "no_schema_alternative_matches",
        "constant_mismatch",
        "value_not_in_enum",
        "string_length_out_of_range",
        "integer_out_of_range",
        "number_out_of_range",
        "array_length_out_of_range",
        "duplicate_array_item",
        "schema_type_missing",
        "schema_properties_missing",
        "schema_required_missing",
        "schema_invalid_required",
        "unsupported_schema_type",
        "execute_requires_actions_without_question",
        "clarify_requires_question_without_actions",
        "noop_cannot_contain_actions_or_question",
        "undo_must_be_only_action",
        "too_many_actions",
        "plan_text_too_long",
    ];
    let (code, suffix) = message
        .split_once(':')
        .map_or((message, None), |(code, rest)| (code, Some(rest)));
    if !CODES.contains(&code) {
        return false;
    }
    let Some(suffix) = suffix else {
        return true;
    };
    // validate_schema emits unknown_argument:<arbitrary model key>. The key
    // is data, even if it contains auth/audio/network words, and is never shown.
    if code == "unknown_argument" {
        return !suffix.is_empty();
    }
    // session's local validate_plan wrapper appends exactly these diagnostic
    // counts. Do not accept an arbitrary body after a familiar code.
    let fields: Vec<_> = suffix.split_whitespace().collect();
    if fields.len() != 3 {
        return false;
    }
    let Some(count) = fields[0].strip_prefix("actions=") else {
        return false;
    };
    !count.is_empty()
        && count.bytes().all(|c| c.is_ascii_digit())
        && count.parse::<usize>().is_ok()
        && matches!(
            fields[1],
            "question_present=true" | "question_present=false"
        )
        && matches!(
            fields[2],
            "question_nonempty=true" | "question_nonempty=false"
        )
}

pub fn describe(error: &std::io::Error) -> (&'static str, &'static str) {
    let s = error.to_string();
    if local_plan_validation_error(&s) {
        return ("plan_invalid", "未能确认完整操作计划，请重新说一次。");
    }
    if s.contains("wake_setting_changed_after_stop") {
        (
            "wake_setting_changed_after_stop",
            "对话已结束；语音唤醒设置刚刚被本机更改，已保留新设置。",
        )
    } else if s.contains("model_access_denied")
        || s.contains("model_not_found")
        || s.contains("model_not_available")
    {
        (
            "model_access_denied",
            "当前账户无法使用所配置的语音模型，请检查模型名称和 API 权限。",
        )
    } else if s.contains("configuration")
        || s.contains("invalid Eva configuration")
        || s.contains("invalid_proxy_url")
        || s.contains("proxy_host_missing")
        || s.contains("only HTTP CONNECT")
        || s.contains("invalid_realtime_url")
        || s.contains("invalid_header_param")
        || s.contains("invalid_input_device")
        || s.contains("version_incompatible")
    {
        (
            "configuration_invalid",
            "语音配置或服务版本不兼容，请运行 jwm-eva doctor 检查。",
        )
    } else if s.contains("realtime_tls_native_roots_unavailable")
        || s.contains("realtime_tls_configuration_failed")
    {
        (
            "tls_configuration_invalid",
            "系统证书或加密配置不可用，请检查环境后重新开启语音唤醒。",
        )
    } else if s.contains("realtime_request_rejected") {
        (
            "realtime_request_rejected",
            "语音服务拒绝了当前会话配置，请检查模型和服务版本。",
        )
    } else if s.contains("auth_failed")
        || s.contains("credential")
        || s.contains("api_key")
        || s.contains("Secret Service")
        || s.contains("OPENAI_API_KEY")
    {
        (
            "auth_failed",
            "尚未配置可用的 API 凭据，请运行 jwm-eva doctor 检查。",
        )
    } else if s.contains("utterance_timeout") || s.contains("utterance_too_long") {
        ("utterance_too_long", "语句过长，本次没有执行，请分开说。")
    } else if s.starts_with("speech_")
        || s.starts_with("silero_")
        || s == "local_voice_invalid_probability"
        || s == "local_voice_detector_failed"
    {
        (
            "local_voice_detection_failed",
            "本地人声检测未能完成，音频未继续上传，请重新开启 Eva。",
        )
    } else if s.contains("input_audio_timestamp")
        || s.contains("input_out_of_order")
        || s.starts_with("local_voice_audio_")
        || s == "local_voice_invalid_audio_source"
    {
        (
            "audio_sequence_invalid",
            "音频片段的顺序无法核对，请重新开始说一次。",
        )
    } else if s.contains("audio")
        || s.contains("PipeWire")
        || s.starts_with("pipewire_")
        || s.starts_with("pulse_")
        || s.contains("capture")
        || s.contains("playback")
    {
        (
            "audio_unavailable",
            "音频设备暂不可用，请检查麦克风和音频设置。",
        )
    } else if s.contains("quota_exceeded") {
        (
            "quota_exceeded",
            "API 额度不足。按 Alt+Ctrl+I 查看本地费用与官方账户入口。",
        )
    } else if s.contains("rate_limited")
        || s.contains("rate_limit_exceeded")
        || s.contains("rate_limit_error")
    {
        ("rate_limited", "API 请求过于频繁，请稍后重试。")
    } else if s.contains("budget") {
        (
            "budget_exceeded",
            "已达到本地费用预算。按 Alt+Ctrl+I 查看费用，本次没有继续请求模型。",
        )
    } else if s.contains("latest_plan_not_reversible") {
        (
            "latest_plan_not_reversible",
            "最近的修改无法撤销，不能跳过它撤销更早操作。",
        )
    } else if s.contains("undo_conflict") {
        ("undo_conflict", "桌面状态已被修改，本次没有覆盖这些变化。")
    } else if s.contains("undo_expired") || s.contains("nothing_to_undo") {
        ("nothing_to_undo", "当前没有仍有效的可撤销操作。")
    } else if s.contains("undo_disabled") {
        ("undo_disabled", "本地配置已关闭撤销功能。")
    } else if s.contains("execution_deadline_expired") {
        ("execution_deadline_expired", "操作已超时，未继续执行。")
    } else if s.contains("application_not_registered") {
        ("application_not_registered", "该应用尚未在本地配置中登记。")
    } else if s.contains("confirmation") {
        ("confirmation_expired", "确认已过期，本次没有继续操作。")
    } else if s.contains("target") || s.contains("context") || s.contains("focus") {
        ("stale_context", "桌面目标已变化，请重新说一次。")
    } else if s.contains("locked") {
        ("session_locked", "桌面已锁定，语音会话已结束。")
    } else if s.contains("timeout") {
        ("connect_timeout", "等待服务超时，请稍后重试。")
    } else if s.contains("unsupported") {
        ("unsupported_action", "当前桌面不支持这项操作。")
    } else if s.contains("plan")
        || s.contains("response")
        || s.contains("discovery")
        || s.contains("tool_call")
        || s.contains("tool_output")
    {
        ("plan_invalid", "未能确认完整操作计划，请重新说一次。")
    } else {
        (
            "service_unavailable",
            "语音服务暂不可用，请运行 jwm-eva doctor 检查。",
        )
    }
}

/// Fixed diagnostic labels distinguish playback failures without logging
/// provider messages, device names or other untrusted error text.
pub fn feedback_code(error: &std::io::Error) -> &'static str {
    match error.to_string().as_str() {
        "playback_queue_overflow" => "playback_queue_overflow",
        "playback_queue_stalled" => "playback_queue_stalled",
        "playback_frame_too_large" => "playback_frame_too_large",
        "pulse_playback_underrun" => "pulse_playback_underrun",
        "playback_drain_timeout" => "playback_drain_timeout",
        "pulse_playback_device_changed" => "pulse_playback_device_changed",
        "feedback_failed" => "feedback_failed",
        "feedback_timeout" => "feedback_timeout",
        _ => describe(error).0,
    }
}

pub fn session_code(error: &std::io::Error) -> &'static str {
    match error.to_string().as_str() {
        "connect_timeout" => "connect_timeout",
        "context_timeout" => "context_timeout",
        "session_timeout" => "session_timeout",
        "plan_timeout" => "plan_timeout",
        "invalid_discovery_arguments" => "invalid_discovery_arguments",
        "invalid_discovery_categories" => "invalid_discovery_categories",
        "input_out_of_order" => "input_out_of_order",
        "input_audio_timestamp_overflow" => "input_audio_timestamp_overflow",
        "realtime_write_timeout" => "realtime_write_timeout",
        "realtime_ping_timeout" => "realtime_ping_timeout",
        "interrupt_clear_timeout" => "interrupt_clear_timeout",
        "pulse_capture_timeout" => "pulse_capture_timeout",
        _ if error.kind() == std::io::ErrorKind::TimedOut => "local_io_timeout",
        _ => feedback_code(error),
    }
}

/// Model arguments can appear in schema errors (for example an unknown key).
/// Keep only a known local validation category, never the supplied key/value.
pub fn plan_code(error: &std::io::Error) -> &'static str {
    const CODES: &[&str] = &[
        "plan_invalid_json",
        "plan_invalid_structure",
        "plan_not_object",
        "missing_plan_field",
        "plan_missing",
        "missing_session_intent",
        "invalid_session_intent",
        "session_control_requires_noop",
        "session_control_task_conflict",
        "missing_task_intent",
        "invalid_task_intent",
        "undo_plan_requires_new_task",
        "action_schema_not_loaded",
        "unsupported_action",
        "unknown_argument",
        "missing_required_argument",
        "expected_object",
        "expected_string",
        "expected_integer",
        "expected_number",
        "expected_null",
        "expected_boolean",
        "expected_array",
        "no_schema_alternative_matches",
        "constant_mismatch",
        "value_not_in_enum",
        "string_length_out_of_range",
        "integer_out_of_range",
        "number_out_of_range",
        "array_length_out_of_range",
        "duplicate_array_item",
        "execute_requires_actions_without_question",
        "clarify_requires_question_without_actions",
        "noop_cannot_contain_actions_or_question",
        "undo_must_be_only_action",
        "too_many_actions",
        "plan_text_too_long",
        "target_window_gone",
        "target_monitor_gone",
        "session_locked",
        "expected_exactly_one_tool_call",
        "unexpected_tool_output",
        "missing_final_tool_call",
        "response_not_completed",
        "reused_tool_call_id",
        "invalid_call_id",
        "invalid_tool_arguments",
    ];
    let text = error.to_string();
    let code = text.split(':').next().unwrap_or_default();
    CODES
        .iter()
        .copied()
        .find(|known| *known == code)
        .unwrap_or("plan_invalid")
}

#[cfg(test)]
mod tests {
    #[test]
    fn planning_diagnostics_never_include_untrusted_fields_or_error_bodies() {
        for (message, expected) in [
            (
                "unknown_argument:private-credential-and-window-title",
                "unknown_argument",
            ),
            (
                "execute_requires_actions_without_question: actions=1 question_present=true",
                "execute_requires_actions_without_question",
            ),
            ("undo_plan_requires_new_task", "undo_plan_requires_new_task"),
            (
                "private error body containing unknown_argument",
                "plan_invalid",
            ),
            ("unrecognized_private_code:private-value", "plan_invalid"),
        ] {
            assert_eq!(super::plan_code(&std::io::Error::other(message)), expected);
        }
    }

    #[test]
    fn feedback_diagnostics_only_expose_fixed_codes() {
        assert_eq!(
            super::feedback_code(&std::io::Error::other("playback_queue_overflow")),
            "playback_queue_overflow"
        );
        assert_eq!(
            super::feedback_code(&std::io::Error::other("pulse_playback_underrun")),
            "pulse_playback_underrun"
        );
        assert_eq!(
            super::feedback_code(&std::io::Error::other(
                "playback_queue_overflow: private device and provider details"
            )),
            "audio_unavailable"
        );
        assert_eq!(
            super::session_code(&std::io::Error::other("realtime_write_timeout")),
            "realtime_write_timeout"
        );
        assert_eq!(
            super::session_code(&std::io::Error::other(
                "realtime_write_timeout: private transport details"
            )),
            "connect_timeout"
        );
    }

    #[test]
    fn execution_and_billing_errors_keep_their_public_meaning() {
        for code in [
            "latest_plan_not_reversible",
            "execution_deadline_expired",
            "application_not_registered",
            "quota_exceeded",
            "rate_limited",
        ] {
            let (actual, text) = super::describe(&std::io::Error::other(code));
            assert_eq!(actual, code);
            assert!(!text.contains(code));
        }
        let (code, _) = super::describe(&std::io::Error::other(
            "expected_exactly_one_tool_call: output_items=2 function_calls=2",
        ));
        assert_eq!(code, "plan_invalid");
        for error in [
            "pulse_context_failed",
            "pulse_source_unavailable_or_monitor",
            "pipewire_probe_timeout",
        ] {
            assert_eq!(
                super::describe(&std::io::Error::other(error)).0,
                "audio_unavailable"
            );
        }
    }
    #[test]
    fn retry_categories_and_public_errors_never_echo_sensitive_details() {
        use super::{Recovery, recovery};
        for code in [
            "auth_failed",
            "invalid_credential",
            "api_key_file required",
            "OPENAI_API_KEY is not configured",
            "model_access_denied",
            "quota_exceeded",
            "budget_exceeded",
            "wake_configuration_mismatch",
            "invalid_proxy_url",
            "realtime_request_rejected",
        ] {
            let error = std::io::Error::other(format!("{code}: secret-key https://private/path"));
            assert_eq!(recovery(&error), Recovery::Permanent);
            let (public, text) = super::describe(&error);
            assert!(!public.contains("private"));
            assert!(!text.contains("secret-key"));
            assert!(!text.contains("https://"));
        }
        for code in ["rate_limited", "rate_limit_exceeded"] {
            assert_eq!(
                recovery(&std::io::Error::other(code)),
                Recovery::RateLimited
            );
        }
        for code in [
            "realtime_connection_failed",
            "connect_timeout",
            "realtime_service_unavailable",
            "audio_capture_device_unavailable",
            "unclassified-private-message",
        ] {
            assert_eq!(recovery(&std::io::Error::other(code)), Recovery::Transient);
        }
        assert_eq!(
            super::describe(&std::io::Error::other("wake_setting_changed_after_stop")).0,
            "wake_setting_changed_after_stop"
        );
    }
    #[test]
    fn actual_review_and_argument_validation_failures_have_fixed_plan_feedback() {
        for code in [
            "no_schema_alternative_matches", // v21 eva-016
            "missing_required_argument",     // v21 eva-134
            "action_schema_not_loaded",
            "unknown_argument:arbitrary-model-key",
            "expected_object",
            "expected_string",
            "expected_integer",
            "expected_number",
            "expected_null",
            "expected_boolean",
            "expected_array",
            "constant_mismatch",
            "value_not_in_enum",
            "string_length_out_of_range",
            "integer_out_of_range",
            "number_out_of_range",
            "array_length_out_of_range",
            "duplicate_array_item",
            "schema_type_missing",
            "schema_properties_missing",
            "schema_required_missing",
            "schema_invalid_required",
            "unsupported_schema_type",
            "execute_requires_actions_without_question",
            "clarify_requires_question_without_actions",
            "noop_cannot_contain_actions_or_question",
            "undo_must_be_only_action",
            "too_many_actions",
            "plan_text_too_long",
            "missing_required_argument: actions=1 question_present=false question_nonempty=false",
            "no_schema_alternative_matches: actions=1 question_present=false question_nonempty=false",
        ] {
            assert_eq!(
                super::describe(&std::io::Error::other(code)),
                ("plan_invalid", "未能确认完整操作计划，请重新说一次。"),
                "{code}"
            );
        }
    }
    #[test]
    fn known_validator_prefix_never_exposes_or_interprets_an_untrusted_argument_name() {
        for key in [
            "api_key=sk-private",
            "audio_device:private/path",
            "rate_limited",
            "target_window_gone",
            "https://private.invalid/secret",
        ] {
            let (category, text) =
                super::describe(&std::io::Error::other(format!("unknown_argument:{key}")));
            assert_eq!(category, "plan_invalid");
            assert_eq!(text, "未能确认完整操作计划，请重新说一次。");
            assert!(!text.contains(key));
        }
    }
    #[test]
    fn schema_words_inside_unrelated_errors_do_not_override_existing_categories() {
        for (message, category) in [
            ("auth_failed: missing_required_argument", "auth_failed"),
            (
                "rate_limited: no_schema_alternative_matches",
                "rate_limited",
            ),
            ("quota_exceeded: expected_string", "quota_exceeded"),
            (
                "pulse_capture_failed: expected_integer",
                "audio_unavailable",
            ),
            (
                "input_audio_timestamp_overflow: expected_number",
                "audio_sequence_invalid",
            ),
            (
                "speech_target_gone: expected_array",
                "local_voice_detection_failed",
            ),
            (
                "target_window_gone: actions=1 question_present=false question_nonempty=false",
                "stale_context",
            ),
            ("target_window_gone", "stale_context"), // v21 eva-116 unchanged
            (
                "connect_timeout: no_schema_alternative_matches",
                "connect_timeout",
            ),
            (
                "realtime_service_unavailable: missing_required_argument",
                "service_unavailable",
            ),
            (
                "realtime_request_rejected: expected_string",
                "realtime_request_rejected",
            ),
            ("unsupported_action", "unsupported_action"),
            ("session_locked", "session_locked"),
            (
                "private failure mentioning no_schema_alternative_matches",
                "service_unavailable",
            ),
            ("not_missing_required_argument", "service_unavailable"),
            ("missing_required_argument_extra", "service_unavailable"),
            (
                "missing_required_argument: arbitrary provider details",
                "service_unavailable",
            ),
            (
                "missing_required_argument: actions=private question_present=false question_nonempty=false",
                "service_unavailable",
            ),
            ("unclassified-private-message", "service_unavailable"),
        ] {
            assert_eq!(
                super::describe(&std::io::Error::other(message)).0,
                category,
                "{message}"
            );
        }
    }
}
