//! Public errors carry stable codes and short user text, never transport bodies.
pub fn describe(error: &std::io::Error) -> (&'static str, &'static str) {
    let s = error.to_string();
    if s.contains("auth_failed")
        || s.contains("credential")
        || s.contains("Secret Service")
        || s.contains("OPENAI_API_KEY")
    {
        (
            "auth_failed",
            "尚未配置可用的 API 凭据，请运行 jwm-eva doctor 检查。",
        )
    } else if s.contains("utterance_timeout") || s.contains("utterance_too_long") {
        ("utterance_too_long", "语句过长，本次没有执行，请分开说。")
    } else if s.contains("input_audio_timestamp") || s.contains("input_out_of_order") {
        (
            "audio_sequence_invalid",
            "音频片段的顺序无法核对，请重新开始说一次。",
        )
    } else if s.contains("audio")
        || s.contains("PipeWire")
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
    } else if s.contains("rate_limited") {
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
    } else if s.contains("plan") || s.contains("response") || s.contains("discovery") {
        ("plan_invalid", "未能确认完整操作计划，请重新说一次。")
    } else {
        (
            "service_unavailable",
            "语音服务暂不可用，请运行 jwm-eva doctor 检查。",
        )
    }
}

#[cfg(test)]
mod tests {
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
    }
}
